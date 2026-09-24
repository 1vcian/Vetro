//! Ingresso nelle eccezioni a EL1 e ritorno (ERET): pseudocodice
//! `AArch64.TakeException` e `AArch64.ExceptionReturn` (Arm ARM, D1.10-11),
//! senza EL2/EL3 né AArch32.

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

/// ESR_EL1.IL: istruzione a 32 bit (sempre, in AArch64).
pub(crate) const IL: u64 = 1 << 25;

/// ESR_EL1 per `class` e `iss`.
#[inline]
pub(crate) fn esr(class: u64, iss: u64) -> u64 {
    class << 26 | IL | iss
}

/// Tipo di eccezione: sceglie l'offset dentro il gruppo di vettori.
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
    /// Prende un'eccezione a EL1. `esr` e `far` si scrivono solo se presenti
    /// (IRQ e FIQ non toccano ESR_EL1; FAR_EL1 resta com'è quando
    /// l'architettura lo dice UNKNOWN, come fa QEMU). `preferred` va in
    /// ELR_EL1.
    ///
    /// Serve anche alla piattaforma, per esempio per consegnare un
    /// Undefined dopo una HVC che non vuole gestire.
    pub fn take_exception(
        &mut self,
        kind: ExceptionKind,
        esr: Option<u64>,
        far: Option<u64>,
        preferred: u64,
    ) {
        let from_el = self.sys.el;
        let group = if from_el == 0 {
            0x400 // da EL0 in AArch64
        } else if self.sys.spsel {
            0x200 // EL1 con SP_EL1
        } else {
            0x000 // EL1 con SP_EL0
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

    /// ERET da EL1. Un SPSR che chiede AArch32, EL2/EL3 o EL0 con SP_EL1 è
    /// un ritorno illegale: EL e SP non cambiano, PSTATE.IL = 1 e la
    /// prossima istruzione prende un'eccezione di stato illegale. NZCV e DAIF
    /// si ripristinano comunque. Il monitor esclusivo locale si azzera; a
    /// ELR si applica il Top Byte Ignore del nuovo livello (come QEMU).
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
