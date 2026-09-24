//! Interfaccia verso la memoria del guest e spazio d'indirizzamento utente.

use std::collections::BTreeMap;
use std::fmt;

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

#[derive(Clone)]
struct Region {
    data: Vec<u8>,
    perm: Perm,
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
            l.entry(&format_args!("{:#x}..{:#x} {:?}", b, b + r.data.len() as u64, r.perm));
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
            return b + r.data.len() as u64 > start;
        }
        false
    }

    /// Mappa `[base, base+data.len())`, che non deve sovrapporsi ad altro.
    pub fn map(&mut self, base: u64, data: Vec<u8>, perm: Perm) -> Result<(), Overlap> {
        let end = base.checked_add(data.len() as u64).ok_or(Overlap { base })?;
        if data.is_empty() || self.overlaps(base, end) {
            return Err(Overlap { base });
        }
        self.regions.insert(base, Region { data, perm });
        Ok(())
    }

    /// Come [`map`](Self::map) ma sostituisce ciò che c'era (MAP_FIXED).
    pub fn map_fixed(&mut self, base: u64, data: Vec<u8>, perm: Perm) {
        if data.is_empty() {
            return;
        }
        self.unmap(base, base + data.len() as u64);
        self.regions.insert(base, Region { data, perm });
    }

    /// Divide la regione che contiene `at` (se c'è) in modo che `at` sia un
    /// confine.
    fn split_at(&mut self, at: u64) {
        let Some((&b, r)) = self.regions.range(..at).next_back() else { return };
        let end = b + r.data.len() as u64;
        if at <= b || at >= end {
            return;
        }
        let r = self.regions.get_mut(&b).unwrap();
        let tail = r.data.split_off((at - b) as usize);
        let perm = r.perm;
        self.regions.insert(at, Region { data: tail, perm });
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
        let len = r.data.len() as u64;
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
            let r_end = b + r.data.len() as u64;
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
        self.regions.iter().map(|(&b, r)| (b, b + r.data.len() as u64, r.perm))
    }

    /// Accesso a un intervallo interamente dentro una regione, oppure byte
    /// per byte quando attraversa regioni adiacenti.
    fn access(
        &mut self,
        addr: u64,
        len: usize,
        a: Access,
        mut f: impl FnMut(&mut [u8], usize),
    ) -> Result<(), MemFault> {
        let fault = |addr| MemFault { addr, access: a };
        if let Some((b, rlen)) = self.find(addr) {
            let r = self.regions.get_mut(&b).unwrap();
            let off = (addr - b) as usize;
            if off + len <= rlen as usize {
                if !r.perm.allows(a) {
                    return Err(fault(addr));
                }
                f(&mut r.data[off..off + len], 0);
                return Ok(());
            }
        }
        // Percorso lento: controlla tutti i byte prima di toccarne uno.
        for k in 0..len {
            let p = addr.wrapping_add(k as u64);
            match self.find(p) {
                Some((b, _)) if self.regions[&b].perm.allows(a) => {}
                _ => return Err(fault(p)),
            }
        }
        for k in 0..len {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("verificato sopra");
            let r = self.regions.get_mut(&b).unwrap();
            let off = (p - b) as usize;
            f(&mut r.data[off..off + 1], k);
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
            r.data[(p - b) as usize] = byte;
        }
        Ok(())
    }
}

impl Memory for UserMemory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        let len = buf.len();
        self.access(addr, len, Access::Read, |src, at| buf[at..at + src.len()].copy_from_slice(src))
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.access(addr, data.len(), Access::Write, |dst, at| dst.copy_from_slice(&data[at..at + dst.len()]))
    }

    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault> {
        let mut b = [0u8; 4];
        self.access(addr, 4, Access::Fetch, |src, at| b[at..at + src.len()].copy_from_slice(src))?;
        Ok(u32::from_le_bytes(b))
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
}
