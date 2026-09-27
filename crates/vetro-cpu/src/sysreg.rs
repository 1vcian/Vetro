//! System registers: names and encodings `(op0, op1, CRn, CRm, op2)` of MRS and
//! MSR (Arm ARM, C5.3 and D17).
//!
//! The decoder recognises here all the registers that Vetro models, at any
//! exception level; the executor decides on access (EL0 or EL1, read-only
//! or write-only, trap). In user mode only those of M1 apply
//! (see [`SysReg::is_el0_legacy`]); the EL0 debug channel is UNDEFINED
//! as in QEMU user (see [`SysReg::is_el0_dcc`]); the others stay
//! `Unimplemented` as before system mode.

/// Modelled system registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysReg {
    // --- Accessible at EL0 since M1 ---
    Nzcv,
    TpidrEl0,
    /// Read-only at EL0, read and write at EL1.
    TpidrroEl0,
    Fpcr,
    Fpsr,
    /// Read-only: DC ZVA block size.
    DczidEl0,
    /// Read-only: cache geometry.
    CtrEl0,

    // --- PSTATE and special registers ---
    Daif,
    CurrentEl,
    SpSel,
    SpEl0,
    ElrEl1,
    SpsrEl1,

    // --- System control and MMU ---
    SctlrEl1,
    ActlrEl1,
    CpacrEl1,
    TcrEl1,
    Ttbr0El1,
    Ttbr1El1,
    MairEl1,
    AmairEl1,
    ContextidrEl1,

    // --- Exceptions ---
    VbarEl1,
    EsrEl1,
    FarEl1,
    Afsr0El1,
    Afsr1El1,
    ParEl1,
    IsrEl1,
    RvbarEl1,
    TpidrEl1,
    CntkctlEl1,

    // --- Identification ---
    MidrEl1,
    MpidrEl1,
    RevidrEl1,
    AidrEl1,
    ClidrEl1,
    CcsidrEl1,
    CsselrEl1,
    /// ID space (op0 = 3, op1 = 0, CRn = 0, CRm = 1..=7): index
    /// `CRm * 8 + op2`. Reserved encodings are zero.
    Id(u8),

    // --- Debug (value storage only, no debug exceptions) ---
    MdscrEl1,
    MdccintEl1,
    OslarEl1,
    OslsrEl1,
    OsdlrEl1,
    MdrarEl1,
    /// DBGBVR<n>_EL1, n < 6 on the Cortex-A53.
    DbgbvrEl1(u8),
    DbgbcrEl1(u8),
    /// DBGWVR<n>_EL1, n < 4 on the Cortex-A53.
    DbgwvrEl1(u8),
    DbgwcrEl1(u8),
    /// OSDTRRX_EL1, OSDTRTX_EL1, OSECCR_EL1: RAZ/WI at EL1 (QEMU implements
    /// neither the debug communication channel nor EDECCR).
    DbgRazWiEl1,
    /// MDCCSR_EL0: read-only, reads as zero; readable from EL0 if
    /// MDSCR_EL1.TDCC = 0, otherwise trap to EL1.
    MdccsrEl0,
    /// DBGDTR_EL0 and DBGDTRRX_EL0/DBGDTRTX_EL0: RAZ/WI, accessible from EL0
    /// like MDCCSR_EL0.
    DbgdtrEl0,
    /// DBGCLAIMSET_EL1: always reads 0xff, writing sets bits [7:0].
    DbgclaimsetEl1,
    /// DBGCLAIMCLR_EL1: reads the CLAIM bits, writing clears them.
    DbgclaimclrEl1,
    PmuserenrEl0,

    // --- Cortex-A53 IMPLEMENTATION DEFINED (like QEMU) ---
    /// L2CTLR, L2ECTLR, L2ACTLR, CPUACTLR, CPUECTLR, CPUMERRSR, L2MERRSR:
    /// RAZ/WI at EL1.
    ImpDefEl1,
    /// CBAR_EL1: peripheral base (the GIC distributor), read-only.
    CbarEl1,

    /// Register handled by the environment (generic timer, GIC CPU
    /// interface): the CPU checks the access and passes read and write to
    /// [`CpuEnv`](crate::sys::CpuEnv).
    Env(EnvReg),
}

/// Registers the CPU does not keep itself: the platform implements them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvReg {
    // Generic timer.
    CntfrqEl0,
    CntpctEl0,
    CntvctEl0,
    CntpTvalEl0,
    CntpCtlEl0,
    CntpCvalEl0,
    CntvTvalEl0,
    CntvCtlEl0,
    CntvCvalEl0,
    // GICv3 CPU interface (group 0 and 1).
    IccPmrEl1,
    IccIar0El1,
    IccEoir0El1,
    IccHppir0El1,
    IccBpr0El1,
    IccAp0r0El1,
    IccAp1r0El1,
    IccDirEl1,
    IccRprEl1,
    IccSgi1rEl1,
    IccAsgi1rEl1,
    IccSgi0rEl1,
    IccIar1El1,
    IccEoir1El1,
    IccHppir1El1,
    IccBpr1El1,
    IccCtlrEl1,
    IccSreEl1,
    IccIgrpen0El1,
    IccIgrpen1El1,
}

/// Directions allowed by a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rw {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

impl EnvReg {
    /// Generic timer registers (EL0 access governed by CNTKCTL_EL1).
    pub fn is_timer(self) -> bool {
        use EnvReg::*;
        matches!(
            self,
            CntfrqEl0
                | CntpctEl0
                | CntvctEl0
                | CntpTvalEl0
                | CntpCtlEl0
                | CntpCvalEl0
                | CntvTvalEl0
                | CntvCtlEl0
                | CntvCvalEl0
        )
    }

    pub fn rw(self) -> Rw {
        use EnvReg::*;
        match self {
            CntpctEl0 | CntvctEl0 | IccIar0El1 | IccIar1El1 | IccHppir0El1 | IccHppir1El1 | IccRprEl1 => {
                Rw::ReadOnly
            }
            IccEoir0El1 | IccEoir1El1 | IccDirEl1 | IccSgi1rEl1 | IccAsgi1rEl1 | IccSgi0rEl1 => Rw::WriteOnly,
            _ => Rw::ReadWrite,
        }
    }
}

impl SysReg {
    /// The registers the M1 decoder knew: the only ones executed in
    /// user mode.
    pub fn is_el0_legacy(self) -> bool {
        use SysReg::*;
        matches!(self, Nzcv | TpidrEl0 | TpidrroEl0 | Fpcr | Fpsr | DczidEl0 | CtrEl0)
    }

    /// EL0 debug channel (MDCCSR_EL0, DBGDTR*_EL0). In user mode
    /// it is UNDEFINED (SIGILL): QEMU user, like Linux, sets MDSCR_EL1.TDCC.
    pub fn is_el0_dcc(self) -> bool {
        matches!(self, SysReg::MdccsrEl0 | SysReg::DbgdtrEl0)
    }

    /// Directions allowed at EL1 (at EL0 the access rules decide).
    pub fn rw(self) -> Rw {
        use SysReg::*;
        match self {
            DczidEl0 | CtrEl0 | CurrentEl | IsrEl1 | RvbarEl1 | MidrEl1 | MpidrEl1 | RevidrEl1 | AidrEl1
            | ClidrEl1 | CcsidrEl1 | Id(_) | OslsrEl1 | MdrarEl1 | CbarEl1 | MdccsrEl0 => Rw::ReadOnly,
            OslarEl1 => Rw::WriteOnly,
            Env(e) => e.rw(),
            _ => Rw::ReadWrite,
        }
    }

    /// Encodings of registers that the Cortex-A53 has but Vetro does not model
    /// yet: the PMU (PMUv3). Everything else outside [`SysReg::lookup`] does not
    /// exist on the A53 (later extensions or free encodings) and is
    /// UNDEFINED, as in QEMU.
    pub fn is_unmodelled_a53(op0: u32, op1: u32, crn: u32, crm: u32, _op2: u32) -> bool {
        op0 == 3 && matches!((op1, crn, crm), (3, 9, 12..=14) | (0, 9, 14) | (3, 14, 8..=15))
    }

    /// Register from the MRS/MSR encoding, if Vetro models it.
    pub fn lookup(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> Option<SysReg> {
        use EnvReg::*;
        use SysReg::*;
        Some(match (op0, op1, crn, crm, op2) {
            // Debug (op0 = 2), like QEMU (debug_cp_reginfo). The Cortex-A53
            // has 6 breakpoints and 4 watchpoints; DBGPRCR_EL1, DBGAUTHSTATUS_EL1
            // and DBGVCR32_EL2 do not exist in QEMU (UNDEFINED).
            (2, 0, 0, 0 | 3 | 6, 2) => DbgRazWiEl1,
            (2, 3, 0, 1, 0) => MdccsrEl0,
            (2, 3, 0, 4 | 5, 0) => DbgdtrEl0,
            (2, 0, 7, 8, 6) => DbgclaimsetEl1,
            (2, 0, 7, 9, 6) => DbgclaimclrEl1,
            (2, 0, 0, 2, 0) => MdccintEl1,
            (2, 0, 0, 2, 2) => MdscrEl1,
            (2, 0, 0, n, 4) if n < 6 => DbgbvrEl1(n as u8),
            (2, 0, 0, n, 5) if n < 6 => DbgbcrEl1(n as u8),
            (2, 0, 0, n, 6) if n < 4 => DbgwvrEl1(n as u8),
            (2, 0, 0, n, 7) if n < 4 => DbgwcrEl1(n as u8),
            (2, 0, 1, 0, 0) => MdrarEl1,
            (2, 0, 1, 0, 4) => OslarEl1,
            (2, 0, 1, 1, 4) => OslsrEl1,
            (2, 0, 1, 3, 4) => OsdlrEl1,

            // Identification.
            (3, 0, 0, 0, 0) => MidrEl1,
            (3, 0, 0, 0, 5) => MpidrEl1,
            (3, 0, 0, 0, 6) => RevidrEl1,
            (3, 0, 0, crm @ 1..=7, op2) => Id((crm * 8 + op2) as u8),
            (3, 1, 0, 0, 0) => CcsidrEl1,
            (3, 1, 0, 0, 1) => ClidrEl1,
            (3, 1, 0, 0, 7) => AidrEl1,
            (3, 2, 0, 0, 0) => CsselrEl1,
            (3, 3, 0, 0, 1) => CtrEl0,
            (3, 3, 0, 0, 7) => DczidEl0,

            // Control and MMU.
            (3, 0, 1, 0, 0) => SctlrEl1,
            (3, 0, 1, 0, 1) => ActlrEl1,
            (3, 0, 1, 0, 2) => CpacrEl1,
            (3, 0, 2, 0, 0) => Ttbr0El1,
            (3, 0, 2, 0, 1) => Ttbr1El1,
            (3, 0, 2, 0, 2) => TcrEl1,
            (3, 0, 10, 2, 0) => MairEl1,
            (3, 0, 10, 3, 0) => AmairEl1,
            (3, 0, 13, 0, 1) => ContextidrEl1,
            (3, 0, 13, 0, 4) => TpidrEl1,
            (3, 0, 14, 1, 0) => CntkctlEl1,

            // PSTATE and special registers.
            (3, 0, 4, 0, 0) => SpsrEl1,
            (3, 0, 4, 0, 1) => ElrEl1,
            (3, 0, 4, 1, 0) => SpEl0,
            (3, 0, 4, 2, 0) => SpSel,
            (3, 0, 4, 2, 2) => CurrentEl,
            (3, 3, 4, 2, 0) => Nzcv,
            (3, 3, 4, 2, 1) => Daif,
            (3, 3, 4, 4, 0) => Fpcr,
            (3, 3, 4, 4, 1) => Fpsr,

            // Exceptions.
            (3, 0, 5, 1, 0) => Afsr0El1,
            (3, 0, 5, 1, 1) => Afsr1El1,
            (3, 0, 5, 2, 0) => EsrEl1,
            (3, 0, 6, 0, 0) => FarEl1,
            (3, 0, 7, 4, 0) => ParEl1,
            (3, 0, 12, 0, 0) => VbarEl1,
            (3, 0, 12, 0, 1) => RvbarEl1,
            (3, 0, 12, 1, 0) => IsrEl1,

            (3, 3, 9, 14, 0) => PmuserenrEl0,

            // Cortex-A53 IMPLEMENTATION DEFINED (QEMU:
            // cortex_a72_a57_a53_cp_reginfo).
            (3, 1, 11, 0, 2 | 3) | (3, 1, 15, 0, 0) | (3, 1, 15, 2, 0..=3) => ImpDefEl1,
            (3, 1, 15, 3, 0) => CbarEl1,
            (3, 3, 13, 0, 2) => TpidrEl0,
            (3, 3, 13, 0, 3) => TpidrroEl0,

            // Generic timer.
            (3, 3, 14, 0, 0) => Env(CntfrqEl0),
            (3, 3, 14, 0, 1) => Env(CntpctEl0),
            (3, 3, 14, 0, 2) => Env(CntvctEl0),
            (3, 3, 14, 2, 0) => Env(CntpTvalEl0),
            (3, 3, 14, 2, 1) => Env(CntpCtlEl0),
            (3, 3, 14, 2, 2) => Env(CntpCvalEl0),
            (3, 3, 14, 3, 0) => Env(CntvTvalEl0),
            (3, 3, 14, 3, 1) => Env(CntvCtlEl0),
            (3, 3, 14, 3, 2) => Env(CntvCvalEl0),

            // GICv3, system register interface (5 priority bits:
            // only AP0R0 and AP1R0 exist).
            (3, 0, 4, 6, 0) => Env(IccPmrEl1),
            (3, 0, 12, 8, 0) => Env(IccIar0El1),
            (3, 0, 12, 8, 1) => Env(IccEoir0El1),
            (3, 0, 12, 8, 2) => Env(IccHppir0El1),
            (3, 0, 12, 8, 3) => Env(IccBpr0El1),
            (3, 0, 12, 8, 4) => Env(IccAp0r0El1),
            (3, 0, 12, 9, 0) => Env(IccAp1r0El1),
            (3, 0, 12, 11, 1) => Env(IccDirEl1),
            (3, 0, 12, 11, 3) => Env(IccRprEl1),
            (3, 0, 12, 11, 5) => Env(IccSgi1rEl1),
            (3, 0, 12, 11, 6) => Env(IccAsgi1rEl1),
            (3, 0, 12, 11, 7) => Env(IccSgi0rEl1),
            (3, 0, 12, 12, 0) => Env(IccIar1El1),
            (3, 0, 12, 12, 1) => Env(IccEoir1El1),
            (3, 0, 12, 12, 2) => Env(IccHppir1El1),
            (3, 0, 12, 12, 3) => Env(IccBpr1El1),
            (3, 0, 12, 12, 4) => Env(IccCtlrEl1),
            (3, 0, 12, 12, 5) => Env(IccSreEl1),
            (3, 0, 12, 12, 6) => Env(IccIgrpen0El1),
            (3, 0, 12, 12, 7) => Env(IccIgrpen1El1),
            _ => return None,
        })
    }
}
