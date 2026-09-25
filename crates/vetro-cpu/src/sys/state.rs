//! Stato della modalità sistema: PSTATE oltre NZCV, stack pointer per
//! livello e registri di sistema tenuti dalla CPU.

use crate::state::Cpu;

use super::TranslationRegs;

/// Modalità della CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Linux user mode (M1/M2): sempre EL0, eccezioni restituite al
    /// chiamante, memoria [`Memory`](crate::Memory) senza MMU.
    #[default]
    User,
    /// EL0 ed EL1 con MMU, vettori delle eccezioni e interrupt.
    System,
}

/// Dove arrivano le chiamate PSCI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PsciConduit {
    /// HVC a EL1 diventa [`SysEvent::Hvc`](super::SysEvent::Hvc); SMC è
    /// UNDEFINED (niente EL3). Default di `qemu-system-aarch64 -M virt`.
    #[default]
    Hvc,
    /// SMC a EL1 diventa [`SysEvent::Smc`](super::SysEvent::Smc); HVC è
    /// UNDEFINED (niente EL2).
    Smc,
    /// Nessun PSCI: HVC e SMC sono UNDEFINED.
    None,
}

/// Configurazione della modalità sistema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysConfig {
    pub psci: PsciConduit,
    /// MPIDR_EL1: 0x8000_0000 per la CPU 0 di `-M virt` (bit 31 RES1).
    pub mpidr: u64,
    /// Interfaccia CPU del GICv3 a registri di sistema presente: rende
    /// accessibili gli ICC_* e alza i campi GIC di ID_AA64PFR0/ID_PFR1.
    pub gicv3: bool,
    /// CBAR_EL1: base delle periferiche (su `-M virt` il distributore GIC).
    pub cbar: u64,
}

impl Default for SysConfig {
    fn default() -> Self {
        SysConfig { psci: PsciConduit::Hvc, mpidr: 0x8000_0000, gicv3: true, cbar: 0x0800_0000 }
    }
}

/// Bit di SCTLR_EL1 letti dalla CPU.
pub mod sctlr {
    pub const M: u64 = 1 << 0;
    /// Controllo di allineamento per tutti gli accessi ai dati.
    pub const A: u64 = 1 << 1;
    /// Controllo di allineamento dello SP a EL1.
    pub const SA: u64 = 1 << 3;
    /// Controllo di allineamento dello SP a EL0.
    pub const SA0: u64 = 1 << 4;
    /// EL0 può accedere a DAIF.
    pub const UMA: u64 = 1 << 9;
    /// DC ZVA permesso a EL0.
    pub const DZE: u64 = 1 << 14;
    /// CTR_EL0 leggibile a EL0.
    pub const UCT: u64 = 1 << 15;
    /// WFI a EL0 non trappata.
    pub const NTWI: u64 = 1 << 16;
    /// WFE a EL0 non trappata (in Vetro, come in QEMU, WFE non si trappa
    /// mai perché non attende).
    pub const NTWE: u64 = 1 << 18;
    /// Accessi ai dati a EL0 big-endian.
    pub const E0E: u64 = 1 << 24;
    /// Accessi a EL1 (e descrittori) big-endian.
    pub const EE: u64 = 1 << 25;
    /// DC CVAU, DC CVAC, DC CIVAC, IC IVAU permessi a EL0.
    pub const UCI: u64 = 1 << 26;
    /// Bit di MTE che QEMU azzera sulle CPU senza MTE.
    pub const MTE_BITS: u64 = 1 << 37 | 0b11 << 38 | 0b11 << 40 | 1 << 42 | 1 << 43;
}

/// Campi di CPACR_EL1.
pub mod cpacr {
    pub const FPEN_SHIFT: u32 = 20;
}

/// Bit di CNTKCTL_EL1 che governano l'accesso al timer da EL0.
pub mod cntkctl {
    pub const EL0PCTEN: u64 = 1 << 0;
    pub const EL0VCTEN: u64 = 1 << 1;
    pub const EL0VTEN: u64 = 1 << 8;
    pub const EL0PTEN: u64 = 1 << 9;
}

/// Campi di SPSR_EL1 (formato PSTATE da AArch64).
pub mod spsr {
    pub const NZCV: u64 = 0xf000_0000;
    pub const IL: u64 = 1 << 20;
    pub const DAIF: u64 = 0x3c0;
    /// M[4:0]: 0 = EL0t, 4 = EL1t, 5 = EL1h.
    pub const M: u64 = 0x1f;
}

/// Bit di DAIF (stessa posizione in PSTATE, SPSR e nel registro DAIF).
pub(crate) mod daif {
    pub const D: u32 = 1 << 9;
    pub const A: u32 = 1 << 8;
    pub const I: u32 = 1 << 7;
    pub const F: u32 = 1 << 6;
    pub const ALL: u32 = D | A | I | F;
}

/// Stato della modalità sistema. `Default` è lo stato della modalità
/// utente (EL0, SP_EL0): la modalità utente non lo guarda mai.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SysState {
    pub mode: Mode,
    pub cfg: SysConfig,

    // PSTATE (NZCV sta in `Cpu::nzcv`).
    /// PSTATE.EL (0 o 1).
    pub el: u8,
    /// PSTATE.SP: a EL1, vero = SP_EL1, falso = SP_EL0.
    pub spsel: bool,
    /// PSTATE.{D,A,I,F} nei bit 9:6.
    pub daif: u32,
    /// PSTATE.IL (Illegal Execution state).
    pub il: bool,

    /// Stack pointer non selezionati: `sp_el[n]` vale solo quando SP_ELn non
    /// è quello in uso, che sta sempre in `Cpu::sp`.
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

    // Debug: solo memoria, nessuna eccezione di debug.
    pub mdscr_el1: u64,
    /// OSLSR_EL1.OSLK (OS lock): 1 al reset, azzerato da OSLAR_EL1.
    pub oslk: bool,
    pub osdlr_el1: u64,
    pub dbgbvr: [u64; 6],
    pub dbgbcr: [u64; 6],
    pub dbgwvr: [u64; 4],
    pub dbgwcr: [u64; 4],
    pub pmuserenr_el0: u64,

    /// SError in attesa (ISS di ESR_EL1), consegnato quando PSTATE.A = 0.
    pub serror_pending: Option<u32>,
}

impl Cpu {
    /// Porta la CPU in modalità sistema, nello stato di reset della
    /// Cortex-A53 senza EL2/EL3 (come `qemu-system-aarch64 -M virt`): EL1h,
    /// DAIF mascherati, MMU spenta, SCTLR_EL1 = 0x00c50838, CPACR_EL1 = 0
    /// (FP/SIMD trappati), OS lock attivo. Registri generali, PC e SIMD
    /// restano quelli che ci sono: li imposta il caricatore.
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

    /// Indice dello stack pointer in uso (0 = SP_EL0, 1 = SP_EL1).
    #[inline]
    pub(crate) fn sp_index(el: u8, spsel: bool) -> usize {
        if el == 0 || !spsel { 0 } else { 1 }
    }

    /// Valore di SP_ELn qualunque sia quello in uso.
    pub fn sp_el(&self, n: usize) -> u64 {
        if Self::sp_index(self.sys.el, self.sys.spsel) == n { self.sp } else { self.sys.sp_el[n] }
    }

    /// Scrive SP_ELn qualunque sia quello in uso.
    pub fn set_sp_el(&mut self, n: usize, v: u64) {
        if Self::sp_index(self.sys.el, self.sys.spsel) == n {
            self.sp = v;
        } else {
            self.sys.sp_el[n] = v;
        }
    }

    /// Cambia PSTATE.EL e PSTATE.SP scambiando lo stack pointer in uso.
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

    /// PSTATE nel formato di SPSR_EL1.
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

    /// Vero se le istruzioni FP/SIMD sono trappate al livello corrente
    /// (CPACR_EL1.FPEN: 00 e 10 trappano EL0 ed EL1, 01 solo EL0).
    pub(crate) fn fp_trapped(&self) -> bool {
        match self.sys.cpacr_el1 >> cpacr::FPEN_SHIFT & 3 {
            0b11 => false,
            0b01 => self.sys.el == 0,
            _ => true,
        }
    }

    /// `AArch64.BranchAddr`: con Top Byte Ignore attivo per la metà di
    /// `target` il tag si toglie estendendo il bit 55 (come QEMU).
    pub(crate) fn branch_addr(&self, target: u64) -> u64 {
        let tbi = if target >> 55 & 1 != 0 { 1u64 << 38 } else { 1u64 << 37 };
        if self.sys.tcr_el1 & tbi != 0 { crate::bits::sext(target, 56) as u64 } else { target }
    }
}
