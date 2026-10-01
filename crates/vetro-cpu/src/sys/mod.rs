//! System mode: EL0 and EL1, exceptions, system registers, MMU and
//! interrupt (ADR 0009, `docs/specs/cpu.md`).
//!
//! User mode ([`Cpu::step`] with a [`Memory`](crate::Memory))
//! stays as it was: exceptions return to the caller and no EL1 register
//! comes into play. System mode ([`Cpu::step_system`]) delivers
//! exceptions to the guest through VBAR_EL1 and routes every memory access
//! through a [`SysBus`] (the MMU of `vetro-mmu`); what belongs to the
//! platform (IRQ line, generic timer, GIC CPU interface) comes from
//! a [`CpuEnv`].

mod except;
pub mod id;
mod mem;
mod regs;
mod state;
mod step;
#[cfg(test)]
mod tests;

pub use except::{ExceptionKind, ec};
pub use state::{Mode, PsciConduit, SysConfig, SysState, cntkctl, cpacr, sctlr, spsr};

use crate::mem::Access;
use crate::sysreg::EnvReg;

/// Registers that govern stage 1 translation, copied from the CPU on every
/// access: the CPU is their only owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TranslationRegs {
    pub sctlr: u64,
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub mair: u64,
}

/// Translation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessReq {
    pub access: Access,
    /// Privilege for the permission check: PSTATE.EL, or 0 for
    /// LDTR/STTR executed at EL1.
    pub el: u8,
    /// False if the access is not aligned to its size (or is a DC
    /// ZVA): on Device memory it becomes an alignment fault, checked
    /// after the walk and before the permissions as in the pseudocode.
    pub aligned: bool,
}

/// Negative outcome of a translation or of a physical access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusFault {
    /// Architectural abort: DFSC/IFSC code and EA bit (1 for a slave
    /// error, 0 for a decode error, like QEMU).
    Abort { fsc: u8, ea: bool },
    /// Valid configuration that Vetro does not implement (e.g. 64 KiB granule):
    /// it is not delivered to the guest, execution stops.
    Unimplemented(&'static str),
}

impl BusFault {
    /// FSC code of a synchronous external abort on the access (not on the walk).
    pub const FSC_EXTERNAL: u8 = 0b01_0000;
    /// FSC code of an alignment fault.
    pub const FSC_ALIGNMENT: u8 = 0b10_0001;
}

/// Outcome of an AT instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtResult {
    /// Value to write into PAR_EL1 (success or fault reported in PAR).
    Par(u64),
    /// External abort during the walk: taken as a Data Abort (CM = 1).
    Abort {
        fsc: u8,
        ea: bool,
    },
    Unimplemented(&'static str),
}

/// System memory as seen by the CPU: translation and physical accesses.
/// Implemented by `vetro-mmu` (`MmuBus`) on top of the platform's physical
/// memory.
pub trait SysBus {
    /// Translates `va` and checks the permissions; returns the physical address.
    fn translate(&mut self, regs: &TranslationRegs, va: u64, req: AccessReq) -> Result<u64, BusFault>;
    /// Physical read (a piece that does not cross pages).
    fn read_phys(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusFault>;
    /// Physical write (a piece that does not cross pages).
    fn write_phys(&mut self, pa: u64, data: &[u8]) -> Result<(), BusFault>;
    /// AT S1E{0,1}{R,W}: walk without the TLB.
    fn at(&mut self, regs: &TranslationRegs, va: u64, access: Access, el: u8) -> AtResult;
    /// TLBI from the decoder (the IS variants must be applied to every core).
    fn tlbi(&mut self, op: TlbiOp, xt: u64);
    /// Flushes the TLB (writes to SCTLR_EL1 and TCR_EL1, like QEMU).
    fn tlb_flush_all(&mut self);
    /// Physical compare-and-exchange (a piece that does not cross pages, of
    /// 1, 2, 4, 8 or 16 bytes): writes `new` if the bytes are `old`; true if it
    /// wrote. Atomic with respect to the other cores of a parallel machine
    /// (ADR 0042); the default reads, compares and writes.
    fn cmpxchg_phys(&mut self, pa: u64, old: &[u8], new: &[u8]) -> Result<bool, BusFault> {
        let mut cur = [0u8; 16];
        let cur = &mut cur[..old.len()];
        self.read_phys(pa, cur)?;
        if cur != old {
            return Ok(false);
        }
        self.write_phys(pa, new)?;
        Ok(true)
    }
}

/// What the platform provides to the core: interrupt lines and system
/// registers that do not live in the CPU. Time (CNTPCT) enters only from here,
/// so it stays deterministic and recordable.
pub trait CpuEnv {
    /// Level of the IRQ line to this core (GIC output).
    fn irq_line(&mut self) -> bool;
    /// Level of the FIQ line (Vetro's GIC does not drive it).
    fn fiq_line(&mut self) -> bool {
        false
    }
    /// MRS of a platform register, already authorised by the CPU.
    fn read_sysreg(&mut self, reg: EnvReg) -> u64;
    /// MSR of a platform register, already authorised by the CPU.
    fn write_sysreg(&mut self, reg: EnvReg, value: u64);
}

/// Outcome of [`Cpu::step_system`](crate::Cpu::step_system).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysEvent {
    /// One instruction executed.
    Executed,
    /// Exception taken: the PC is already at the vector. `esr` is 0 for IRQ and FIQ
    /// (which do not write ESR_EL1). No instruction executed.
    Exception { kind: ExceptionKind, esr: u64, from_el: u8 },
    /// WFI executed (PC already at the next instruction): the platform can
    /// advance time up to the next interrupt.
    WaitForInterrupt,
    /// WFE or YIELD executed (PC already at the next instruction): a hint
    /// that this core waits for another one. Nothing happens to the core
    /// (like QEMU, WFE never waits); a machine with several cores scheduled
    /// on one thread can let the next core run (QEMU's `EXCP_YIELD` with
    /// single-threaded TCG).
    Yield,
    /// HVC of the PSCI conduit: PC already after the instruction, arguments in x0-x7,
    /// result to be written into x0 (all HVCs go to the conduit, as in
    /// QEMU: an unknown function returns NOT_SUPPORTED).
    Hvc(u16),
    /// SMC of the PSCI conduit (if configured that way).
    Smc(u16),
    /// Valid instruction or configuration that Vetro does not implement. State
    /// unchanged, PC at the instruction (`raw` = 0 if the limitation is in the fetch).
    Unimplemented { raw: u32, what: &'static str },
}

/// TLBI instructions of the EL1&0 regime (SYS #0, C8, CRm, #op2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlbiOp {
    Vmalle1,
    Vae1,
    Aside1,
    Vaae1,
    Vale1,
    Vaale1,
    Vmalle1is,
    Vae1is,
    Aside1is,
    Vaae1is,
    Vale1is,
    Vaale1is,
}

impl TlbiOp {
    /// Recognises a TLBI from the SYS fields (op0 = 1 implied).
    pub fn from_sys(op1: u32, crn: u32, crm: u32, op2: u32) -> Option<TlbiOp> {
        if op1 != 0 || crn != 8 {
            return None;
        }
        use TlbiOp::*;
        Some(match (crm, op2) {
            (3, 0) => Vmalle1is,
            (3, 1) => Vae1is,
            (3, 2) => Aside1is,
            (3, 3) => Vaae1is,
            (3, 5) => Vale1is,
            (3, 7) => Vaale1is,
            (7, 0) => Vmalle1,
            (7, 1) => Vae1,
            (7, 2) => Aside1,
            (7, 3) => Vaae1,
            (7, 5) => Vale1,
            (7, 7) => Vaale1,
            _ => return None,
        })
    }

    /// Inner Shareable variant: the system applies it to the TLB of every core.
    pub fn is_broadcast(self) -> bool {
        use TlbiOp::*;
        matches!(self, Vmalle1is | Vae1is | Aside1is | Vaae1is | Vale1is | Vaale1is)
    }
}
