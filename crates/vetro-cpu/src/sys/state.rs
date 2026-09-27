//! System mode state: PSTATE beyond NZCV, per-level stack pointers
//! and system registers kept by the CPU.

use crate::state::Cpu;

use super::TranslationRegs;

/// CPU mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Linux user mode (M1/M2): always EL0, exceptions returned to the
    /// caller, [`Memory`](crate::Memory) memory without MMU.
    #[default]
    User,
    /// EL0 and EL1 with MMU, exception vectors and interrupts.
    System,
}

/// Where PSCI calls go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PsciConduit {
    /// HVC at EL1 becomes [`SysEvent::Hvc`](super::SysEvent::Hvc); SMC is
    /// UNDEFINED (no EL3). Default of `qemu-system-aarch64 -M virt`.
    #[default]
    Hvc,
    /// SMC at EL1 becomes [`SysEvent::Smc`](super::SysEvent::Smc); HVC is
    /// UNDEFINED (no EL2).
    Smc,
    /// No PSCI: HVC and SMC are UNDEFINED.
    None,
}

/// System mode configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysConfig {
    pub psci: PsciConduit,
    /// MPIDR_EL1: 0x8000_0000 for CPU 0 of `-M virt` (bit 31 RES1).
    pub mpidr: u64,
    /// GICv3 system register CPU interface present: makes the ICC_*
    /// accessible and raises the GIC fields of ID_AA64PFR0/ID_PFR1.
    pub gicv3: bool,
    /// CBAR_EL1: peripheral base (on `-M virt` the GIC distributor).
    pub cbar: u64,
}

impl Default for SysConfig {
    fn default() -> Self {
        SysConfig { psci: PsciConduit::Hvc, mpidr: 0x8000_0000, gicv3: true, cbar: 0x0800_0000 }
    }
}

/// SCTLR_EL1 bits read by the CPU.
pub mod sctlr {
    pub const M: u64 = 1 << 0;
    /// Alignment check for all data accesses.
    pub const A: u64 = 1 << 1;
    /// SP alignment check at EL1.
    pub const SA: u64 = 1 << 3;
    /// SP alignment check at EL0.
    pub const SA0: u64 = 1 << 4;
    /// EL0 can access DAIF.
    pub const UMA: u64 = 1 << 9;
    /// DC ZVA permitted at EL0.
    pub const DZE: u64 = 1 << 14;
    /// CTR_EL0 readable at EL0.
    pub const UCT: u64 = 1 << 15;
    /// WFI at EL0 not trapped.
    pub const NTWI: u64 = 1 << 16;
    /// WFE at EL0 not trapped (in Vetro, as in QEMU, WFE is never trapped
    /// because it does not wait).
    pub const NTWE: u64 = 1 << 18;
    /// Big-endian data accesses at EL0.
    pub const E0E: u64 = 1 << 24;
    /// Big-endian accesses at EL1 (and descriptors).
    pub const EE: u64 = 1 << 25;
    /// DC CVAU, DC CVAC, DC CIVAC, IC IVAU permitted at EL0.
    pub const UCI: u64 = 1 << 26;
    /// MTE bits that QEMU clears on CPUs without MTE.
    pub const MTE_BITS: u64 = 1 << 37 | 0b11 << 38 | 0b11 << 40 | 1 << 42 | 1 << 43;
}

/// CPACR_EL1 fields.
pub mod cpacr {
    pub const FPEN_SHIFT: u32 = 20;
}

/// CNTKCTL_EL1 bits that govern timer access from EL0.
pub mod cntkctl {
    pub const EL0PCTEN: u64 = 1 << 0;
    pub const EL0VCTEN: u64 = 1 << 1;
    pub const EL0VTEN: u64 = 1 << 8;
    pub const EL0PTEN: u64 = 1 << 9;
}

/// SPSR_EL1 fields (PSTATE format from AArch64).
pub mod spsr {
    pub const NZCV: u64 = 0xf000_0000;
    pub const IL: u64 = 1 << 20;
    pub const DAIF: u64 = 0x3c0;
    /// M[4:0]: 0 = EL0t, 4 = EL1t, 5 = EL1h.
    pub const M: u64 = 0x1f;
}

/// DAIF bits (same position in PSTATE, SPSR and the DAIF register).
pub(crate) mod daif {
    pub const D: u32 = 1 << 9;
    pub const A: u32 = 1 << 8;
    pub const I: u32 = 1 << 7;
    pub const F: u32 = 1 << 6;
    pub const ALL: u32 = D | A | I | F;
}

/// System mode state. `Default` is the user mode state
/// (EL0, SP_EL0): user mode never looks at it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SysState {
    pub mode: Mode,
    pub cfg: SysConfig,

    // PSTATE (NZCV lives in `Cpu::nzcv`).
    /// PSTATE.EL (0 or 1).
    pub el: u8,
    /// PSTATE.SP: at EL1, true = SP_EL1, false = SP_EL0.
    pub spsel: bool,
    /// PSTATE.{D,A,I,F} in bits 9:6.
    pub daif: u32,
    /// PSTATE.IL (Illegal Execution state).
    pub il: bool,

    /// Unselected stack pointers: `sp_el[n]` is valid only when SP_ELn is not
    /// the one in use, which always lives in `Cpu::sp`.
    pub sp_el: [u64; 2],

    pub elr_el1: u64,
    pub spsr_el1: u64,
    pub vbar_el1: u64,
    pub esr_el1: u64,
    pub far_el1: u64,
    pub sctlr_el1: u64,
    pub tcr_el1: u64,
    pub ttbr0_el1: u64,
    pub ttbr1_el1: u64,
    pub mair_el1: u64,
    pub contextidr_el1: u64,
    pub cpacr_el1: u64,
    pub tpidr_el1: u64,
    pub par_el1: u64,
    pub cntkctl_el1: u64,
    pub csselr_el1: u64,

    // Debug: storage only, no debug exceptions.
    pub mdscr_el1: u64,
    /// OSLSR_EL1.OSLK (OS lock): 1 at reset, cleared by OSLAR_EL1.
    pub oslk: bool,
    pub osdlr_el1: u64,
    pub dbgbvr: [u64; 6],
    pub dbgbcr: [u64; 6],
    pub dbgwvr: [u64; 4],
    pub dbgwcr: [u64; 4],
    /// CLAIM bits (DBGCLAIMSET_EL1/DBGCLAIMCLR_EL1), 8 as in QEMU.
    pub dbgclaim: u8,
    pub pmuserenr_el0: u64,

    /// Pending SError (ISS of ESR_EL1), delivered when PSTATE.A = 0.
    pub serror_pending: Option<u32>,
}

impl Cpu {
    /// Puts the CPU into system mode, in the reset state of the
    /// Cortex-A53 without EL2/EL3 (like `qemu-system-aarch64 -M virt`): EL1h,
    /// DAIF masked, MMU off, SCTLR_EL1 = 0x00c50838, CPACR_EL1 = 0
    /// (FP/SIMD trapped), OS lock active. General registers, PC and SIMD
    /// stay as they are: the loader sets them.
    pub fn reset_system(&mut self, cfg: SysConfig) {
        self.sys = SysState {
            mode: Mode::System,
            cfg,
            el: 1,
            spsel: true,
            daif: daif::ALL,
            sctlr_el1: super::id::SCTLR_EL1_RESET,
            oslk: true,
            ..SysState::default()
        };
        self.monitor = None;
    }

    /// Index of the stack pointer in use (0 = SP_EL0, 1 = SP_EL1).
    #[inline]
    pub(crate) fn sp_index(el: u8, spsel: bool) -> usize {
        if el == 0 || !spsel { 0 } else { 1 }
    }

    /// Value of SP_ELn whichever one is in use.
    pub fn sp_el(&self, n: usize) -> u64 {
        if Self::sp_index(self.sys.el, self.sys.spsel) == n { self.sp } else { self.sys.sp_el[n] }
    }

    /// Writes SP_ELn whichever one is in use.
    pub fn set_sp_el(&mut self, n: usize, v: u64) {
        if Self::sp_index(self.sys.el, self.sys.spsel) == n {
            self.sp = v;
        } else {
            self.sys.sp_el[n] = v;
        }
    }

    /// Changes PSTATE.EL and PSTATE.SP, swapping the stack pointer in use.
    pub(crate) fn set_el_sp(&mut self, el: u8, spsel: bool) {
        let old = Self::sp_index(self.sys.el, self.sys.spsel);
        let new = Self::sp_index(el, spsel);
        if old != new {
            self.sys.sp_el[old] = self.sp;
            self.sp = self.sys.sp_el[new];
        }
        self.sys.el = el;
        self.sys.spsel = spsel;
    }

    /// PSTATE in SPSR_EL1 format.
    pub fn pstate_spsr(&self) -> u64 {
        let m = u64::from(self.sys.el) << 2 | u64::from(self.sys.spsel);
        u64::from(self.nzcv & 0xf000_0000) | u64::from(self.sys.il) << 20 | u64::from(self.sys.daif) | m
    }

    pub(crate) fn translation_regs(&self) -> TranslationRegs {
        let s = &self.sys;
        TranslationRegs {
            sctlr: s.sctlr_el1,
            tcr: s.tcr_el1,
            ttbr0: s.ttbr0_el1,
            ttbr1: s.ttbr1_el1,
            mair: s.mair_el1,
        }
    }

    /// True if FP/SIMD instructions are trapped at the current level
    /// (CPACR_EL1.FPEN: 00 and 10 trap EL0 and EL1, 01 only EL0).
    pub(crate) fn fp_trapped(&self) -> bool {
        match self.sys.cpacr_el1 >> cpacr::FPEN_SHIFT & 3 {
            0b11 => false,
            0b01 => self.sys.el == 0,
            _ => true,
        }
    }

    /// `AArch64.BranchAddr`: with Top Byte Ignore active for the half of
    /// `target` the tag is removed by extending bit 55 (like QEMU).
    pub(crate) fn branch_addr(&self, target: u64) -> u64 {
        let tbi = if target >> 55 & 1 != 0 { 1u64 << 38 } else { 1u64 << 37 };
        if self.sys.tcr_el1 & tbi != 0 { crate::bits::sext(target, 56) as u64 } else { target }
    }
}
