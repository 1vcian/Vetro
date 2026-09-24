//! Interfaccia verso la memoria del guest e spazio d'indirizzamento utente.

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const R: Perm = Perm { read: true, write: false, exec: false };
    pub const RW: Perm = Perm { read: true, write: true, exec: false };
    pub const RX: Perm = Perm { read: true, write: false, exec: true };
    pub const RWX: Perm = Perm { read: true, write: true, exec: true };

    fn allows(self, a: Access) -> bool {
        match a {
            Access::Read => self.read,
            Access::Write => self.write,
            Access::Fetch => self.exec,
        }
    }
}

struct Region {
    base: u64,
    data: Vec<u8>,
    perm: Perm,
}

impl Region {
    fn end(&self) -> u64 {
        self.base + self.data.len() as u64
    }
}

/// Spazio d'indirizzamento di un processo in user mode: regioni disgiunte
/// con permessi. È l'implementazione di [`Memory`] per M1–M2; da M3 la
/// traduzione la fa la MMU.
#[derive(Default)]
pub struct UserMemory {
    regions: Vec<Region>,
}

impl fmt::Debug for UserMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut l = f.debug_list();
        for r in &self.regions {
            l.entry(&format_args!("{:#x}..{:#x} {:?}", r.base, r.end(), r.perm));
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

    /// Mappa `[base, base+data.len())`. Le regioni non possono sovrapporsi.
    pub fn map(&mut self, base: u64, data: Vec<u8>, perm: Perm) -> Result<(), Overlap> {
        let end = base.checked_add(data.len() as u64).ok_or(Overlap { base })?;
        if self.regions.iter().any(|r| base < r.end() && r.base < end) {
            return Err(Overlap { base });
        }
        let at = self.regions.partition_point(|r| r.base < base);
        self.regions.insert(at, Region { base, data, perm });
        Ok(())
    }

    fn region(&self, addr: u64) -> Option<usize> {
        let i = self.regions.partition_point(|r| r.base <= addr);
        let r = self.regions.get(i.checked_sub(1)?)?;
        (addr < r.end()).then_some(i - 1)
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
        if let Some(i) = self.region(addr) {
            let r = &mut self.regions[i];
            let off = (addr - r.base) as usize;
            if off + len <= r.data.len() {
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
            match self.region(p) {
                Some(i) if self.regions[i].perm.allows(a) => {}
                _ => return Err(fault(p)),
            }
        }
        for k in 0..len {
            let p = addr.wrapping_add(k as u64);
            let i = self.region(p).expect("verificato sopra");
            let r = &mut self.regions[i];
            let off = (p - r.base) as usize;
            f(&mut r.data[off..off + 1], k);
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
}
