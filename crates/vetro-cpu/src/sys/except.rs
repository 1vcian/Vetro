//! Exception entry to EL1 and return (ERET): pseudocode
//! `AArch64.TakeException` and `AArch64.ExceptionReturn` (Arm ARM, D1.10-11),
//! without EL2/EL3 or AArch32.

use crate::state::Cpu;

use super::state::{daif, spsr};

/// Exception Class (ESR_EL1.EC).
pub mod ec {
    pub const UNKNOWN: u64 = 0x00;
    pub const WFX: u64 = 0x01;
    pub const FP_ACCESS: u64 = 0x07;
    pub const ILLEGAL_STATE: u64 = 0x0e;
    pub const SVC: u64 = 0x15;
    pub const HVC: u64 = 0x16;
    pub const SMC: u64 = 0x17;
    pub const SYSREG: u64 = 0x18;
    pub const INSN_ABORT_LOWER: u64 = 0x20;
    pub const INSN_ABORT_SAME: u64 = 0x21;
    pub const PC_ALIGN: u64 = 0x22;
    pub const DATA_ABORT_LOWER: u64 = 0x24;
    pub const DATA_ABORT_SAME: u64 = 0x25;
    pub const SP_ALIGN: u64 = 0x26;
    pub const SERROR: u64 = 0x2f;
    pub const BRK: u64 = 0x3c;
}

/// ESR_EL1.IL: 32-bit instruction (always, in AArch64).
pub(crate) const IL: u64 = 1 << 25;

/// ESR_EL1 for `class` and `iss`.
#[inline]
pub(crate) fn esr(class: u64, iss: u64) -> u64 {
    class << 26 | IL | iss
}

/// Exception type: selects the offset within the vector group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionKind {
    Sync,
    Irq,
    Fiq,
    SError,
}

impl ExceptionKind {
    fn offset(self) -> u64 {
        match self {
            ExceptionKind::Sync => 0x000,
            ExceptionKind::Irq => 0x080,
            ExceptionKind::Fiq => 0x100,
            ExceptionKind::SError => 0x180,
        }
    }
}

impl Cpu {
    /// Takes an exception to EL1. `esr` and `far` are written only if present
    /// (IRQ and FIQ do not touch ESR_EL1; FAR_EL1 stays as it is when
    /// the architecture says UNKNOWN, as QEMU does). `preferred` goes into
    /// ELR_EL1.
    ///
    /// Also used by the platform, for example to deliver an
    /// Undefined after an HVC it does not want to handle.
    pub fn take_exception(
        &mut self,
        kind: ExceptionKind,
        esr: Option<u64>,
        far: Option<u64>,
        preferred: u64,
    ) {
        let from_el = self.sys.el;
        let group = if from_el == 0 {
            0x400 // from EL0 in AArch64
        } else if self.sys.spsel {
            0x200 // EL1 with SP_EL1
        } else {
            0x000 // EL1 with SP_EL0
        };
        self.sys.spsr_el1 = self.pstate_spsr();
        self.sys.elr_el1 = preferred;
        if let Some(e) = esr {
            self.sys.esr_el1 = e;
        }
        if let Some(f) = far {
            self.sys.far_el1 = f;
        }
        self.set_el_sp(1, true);
        self.sys.daif = daif::ALL;
        self.sys.il = false;
        self.pc = self.sys.vbar_el1.wrapping_add(group + kind.offset());
    }

    /// ERET from EL1. An SPSR that asks for AArch32, EL2/EL3 or EL0 with SP_EL1 is
    /// an illegal return: EL and SP do not change, PSTATE.IL = 1 and the
    /// next instruction takes an illegal state exception. NZCV and DAIF
    /// are restored anyway. The local exclusive monitor is cleared; the
    /// Top Byte Ignore of the new level is applied to ELR (like QEMU).
    pub(crate) fn exception_return(&mut self) {
        let s = self.sys.spsr_el1;
        let target = self.sys.elr_el1;
        self.nzcv = (s & spsr::NZCV) as u32;
        self.sys.daif = (s & spsr::DAIF) as u32;
        self.monitor = None;
        match s & spsr::M {
            m @ (0b00000 | 0b00100 | 0b00101) => {
                self.set_el_sp((m >> 2) as u8, m & 1 != 0);
                self.sys.il = s & spsr::IL != 0;
                self.pc = self.branch_addr(target);
            }
            _ => {
                self.sys.il = true;
                self.pc = target;
            }
        }
    }
}
