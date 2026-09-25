//! Lock POSIX sui file (fcntl F_GETLK/F_SETLK/F_SETLKW).
//!
//! I processi guest vivono tutti nello stesso processo host, quindi i lock
//! dell'host non distinguerebbero i proprietari: la tabella è nostra, per
//! (dispositivo, inode), con regioni `[start, end)` per processo.

use super::abi::*;
use super::fs::Kind;
use super::{Kernel, Pid};

pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    pub owner: Pid,
    pub start: u64,
    /// Esclusivo; `u64::MAX` = fino all'infinito.
    pub end: u64,
    pub kind: i16,
}

#[derive(Default)]
pub struct LockTable {
    files: std::collections::HashMap<(u64, u64), Vec<Lock>>,
    /// Chi è fermo in F_SETLKW, e su quale richiesta (per trovare i cicli).
    waiting: std::collections::HashMap<Pid, ((u64, u64), u64, u64, i16)>,
    /// Proprietari OFD (id negativi) e la loro descrizione di file: quando
    /// l'ultimo riferimento sparisce i lock si rilasciano.
    ofd: std::collections::HashMap<Pid, std::rc::Weak<std::cell::RefCell<super::fs::OpenFile>>>,
    next_ofd: Pid,
}

impl LockTable {
    fn conflict(&self, key: (u64, u64), owner: Pid, start: u64, end: u64, kind: i16) -> Option<Lock> {
        self.files.get(&key)?.iter().copied().find(|l| {
            l.owner != owner && l.start < end && start < l.end && (l.kind == F_WRLCK || kind == F_WRLCK)
        })
    }

    /// Applica un lock (o lo sblocco) del proprietario, dividendo e fondendo
    /// le sue regioni come fa Linux.
    fn apply(&mut self, key: (u64, u64), owner: Pid, start: u64, end: u64, kind: i16) {
        let list = self.files.entry(key).or_default();
        let mut out = Vec::with_capacity(list.len() + 2);
        for l in list.drain(..) {
            if l.owner != owner || l.end <= start || end <= l.start {
                out.push(l);
                continue;
            }
            if l.start < start {
                out.push(Lock { end: start, ..l });
            }
            if end < l.end {
                out.push(Lock { start: end, ..l });
            }
        }
        if kind != F_UNLCK {
            out.push(Lock { owner, start, end, kind });
        }
        // Fondi le regioni contigue o sovrapposte dello stesso proprietario e tipo.
        out.sort_by_key(|l| (l.owner, l.kind, l.start));
        let mut merged: Vec<Lock> = Vec::with_capacity(out.len());
        for l in out {
            if let Some(last) = merged.last_mut()
                && last.owner == l.owner
                && last.kind == l.kind
                && l.start <= last.end
            {
                last.end = last.end.max(l.end);
                continue;
            }
            merged.push(l);
        }
        // Come la lista di Linux: per proprietario, in ordine di inizio
        // (F_GETLK restituisce il primo conflitto).
        merged.sort_by_key(|l| (l.owner, l.start));
        *list = merged;
    }

    /// Vero se aspettare `blocker` chiuderebbe un ciclo di attese che torna a
    /// `owner` (posix_locks_deadlock, con lo stesso limite di passi).
    fn deadlock(&self, owner: Pid, mut blocker: Pid) -> bool {
        for _ in 0..10 {
            if blocker == owner {
                return true;
            }
            let Some(&(key, start, end, kind)) = self.waiting.get(&blocker) else { return false };
            match self.conflict(key, blocker, start, end, kind) {
                Some(l) => blocker = l.owner,
                None => return false,
            }
        }
        false
    }

    /// Toglie i lock delle descrizioni OFD ormai chiuse.
    fn purge_ofd(&mut self) {
        let dead: Vec<Pid> =
            self.ofd.iter().filter(|(_, w)| w.strong_count() == 0).map(|(&o, _)| o).collect();
        for o in dead {
            self.ofd.remove(&o);
            self.release(o, None);
        }
    }

    /// Il proprietario OFD di una descrizione di file (assegnato alla prima volta).
    fn ofd_owner(&mut self, f: &std::rc::Rc<std::cell::RefCell<super::fs::OpenFile>>) -> Pid {
        let id = f.borrow().ofd_owner;
        if id != 0 {
            return id;
        }
        self.next_ofd -= 1;
        let id = self.next_ofd.min(-2);
        self.next_ofd = id;
        f.borrow_mut().ofd_owner = id;
        self.ofd.insert(id, std::rc::Rc::downgrade(f));
        id
    }

    /// Rilascia tutti i lock di `owner` (su un file o ovunque).
    pub fn release(&mut self, owner: Pid, key: Option<(u64, u64)>) {
        if key.is_none() {
            self.waiting.remove(&owner);
        }
        for (k, list) in self.files.iter_mut() {
            if key.is_none_or(|x| x == *k) {
                list.retain(|l| l.owner != owner);
            }
        }
    }
}

/// Esito di F_SETLKW quando deve aspettare.
pub enum LockResult {
    Done(i64),
    Wait,
}

impl Kernel {
    /// fcntl per i lock: `cmd` è F_GETLK (5), F_SETLK (6), F_SETLKW (7) o le
    /// varianti OFD F_OFD_GETLK (36), F_OFD_SETLK (37), F_OFD_SETLKW (38).
    pub(super) fn fcntl_lock(&mut self, t: usize, fd: i64, cmd: u64, arg: u64) -> Result<LockResult, i64> {
        use std::os::unix::fs::MetadataExt;
        let f = self.tasks[t].files.borrow().get(fd)?;
        let mm = self.tasks[t].mm.clone();
        let raw = read_bytes(&mut mm.borrow_mut().mem, arg, 32)?;
        let (key, size, readable, writable, pos) = {
            let mut fb = f.borrow_mut();
            let (readable, writable) = (fb.readable(), fb.writable());
            let pos = fb.lseek(0, 1).unwrap_or(0) as u64;
            // Sui file dell'host la chiave è l'inode; il resto (pipe, console)
            // è un oggetto del kernel emulato, identificato dalla descrizione.
            let (key, size) = match &fb.kind {
                Kind::Host { file, .. } => {
                    let m = file.metadata().map_err(|e| host_errno(&e))?;
                    ((m.dev(), m.ino()), m.len())
                }
                Kind::Path { .. } => return Err(EBADF),
                _ => ((u64::MAX, std::rc::Rc::as_ptr(&f) as u64), 0),
            };
            (key, size, readable, writable, pos)
        };
        let ofd = matches!(cmd, 36..=38);
        let cmd = if ofd { cmd - 31 } else { cmd };
        if ofd && i32::from_le_bytes(raw[24..28].try_into().unwrap()) != 0 {
            return Err(EINVAL); // l_pid deve essere 0
        }
        self.locks.purge_ofd();
        let kind = i16::from_le_bytes([raw[0], raw[1]]);
        let whence = i16::from_le_bytes([raw[2], raw[3]]);
        let l_start = i64::from_le_bytes(raw[8..16].try_into().unwrap());
        let l_len = i64::from_le_bytes(raw[16..24].try_into().unwrap());
        let base: i64 = match whence {
            0 => 0,
            1 => pos as i64,
            2 => size as i64,
            _ => return Err(EINVAL),
        };
        let mut start = base.checked_add(l_start).ok_or(EINVAL)?;
        let mut end: i64 = if l_len == 0 { i64::MAX } else { start.checked_add(l_len).ok_or(EINVAL)? };
        if l_len < 0 {
            (start, end) = (end, start);
        }
        if start < 0 {
            return Err(EINVAL);
        }
        let (start, end) = (start as u64, if end == i64::MAX { u64::MAX } else { end as u64 });
        if !matches!(kind, F_RDLCK | F_WRLCK | F_UNLCK) {
            return Err(EINVAL);
        }
        let owner = if ofd { self.locks.ofd_owner(&f) } else { self.tasks[t].tgid };
        match cmd {
            5 => {
                if kind == F_UNLCK {
                    return Err(EINVAL);
                }
                let mut out = raw.clone();
                match self.locks.conflict(key, owner, start, end, kind) {
                    Some(l) => {
                        out[0..2].copy_from_slice(&l.kind.to_le_bytes());
                        out[2..4].copy_from_slice(&0i16.to_le_bytes());
                        out[8..16].copy_from_slice(&(l.start as i64).to_le_bytes());
                        let len = if l.end == u64::MAX { 0 } else { (l.end - l.start) as i64 };
                        out[16..24].copy_from_slice(&len.to_le_bytes());
                        // I lock OFD non hanno un processo: l_pid = -1.
                        let pid = if l.owner < 0 { -1 } else { l.owner };
                        out[24..28].copy_from_slice(&pid.to_le_bytes());
                    }
                    None => out[0..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
                }
                write_bytes(&mut mm.borrow_mut().mem, arg, &out)?;
                Ok(LockResult::Done(0))
            }
            _ => {
                if kind == F_RDLCK && !readable || kind == F_WRLCK && !writable {
                    return Err(EBADF);
                }
                if kind != F_UNLCK
                    && let Some(l) = self.locks.conflict(key, owner, start, end, kind)
                {
                    if cmd != 7 {
                        return Err(EAGAIN);
                    }
                    if !ofd && self.locks.deadlock(owner, l.owner) {
                        self.locks.waiting.remove(&owner);
                        return Err(EDEADLK);
                    }
                    self.locks.waiting.insert(owner, (key, start, end, kind));
                    return Ok(LockResult::Wait);
                }
                self.locks.waiting.remove(&owner);
                self.locks.apply(key, owner, start, end, kind);
                Ok(LockResult::Done(0))
            }
        }
    }

    /// Alla chiusura di un descrittore: Linux rilascia tutti i lock POSIX del
    /// processo su quel file.
    pub(super) fn release_locks_on_close(
        &mut self,
        t: usize,
        file: &std::cell::RefCell<super::fs::OpenFile>,
    ) {
        use std::os::unix::fs::MetadataExt;
        if let Kind::Host { file, .. } = &file.borrow().kind
            && let Ok(m) = file.metadata()
        {
            let owner = self.tasks[t].tgid;
            self.locks.release(owner, Some((m.dev(), m.ino())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_merge_conflict() {
        let mut t = LockTable::default();
        let k = (1, 2);
        t.apply(k, 10, 0, 100, F_WRLCK);
        t.apply(k, 10, 40, 60, F_RDLCK);
        assert_eq!(t.files[&k].len(), 3);
        assert!(t.conflict(k, 11, 45, 50, F_RDLCK).is_none());
        assert_eq!(t.conflict(k, 11, 10, 20, F_RDLCK).unwrap().kind, F_WRLCK);
        t.apply(k, 10, 40, 60, F_WRLCK);
        assert_eq!(t.files[&k], vec![Lock { owner: 10, start: 0, end: 100, kind: F_WRLCK }]);
        t.release(10, None);
        assert!(t.files[&k].is_empty());
    }

    #[test]
    fn deadlock_cycle() {
        let mut t = LockTable::default();
        let k = (1, 2);
        t.apply(k, 2, 9, 15, F_WRLCK);
        t.apply(k, 3, 17, 23, F_WRLCK);
        // 2 aspetta il lock di 3: nessun ciclo.
        assert!(!t.deadlock(2, 3));
        t.waiting.insert(2, (k, 17, 23, F_WRLCK));
        // 3 che aspetta 2 chiuderebbe il ciclo.
        assert!(t.deadlock(3, 2));
        assert!(!t.deadlock(4, 2));
    }
}
