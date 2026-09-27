//! Adapter from MMU + physical memory to the CPU's system mode
//! [`SysBus`] trait (ADR 0009).

use vetro_cpu::Access;
use vetro_cpu::sys::{AccessReq, AtResult, BusFault, SysBus, TlbiOp, TranslationRegs};

use crate::fault::FaultKind;
use crate::mmu::Mmu;
use crate::regs::MmuRegs;
use crate::walk::PhysMemory;

/// The memory of a core in system mode: the CPU passes its own translation
/// registers with every access, the MMU translates (with the TLB) and physical
/// memory performs it. Built for the duration of one or more steps.
pub struct MmuBus<'a, P: PhysMemory + ?Sized> {
    pub mmu: &'a mut Mmu,
    pub phys: &'a mut P,
}

impl<'a, P: PhysMemory + ?Sized> MmuBus<'a, P> {
    pub fn new(mmu: &'a mut Mmu, phys: &'a mut P) -> Self {
        MmuBus { mmu, phys }
    }

    fn load_regs(&mut self, r: &TranslationRegs) {
        self.mmu.regs = mmu_regs(r);
    }
}

#[inline]
fn mmu_regs(r: &TranslationRegs) -> MmuRegs {
    MmuRegs { sctlr: r.sctlr, tcr: r.tcr, ttbr0: r.ttbr0, ttbr1: r.ttbr1, mair: r.mair }
}

fn bus_fault(kind: FaultKind) -> BusFault {
    match (kind, kind.fsc()) {
        (_, Some(fsc)) => BusFault::Abort { fsc, ea: kind.ea() },
        (FaultKind::Unimplemented(what), None) => BusFault::Unimplemented(what),
        (_, None) => unreachable!("only Unimplemented has no FSC"),
    }
}

impl<P: PhysMemory + ?Sized> SysBus for MmuBus<'_, P> {
    #[inline]
    fn translate(&mut self, regs: &TranslationRegs, va: u64, req: AccessReq) -> Result<u64, BusFault> {
        match self.mmu.translate_pa_with(&mmu_regs(regs), self.phys, va, req.access, req.el, req.aligned) {
            Ok(pa) => Ok(pa),
            Err(f) => {
                self.mmu.set_last_fault(f);
                Err(bus_fault(f.kind))
            }
        }
    }

    fn read_phys(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusFault> {
        self.phys.read(pa, buf).map_err(|e| bus_fault(FaultKind::External(e)))
    }

    fn write_phys(&mut self, pa: u64, data: &[u8]) -> Result<(), BusFault> {
        self.phys.write(pa, data).map_err(|e| bus_fault(FaultKind::External(e)))
    }

    fn at(&mut self, regs: &TranslationRegs, va: u64, access: Access, el: u8) -> AtResult {
        self.load_regs(regs);
        match self.mmu.walk(self.phys, va, access, el) {
            Ok(t) => AtResult::Par(t.par()),
            Err(f) => match f.kind {
                // Like QEMU: an external abort on the walk is taken as a Data
                // Abort instead of ending up in PAR_EL1.
                FaultKind::ExternalWalk(..) => match bus_fault(f.kind) {
                    BusFault::Abort { fsc, ea } => AtResult::Abort { fsc, ea },
                    BusFault::Unimplemented(w) => AtResult::Unimplemented(w),
                },
                FaultKind::Unimplemented(what) => AtResult::Unimplemented(what),
                _ => AtResult::Par(f.par().expect("fault with FSC")),
            },
        }
    }

    fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        self.mmu.tlbi(op, xt);
    }

    fn tlb_flush_all(&mut self) {
        self.mmu.tlb_mut().flush_all();
    }
}
