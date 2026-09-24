//! Interfaccia verso la memoria del guest e spazio d'indirizzamento utente.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Fetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault {
    pub addr: u64,
    pub access: Access,
}

/// Memoria vista dalla CPU (indirizzi virtuali, little-endian).
pub trait Memory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault>;
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault>;
    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault>;
}

/// Permessi di una regione.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const NONE: Perm = Perm { read: false, write: false, exec: false };
    pub const R: Perm = Perm { read: true, write: false, exec: false };
    pub const RW: Perm = Perm { read: true, write: true, exec: false };
    pub const RX: Perm = Perm { read: true, write: false, exec: true };
    pub const RWX: Perm = Perm { read: true, write: true, exec: true };

    /// Dai bit PROT_READ (1), PROT_WRITE (2), PROT_EXEC (4).
    pub fn from_prot(prot: u32) -> Perm {
        Perm { read: prot & 1 != 0, write: prot & 2 != 0, exec: prot & 4 != 0 }
    }

    fn allows(self, a: Access) -> bool {
        match a {
            Access::Read => self.read,
            Access::Write => self.write,
            Access::Fetch => self.exec,
        }
    }
}

const PAGE: usize = 4096;

/// Memoria dietro una regione.
#[derive(Clone)]
enum Backing {
    /// Privata e pigra: solo le pagine scritte esistono, le altre valgono
    /// zero. Un fork copia solo le pagine presenti.
    Pages { pages: BTreeMap<usize, Box<[u8; PAGE]>>, base: usize },
    /// Condivisa (MAP_SHARED): resta la stessa dopo un fork.
    Shared(Rc<RefCell<Vec<u8>>>, usize),
}

#[derive(Clone)]
struct Region {
    backing: Backing,
    len: usize,
    perm: Perm,
}

impl Region {
    /// Regione privata con contenuto iniziale (le pagine a zero non si
    /// allocano).
    fn own(data: Vec<u8>, perm: Perm) -> Region {
        let mut pages = BTreeMap::new();
        for (i, chunk) in data.chunks(PAGE).enumerate() {
            if chunk.iter().any(|&b| b != 0) {
                let mut p = Box::new([0u8; PAGE]);
                p[..chunk.len()].copy_from_slice(chunk);
                pages.insert(i, p);
            }
        }
        Region { len: data.len(), backing: Backing::Pages { pages, base: 0 }, perm }
    }

    /// Regione privata tutta a zero, senza allocare nulla.
    fn zeroed(len: usize, perm: Perm) -> Region {
        Region { len, backing: Backing::Pages { pages: BTreeMap::new(), base: 0 }, perm }
    }

    fn read(&self, off: usize, out: &mut [u8]) {
        match &self.backing {
            Backing::Pages { pages, base } => {
                let mut done = 0;
                while done < out.len() {
                    let abs = base + off + done;
                    let (pi, po) = (abs / PAGE, abs % PAGE);
                    let n = (PAGE - po).min(out.len() - done);
                    match pages.get(&pi) {
                        Some(p) => out[done..done + n].copy_from_slice(&p[po..po + n]),
                        None => out[done..done + n].fill(0),
                    }
                    done += n;
                }
            }
            Backing::Shared(b, base) => {
                let b = b.borrow();
                let start = base + off;
                for (i, o) in out.iter_mut().enumerate() {
                    *o = b.get(start + i).copied().unwrap_or(0);
                }
            }
        }
    }

    fn write(&mut self, off: usize, data: &[u8]) {
        match &mut self.backing {
            Backing::Pages { pages, base } => {
                let mut done = 0;
                while done < data.len() {
                    let abs = *base + off + done;
                    let (pi, po) = (abs / PAGE, abs % PAGE);
                    let n = (PAGE - po).min(data.len() - done);
                    let p = pages.entry(pi).or_insert_with(|| Box::new([0u8; PAGE]));
                    p[po..po + n].copy_from_slice(&data[done..done + n]);
                    done += n;
                }
            }
            Backing::Shared(b, base) => {
                let mut b = b.borrow_mut();
                let start = *base + off;
                if b.len() < start + data.len() {
                    b.resize(start + data.len(), 0);
                }
                b[start..start + data.len()].copy_from_slice(data);
            }
        }
    }

    /// Divide la regione a `k` byte dall'inizio; restituisce la coda.
    fn split_off(&mut self, k: usize) -> Region {
        let tail = match &mut self.backing {
            Backing::Pages { pages, base } => {
                // La coda ha le stesse pagine logiche, spostata di k byte.
                let abs = *base + k;
                let first = abs / PAGE;
                let tail_pages = pages.split_off(&first);
                if !abs.is_multiple_of(PAGE)
                    && let Some(p) = tail_pages.get(&first)
                {
                    pages.insert(first, p.clone());
                }
                Backing::Pages { pages: tail_pages, base: abs }
            }
            Backing::Shared(b, base) => Backing::Shared(b.clone(), *base + k),
        };
        let r = Region { backing: tail, len: self.len - k, perm: self.perm };
        self.len = k;
        r
    }
}

/// Spazio d'indirizzamento di un processo in user mode: regioni disgiunte
/// con permessi, che si possono sovrascrivere, togliere e riproteggere a
/// pezzi (mmap MAP_FIXED, munmap, mprotect). È l'implementazione di
/// [`Memory`] per il livello Linux user mode; da M3 la traduzione la fa la MMU.
#[derive(Default, Clone)]
pub struct UserMemory {
    regions: BTreeMap<u64, Region>,
}

impl fmt::Debug for UserMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut l = f.debug_list();
        for (b, r) in &self.regions {
            l.entry(&format_args!("{:#x}..{:#x} {:?}", b, b + r.len as u64, r.perm));
        }
        l.finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub base: u64,
}

impl UserMemory {
    pub fn new() -> Self {
        Self::default()
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        if let Some((b, r)) = self.regions.range(..end).next_back() {
            return b + r.len as u64 > start;
        }
        false
    }

    /// Mappa `[base, base+data.len())`, che non deve sovrapporsi ad altro.
    pub fn map(&mut self, base: u64, data: Vec<u8>, perm: Perm) -> Result<(), Overlap> {
        let end = base.checked_add(data.len() as u64).ok_or(Overlap { base })?;
        if data.is_empty() || self.overlaps(base, end) {
            return Err(Overlap { base });
        }
        self.regions.insert(base, Region::own(data, perm));
        Ok(())
    }

    /// Come [`map`](Self::map) ma sostituisce ciò che c'era (MAP_FIXED).
    pub fn map_fixed(&mut self, base: u64, data: Vec<u8>, perm: Perm) {
        if data.is_empty() {
            return;
        }
        self.unmap(base, base + data.len() as u64);
        self.regions.insert(base, Region::own(data, perm));
    }

    /// Mappa `len` byte a zero senza allocare (mmap anonima privata).
    pub fn map_zeroed(&mut self, base: u64, len: usize, perm: Perm) {
        if len == 0 {
            return;
        }
        self.unmap(base, base + len as u64);
        self.regions.insert(base, Region::zeroed(len, perm));
    }

    /// Mappa `len` byte del buffer condiviso `buf` da `off` in poi a `base`,
    /// sostituendo ciò che c'era (MAP_SHARED).
    pub fn map_shared(&mut self, base: u64, buf: Rc<RefCell<Vec<u8>>>, off: usize, len: usize, perm: Perm) {
        if len == 0 {
            return;
        }
        self.unmap(base, base + len as u64);
        self.regions.insert(base, Region { backing: Backing::Shared(buf, off), len, perm });
    }

    /// Divide la regione che contiene `at` (se c'è) in modo che `at` sia un
    /// confine.
    fn split_at(&mut self, at: u64) {
        let Some((&b, r)) = self.regions.range(..at).next_back() else { return };
        let end = b + r.len as u64;
        if at <= b || at >= end {
            return;
        }
        let r = self.regions.get_mut(&b).unwrap();
        let tail = r.split_off((at - b) as usize);
        self.regions.insert(at, tail);
    }

    /// Toglie ogni mappatura in `[start, end)`.
    pub fn unmap(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.split_at(start);
        self.split_at(end);
        let keys: Vec<u64> = self.regions.range(start..end).map(|(&k, _)| k).collect();
        for k in keys {
            self.regions.remove(&k);
        }
    }

    /// Cambia i permessi di `[start, end)`. Fallisce (senza cambiare nulla)
    /// se una parte dell'intervallo non è mappata.
    pub fn protect(&mut self, start: u64, end: u64, perm: Perm) -> Result<(), MemFault> {
        if !self.is_mapped(start, end) {
            return Err(MemFault { addr: start, access: Access::Read });
        }
        self.split_at(start);
        self.split_at(end);
        for (_, r) in self.regions.range_mut(start..end) {
            r.perm = perm;
        }
        Ok(())
    }

    /// Vero se ogni byte di `[start, end)` è mappato.
    pub fn is_mapped(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        while at < end {
            match self.find(at) {
                Some((b, len)) => at = b + len,
                None => return false,
            }
        }
        true
    }

    /// Regione che contiene `addr`: (base, lunghezza).
    fn find(&self, addr: u64) -> Option<(u64, u64)> {
        let (&b, r) = self.regions.range(..=addr).next_back()?;
        let len = r.len as u64;
        (addr < b + len).then_some((b, len))
    }

    /// Permessi della pagina che contiene `addr`.
    pub fn perm_at(&self, addr: u64) -> Option<Perm> {
        let (b, _) = self.find(addr)?;
        Some(self.regions[&b].perm)
    }

    /// Cerca dall'alto verso il basso un buco di `len` byte dentro
    /// `[bottom, top)`.
    pub fn find_free(&self, len: u64, bottom: u64, top: u64) -> Option<u64> {
        let mut end = top;
        for (&b, r) in self.regions.range(..top).rev() {
            let r_end = b + r.len as u64;
            let start = r_end.max(bottom);
            if end >= start && end - start >= len {
                return Some(end - len);
            }
            end = end.min(b);
            if end < bottom.saturating_add(len) {
                return None;
            }
        }
        (end >= bottom.saturating_add(len)).then(|| end - len)
    }

    /// Intervalli mappati, per /proc/self/maps e il debug.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64, Perm)> + '_ {
        self.regions.iter().map(|(&b, r)| (b, b + r.len as u64, r.perm))
    }

    /// Regione che contiene tutto `[addr, addr+len)`, con il permesso `a`.
    /// `Ok(None)` se l'intervallo attraversa più regioni (percorso lento).
    fn locate(&self, addr: u64, len: usize, a: Access) -> Result<Option<(u64, usize)>, MemFault> {
        match self.find(addr) {
            Some((b, rlen)) => {
                let off = (addr - b) as usize;
                if !self.regions[&b].perm.allows(a) {
                    return Err(MemFault { addr, access: a });
                }
                if off + len <= rlen as usize { Ok(Some((b, off))) } else { Ok(None) }
            }
            None => Err(MemFault { addr, access: a }),
        }
    }

    /// Controlla byte per byte un intervallo che attraversa più regioni.
    fn check_slow(&self, addr: u64, len: usize, a: Access) -> Result<(), MemFault> {
        for k in 0..len {
            let p = addr.wrapping_add(k as u64);
            match self.find(p) {
                Some((b, _)) if self.regions[&b].perm.allows(a) => {}
                _ => return Err(MemFault { addr: p, access: a }),
            }
        }
        Ok(())
    }

    /// Scrittura che ignora i permessi (caricatore ELF, kernel che prepara
    /// lo stack su pagine già mappate).
    pub fn poke(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        for (k, &byte) in data.iter().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let Some((b, _)) = self.find(p) else {
                return Err(MemFault { addr: p, access: Access::Write });
            };
            let r = self.regions.get_mut(&b).unwrap();
            r.write((p - b) as usize, &[byte]);
        }
        Ok(())
    }
}

impl Memory for UserMemory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        let len = buf.len();
        if let Some((b, off)) = self.locate(addr, len, Access::Read)? {
            self.regions[&b].read(off, buf);
            return Ok(());
        }
        self.check_slow(addr, len, Access::Read)?;
        for (k, o) in buf.iter_mut().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("verificato sopra");
            let mut one = [0u8; 1];
            self.regions[&b].read((p - b) as usize, &mut one);
            *o = one[0];
        }
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        if let Some((b, off)) = self.locate(addr, data.len(), Access::Write)? {
            self.regions.get_mut(&b).unwrap().write(off, data);
            return Ok(());
        }
        self.check_slow(addr, data.len(), Access::Write)?;
        for (k, &byte) in data.iter().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("verificato sopra");
            self.regions.get_mut(&b).unwrap().write((p - b) as usize, &[byte]);
        }
        Ok(())
    }

    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault> {
        let mut w = [0u8; 4];
        if let Some((b, off)) = self.locate(addr, 4, Access::Fetch)? {
            self.regions[&b].read(off, &mut w);
            return Ok(u32::from_le_bytes(w));
        }
        self.check_slow(addr, 4, Access::Fetch)?;
        for (k, o) in w.iter_mut().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("verificato sopra");
            let mut one = [0u8; 1];
            self.regions[&b].read((p - b) as usize, &mut one);
            *o = one[0];
        }
        Ok(u32::from_le_bytes(w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_and_access() {
        let mut m = UserMemory::new();
        m.map(0x1000, vec![0; 0x1000], Perm::RW).unwrap();
        m.map(0x2000, vec![0; 0x1000], Perm::RW).unwrap();
        assert!(m.map(0x1800, vec![0; 16], Perm::R).is_err());
        // attraversa due regioni
        m.write(0x1ffe, &[1, 2, 3, 4]).unwrap();
        let mut b = [0; 4];
        m.read(0x1ffe, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4]);
        assert!(m.read(0x2ffe, &mut b).is_err());
        assert!(m.fetch(0x1000).is_err());
    }

    #[test]
    fn unmap_protect_split() {
        let mut m = UserMemory::new();
        m.map(0x10000, vec![7; 0x4000], Perm::RW).unwrap();
        m.unmap(0x11000, 0x12000);
        assert!(!m.is_mapped(0x10000, 0x14000));
        assert!(m.is_mapped(0x12000, 0x14000));
        m.protect(0x12000, 0x13000, Perm::R).unwrap();
        assert!(m.write(0x12000, &[1]).is_err());
        assert!(m.write(0x13000, &[1]).is_ok());
        let mut b = [0u8; 1];
        m.read(0x13fff, &mut b).unwrap();
        assert_eq!(b[0], 7);
        m.map_fixed(0x10800, vec![9; 0x1000], Perm::R);
        m.read(0x10800, &mut b).unwrap();
        assert_eq!(b[0], 9);
        assert_eq!(m.find_free(0x1000, 0x1000, 0x10000), Some(0xf000));
        assert_eq!(m.find_free(0x2000, 0x10000, 0x14000), None);
        m.poke(0x12000, &[5]).unwrap();
        m.read(0x12000, &mut b).unwrap();
        assert_eq!(b[0], 5);
    }

    #[test]
    fn shared_survives_clone_and_split() {
        let mut a = UserMemory::new();
        let buf = Rc::new(RefCell::new(vec![0u8; 0x2000]));
        a.map_shared(0x40000, buf.clone(), 0, 0x2000, Perm::RW);
        let mut b = a.clone(); // come un fork
        b.write(0x41000, &[42]).unwrap();
        let mut x = [0u8; 1];
        a.read(0x41000, &mut x).unwrap();
        assert_eq!(x[0], 42);
        a.protect(0x40000, 0x41000, Perm::R).unwrap(); // divide la regione
        b.write(0x41001, &[7]).unwrap();
        a.read(0x41001, &mut x).unwrap();
        assert_eq!(x[0], 7);
        assert_eq!(buf.borrow()[0x1001], 7);
    }
}
