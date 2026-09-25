//! Adattatore fra l'interprete (trait [`Memory`]) e il [`SysBus`] in
//! modalità sistema: controlli di allineamento, traduzione pagina per
//! pagina, accessi fisici e fault dettagliati per ESR/FAR.

use crate::mem::{Access, MemFault, Memory};

use super::state::sctlr;
use super::{AccessReq, BusFault, SysBus, TranslationRegs};

/// Dimensione delle pagine da tradurre separatamente (granulo 4 KiB: un
/// accesso non attraversa mai più di un confine di blocco più grande).
const PAGE: u64 = 4096;

/// Fault dell'ultimo accesso fallito, con quello che serve alla sindrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pending {
    Abort {
        /// Indirizzo virtuale per FAR_EL1 (per un accesso a cavallo di due
        /// pagine, il primo byte della pagina che fallisce, come QEMU).
        va: u64,
        fsc: u8,
        ea: bool,
        access: Access,
    },
    Unimplemented(&'static str),
}

pub(crate) struct SysMem<'a, B: SysBus + ?Sized> {
    pub bus: &'a mut B,
    pub regs: TranslationRegs,
    /// PSTATE.EL.
    pub el: u8,
    pub fault: Option<Pending>,
}

impl<'a, B: SysBus + ?Sized> SysMem<'a, B> {
    pub fn new(bus: &'a mut B, regs: TranslationRegs, el: u8) -> Self {
        SysMem { bus, regs, el, fault: None }
    }

    fn fail(&mut self, va: u64, access: Access, f: BusFault) -> MemFault {
        self.fault = Some(match f {
            BusFault::Abort { fsc, ea } => Pending::Abort { va, fsc, ea, access },
            BusFault::Unimplemented(w) => Pending::Unimplemented(w),
        });
        MemFault { addr: va, access }
    }

    /// Esegue un accesso di `len` byte: `op(bus, pa, offset, n)` per ogni
    /// pezzo dentro una pagina. `privilege` è il livello dei permessi;
    /// `device_fault` forza il fault di allineamento su memoria Device
    /// (DC ZVA).
    fn access(
        &mut self,
        va: u64,
        len: usize,
        access: Access,
        privilege: u8,
        device_fault: bool,
        mut op: impl FnMut(&mut B, u64, usize, usize) -> Result<(), BusFault>,
    ) -> Result<(), MemFault> {
        if len == 0 {
            return Ok(());
        }
        let aligned = !device_fault && va.is_multiple_of(len as u64);
        if access != Access::Fetch {
            if !aligned && !device_fault && self.regs.sctlr & sctlr::A != 0 {
                let f = BusFault::Abort { fsc: BusFault::FSC_ALIGNMENT, ea: false };
                return Err(self.fail(va, access, f));
            }
            let big = if self.el == 0 { sctlr::E0E } else { sctlr::EE };
            if self.regs.sctlr & big != 0 {
                let f = BusFault::Unimplemented("accessi ai dati big-endian (SCTLR_EL1.EE/E0E)");
                return Err(self.fail(va, access, f));
            }
        }
        let chunk = |va: u64, rest: usize| rest.min((PAGE - (va & (PAGE - 1))) as usize);
        let req = AccessReq { access, el: privilege, aligned };
        let regs = self.regs;
        // A cavallo di pagina: prima si traducono tutte le pagine, così un
        // fault di traduzione o di permesso non lascia scritture parziali.
        let crosses = chunk(va, len) < len;
        if crosses {
            let mut off = 0;
            while off < len {
                let v = va.wrapping_add(off as u64);
                if let Err(f) = self.bus.translate(&regs, v, req) {
                    return Err(self.fail(v, access, f));
                }
                off += chunk(v, len - off);
            }
        }
        let mut off = 0;
        while off < len {
            let v = va.wrapping_add(off as u64);
            let n = chunk(v, len - off);
            let pa = match self.bus.translate(&regs, v, req) {
                Ok(pa) => pa,
                Err(f) => return Err(self.fail(v, access, f)),
            };
            if let Err(f) = op(self.bus, pa, off, n) {
                return Err(self.fail(v, access, f));
            }
            off += n;
        }
        Ok(())
    }

    fn read_as(&mut self, addr: u64, buf: &mut [u8], privilege: u8) -> Result<(), MemFault> {
        let len = buf.len();
        self.access(addr, len, Access::Read, privilege, false, |b, pa, off, n| {
            b.read_phys(pa, &mut buf[off..off + n])
        })
    }

    fn write_as(&mut self, addr: u64, data: &[u8], privilege: u8) -> Result<(), MemFault> {
        self.access(addr, data.len(), Access::Write, privilege, false, |b, pa, off, n| {
            b.write_phys(pa, &data[off..off + n])
        })
    }
}

impl<B: SysBus + ?Sized> Memory for SysMem<'_, B> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        self.read_as(addr, buf, self.el)
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.write_as(addr, data, self.el)
    }

    #[inline]
    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault> {
        let mut w = [0u8; 4];
        let el = self.el;
        // Caso comune (in `step_system` il PC è sempre allineato): la parola
        // sta in una pagina, quindi una traduzione e una lettura di 4 byte,
        // gli stessi passi di `access` senza il giro generico.
        if addr & 3 == 0 {
            let req = AccessReq { access: Access::Fetch, el, aligned: true };
            let pa = match self.bus.translate(&self.regs, addr, req) {
                Ok(pa) => pa,
                Err(f) => return Err(self.fail(addr, Access::Fetch, f)),
            };
            if let Err(f) = self.bus.read_phys(pa, &mut w) {
                return Err(self.fail(addr, Access::Fetch, f));
            }
            return Ok(u32::from_le_bytes(w));
        }
        self.access(addr, 4, Access::Fetch, el, false, |b, pa, off, n| {
            b.read_phys(pa, &mut w[off..off + n])
        })?;
        Ok(u32::from_le_bytes(w))
    }

    fn read_unpriv(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        self.read_as(addr, buf, 0)
    }

    fn write_unpriv(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.write_as(addr, data, 0)
    }

    fn zero_block(&mut self, addr: u64) -> Result<(), MemFault> {
        let el = self.el;
        let zeros = [0u8; 64];
        self.access(addr, 64, Access::Write, el, true, |b, pa, off, n| b.write_phys(pa, &zeros[off..off + n]))
    }
}
