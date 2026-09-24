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
        *list = merged;
    }

    /// Rilascia tutti i lock di `owner` (su un file o ovunque).
    pub fn release(&mut self, owner: Pid, key: Option<(u64, u64)>) {
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
    /// fcntl per i lock: `cmd` è F_GETLK (5), F_SETLK (6) o F_SETLKW (7).
    pub(super) fn fcntl_lock(&mut self, t: usize, fd: i64, cmd: u64, arg: u64) -> Result<LockResult, i64> {
        use std::os::unix::fs::MetadataExt;
        let f = self.tasks[t].files.borrow().get(fd)?;
        let (key, size, readable, writable, pos) = {
            let mut f = f.borrow_mut();
            let (readable, writable) = (f.readable(), f.writable());
            let pos = f.lseek(0, 1).unwrap_or(0) as u64;
            let Kind::Host { file, .. } = &f.kind else { return Err(EINVAL) };
            let m = file.metadata().map_err(|e| host_errno(&e))?;
            ((m.dev(), m.ino()), m.len(), readable, writable, pos)
        };
        let mm = self.tasks[t].mm.clone();
        let raw = read_bytes(&mut mm.borrow_mut().mem, arg, 32)?;
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
        let owner = self.tasks[t].tgid;
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
                        out[24..28].copy_from_slice(&l.owner.to_le_bytes());
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
                if kind != F_UNLCK && self.locks.conflict(key, owner, start, end, kind).is_some() {
                    return if cmd == 7 { Ok(LockResult::Wait) } else { Err(EAGAIN) };
                }
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
}
