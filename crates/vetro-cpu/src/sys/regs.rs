//! MRS e MSR in modalità sistema: controlli di accesso e semantica dei
//! registri. Maschere e valori di reset come QEMU `-cpu cortex-a53`
//! (verificati scrivendo tutti uno e rileggendo).

use crate::state::{Cpu, FPCR_MASK, FPSR_MASK};
use crate::sysreg::{EnvReg, Rw, SysReg};

use super::state::{cntkctl, daif, sctlr};
use super::{CpuEnv, SysBus, id};

/// Perché un accesso a un registro di sistema non si esegue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deny {
    /// UNDEFINED (EC 0x00).
    Undefined,
    /// Trap a EL1 con EC 0x18 (sindrome dalla codifica).
    Trap,
}

impl Cpu {
    /// Controlla un MRS (`write` = falso) o MSR al livello corrente.
    pub(crate) fn sysreg_access(&self, reg: SysReg, write: bool) -> Result<(), Deny> {
        use SysReg::*;
        match (reg.rw(), write) {
            (Rw::ReadOnly, true) | (Rw::WriteOnly, false) => return Err(Deny::Undefined),
            _ => {}
        }
        if let Env(e) = reg
            && !e.is_timer()
            && !self.sys.cfg.gicv3
        {
            return Err(Deny::Undefined);
        }
        if self.sys.el == 1 {
            return match reg {
                SpEl0 if !self.sys.spsel => Err(Deny::Undefined),
                _ => Ok(()),
            };
        }
        let sctlr = self.sys.sctlr_el1;
        let k = self.sys.cntkctl_el1;
        let enabled = |bit: u64| if k & bit != 0 { Ok(()) } else { Err(Deny::Trap) };
        match reg {
            Nzcv | TpidrEl0 | Fpcr | Fpsr | DczidEl0 => Ok(()),
            TpidrroEl0 | PmuserenrEl0 if write => Err(Deny::Undefined),
            TpidrroEl0 | PmuserenrEl0 => Ok(()),
            CtrEl0 if sctlr & sctlr::UCT != 0 => Ok(()),
            Daif if sctlr & sctlr::UMA != 0 => Ok(()),
            CtrEl0 | Daif => Err(Deny::Trap),
            Env(e) => match e {
                // Come QEMU: prima la trap per CNTKCTL, poi la scrittura
                // (possibile solo al livello più alto) è UNDEFINED.
                EnvReg::CntfrqEl0 => {
                    enabled(cntkctl::EL0PCTEN | cntkctl::EL0VCTEN)?;
                    if write { Err(Deny::Undefined) } else { Ok(()) }
                }
                EnvReg::CntpctEl0 => enabled(cntkctl::EL0PCTEN),
                EnvReg::CntvctEl0 => enabled(cntkctl::EL0VCTEN),
                EnvReg::CntpTvalEl0 | EnvReg::CntpCtlEl0 | EnvReg::CntpCvalEl0 => enabled(cntkctl::EL0PTEN),
                EnvReg::CntvTvalEl0 | EnvReg::CntvCtlEl0 | EnvReg::CntvCvalEl0 => enabled(cntkctl::EL0VTEN),
                _ => Err(Deny::Undefined),
            },
            _ => Err(Deny::Undefined),
        }
    }

    /// MRS già autorizzato.
    pub(crate) fn sysreg_read<E: CpuEnv + ?Sized>(&mut self, reg: SysReg, env: &mut E) -> u64 {
        use SysReg::*;
        let s = &self.sys;
        match reg {
            Nzcv => u64::from(self.nzcv),
            TpidrEl0 => self.tpidr_el0,
            TpidrroEl0 => self.tpidrro_el0,
            Fpcr => u64::from(self.fpcr),
            Fpsr => u64::from(self.fpsr),
            DczidEl0 => {
                // DZP = 1 quando DC ZVA è proibito al livello corrente.
                let dzp = s.el == 0 && s.sctlr_el1 & sctlr::DZE == 0;
                id::DCZID_BS | u64::from(dzp) << 4
            }
            CtrEl0 => id::CTR_EL0,
            Daif => u64::from(s.daif),
            CurrentEl => u64::from(s.el) << 2,
            SpSel => u64::from(s.spsel),
            SpEl0 => self.sp_el(0),
            ElrEl1 => s.elr_el1,
            SpsrEl1 => s.spsr_el1,
            SctlrEl1 => s.sctlr_el1,
            CpacrEl1 => s.cpacr_el1,
            TcrEl1 => s.tcr_el1,
            Ttbr0El1 => s.ttbr0_el1,
            Ttbr1El1 => s.ttbr1_el1,
            MairEl1 => s.mair_el1,
            ContextidrEl1 => s.contextidr_el1,
            VbarEl1 => s.vbar_el1,
            EsrEl1 => s.esr_el1,
            FarEl1 => s.far_el1,
            ParEl1 => s.par_el1,
            TpidrEl1 => s.tpidr_el1,
            CntkctlEl1 => s.cntkctl_el1,
            CsselrEl1 => s.csselr_el1,
            // RAZ/WI in QEMU per la Cortex-A53.
            ActlrEl1 | AmairEl1 | Afsr0El1 | Afsr1El1 | MdccintEl1 => 0,
            IsrEl1 => {
                let (a, i, f) = (s.serror_pending.is_some(), env.irq_line(), env.fiq_line());
                u64::from(a) << 8 | u64::from(i) << 7 | u64::from(f) << 6
            }
            RvbarEl1 | AidrEl1 | MdrarEl1 => 0,
            MidrEl1 => id::MIDR_EL1,
            MpidrEl1 => s.cfg.mpidr,
            RevidrEl1 => id::REVIDR_EL1,
            ClidrEl1 => id::CLIDR_EL1,
            CcsidrEl1 => id::CCSIDR_EL1.get(s.csselr_el1 as usize).copied().unwrap_or(0),
            Id(i) => id::id_reg(i, s.cfg.gicv3),
            MdscrEl1 => s.mdscr_el1,
            // OSLM = 0b10 (bit 3), OSLK nel bit 1.
            OslsrEl1 => 0b1000 | u64::from(s.oslk) << 1,
            OsdlrEl1 => s.osdlr_el1,
            DbgbvrEl1(n) => s.dbgbvr[n as usize],
            DbgbcrEl1(n) => s.dbgbcr[n as usize],
            DbgwvrEl1(n) => s.dbgwvr[n as usize],
            DbgwcrEl1(n) => s.dbgwcr[n as usize],
            PmuserenrEl0 => s.pmuserenr_el0,
            OslarEl1 => unreachable!("sola scrittura, escluso da sysreg_access"),
            Env(e) => env.read_sysreg(e),
        }
    }

    /// MSR già autorizzato. Le scritture di SCTLR_EL1 e TCR_EL1 svuotano il
    /// TLB (come QEMU).
    pub(crate) fn sysreg_write<B: SysBus + ?Sized, E: CpuEnv + ?Sized>(
        &mut self,
        reg: SysReg,
        v: u64,
        bus: &mut B,
        env: &mut E,
    ) {
        use SysReg::*;
        let s = &mut self.sys;
        match reg {
            Nzcv => self.nzcv = v as u32 & 0xf000_0000,
            TpidrEl0 => self.tpidr_el0 = v,
            TpidrroEl0 => self.tpidrro_el0 = v,
            Fpcr => self.fpcr = v as u32 & FPCR_MASK,
            Fpsr => self.fpsr = v as u32 & FPSR_MASK,
            Daif => s.daif = v as u32 & daif::ALL,
            SpSel => {
                let el = s.el;
                self.set_el_sp(el, v & 1 != 0);
            }
            SpEl0 => self.set_sp_el(0, v),
            ElrEl1 => s.elr_el1 = v,
            SpsrEl1 => s.spsr_el1 = v,
            SctlrEl1 => {
                s.sctlr_el1 = v & !sctlr::MTE_BITS;
                bus.tlb_flush_all();
            }
            CpacrEl1 => s.cpacr_el1 = v,
            TcrEl1 => {
                s.tcr_el1 = v;
                bus.tlb_flush_all();
            }
            Ttbr0El1 => s.ttbr0_el1 = v,
            Ttbr1El1 => s.ttbr1_el1 = v,
            MairEl1 => s.mair_el1 = v,
            ContextidrEl1 => s.contextidr_el1 = v,
            // I bit [10:5] restano: QEMU azzera solo i 5 bassi.
            VbarEl1 => s.vbar_el1 = v & !0x1f,
            EsrEl1 => s.esr_el1 = v,
            FarEl1 => s.far_el1 = v,
            ParEl1 => s.par_el1 = v,
            TpidrEl1 => s.tpidr_el1 = v,
            CntkctlEl1 => s.cntkctl_el1 = v,
            CsselrEl1 => s.csselr_el1 = v & 0xf,
            ActlrEl1 | AmairEl1 | Afsr0El1 | Afsr1El1 | MdccintEl1 => {}
            MdscrEl1 => s.mdscr_el1 = v,
            OslarEl1 => s.oslk = v & 1 != 0,
            OsdlrEl1 => s.osdlr_el1 = v & 1,
            DbgbvrEl1(n) => s.dbgbvr[n as usize] = v,
            DbgbcrEl1(n) => s.dbgbcr[n as usize] = v,
            DbgwvrEl1(n) => s.dbgwvr[n as usize] = v & !3,
            DbgwcrEl1(n) => s.dbgwcr[n as usize] = v,
            PmuserenrEl0 => s.pmuserenr_el0 = v & 0xf,
            Env(e) => env.write_sysreg(e, v),
            DczidEl0 | CtrEl0 | CurrentEl | IsrEl1 | RvbarEl1 | MidrEl1 | MpidrEl1 | RevidrEl1 | AidrEl1
            | ClidrEl1 | CcsidrEl1 | Id(_) | OslsrEl1 | MdrarEl1 => {
                unreachable!("sola lettura, escluso da sysreg_access")
            }
        }
    }
}
