//! Registri di sistema: nomi e codifiche `(op0, op1, CRn, CRm, op2)` di MRS e
//! MSR (Arm ARM, C5.3 e D17).
//!
//! Il decoder riconosce qui tutti i registri che Vetro modella, a qualunque
//! livello di eccezione; chi esegue decide l'accesso (EL0 o EL1, sola lettura
//! o sola scrittura, trap). In modalità utente valgono solo quelli di M1
//! (vedi [`SysReg::is_el0_legacy`]); gli altri restano `Unimplemented` come
//! prima della modalità sistema.

/// Registri di sistema modellati.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysReg {
    // --- Accessibili a EL0 fin da M1 ---
    Nzcv,
    TpidrEl0,
    /// Sola lettura a EL0, lettura e scrittura a EL1.
    TpidrroEl0,
    Fpcr,
    Fpsr,
    /// Sola lettura: dimensione del blocco di DC ZVA.
    DczidEl0,
    /// Sola lettura: geometria delle cache.
    CtrEl0,

    // --- PSTATE e registri speciali ---
    Daif,
    CurrentEl,
    SpSel,
    SpEl0,
    ElrEl1,
    SpsrEl1,

    // --- Controllo del sistema e MMU ---
    SctlrEl1,
    ActlrEl1,
    CpacrEl1,
    TcrEl1,
    Ttbr0El1,
    Ttbr1El1,
    MairEl1,
    AmairEl1,
    ContextidrEl1,

    // --- Eccezioni ---
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

    // --- Identificazione ---
    MidrEl1,
    MpidrEl1,
    RevidrEl1,
    AidrEl1,
    ClidrEl1,
    CcsidrEl1,
    CsselrEl1,
    /// Spazio degli ID (op0 = 3, op1 = 0, CRn = 0, CRm = 1..=7): indice
    /// `CRm * 8 + op2`. Le codifiche riservate valgono zero.
    Id(u8),

    // --- Debug (solo memoria dei valori, niente eccezioni di debug) ---
    MdscrEl1,
    MdccintEl1,
    OslarEl1,
    OslsrEl1,
    OsdlrEl1,
    MdrarEl1,
    /// DBGBVR<n>_EL1, n < 6 sulla Cortex-A53.
    DbgbvrEl1(u8),
    DbgbcrEl1(u8),
    /// DBGWVR<n>_EL1, n < 4 sulla Cortex-A53.
    DbgwvrEl1(u8),
    DbgwcrEl1(u8),
    PmuserenrEl0,

    // --- IMPLEMENTATION DEFINED della Cortex-A53 (come QEMU) ---
    /// L2CTLR, L2ECTLR, L2ACTLR, CPUACTLR, CPUECTLR, CPUMERRSR, L2MERRSR:
    /// RAZ/WI a EL1.
    ImpDefEl1,
    /// CBAR_EL1: base delle periferiche (il distributore del GIC), sola lettura.
    CbarEl1,

    /// Registro gestito dall'ambiente (timer generico, interfaccia CPU del
    /// GIC): la CPU controlla l'accesso e passa lettura e scrittura a
    /// [`CpuEnv`](crate::sys::CpuEnv).
    Env(EnvReg),
}

/// Registri che la CPU non tiene in sé: li implementa la piattaforma.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvReg {
    // Timer generico.
    CntfrqEl0,
    CntpctEl0,
    CntvctEl0,
    CntpTvalEl0,
    CntpCtlEl0,
    CntpCvalEl0,
    CntvTvalEl0,
    CntvCtlEl0,
    CntvCvalEl0,
    // Interfaccia CPU del GICv3 (gruppo 0 e 1).
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

/// Direzioni ammesse da un registro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rw {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

impl EnvReg {
    /// Registri del timer generico (accesso da EL0 governato da CNTKCTL_EL1).
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
    /// I registri che il decoder di M1 conosceva: gli unici eseguiti in
    /// modalità utente.
    pub fn is_el0_legacy(self) -> bool {
        use SysReg::*;
        matches!(self, Nzcv | TpidrEl0 | TpidrroEl0 | Fpcr | Fpsr | DczidEl0 | CtrEl0)
    }

    /// Direzioni ammesse a EL1 (a EL0 decidono le regole di accesso).
    pub fn rw(self) -> Rw {
        use SysReg::*;
        match self {
            DczidEl0 | CtrEl0 | CurrentEl | IsrEl1 | RvbarEl1 | MidrEl1 | MpidrEl1 | RevidrEl1 | AidrEl1
            | ClidrEl1 | CcsidrEl1 | Id(_) | OslsrEl1 | MdrarEl1 | CbarEl1 => Rw::ReadOnly,
            OslarEl1 => Rw::WriteOnly,
            Env(e) => e.rw(),
            _ => Rw::ReadWrite,
        }
    }

    /// Codifiche di registri che la Cortex-A53 ha ma Vetro non modella
    /// ancora: la PMU (PMUv3). Tutto il resto fuori da [`SysReg::lookup`] non
    /// esiste sulla A53 (estensioni successive o codifiche libere) ed è
    /// UNDEFINED, come in QEMU.
    pub fn is_unmodelled_a53(op0: u32, op1: u32, crn: u32, crm: u32, _op2: u32) -> bool {
        op0 == 3 && matches!((op1, crn, crm), (3, 9, 12..=14) | (0, 9, 14) | (3, 14, 8..=15))
    }

    /// Registro dalla codifica di MRS/MSR, se Vetro lo modella.
    pub fn lookup(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> Option<SysReg> {
        use EnvReg::*;
        use SysReg::*;
        Some(match (op0, op1, crn, crm, op2) {
            // Debug (op0 = 2). La Cortex-A53 ha 6 breakpoint e 4 watchpoint.
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

            // Identificazione.
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

            // Controllo e MMU.
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

            // PSTATE e registri speciali.
            (3, 0, 4, 0, 0) => SpsrEl1,
            (3, 0, 4, 0, 1) => ElrEl1,
            (3, 0, 4, 1, 0) => SpEl0,
            (3, 0, 4, 2, 0) => SpSel,
            (3, 0, 4, 2, 2) => CurrentEl,
            (3, 3, 4, 2, 0) => Nzcv,
            (3, 3, 4, 2, 1) => Daif,
            (3, 3, 4, 4, 0) => Fpcr,
            (3, 3, 4, 4, 1) => Fpsr,

            // Eccezioni.
            (3, 0, 5, 1, 0) => Afsr0El1,
            (3, 0, 5, 1, 1) => Afsr1El1,
            (3, 0, 5, 2, 0) => EsrEl1,
            (3, 0, 6, 0, 0) => FarEl1,
            (3, 0, 7, 4, 0) => ParEl1,
            (3, 0, 12, 0, 0) => VbarEl1,
            (3, 0, 12, 0, 1) => RvbarEl1,
            (3, 0, 12, 1, 0) => IsrEl1,

            (3, 3, 9, 14, 0) => PmuserenrEl0,

            // IMPLEMENTATION DEFINED della Cortex-A53 (QEMU:
            // cortex_a72_a57_a53_cp_reginfo).
            (3, 1, 11, 0, 2 | 3) | (3, 1, 15, 0, 0) | (3, 1, 15, 2, 0..=3) => ImpDefEl1,
            (3, 1, 15, 3, 0) => CbarEl1,
            (3, 3, 13, 0, 2) => TpidrEl0,
            (3, 3, 13, 0, 3) => TpidrroEl0,

            // Timer generico.
            (3, 3, 14, 0, 0) => Env(CntfrqEl0),
            (3, 3, 14, 0, 1) => Env(CntpctEl0),
            (3, 3, 14, 0, 2) => Env(CntvctEl0),
            (3, 3, 14, 2, 0) => Env(CntpTvalEl0),
            (3, 3, 14, 2, 1) => Env(CntpCtlEl0),
            (3, 3, 14, 2, 2) => Env(CntpCvalEl0),
            (3, 3, 14, 3, 0) => Env(CntvTvalEl0),
            (3, 3, 14, 3, 1) => Env(CntvCtlEl0),
            (3, 3, 14, 3, 2) => Env(CntvCvalEl0),

            // GICv3, interfaccia a registri di sistema (5 bit di priorità:
            // esistono solo AP0R0 e AP1R0).
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
