//! Adattatore da MMU + memoria fisica all'interfaccia `vetro_cpu::Memory`.

use vetro_cpu::{Access, MemFault, Memory};

use crate::fault::{Fault, FaultKind};
use crate::mmu::Mmu;
use crate::regs::PAGE_SIZE;
use crate::walk::{BusError, PhysMemory};

/// Memoria virtuale di un core: traduce ogni accesso con la MMU e lo esegue
/// sulla memoria fisica. Si costruisce per il tempo di uno o più `step`.
///
/// Un accesso a cavallo di due pagine traduce prima tutte le pagine: se una
/// fallisce non si scrive nulla. Un abort esterno sul secondo pezzo di una
/// scrittura lascia invece scritto il primo.
pub struct VirtMemory<'a, P: PhysMemory + ?Sized> {
    pub mmu: &'a mut Mmu,
    pub phys: &'a mut P,
    /// Privilegio degli accessi (0 o 1).
    pub el: u8,
}

impl<'a, P: PhysMemory + ?Sized> VirtMemory<'a, P> {
    pub fn new(mmu: &'a mut Mmu, phys: &'a mut P, el: u8) -> Self {
        VirtMemory { mmu, phys, el }
    }

    /// Fault dettagliato (con ESR e FAR) dell'ultimo accesso fallito.
    pub fn last_fault(&self) -> Option<Fault> {
        self.mmu.last_fault()
    }

    fn fail(&mut self, f: Fault) -> MemFault {
        self.mmu.set_last_fault(f);
        f.mem_fault()
    }

    fn translate(&mut self, va: u64, access: Access) -> Result<u64, MemFault> {
        match self.mmu.translate(self.phys, va, access, self.el) {
            Ok(t) => Ok(t.pa),
            Err(f) => Err(self.fail(f)),
        }
    }

    /// Chiama `f(phys, pa, offset, len)` per ogni pezzo dell'accesso che sta
    /// in una pagina.
    fn access(
        &mut self,
        va: u64,
        len: usize,
        access: Access,
        mut f: impl FnMut(&mut P, u64, usize, usize) -> Result<(), BusError>,
    ) -> Result<(), MemFault> {
        let chunk = |va: u64, rest: usize| rest.min((PAGE_SIZE - (va & (PAGE_SIZE - 1))) as usize);
        if len == 0 {
            return Ok(());
        }
        let el = self.el;
        let external = |e, va| Fault { kind: FaultKind::External(e), va, access, el };
        if chunk(va, len) < len {
            let mut off = 0;
            while off < len {
                let v = va.wrapping_add(off as u64);
                self.translate(v, access)?;
                off += chunk(v, len - off);
            }
        }
        let mut off = 0;
        while off < len {
            let v = va.wrapping_add(off as u64);
            let n = chunk(v, len - off);
            let pa = self.translate(v, access)?;
            if let Err(e) = f(self.phys, pa, off, n) {
                return Err(self.fail(external(e, v)));
            }
            off += n;
        }
        Ok(())
    }
}

impl<P: PhysMemory + ?Sized> Memory for VirtMemory<'_, P> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        let len = buf.len();
        self.access(addr, len, Access::Read, |p, pa, off, n| p.read(pa, &mut buf[off..off + n]))
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.access(addr, data.len(), Access::Write, |p, pa, off, n| p.write(pa, &data[off..off + n]))
    }

    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault> {
        let mut b = [0u8; 4];
        self.access(addr, 4, Access::Fetch, |p, pa, off, n| p.read(pa, &mut b[off..off + n]))?;
        Ok(u32::from_le_bytes(b))
    }
}
