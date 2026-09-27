//! Adapter between the interpreter ([`Memory`] trait) and the [`SysBus`] in
//! system mode: alignment checks, page-by-page translation,
//! physical accesses and detailed faults for ESR/FAR.

use crate::mem::{Access, MemFault, Memory};

use super::state::sctlr;
use super::{AccessReq, BusFault, SysBus, TranslationRegs};

/// Size of the pages to translate separately (4 KiB granule: an
/// access never crosses more than one boundary of a larger block).
const PAGE: u64 = 4096;

/// Fault of the last failed access, with what the syndrome needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pending {
    Abort {
        /// Virtual address for FAR_EL1 (for an access straddling two
        /// pages, the first byte of the page that fails, like QEMU).
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

    /// Performs an access of `len` bytes: `op(bus, pa, offset, n)` for every
    /// piece within a page. `privilege` is the permission level;
    /// `device_fault` forces the alignment fault on Device memory
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
                let f = BusFault::Unimplemented("big-endian data accesses (SCTLR_EL1.EE/E0E)");
                return Err(self.fail(va, access, f));
            }
        }
        let chunk = |va: u64, rest: usize| rest.min((PAGE - (va & (PAGE - 1))) as usize);
        let req = AccessReq { access, el: privilege, aligned };
        let regs = self.regs;
        // Straddling a page: all pages are translated first, so a
        // translation or permission fault leaves no partial writes.
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
        // Common case (in `step_system` the PC is always aligned): the word
        // fits in one page, hence one translation and one 4-byte read,
        // the same steps as `access` without the generic path.
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
