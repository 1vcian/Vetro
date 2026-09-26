//! Traduttore: da una regione di istruzioni decodificate (i blocchi base di
//! una pagina, ADR 0024) a una funzione WASM con l'ABI di docs/specs/jit.md,
//! più il modulo di runtime ([`runtime`]) e il dispatcher ([`dispatcher`]).
//!
//! La semantica di ogni istruzione tradotta è quella di
//! `vetro_cpu::exec` (stessi flag, stessi troncamenti a 32 bit, stessi casi
//! di XZR/SP); la parità si verifica con i test di `vetro-jit-native` e di
//! `tests/diff`.
//!
//! Dentro la regione i registri stanno in variabili locali: si caricano
//! all'inizio quelli letti o scritti e si riscrivono in `JitState` nella
//! coda comune a tutte le uscite. Un'uscita per fault (dopo un accesso
//! fallito) lascia i registri delle istruzioni precedenti, `pc` e `steps`
//! dell'istruzione che ha fallito: lo stato è quello che l'interprete
//! avrebbe prima di eseguirla. I flag NZCV sono pigri: operandi e tipo
//! dell'ultima istruzione che li scrive, calcolati solo se servono.
//!
//! Il codice è compatto (in V8 la compilazione costa in proporzione ai
//! byte): i percorsi lenti e gli accessi a coppie, Q e non allineati stanno
//! nel runtime, compilato una volta; il percorso veloce della TLB software
//! degli accessi allineati resta nella regione.

use crate::engine::TABLE_SIZE;
use crate::state::{area, off};
use crate::wasm::{BLOCK_EMPTY, Func, MemoryImport, Module, ValType, op};
use crate::{FAULT, NEXT, STOP, SVC, YIELD};
use vetro_cpu::Insn;
use vetro_cpu::decode::{
    AddrMode, BfOp, BrOp, CcmpOperand, CselOp, Dp1Op, Dp2Op, Dp3Op, Index, LogicOp, MemOp, MovOp,
    PstateField, Shift, SysReg,
};
use vetro_cpu::simd::{CopyOp, IntInsn, MovImmOp, SimdInsn, VecMemInsn};

mod fp;
mod vec;

/// PSTATE.{D,A,I,F} nei bit 9:6 (come `SysState::daif`).
const DAIF_ALL: u32 = 0x3c0;

/// Percorso veloce della TLB in linea nelle regioni (esperimento).
const INLINE_TLB: bool = true;

/// Istruzioni massime per blocco (ADR 0012).
pub const MAX_BLOCK: usize = 64;

/// `size` di `st` per DC ZVA: azzera i 64 byte (allineati) all'indirizzo,
/// con le regole di `zero_block` (fault di allineamento su memoria Device).
pub const ZVA_BYTES: u32 = 64;

/// Come si comporta un'istruzione per il traduttore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Tradotta, il blocco continua. I salti condizionati (B.cond,
    /// CBZ/CBNZ, TBZ/TBNZ) sono di questo tipo: presi escono dal blocco
    /// ("uscita laterale"), altrimenti il blocco prosegue.
    Linear,
    /// Tradotta, e chiude il blocco (salto incondizionato).
    Branch,
    /// Chiude il blocco con il codice `SVC`, senza essere eseguita:
    /// l'esegue l'interprete.
    Svc,
    /// Non tradotta: il blocco finisce prima e la esegue l'interprete.
    Unsupported,
}

/// Classificazione di un'istruzione.
pub fn kind(insn: &Insn) -> Kind {
    use Kind::*;
    match *insn {
        Insn::AddSubImm { .. }
        | Insn::LogicalImm { .. }
        | Insn::MoveWide { .. }
        | Insn::Adr { .. }
        | Insn::Bitfield { .. }
        | Insn::Extract { .. }
        | Insn::LogicalReg { .. }
        | Insn::AddSubReg { .. }
        | Insn::AddSubExt { .. }
        | Insn::AddSubCarry { .. }
        | Insn::CondCmp { .. }
        | Insn::CondSel { .. }
        | Insn::Dp1 { .. }
        | Insn::Dp3 { .. }
        | Insn::Nop
        | Insn::Barrier
        | Insn::CacheMaint
        | Insn::Wfi
        | Insn::Wfe
        | Insn::LdLiteral { .. }
        | Insn::LdStPair { .. }
        | Insn::LoadAcquire { .. }
        | Insn::StoreRelease { .. }
        | Insn::BCond { .. }
        | Insn::Cbz { .. }
        | Insn::Tbz { .. } => Linear,
        Insn::Dp2 { .. } => Linear,
        Insn::Mrs { reg: SysReg::Nzcv, .. } | Insn::Msr { reg: SysReg::Nzcv, .. } => Linear,
        Insn::LdSt { .. } => Linear,
        // SIMD (ADR 0024): load/store di registri V singoli e in coppia,
        // DUP/INS/UMOV/SMOV, MOVI/MVNI/ORR/BIC immediati.
        Insn::Simd(SimdInsn::Mem(VecMemInsn::Reg { .. } | VecMemInsn::Pair { .. })) => Linear,
        // Tutte le istruzioni SIMD/FP senza memoria (ADR 0026): quelle
        // senza una forma in linea le esegue l'interprete dalla regione
        // (`env.simd`, [`crate::helper`]).
        Insn::Simd(SimdInsn::Int(_) | SimdInsn::Fp(_) | SimdInsn::Crypto(_)) => Linear,
        Insn::B { .. } | Insn::BranchReg { .. } => Branch,
        Insn::Svc { .. } => Svc,
        _ => Unsupported,
    }
}

/// Parametri di un blocco della modalità sistema: il livello di eccezione e
/// il Top Byte Ignore delle due metà dello spazio virtuale (TCR_EL1.TBI0 e
/// TBI1), che decide l'indirizzo dei salti (`AArch64.BranchAddr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SysTarget {
    pub el: u8,
    pub tbi0: bool,
    pub tbi1: bool,
    /// PSTATE.SP (a EL1: SP_EL0 si legge con MRS solo se vale 1).
    pub spsel: bool,
    /// Istruzioni FP/SIMD permesse a questo EL (CPACR_EL1.FPEN): senza, le
    /// traduce solo l'interprete (trap).
    pub fp: bool,
}

/// Da dove legge un MRS tradotto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MrsSrc {
    /// Campo di `JitState`.
    State(u32),
    /// Campo a 32 bit di `JitState`.
    State32(u32),
    Const(u64),
}

/// MRS che un blocco della modalità sistema esegue da sé: i registri che
/// cambiano solo con MSR tradotti (o fra una corsa e l'altra), con i
/// permessi di `sysreg_access` per il livello del blocco.
fn sys_mrs(reg: SysReg, s: SysTarget) -> Option<MrsSrc> {
    let el1 = s.el == 1;
    Some(match reg {
        SysReg::TpidrEl0 => MrsSrc::State(off::TPIDR_EL0),
        SysReg::TpidrroEl0 => MrsSrc::State(off::TPIDRRO_EL0),
        SysReg::DczidEl0 => MrsSrc::State(off::DCZID),
        SysReg::TpidrEl1 if el1 => MrsSrc::State(off::TPIDR_EL1),
        SysReg::SpEl0 if el1 && s.spsel => MrsSrc::State(off::SP_EL0),
        SysReg::TcrEl1 if el1 => MrsSrc::State(off::TCR),
        SysReg::CurrentEl if el1 => MrsSrc::Const(4),
        SysReg::Daif if el1 => MrsSrc::State32(off::DAIF),
        SysReg::ElrEl1 if el1 => MrsSrc::State(off::ELR_EL1),
        SysReg::SpsrEl1 if el1 => MrsSrc::State(off::SPSR_EL1),
        SysReg::EsrEl1 if el1 => MrsSrc::State(off::ESR_EL1),
        SysReg::FarEl1 if el1 => MrsSrc::State(off::FAR_EL1),
        _ => return None,
    })
}

/// MSR che un blocco della modalità sistema esegue da sé: campo di
/// `JitState` da scrivere.
fn sys_msr(reg: SysReg, s: SysTarget) -> Option<u32> {
    let el1 = s.el == 1;
    Some(match reg {
        SysReg::TpidrEl0 => off::TPIDR_EL0,
        SysReg::TpidrroEl0 if el1 => off::TPIDRRO_EL0,
        SysReg::TpidrEl1 if el1 => off::TPIDR_EL1,
        SysReg::SpEl0 if el1 && s.spsel => off::SP_EL0,
        SysReg::ElrEl1 if el1 => off::ELR_EL1,
        SysReg::SpsrEl1 if el1 => off::SPSR_EL1,
        // MSR DAIF: a parte (può smascherare interrupt).
        SysReg::Daif if el1 => off::DAIF,
        _ => return None,
    })
}

impl SysTarget {
    /// `AArch64.BranchAddr` come `Cpu::branch_addr`: con TBI attivo per la
    /// metà di `t` il tag si toglie estendendo il bit 55.
    pub fn branch_addr(&self, t: u64) -> u64 {
        let tbi = if t >> 55 & 1 != 0 { self.tbi1 } else { self.tbi0 };
        if tbi { ((t << 8) as i64 >> 8) as u64 } else { t }
    }
}

/// Classificazione di un'istruzione in modalità utente (`sys = None`) o
/// sistema. In modalità sistema restano all'interprete anche WFI (attesa
/// degli interrupt), LDTR/STTR (permessi di EL0) e le manutenzioni delle
/// cache a EL0 (SCTLR_EL1.UCI); si traducono in più le esclusive (il
/// monitor sta in `JitState`), DC ZVA e alcuni MRS/MSR ([`sys_mrs`],
/// [`sys_msr`]).
pub fn kind_in(insn: &Insn, sys: Option<SysTarget>) -> Kind {
    if let Some(s) = sys {
        match *insn {
            Insn::Wfi | Insn::LdSt { unpriv: true, .. } => return Kind::Unsupported,
            // Trap di CPACR_EL1.FPEN: all'interprete.
            Insn::Simd(_) if !s.fp => return Kind::Unsupported,
            Insn::CacheMaint if s.el == 0 => return Kind::Unsupported,
            Insn::Exclusive { .. } | Insn::DcZva { .. } => return Kind::Linear,
            // DAIFSet/DAIFClr a EL1 (DAIFClr esce con YIELD).
            Insn::MsrImm { field: PstateField::DaifSet | PstateField::DaifClr, .. } if s.el == 1 => {
                return Kind::Linear;
            }
            Insn::Mrs { reg, .. } if reg != SysReg::Nzcv => {
                return if sys_mrs(reg, s).is_some() { Kind::Linear } else { Kind::Unsupported };
            }
            Insn::Msr { reg, .. } if reg != SysReg::Nzcv => {
                return if sys_msr(reg, s).is_some() { Kind::Linear } else { Kind::Unsupported };
            }
            _ => {}
        }
    }
    kind(insn)
}

/// Un blocco base: istruzioni consecutive (già decodificate) da `pc`.
/// Finisce con un salto (anche condizionato), una SVC, o prima di
/// un'istruzione che non si traduce, dell'inizio di un altro blocco della
/// regione, della fine della pagina o dopo [`MAX_BLOCK`] istruzioni.
#[derive(Clone, Debug)]
pub struct Bb {
    pub pc: u64,
    pub insns: Vec<Insn>,
    /// Le parole delle istruzioni (per `env.simd`).
    pub words: Vec<u32>,
}

impl Bb {
    /// Istruzioni eseguite al massimo da una corsa del blocco (l'eventuale
    /// SVC finale non si esegue).
    pub fn max_steps(&self) -> u64 {
        let n = self.insns.len();
        match self.insns.last() {
            Some(i) if kind(i) == Kind::Svc => (n - 1) as u64,
            _ => n as u64,
        }
    }
}

/// Una regione da tradurre in una funzione (ADR 0024): i blocchi base di
/// una pagina raggiungibili dall'ingresso `pc` con salti diretti (anche
/// cicli), con i parametri della modalità sistema. I salti fra blocchi
/// della regione restano dentro la funzione; ogni altro salto è un'uscita.
#[derive(Clone, Debug)]
pub struct Region {
    pub pc: u64,
    /// In ordine di indirizzo; uno inizia a `pc`.
    pub bbs: Vec<Bb>,
    pub sys: Option<SysTarget>,
}

/// Istruzioni massime di una regione.
pub const MAX_REGION: usize = 64;

/// Blocchi base di una regione che possono farne da ingresso.
pub const MAX_ENTRIES: usize = 64;

/// Salti condizionati (B.cond, CBZ/CBNZ, TBZ/TBNZ): chiudono un blocco base.
pub fn is_cond_branch(insn: &Insn) -> bool {
    matches!(insn, Insn::BCond { .. } | Insn::Cbz { .. } | Insn::Tbz { .. })
}

/// Destinazione di un salto diretto (B, BL, salti condizionati) a `pc`,
/// con `AArch64.BranchAddr` in modalità sistema.
pub fn direct_target(insn: &Insn, pc: u64, sys: Option<SysTarget>) -> Option<u64> {
    let off = match *insn {
        Insn::B { offset, .. } => offset,
        Insn::BCond { offset, .. } | Insn::Cbz { offset, .. } | Insn::Tbz { offset, .. } => offset,
        _ => return None,
    };
    let t = pc.wrapping_add(off as u64);
    Some(match sys {
        Some(s) => s.branch_addr(t),
        None => t,
    })
}

impl Region {
    /// Una sequenza lineare di istruzioni da `pc`, divisa in blocchi base
    /// dopo ogni salto e SVC (per i test).
    pub fn linear(pc: u64, words: Vec<u32>, sys: Option<SysTarget>) -> Region {
        let mut bbs = Vec::new();
        let mut cur = Bb { pc, insns: Vec::new(), words: Vec::new() };
        for (i, w) in words.into_iter().enumerate() {
            let insn = vetro_cpu::decode(w);
            let end = is_cond_branch(&insn) || kind(&insn) != Kind::Linear;
            cur.insns.push(insn);
            cur.words.push(w);
            if end || cur.insns.len() == MAX_BLOCK {
                let next = pc.wrapping_add(4 * (i as u64 + 1));
                bbs.push(std::mem::replace(&mut cur, Bb { pc: next, insns: Vec::new(), words: Vec::new() }));
            }
        }
        if !cur.insns.is_empty() {
            bbs.push(cur);
        }
        Region { pc, bbs, sys }
    }

    /// Il blocco d'ingresso.
    pub fn entry(&self) -> &Bb {
        self.bbs.iter().find(|b| b.pc == self.pc).expect("blocco d'ingresso")
    }

    /// Passi massimi del blocco d'ingresso: una corsa che vi entra ne fa
    /// almeno uno se il limite li concede (ogni altro blocco controlla da
    /// sé il limite di passi).
    pub fn max_steps(&self) -> u64 {
        self.entry().max_steps()
    }

    /// Ingressi della regione: (indirizzo, indice del blocco base, passi
    /// massimi del blocco) per ogni blocco base che esegue almeno
    /// un'istruzione e ha indice minore di [`MAX_ENTRIES`] (la cache dei
    /// salti tiene l'indice in 6 bit). Chi chiama la regione scrive
    /// l'indice in `JitState::entry`.
    pub fn entries(&self) -> Vec<(u64, u32, u64)> {
        self.bbs
            .iter()
            .enumerate()
            .filter(|(i, b)| *i < MAX_ENTRIES && b.max_steps() > 0)
            .map(|(i, b)| (b.pc, i as u32, b.max_steps()))
            .collect()
    }

    /// Indice del blocco base d'ingresso.
    pub fn entry_index(&self) -> u32 {
        self.bbs.iter().position(|b| b.pc == self.pc).expect("blocco d'ingresso") as u32
    }

    /// Istruzioni della regione.
    pub fn len(&self) -> usize {
        self.bbs.iter().map(|b| b.insns.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.bbs.is_empty()
    }
}

/// Scopre la regione che inizia a `pc`: dall'ingresso segue il codice in
/// sequenza e i salti diretti (presi e non presi) che restano nella pagina
/// di `pc`, fino a `max` istruzioni. `fetch(a)` dà la parola all'indirizzo
/// `a` (della stessa pagina), o `None` se non si può leggere.
///
/// Restituisce la regione e la sua firma (posizione e lunghezza di ogni
/// blocco, poi le parole: lo stesso codice allo stesso indirizzo ha la
/// stessa firma), o `None` se la prima istruzione non si traduce o è una
/// SVC.
pub fn discover(
    pc: u64,
    sys: Option<SysTarget>,
    max: usize,
    mut fetch: impl FnMut(u64) -> Option<u32>,
) -> Option<(Region, Vec<u32>)> {
    use std::collections::{BTreeMap, BTreeSet};
    if !pc.is_multiple_of(4) {
        return None;
    }
    let page = pc >> 12;
    let in_page = |a: u64| a >> 12 == page && a.is_multiple_of(4);
    // Inizi di blocco dai salti; gli altri (al più MAX_REGION / MAX_BLOCK)
    // li aggiunge la divisione dei blocchi lunghi: tutti restano sotto
    // MAX_ENTRIES e ogni blocco base può fare da ingresso.
    const MAX_LEADERS: usize = MAX_ENTRIES - MAX_REGION / MAX_BLOCK;
    let mut decoded: BTreeMap<u64, (u32, Insn)> = BTreeMap::new();
    let mut leaders: BTreeSet<u64> = BTreeSet::new();
    let mut work = vec![pc];
    leaders.insert(pc);
    while let Some(s) = work.pop() {
        let mut a = s;
        while in_page(a) && decoded.len() < max {
            if a != s && decoded.contains_key(&a) {
                // Codice già visto: un inizio (se c'è posto; altrimenti il
                // blocco base lo ripete).
                if leaders.len() < MAX_LEADERS {
                    leaders.insert(a);
                }
                break;
            }
            if decoded.contains_key(&a) {
                break;
            }
            let Some(w) = fetch(a) else { break };
            let insn = vetro_cpu::decode(w);
            let k = kind_in(&insn, sys);
            if k == Kind::Unsupported || (k == Kind::Svc && a == pc) {
                break;
            }
            decoded.insert(a, (w, insn));
            let mut follow = |t: u64| {
                if in_page(t) && leaders.len() < MAX_LEADERS && leaders.insert(t) {
                    work.push(t);
                }
            };
            match k {
                Kind::Svc => break,
                Kind::Branch => {
                    if let Some(t) = direct_target(&insn, a, sys) {
                        follow(t);
                    }
                    break;
                }
                _ if is_cond_branch(&insn) => {
                    let t = direct_target(&insn, a, sys).expect("salto diretto");
                    follow(t);
                    follow(a.wrapping_add(4));
                    break;
                }
                _ => {}
            }
            a = a.wrapping_add(4);
        }
    }
    if !decoded.contains_key(&pc) {
        return None;
    }
    // Blocchi base: da ogni inizio, fino a un salto, a un altro inizio o a
    // un buco; al più MAX_BLOCK istruzioni (poi un inizio nuovo).
    let mut bbs = Vec::new();
    let mut todo: Vec<u64> = leaders.iter().copied().filter(|l| decoded.contains_key(l)).collect();
    let mut seen: BTreeSet<u64> = todo.iter().copied().collect();
    while let Some(l) = todo.pop() {
        let mut bb = Bb { pc: l, insns: Vec::new(), words: Vec::new() };
        let mut a = l;
        while let Some(&(w, insn)) = decoded.get(&a) {
            if a != l && leaders.contains(&a) {
                break;
            }
            if bb.insns.len() == MAX_BLOCK {
                if seen.insert(a) {
                    leaders.insert(a);
                    todo.push(a);
                }
                break;
            }
            bb.insns.push(insn);
            bb.words.push(w);
            if kind_in(&insn, sys) != Kind::Linear || is_cond_branch(&insn) {
                break;
            }
            a = a.wrapping_add(4);
        }
        bbs.push(bb);
    }
    bbs.sort_by_key(|b| b.pc);
    let mut sig = Vec::new();
    for b in &bbs {
        sig.push(((b.pc & 0xfff) as u32) << 16 | b.insns.len() as u32);
        for i in 0..b.insns.len() as u64 {
            sig.push(decoded[&(b.pc + 4 * i)].0);
        }
    }
    Some((Region { pc, bbs, sys }, sig))
}

/// Variabili locali: 0 = puntatore a `JitState`, 1..=31 x0..x30, 32 SP,
/// 33 NZCV (i32), poi temporanei.
const L_STATE: u32 = 0;
const L_NZCV: u32 = 33;
const L_T64: u32 = 34;
const N_T64: u32 = 18;
/// `steps` all'inizio del blocco base in corso.
const L_STEPS: u32 = L_T64 + 10;
/// `limit` di `JitState` (regioni con più blocchi).
const L_LIMIT: u32 = L_T64 + 13;
/// Inizio della pagina della regione: base dei `pc` passati ai percorsi
/// lenti e delle uscite.
const L_PC0: u32 = L_T64 + 14;
/// Flag pigri (ADR 0024): operandi e risultato dell'ultima istruzione che
/// scrive NZCV, e il suo tipo in `L_FK` (0 = NZCV già in `L_NZCV`).
const L_FA: u32 = L_T64 + 15;
const L_FB: u32 = L_T64 + 16;
const L_FR: u32 = L_T64 + 17;
/// Uscita in corso: nuovo `pc`, passi fatti e codice (per la coda comune).
const L_EXIT_PC: u32 = L_T64 + 11;
const L_EXIT_DONE: u32 = L_T64 + 12;
const L_T32: u32 = L_T64 + N_T64;
const N_T32: u32 = 7;
const L_EXIT_CODE: u32 = L_T32 + 4;
/// Prossimo blocco base (indice) per il `br_table` della regione.
const L_NEXT: u32 = L_T32 + 5;
const L_FK: u32 = L_T32 + 6;
/// Temporanei v128 (SIMD in linea, ADR 0026).
const L_V0: u32 = L_T32 + N_T32;
const N_V128: u32 = 2;

/// Codice d'uscita interno: STOP dopo l'istruzione corrente, di cui lo
/// store ha già salvato `pc` e `steps` (la coda li porta all'istruzione
/// successiva ed esce con `STOP`).
const EXIT_STOP_SAVED: u32 = 0x10;
const _: () = assert!(EXIT_STOP_SAVED > crate::YIELD, "codice interno distinto da quelli dell'ABI");

/// Tipi dei flag pigri (`L_FK`): somma, differenza, logica, a 64 o 32 bit.
const FK_ADD64: i32 = crate::state::fk::ADD64 as i32;
const FK_SUB64: i32 = crate::state::fk::SUB64 as i32;
const FK_ADD32: i32 = crate::state::fk::ADD32 as i32;
const FK_SUB32: i32 = crate::state::fk::SUB32 as i32;
const FK_LOGIC64: i32 = crate::state::fk::LOGIC64 as i32;
const FK_LOGIC32: i32 = crate::state::fk::LOGIC32 as i32;

/// Che cosa sa il traduttore di NZCV nel punto corrente.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fl {
    /// Dipende dal percorso: `L_FK` dice se è pigro.
    Unknown,
    /// In `L_NZCV` (`L_FK` = 0).
    Materialized,
    /// Pigro, del tipo dato (`L_FK` ha lo stesso valore).
    Lazy(i32),
}

/// Bit dei registri nelle maschere di lettura/scrittura: 0..=30 x, 31 SP,
/// 32 NZCV.
const B_SP: u32 = 31;
const B_NZCV: u32 = 32;

#[inline]
const fn t64(i: u32) -> u32 {
    L_T64 + i
}
#[inline]
const fn t32(i: u32) -> u32 {
    L_T32 + i
}

/// Genera il modulo WASM con un blocco per elemento di `blocks`: la
/// funzione del blocco `i` si esporta come `b<i>`. In modalità sistema il
/// motore le mette poi nella tabella del dispatcher (`Engine::place`).
pub fn module(blocks: &[Region], memory: MemoryImport) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t_blk = m.ty(&[I32], &[I32]);
    m.import_memory("env", "mem", memory);
    for id in 0..n_rt() {
        let (name, p, r) = rt_sig(id);
        let t = m.ty(&p, &r);
        assert_eq!(m.import_func("rt", &name, t), id);
    }
    for (i, b) in blocks.iter().enumerate() {
        let f = function(b);
        let idx = m.func(t_blk, f);
        m.export_func(&format!("b{i}"), idx);
    }
    m.encode()
}

/// Funzioni del modulo di runtime (ADR 0024), importate da ogni modulo di
/// regioni come `rt.<nome>` negli indici `0..N_RT` (nello stesso ordine):
/// i percorsi lenti e gli accessi alla memoria fuori dal codice delle
/// regioni, compilati una volta sola.
const F_SAVE: u32 = 0;
const F_LD_SLOW: u32 = 1;
const F_ST_SLOW: u32 = 2;
const F_NZCV: u32 = 3;
/// Accessi della modalità sistema con la TLB software (percorso veloce e
/// lento): per EL (0, 1) e dimensione (1, 2, 4, 8 byte).
const F_LD_TLB: u32 = 4;
const F_ST_TLB: u32 = 12;
/// Coppie (LDP/STP) con la TLB software: per EL e dimensione (4, 8 byte).
const F_LDP_TLB: u32 = 20;
const F_STP_TLB: u32 = 24;
/// Coppie senza TLB (modalità utente).
const F_LDP_SLOW: u32 = 28;
const F_STP_SLOW: u32 = 29;
/// Fine di una regione: `pc`, `steps` e codice d'uscita in `JitState`.
const F_FINISH: u32 = 30;
/// Registri SIMD/FP in `JitState` se non ci sono già (`env.vsync`).
const F_VSYNC: u32 = 31;
/// Accessi Q (16 byte): per EL con la TLB software, e senza.
const F_LDQ_TLB: u32 = 32;
const F_LDQ_SLOW: u32 = 34;
const F_STQ_TLB: u32 = 35;
const F_STQ_SLOW: u32 = 37;
/// Metà da 8 byte di un Q allineato a 8 ma non a 16: TLB degli accessi non
/// allineati (`area::tlb_u`), poi l'host (per EL).
const F_LDU: u32 = 38;
const F_STU: u32 = 40;
/// `simd(state, parola, x, nzcv) -> valore`: `env.simd` (ADR 0026).
const F_SIMD: u32 = 42;
/// Percorsi veloci della virgola mobile (`rt.fp<k>`, [`fp::rt_ops`]).
const F_FP0: u32 = 43;

/// Funzioni del runtime.
fn n_rt() -> u32 {
    F_FP0 + fp::rt_ops().len() as u32
}

/// Bit di `size` per `env.ld`/`env.st`: metà di un accesso da 16 byte non
/// allineato a 16. L'host tratta la metà come non allineata (SCTLR_EL1.A,
/// memoria Device), come l'interprete tratta l'accesso intero.
pub const SIZE_PART_OF_MISALIGNED: u32 = 0x80;

fn f_tlb(base: u32, el: u8, bytes: u32) -> u32 {
    base + el as u32 * 4 + bytes.trailing_zeros()
}

fn f_pair(base: u32, el: u8, bytes: u32) -> u32 {
    base + el as u32 * 2 + (bytes == 8) as u32
}

/// Nome e firma della funzione `id` del runtime.
fn rt_sig(id: u32) -> (String, Vec<ValType>, Vec<ValType>) {
    use ValType::*;
    let el_n = |base: u32, per: u32| {
        let k = id - base;
        (k / per, k % per)
    };
    match id {
        F_SAVE => ("save".into(), vec![I32, I64, I64, I32], vec![]),
        F_LD_SLOW => ("ld_slow".into(), vec![I32, I64, I32, I64, I64, I32], vec![I64, I32]),
        F_ST_SLOW => ("st_slow".into(), vec![I32, I64, I32, I64, I64, I64, I32], vec![I32]),
        F_NZCV => ("nzcv".into(), vec![I32, I64, I64, I64, I32], vec![I32]),
        F_LD_TLB..F_ST_TLB => {
            let (el, lg) = el_n(F_LD_TLB, 4);
            (format!("ld{el}_{}", 1 << lg), vec![I32, I64, I64, I64, I32], vec![I64, I32])
        }
        F_ST_TLB..F_LDP_TLB => {
            let (el, lg) = el_n(F_ST_TLB, 4);
            (format!("st{el}_{}", 1 << lg), vec![I32, I64, I64, I64, I64, I32], vec![I32])
        }
        F_LDP_TLB..F_STP_TLB => {
            let (el, w) = el_n(F_LDP_TLB, 2);
            (format!("ldp{el}_{}", 4 << w), vec![I32, I64, I64, I64, I32], vec![I64, I64, I32])
        }
        F_STP_TLB..F_LDP_SLOW => {
            let (el, w) = el_n(F_STP_TLB, 2);
            (format!("stp{el}_{}", 4 << w), vec![I32, I64, I64, I64, I64, I64, I32], vec![I32])
        }
        F_LDP_SLOW => ("ldp_slow".into(), vec![I32, I64, I32, I64, I64, I32], vec![I64, I64, I32]),
        F_STP_SLOW => ("stp_slow".into(), vec![I32, I64, I32, I64, I64, I64, I64, I32], vec![I32]),
        F_FINISH => ("finish".into(), vec![I32, I32, I64, I64], vec![I32]),
        F_VSYNC => ("vsync".into(), vec![I32], vec![]),
        F_LDQ_TLB | 33 => {
            (format!("ldq{}", id - F_LDQ_TLB), vec![I32, I64, I64, I64, I32], vec![I64, I64, I32])
        }
        F_STQ_TLB | 36 => {
            (format!("stq{}", id - F_STQ_TLB), vec![I32, I64, I64, I64, I64, I64, I32], vec![I32])
        }
        F_LDQ_SLOW => ("ldq_slow".into(), vec![I32, I64, I64, I64, I32], vec![I64, I64, I32]),
        F_LDU | 39 => (format!("ldu{}", id - F_LDU), vec![I32, I64, I64, I64, I32], vec![I64, I32]),
        F_STU | 41 => (format!("stu{}", id - F_STU), vec![I32, I64, I64, I64, I64, I32], vec![I32]),
        F_STQ_SLOW => ("stq_slow".into(), vec![I32, I64, I64, I64, I64, I64, I32], vec![I32]),
        F_SIMD => ("simd".into(), vec![I32, I32, I64, I32], vec![I64]),
        _ if id >= F_FP0 && id < n_rt() => fp::rt_sig((id - F_FP0) as usize),
        _ => unreachable!("funzione del runtime sconosciuta: {id}"),
    }
}

/// Il modulo di runtime (ADR 0024): importa `env.mem`, `env.ld` ed `env.st`
/// ed esporta le funzioni `rt.*` che i moduli di regioni importano. Il
/// motore lo istanzia una volta ([`crate::Engine::runtime`]).
///
/// - `save(state, pc0, steps, packed)`: `pc` = `pc0 + (packed & 0xfff)` e
///   `steps` = `steps + (packed >> 12)` in `JitState` (prima di un accesso
///   che può fallire: la spec li vuole salvati durante `ld`/`st`);
/// - `ld_slow(state, va, size, pc0, steps, packed) -> (valore, fault)`:
///   `save` e `env.ld`; `fault` è `exit_detail` (0 se riuscito);
/// - `st_slow(state, va, size, valore, pc0, steps, packed) -> esito`:
///   `save` e `env.st`; 0, `FAULT` o `STOP`;
/// - `nzcv(k, a, b, r, vecchio) -> NZCV`: i flag di un'istruzione di tipo
///   `k` (`FK_*`) con operandi `a`, `b` e risultato `r` (troncati a 32 bit
///   per i tipi a 32), o `vecchio` se `k` = 0;
/// - `ld<el>_<n>`, `st<el>_<n>`: come `ld_slow`/`st_slow` con la TLB
///   software dell'EL prima (accessi allineati a pagine nella TLB);
/// - `ldp<el>_<n>`, `stp<el>_<n>`, `ldp_slow`, `stp_slow`: le coppie, due
///   accessi a `va` e `va + n` (il secondo non si fa se il primo fallisce;
///   un load in coppia non restituisce nulla se uno dei due fallisce).
pub fn runtime(memory: MemoryImport) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t_ld = m.ty(&[I32, I64, I32], &[I64]);
    let t_st = m.ty(&[I32, I64, I32, I64], &[I32]);
    m.import_memory("env", "mem", memory);
    let t_vsync = m.ty(&[I32], &[]);
    let (ld, st) = (m.import_func("env", "ld", t_ld), m.import_func("env", "st", t_st));
    let vsync = m.import_func("env", "vsync", t_vsync);
    let t_simd = m.ty(&[I32, I32, I64, I32], &[I64]);
    let simd = m.import_func("env", "simd", t_simd);
    // Indice nel runtime della funzione `id` (dopo gli import `env.*`).
    fn rt(id: u32) -> u32 {
        id + 4
    }
    fn def(m: &mut Module, id: u32, f: Func) {
        let (name, p, r) = rt_sig(id);
        let t = m.ty(&p, &r);
        assert_eq!(m.func(t, f), rt(id));
        m.export_func(&name, rt(id));
    }

    let mut f = Func::default();
    f.local_get(0).local_get(1).local_get(3).i32_const(0xfff).op(op::I32_AND).op(op::I64_EXTEND_I32_U);
    f.op(op::I64_ADD).i64_store(off::PC);
    f.local_get(0).local_get(2).local_get(3).i32_const(12).op(op::I32_SHR_U).op(op::I64_EXTEND_I32_U);
    f.op(op::I64_ADD).i64_store(off::STEPS);
    def(&mut m, F_SAVE, f);

    let mut f = Func::default();
    f.local_get(0).local_get(3).local_get(4).local_get(5).call(rt(F_SAVE));
    f.local_get(0).local_get(1).local_get(2).call(ld);
    f.local_get(0).i32_load(off::EXIT_DETAIL);
    def(&mut m, F_LD_SLOW, f);

    let mut f = Func::default();
    f.local_get(0).local_get(4).local_get(5).local_get(6).call(rt(F_SAVE));
    f.local_get(0).local_get(1).local_get(2).local_get(3).call(st);
    f.if_(I32 as u8).local_get(0).i32_load(off::EXIT_DETAIL).else_().i32_const(0).end();
    def(&mut m, F_ST_SLOW, f);

    // nzcv(k 0, a 1, b 2, r 3, vecchio 4); locale 5: bit del segno (i64).
    let (k, a, b, r, old, sh) = (0, 1, 2, 3, 4, 5);
    let mut f = Func { locals: vec![(1, I64)], ..Func::default() };
    f.local_get(k).op(op::I32_EQZ).if_(BLOCK_EMPTY).local_get(old).op(op::RETURN).end();
    f.local_get(k).i32_const(FK_ADD32).op(op::I32_EQ);
    f.local_get(k).i32_const(FK_SUB32).op(op::I32_EQ).op(op::I32_OR);
    f.local_get(k).i32_const(FK_LOGIC32).op(op::I32_EQ).op(op::I32_OR);
    f.if_(I64 as u8).i64_const(31).else_().i64_const(63).end().local_set(sh);
    // N << 31 | Z << 30
    f.local_get(r).local_get(sh).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
    f.i32_const(31).op(op::I32_SHL);
    f.local_get(r).op(op::I64_EQZ).i32_const(30).op(op::I32_SHL).op(op::I32_OR);
    // C << 29 | V << 28 (0 per la logica)
    f.local_get(k).i32_const(FK_LOGIC64).op(op::I32_GE_U).if_(I32 as u8).i32_const(0).else_();
    f.local_get(k).i32_const(1).op(op::I32_AND).if_(I32 as u8);
    // somma: C = r < a; V = (!(a ^ b) & (a ^ r)) >> sh
    f.local_get(r).local_get(a).op(op::I64_LT_U).i32_const(29).op(op::I32_SHL);
    f.local_get(a).local_get(b).op(op::I64_XOR).i64_const(-1).op(op::I64_XOR);
    f.local_get(a).local_get(r).op(op::I64_XOR).op(op::I64_AND);
    f.local_get(sh).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
    f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
    f.else_();
    // differenza: C = a >= b; V = ((a ^ b) & (a ^ r)) >> sh
    f.local_get(a).local_get(b).op(op::I64_GE_U).i32_const(29).op(op::I32_SHL);
    f.local_get(a).local_get(b).op(op::I64_XOR);
    f.local_get(a).local_get(r).op(op::I64_XOR).op(op::I64_AND);
    f.local_get(sh).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
    f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
    f.end();
    f.end();
    f.op(op::I32_OR);
    def(&mut m, F_NZCV, f);

    // ld<el>_<n>(state 0, va 1, pc0 2, steps 3, packed 4) e
    // st<el>_<n>(state 0, va 1, valore 2, pc0 3, steps 4, packed 5).
    for write in [false, true] {
        for el in 0..2u8 {
            for lg in 0..4u32 {
                let bytes = 1u32 << lg;
                let tlb = area::tlb(el, write);
                let (va, e) = (1, if write { 6 } else { 5 });
                let mut f = Func { locals: vec![(1, I32)], ..Func::default() };
                // e = state + ((va >> 12) & 511) * 16; colpo se tag == va & (!0xfff | (n - 1))
                f.local_get(va).i64_const(8).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
                f.i32_const(((area::TLB_ENTRIES - 1) << 4) as i32).op(op::I32_AND);
                f.local_get(0).op(op::I32_ADD).local_tee(e).i64_load(tlb);
                f.local_get(va).i64_const((!0xfffu64 | (bytes as u64 - 1)) as i64).op(op::I64_AND);
                f.op(op::I64_EQ).if_(BLOCK_EMPTY);
                f.local_get(e).i64_load(tlb + 8).local_get(va).op(op::I64_ADD).op(op::I32_WRAP_I64);
                if write {
                    f.local_get(2).i64_store_n(bytes, 0).i32_const(0);
                } else {
                    f.i64_load_n(bytes, 0).i32_const(0);
                }
                f.op(op::RETURN).end();
                if bytes > 1 {
                    // Non allineato: TLB degli accessi non allineati, se non
                    // sconfina nella pagina successiva.
                    let tu = area::tlb_u(el, write);
                    f.local_get(va).op(op::I32_WRAP_I64).i32_const(bytes as i32 - 1).op(op::I32_AND);
                    f.local_get(e)
                        .i64_load(tu)
                        .local_get(va)
                        .i64_const(!0xfff)
                        .op(op::I64_AND)
                        .op(op::I64_EQ);
                    f.op(op::I32_AND);
                    f.local_get(va).op(op::I32_WRAP_I64).i32_const(0xfff).op(op::I32_AND);
                    f.i32_const((0x1000 - bytes) as i32).op(op::I32_LE_U).op(op::I32_AND);
                    f.if_(BLOCK_EMPTY);
                    f.local_get(e).i64_load(tu + 8).local_get(va).op(op::I64_ADD).op(op::I32_WRAP_I64);
                    if write {
                        f.local_get(2).i64_store_n(bytes, 0).i32_const(0);
                    } else {
                        f.i64_load_n(bytes, 0).i32_const(0);
                    }
                    f.op(op::RETURN).end();
                }
                if write {
                    f.local_get(0).local_get(va).i32_const(bytes as i32).local_get(2);
                    f.local_get(3).local_get(4).local_get(5).call(rt(F_ST_SLOW));
                } else {
                    f.local_get(0).local_get(va).i32_const(bytes as i32);
                    f.local_get(2).local_get(3).local_get(4).call(rt(F_LD_SLOW));
                }
                def(&mut m, f_tlb(if write { F_ST_TLB } else { F_LD_TLB }, el, bytes), f);
            }
        }
    }

    // Coppie: parametri state 0, va 1, [size 2,] valori (store), pc0,
    // steps, packed. Il secondo accesso è a `va + n` (`n` = size se c'è).
    let second = |f: &mut Func, dynamic: bool, n: u32| {
        f.local_get(0).local_get(1);
        if dynamic {
            f.local_get(2).op(op::I64_EXTEND_I32_U).op(op::I64_ADD);
        } else {
            f.i64_const(n as i64).op(op::I64_ADD);
        }
    };
    let pair_ld = |m: &mut Module, id: u32, one: u32, dynamic: bool, n: u32| {
        let o = dynamic as u32;
        let (pc0, steps, packed) = (2 + o, 3 + o, 4 + o);
        let (v, x, v2) = (5 + o, 6 + o, 7 + o);
        let mut f = Func { locals: vec![(1, I64), (1, I32), (1, I64)], ..Func::default() };
        f.local_get(0).local_get(1);
        if dynamic {
            f.local_get(2);
        }
        f.local_get(pc0).local_get(steps).local_get(packed).call(rt(one)).local_set(x).local_set(v);
        f.local_get(x).if_(BLOCK_EMPTY).i64_const(0).i64_const(0).local_get(x).op(op::RETURN).end();
        second(&mut f, dynamic, n);
        if dynamic {
            f.local_get(2);
        }
        f.local_get(pc0).local_get(steps).local_get(packed).call(rt(one)).local_set(x).local_set(v2);
        f.local_get(v).local_get(v2).local_get(x);
        def(m, id, f);
    };
    let pair_st = |m: &mut Module, id: u32, one: u32, dynamic: bool, n: u32| {
        let o = dynamic as u32;
        let (v1, v2, pc0, steps, packed) = (2 + o, 3 + o, 4 + o, 5 + o, 6 + o);
        let (x1, x2) = (7 + o, 8 + o);
        let mut f = Func { locals: vec![(2, I32)], ..Func::default() };
        f.local_get(0).local_get(1);
        if dynamic {
            f.local_get(2);
        }
        f.local_get(v1).local_get(pc0).local_get(steps).local_get(packed).call(rt(one)).local_tee(x1);
        f.i32_const(FAULT as i32)
            .op(op::I32_EQ)
            .if_(BLOCK_EMPTY)
            .i32_const(FAULT as i32)
            .op(op::RETURN)
            .end();
        second(&mut f, dynamic, n);
        if dynamic {
            f.local_get(2);
        }
        f.local_get(v2).local_get(pc0).local_get(steps).local_get(packed).call(rt(one)).local_tee(x2);
        f.i32_const(FAULT as i32).op(op::I32_EQ).if_(I32 as u8).i32_const(FAULT as i32).else_();
        f.local_get(x1).local_get(x2).op(op::I32_OR).end();
        def(m, id, f);
    };
    for el in 0..2u8 {
        for n in [4u32, 8] {
            pair_ld(&mut m, f_pair(F_LDP_TLB, el, n), f_tlb(F_LD_TLB, el, n), false, n);
        }
    }
    for el in 0..2u8 {
        for n in [4u32, 8] {
            pair_st(&mut m, f_pair(F_STP_TLB, el, n), f_tlb(F_ST_TLB, el, n), false, n);
        }
    }
    pair_ld(&mut m, F_LDP_SLOW, F_LD_SLOW, true, 0);
    pair_st(&mut m, F_STP_SLOW, F_ST_SLOW, true, 0);

    // finish(state 0, codice 1, pc 2, steps 3) -> codice: per FAULT `pc` e
    // `steps` sono già salvati; per EXIT_STOP_SAVED sono quelli dello
    // store, e passano all'istruzione successiva (codice STOP).
    let mut f = Func::default();
    f.local_get(1).i32_const(FAULT as i32).op(op::I32_EQ).if_(BLOCK_EMPTY);
    f.i32_const(FAULT as i32).op(op::RETURN).end();
    f.local_get(1).i32_const(EXIT_STOP_SAVED as i32).op(op::I32_EQ).if_(BLOCK_EMPTY);
    f.local_get(0).local_get(0).i64_load(off::PC).i64_const(4).op(op::I64_ADD).i64_store(off::PC);
    f.local_get(0).local_get(0).i64_load(off::STEPS).i64_const(1).op(op::I64_ADD).i64_store(off::STEPS);
    f.i32_const(STOP as i32).op(op::RETURN).end();
    f.local_get(0).local_get(2).i64_store(off::PC);
    f.local_get(0).local_get(3).i64_store(off::STEPS);
    f.local_get(1);
    def(&mut m, F_FINISH, f);

    // vsync(state): i registri SIMD/FP dall'host, una volta per corsa.
    let mut f = Func::default();
    f.local_get(0).i32_load(off::V_VALID).op(op::I32_EQZ).if_(BLOCK_EMPTY);
    f.local_get(0).call(vsync).end();
    def(&mut m, F_VSYNC, f);

    // Accessi Q (16 byte) come due da 8, con le regole dell'accesso intero
    // di `SysMem::access`: a cavallo di pagina FAULT (l'interprete traduce
    // tutte le pagine prima di scrivere); in modalità sistema, se allineato
    // a 8 ma non a 16, le metà vanno all'host marcate come non allineate
    // (`SIZE_PART_OF_MISALIGNED`); altrimenti le metà usano la TLB (quelle
    // non allineate a 8 finiscono comunque all'host, come non allineate).
    // ldq(state 0, va 1, pc0 2, steps 3, packed 4) -> (basso, alto, fault)
    // stq(state 0, va 1, basso 2, alto 3, pc0 4, steps 5, packed 6) -> esito
    for (id, el) in [(F_LDQ_TLB, Some(0u8)), (F_LDQ_TLB + 1, Some(1)), (F_LDQ_SLOW, None)] {
        let (pc0, steps, packed, v, x, v2) = (2, 3, 4, 5, 6, 7);
        let mut f = Func { locals: vec![(1, I64), (1, I32), (1, I64)], ..Func::default() };
        f.local_get(1)
            .op(op::I32_WRAP_I64)
            .i32_const(0xfff)
            .op(op::I32_AND)
            .i32_const(0xff0)
            .op(op::I32_GT_S);
        f.if_(BLOCK_EMPTY);
        f.local_get(0).local_get(pc0).local_get(steps).local_get(packed).call(rt(F_SAVE));
        f.i64_const(0).i64_const(0).i32_const(FAULT as i32).op(op::RETURN).end();
        let half = |f: &mut Func, off: i64, misaligned: bool| {
            f.local_get(0).local_get(1);
            if off != 0 {
                f.i64_const(off).op(op::I64_ADD);
            }
            match (el, misaligned) {
                (Some(el), false) => {
                    f.local_get(pc0).local_get(steps).local_get(packed).call(rt(f_tlb(F_LD_TLB, el, 8)));
                }
                (Some(el), true) => {
                    f.local_get(pc0).local_get(steps).local_get(packed).call(rt(F_LDU + el as u32));
                }
                (None, _) => {
                    f.i32_const(8).local_get(pc0).local_get(steps).local_get(packed).call(rt(F_LD_SLOW));
                }
            }
        };
        let both = |f: &mut Func, misaligned: bool| {
            half(f, 0, misaligned);
            f.local_set(x).local_set(v).local_get(x).if_(BLOCK_EMPTY);
            f.i64_const(0).i64_const(0).local_get(x).op(op::RETURN).end();
            half(f, 8, misaligned);
            f.local_set(x).local_set(v2).local_get(v).local_get(v2).local_get(x).op(op::RETURN);
        };
        if el.is_some() {
            f.local_get(1).op(op::I32_WRAP_I64).i32_const(15).op(op::I32_AND).i32_const(8).op(op::I32_EQ);
            f.if_(BLOCK_EMPTY);
            both(&mut f, true);
            f.end();
        }
        both(&mut f, false);
        f.op(op::UNREACHABLE);
        def(&mut m, id, f);
    }
    for (id, el) in [(F_STQ_TLB, Some(0u8)), (F_STQ_TLB + 1, Some(1)), (F_STQ_SLOW, None)] {
        let (lo, hi, pc0, steps, packed, x1, x2) = (2, 3, 4, 5, 6, 7, 8);
        let mut f = Func { locals: vec![(2, I32)], ..Func::default() };
        f.local_get(1)
            .op(op::I32_WRAP_I64)
            .i32_const(0xfff)
            .op(op::I32_AND)
            .i32_const(0xff0)
            .op(op::I32_GT_S);
        f.if_(BLOCK_EMPTY);
        f.local_get(0).local_get(pc0).local_get(steps).local_get(packed).call(rt(F_SAVE));
        f.i32_const(FAULT as i32).op(op::RETURN).end();
        let half = |f: &mut Func, off: i64, val: u32, misaligned: bool| {
            f.local_get(0).local_get(1);
            if off != 0 {
                f.i64_const(off).op(op::I64_ADD);
            }
            match (el, misaligned) {
                (Some(el), false) => {
                    f.local_get(val).local_get(pc0).local_get(steps).local_get(packed);
                    f.call(rt(f_tlb(F_ST_TLB, el, 8)));
                }
                (Some(el), true) => {
                    f.local_get(val).local_get(pc0).local_get(steps).local_get(packed);
                    f.call(rt(F_STU + el as u32));
                }
                (None, _) => {
                    f.i32_const(8).local_get(val).local_get(pc0).local_get(steps).local_get(packed);
                    f.call(rt(F_ST_SLOW));
                }
            }
        };
        let both = |f: &mut Func, misaligned: bool| {
            half(f, 0, lo, misaligned);
            f.local_tee(x1).i32_const(FAULT as i32).op(op::I32_EQ).if_(BLOCK_EMPTY);
            f.i32_const(FAULT as i32).op(op::RETURN).end();
            half(f, 8, hi, misaligned);
            f.local_tee(x2).i32_const(FAULT as i32).op(op::I32_EQ).if_(BLOCK_EMPTY);
            f.i32_const(FAULT as i32).op(op::RETURN).end();
            f.local_get(x1).local_get(x2).op(op::I32_OR).op(op::RETURN);
        };
        if el.is_some() {
            f.local_get(1).op(op::I32_WRAP_I64).i32_const(15).op(op::I32_AND).i32_const(8).op(op::I32_EQ);
            f.if_(BLOCK_EMPTY);
            both(&mut f, true);
            f.end();
        }
        both(&mut f, false);
        f.op(op::UNREACHABLE);
        def(&mut m, id, f);
    }

    // ldu<el>(state 0, va 1, pc0 2, steps 3, packed 4) -> (valore, fault) e
    // stu<el>(state 0, va 1, valore 2, pc0 3, steps 4, packed 5) -> esito:
    // `va` allineato a 8, metà di un Q non allineato a 16.
    for write in [false, true] {
        for el in 0..2u8 {
            let tu = area::tlb_u(el, write);
            let (va, e) = (1, if write { 6 } else { 5 });
            let mut f = Func { locals: vec![(1, I32)], ..Func::default() };
            f.local_get(va).i64_const(8).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
            f.i32_const(((area::TLB_ENTRIES - 1) << 4) as i32).op(op::I32_AND);
            f.local_get(0).op(op::I32_ADD).local_tee(e).i64_load(tu);
            f.local_get(va).i64_const(!0xfff).op(op::I64_AND).op(op::I64_EQ).if_(BLOCK_EMPTY);
            f.local_get(e).i64_load(tu + 8).local_get(va).op(op::I64_ADD).op(op::I32_WRAP_I64);
            if write {
                f.local_get(2).i64_store_n(8, 0).i32_const(0);
            } else {
                f.i64_load_n(8, 0).i32_const(0);
            }
            f.op(op::RETURN).end();
            let size = (8 | SIZE_PART_OF_MISALIGNED) as i32;
            if write {
                f.local_get(0).local_get(va).i32_const(size).local_get(2);
                f.local_get(3).local_get(4).local_get(5).call(rt(F_ST_SLOW));
            } else {
                f.local_get(0).local_get(va).i32_const(size);
                f.local_get(2).local_get(3).local_get(4).call(rt(F_LD_SLOW));
            }
            def(&mut m, if write { F_STU } else { F_LDU } + el as u32, f);
        }
    }

    // simd(state, parola, x, nzcv) -> valore: `env.simd`.
    let mut f = Func::default();
    f.local_get(0).local_get(1).local_get(2).local_get(3).call(simd);
    def(&mut m, F_SIMD, f);

    // Percorsi veloci della virgola mobile (ADR 0026).
    for k in 0..fp::rt_ops().len() {
        def(&mut m, F_FP0 + k as u32, fp::build(k, simd));
    }
    m.encode()
}

/// Il dispatcher della modalità sistema, esportato come `b0`: a partire
/// da `pc` cerca il blocco nella cache dei salti ([`area::JC`]) e lo chiama
/// dalla tabella, finché trova blocchi validi per `ctx` che stanno nel
/// limite di passi e finiscono con `NEXT`. Una voce assente la chiede
/// all'host (`env.resolve`). Restituisce `NEXT` (voce assente anche per
/// l'host, o limite) o il codice d'uscita del blocco.
pub fn dispatcher(memory: MemoryImport) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t = m.ty(&[I32], &[I32]);
    m.import_memory("env", "mem", memory);
    m.import_table("env", "tbl", TABLE_SIZE);
    let resolve = m.import_func("env", "resolve", t);
    // locali: 0 stato, 1 pc (i64), 2 voce (i32), 3 w (i32), 4 codice (i32)
    let (s, pc, e, w, code) = (0, 1, 2, 3, 4);
    let mut f = Func { locals: vec![(1, I64), (3, I32)], ..Func::default() };
    f.loop_(BLOCK_EMPTY);
    // e = s + ((pc >> 2) & (JC_ENTRIES - 1)) * 16
    f.local_get(s).i64_load(off::PC).local_tee(pc);
    f.i64_const(2).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
    f.i32_const((area::JC_ENTRIES - 1) as i32).op(op::I32_AND).i32_const(4).op(op::I32_SHL);
    f.local_get(s).op(op::I32_ADD).local_set(e);
    // voce di un altro pc o di un altro contesto: la chiede all'host
    // (`env.resolve`), che la scrive se il blocco c'è; altrimenti all'host.
    f.local_get(e).i64_load(area::JC).local_get(pc).op(op::I64_NE);
    f.local_get(e).i32_load(area::JC + 8).local_get(s).i32_load(off::CTX).op(op::I32_NE);
    f.op(op::I32_OR).if_(BLOCK_EMPTY);
    f.local_get(s).call(resolve).op(op::I32_EQZ);
    f.if_(BLOCK_EMPTY).i32_const(crate::NEXT as i32).op(op::RETURN).end();
    f.end();
    // steps + passi massimi del blocco > limit: all'host
    f.local_get(e).i32_load(area::JC + 12).local_set(w);
    f.local_get(s).i64_load(off::STEPS);
    f.local_get(w).i32_const(0xff).op(op::I32_AND).op(op::I64_EXTEND_I32_U).op(op::I64_ADD);
    f.local_get(s).i64_load(off::LIMIT).op(op::I64_GT_U);
    f.if_(BLOCK_EMPTY).i32_const(crate::NEXT as i32).op(op::RETURN).end();
    // entry = w >> 26; codice = tabella[(w >> 8) & (TABLE_SIZE - 1)](s)
    f.local_get(s).local_get(w).i32_const(26).op(op::I32_SHR_U).i32_store(off::ENTRY);
    f.local_get(s).local_get(w).i32_const(8).op(op::I32_SHR_U);
    f.i32_const((TABLE_SIZE - 1) as i32).op(op::I32_AND).call_indirect(t);
    f.local_tee(code).if_(BLOCK_EMPTY).local_get(code).op(op::RETURN).end();
    f.br(0);
    f.end();
    f.op(op::UNREACHABLE);
    let idx = m.func(t, f);
    m.export_func("b0", idx);
    m.encode()
}

/// Funzione WASM di una regione.
///
/// Struttura: un blocco esterno da cui escono tutte le uscite verso la
/// coda comune; dentro, con più blocchi base, un `loop` con un `br_table`
/// sull'indice del prossimo blocco base (`L_NEXT`). Il codice dei blocchi
/// base segue l'ordine degli indirizzi, quindi il passaggio al successivo
/// in memoria è gratuito; gli altri salti interni impostano `L_NEXT` e
/// tornano al `loop`. Ogni blocco base controlla prima di iniziare che i
/// suoi passi stiano nel limite (`limit`), altrimenti esce con `NEXT` al
/// suo inizio: il numero di istruzioni resta esatto anche nei cicli.
pub fn function(r: &Region) -> Func {
    assert!(!r.bbs.is_empty() && r.len() <= MAX_REGION.max(MAX_BLOCK));
    // Il `loop` serve con più blocchi base o con un blocco che salta a sé.
    let multi = r.bbs.len() > 1
        || r.bbs[0].insns.last().and_then(|i| {
            let at = r.bbs[0].pc.wrapping_add(4 * (r.bbs[0].insns.len() as u64 - 1));
            direct_target(i, at, r.sys)
        }) == Some(r.bbs[0].pc);
    let mut body = Func::default();
    // Tutte le uscite escono da questo blocco verso la coda comune.
    body.block(BLOCK_EMPTY);
    let mut t = Tx {
        f: body,
        read: 0,
        written: 0,
        pc: r.pc,
        word: 0,
        index: 0,
        pc0: r.pc & !0xfff,
        fl: Fl::Unknown,
        sp_ok: false,
        sys: r.sys,
        bb_index: r.bbs.iter().enumerate().map(|(i, b)| (b.pc, i)).collect(),
        cur: 0,
        loop_depth: 0,
        simd: false,
        inline_tlb: INLINE_TLB,
    };
    let n = r.bbs.len();
    if multi {
        t.f.loop_(BLOCK_EMPTY);
        t.loop_depth = t.f.depth;
        for _ in 0..n {
            t.f.block(BLOCK_EMPTY);
        }
        let labels: Vec<u32> = (0..n as u32).collect();
        t.f.local_get(L_NEXT).br_table(&labels, 0);
    }
    for (i, bb) in r.bbs.iter().enumerate() {
        if multi {
            t.f.end();
        }
        t.cur = i;
        t.bb(bb, multi);
    }
    if multi {
        t.f.end(); // loop
        t.f.op(op::UNREACHABLE);
    }
    t.f.end();
    debug_assert_eq!(t.f.depth, 0);
    // Coda comune: si riscrivono tutti i registri che la regione scrive
    // (quelli non ancora scritti al punto d'uscita hanno il valore
    // d'ingresso), poi `pc`, `steps` e il codice.
    // Per FAULT `pc` e `steps` li ha già salvati il percorso lento (o
    // `exit_fault`).
    let all = t.written;
    t.flush();
    let f = &mut t.f;
    // NEXT (il caso comune) senza chiamate; gli altri con `rt.finish`.
    f.local_get(L_EXIT_CODE).if_(ValType::I32 as u8);
    f.local_get(L_STATE).local_get(L_EXIT_CODE).local_get(L_EXIT_PC);
    f.local_get(L_STEPS).local_get(L_EXIT_DONE).op(op::I64_ADD).call(F_FINISH);
    f.else_();
    f.local_get(L_STATE).local_get(L_EXIT_PC).i64_store(off::PC);
    f.local_get(L_STATE).local_get(L_STEPS).local_get(L_EXIT_DONE).op(op::I64_ADD).i64_store(off::STEPS);
    f.i32_const(NEXT as i32);
    f.end();
    // Prologo: `steps`, `limit`, il blocco d'ingresso e i registri letti o
    // scritti.
    let load = t.read | all;
    let mut pro = Func::default();
    pro.local_get(L_STATE).i64_load(off::STEPS).local_set(L_STEPS);
    pro.i64_const(t.pc0 as i64).local_set(L_PC0);
    if t.simd {
        // Registri SIMD/FP in JitState (una volta per corsa).
        pro.local_get(L_STATE).call(F_VSYNC);
    }
    if multi {
        pro.local_get(L_STATE).i64_load(off::LIMIT).local_set(L_LIMIT);
        pro.local_get(L_STATE).i32_load(off::ENTRY).local_set(L_NEXT);
    }
    for r in 0..=B_SP {
        if load & (1 << r) != 0 {
            pro.local_get(L_STATE).i64_load(off::X + 8 * r).local_set(1 + r);
        }
    }
    if load & (1 << B_NZCV) != 0 {
        pro.local_get(L_STATE).i32_load(off::NZCV).local_set(L_NZCV);
        pro.local_get(L_STATE).i32_load(off::FK).local_set(L_FK);
        pro.local_get(L_STATE).i64_load(off::FA).local_set(L_FA);
        pro.local_get(L_STATE).i64_load(off::FB).local_set(L_FB);
        pro.local_get(L_STATE).i64_load(off::FR).local_set(L_FR);
    }
    pro.code.extend_from_slice(&t.f.code);
    pro.locals = vec![
        (32, ValType::I64),
        (1, ValType::I32),
        (N_T64, ValType::I64),
        (N_T32, ValType::I32),
        (N_V128, ValType::V128),
    ];
    pro
}

/// Traduttore di una regione.
struct Tx {
    f: Func,
    /// Registri letti dalla regione (da caricare nel prologo).
    read: u64,
    /// Registri scritti dalla regione (da riscrivere nella coda).
    written: u64,
    /// Indirizzo dell'istruzione corrente.
    pc: u64,
    /// Parola dell'istruzione corrente.
    word: u32,
    /// Indice dell'istruzione corrente nel blocco base.
    index: u64,
    /// Inizio della pagina (valore di `L_PC0`).
    pc0: u64,
    /// Stato dei flag nel punto corrente.
    fl: Fl,
    /// SP già controllato allineato a 16 nel blocco base, e non cambiato da
    /// allora (modalità sistema).
    sp_ok: bool,
    /// Modalità sistema: accessi con la TLB software, allineamento di SP,
    /// TBI sui salti.
    sys: Option<SysTarget>,
    /// Inizio di ogni blocco base → indice.
    bb_index: std::collections::HashMap<u64, usize>,
    /// Blocco base corrente.
    cur: usize,
    /// Profondità del `loop` della regione (0 se c'è un solo blocco base).
    loop_depth: u32,
    /// La regione usa i registri SIMD/FP (`JitState::v`).
    simd: bool,
    /// Percorso veloce della TLB in linea (altrimenti sempre `rt.*`).
    inline_tlb: bool,
}

/// Da dove viene il nuovo `pc` di un'uscita.
enum PcSrc {
    Const(u64),
    /// Valore in cima allo stack (i64), consumato.
    Stack,
}

impl Tx {
    /// Codice di un blocco base.
    fn bb(&mut self, bb: &Bb, multi: bool) {
        // All'inizio di un blocco base flag e allineamento di SP dipendono
        // dal percorso.
        self.fl = Fl::Unknown;
        self.sp_ok = false;
        let steps = bb.max_steps();
        if multi && steps > 0 {
            // Passi oltre il limite: uscita all'inizio del blocco base.
            self.f.local_get(L_STEPS).i64_const(steps as i64).op(op::I64_ADD);
            self.f.local_get(L_LIMIT).op(op::I64_GT_U).if_(BLOCK_EMPTY);
            self.exit_const(NEXT, bb.pc, 0);
            self.f.end();
        }
        for (i, insn) in bb.insns.iter().enumerate() {
            self.index = i as u64;
            self.pc = bb.pc.wrapping_add(4 * i as u64);
            self.word = bb.words[i];
            let k = kind_in(insn, self.sys);
            assert!(k != Kind::Unsupported, "istruzione non traducibile nel blocco: {insn:?}");
            if k == Kind::Svc {
                assert_eq!(i + 1, bb.insns.len(), "SVC non in fondo al blocco");
                self.exit_const(SVC, self.pc, self.index);
                return;
            }
            self.insn(insn);
            if k == Kind::Branch || is_cond_branch(insn) {
                assert_eq!(i + 1, bb.insns.len(), "salto non in fondo al blocco");
                return;
            }
        }
        // Il blocco base continua all'istruzione successiva.
        let n = bb.insns.len() as u64;
        self.index = n;
        self.jump(bb.pc.wrapping_add(4 * n), n, true);
    }

    /// Salto a `target` (costante) dopo `done` istruzioni del blocco base:
    /// dentro la regione se `target` inizia un suo blocco base (senza
    /// codice se è il successivo e `fall`), altrimenti uscita con `NEXT`.
    fn jump(&mut self, target: u64, done: u64, fall: bool) {
        let Some(&j) = self.bb_index.get(&target) else {
            self.exit_const(NEXT, target, done);
            return;
        };
        let f = &mut self.f;
        f.local_get(L_STEPS).i64_const(done as i64).op(op::I64_ADD).local_set(L_STEPS);
        if fall && j == self.cur + 1 {
            return;
        }
        f.i32_const(j as i32).local_set(L_NEXT);
        let rel = f.depth - self.loop_depth;
        f.br(rel);
    }

    // --- registri -----------------------------------------------------

    /// `xr(r)`: 31 vale XZR.
    fn get_x(&mut self, r: u8) {
        if r == 31 {
            self.f.i64_const(0);
        } else {
            self.read |= 1 << r;
            self.f.local_get(1 + r as u32);
        }
    }

    /// `xsp(r)`: 31 vale SP.
    fn get_xsp(&mut self, r: u8) {
        self.read |= 1 << r;
        self.f.local_get(1 + r as u32);
    }

    /// `set_x(r, top)`: 31 (XZR) scarta.
    fn set_x(&mut self, r: u8) {
        if r == 31 {
            self.f.op(op::DROP);
        } else {
            self.written |= 1 << r;
            self.f.local_set(1 + r as u32);
        }
    }

    /// `set_xsp(r, top)`: 31 vale SP.
    fn set_xsp(&mut self, r: u8) {
        self.written |= 1 << r;
        if r == 31 {
            self.sp_ok = false;
        }
        self.f.local_set(1 + r as u32);
    }

    /// NZCV (i32) sullo stack, calcolato se pigro.
    fn get_nzcv(&mut self) {
        self.materialize();
        self.f.local_get(L_NZCV);
    }

    /// NZCV = cima dello stack (i32): non più pigro.
    fn set_nzcv(&mut self) {
        self.read |= 1 << B_NZCV;
        self.written |= 1 << B_NZCV;
        self.f.local_set(L_NZCV);
        self.f.i32_const(0).local_set(L_FK);
        self.fl = Fl::Materialized;
    }

    /// Porta NZCV in `L_NZCV` se è (o può essere) pigro.
    fn materialize(&mut self) {
        self.read |= 1 << B_NZCV;
        if self.fl == Fl::Materialized {
            return;
        }
        let f = &mut self.f;
        let known = matches!(self.fl, Fl::Lazy(_));
        if !known {
            f.local_get(L_FK).if_(BLOCK_EMPTY);
        }
        f.local_get(L_FK).local_get(L_FA).local_get(L_FB).local_get(L_FR).local_get(L_NZCV);
        f.call(F_NZCV).local_set(L_NZCV).i32_const(0).local_set(L_FK);
        if !known {
            f.end();
        }
        self.fl = Fl::Materialized;
    }

    /// Flag pigri di tipo `k`: operandi già in `L_FA`, `L_FB` e risultato
    /// in `L_FR`.
    fn set_lazy(&mut self, k: i32) {
        self.read |= 1 << B_NZCV;
        self.written |= 1 << B_NZCV;
        self.f.i32_const(k).local_set(L_FK);
        self.fl = Fl::Lazy(k);
    }

    /// Tronca a 32 bit (estensione a zero) se `!sf`.
    fn trunc(&mut self, sf: bool) {
        if !sf {
            self.f.i64_const(0xffff_ffff).op(op::I64_AND);
        }
    }

    // --- uscite -------------------------------------------------------

    /// Riscrive in `JitState` i registri scritti finora.
    fn flush(&mut self) {
        for r in 0..=B_SP {
            if self.written & (1 << r) != 0 {
                self.f.local_get(L_STATE).local_get(1 + r).i64_store(off::X + 8 * r);
            }
        }
        if self.written & (1 << B_NZCV) != 0 {
            // I flag pigri passano alla regione successiva (e all'host) così
            // come sono.
            let f = &mut self.f;
            f.local_get(L_STATE).local_get(L_NZCV).i32_store(off::NZCV);
            f.local_get(L_STATE).local_get(L_FK).i32_store(off::FK);
            f.local_get(L_STATE).local_get(L_FA).i64_store(off::FA);
            f.local_get(L_STATE).local_get(L_FB).i64_store(off::FB);
            f.local_get(L_STATE).local_get(L_FR).i64_store(off::FR);
        }
    }

    /// Uscita con codice `code`, nuovo `pc` e `steps` aumentato di `done`
    /// (istruzioni fatte nel blocco base): salto alla coda comune (fine
    /// della funzione), che riscrive i registri e `JitState`.
    fn exit(&mut self, code: u32, pc: PcSrc, done: u64) {
        if let PcSrc::Const(v) = pc {
            // Relativo all'inizio della pagina: costanti più corte.
            let d = v.wrapping_sub(self.pc0) as i64;
            if d.unsigned_abs() < 1 << 20 {
                self.f.local_get(L_PC0).i64_const(d).op(op::I64_ADD);
            } else {
                self.f.i64_const(v as i64);
            }
        }
        self.f.local_set(L_EXIT_PC);
        // Le locali partono da zero e si scrivono solo prima di uscire:
        // `done` = 0 e `NEXT` (0) non servono.
        if done != 0 {
            self.f.i64_const(done as i64).local_set(L_EXIT_DONE);
        }
        if code != NEXT {
            self.f.i32_const(code as i32).local_set(L_EXIT_CODE);
        }
        let depth = self.f.depth;
        debug_assert!(depth >= 1, "uscita fuori dal blocco comune");
        self.f.br(depth - 1);
    }

    fn exit_const(&mut self, code: u32, pc: u64, done: u64) {
        self.exit(code, PcSrc::Const(pc), done);
    }

    /// Argomenti `pc0, steps, packed` dei percorsi lenti per l'istruzione
    /// corrente.
    fn slow_args(&mut self) {
        let packed = (self.pc & 0xfff) as i32 | (self.index as i32) << 12;
        self.f.local_get(L_PC0).local_get(L_STEPS).i32_const(packed);
    }

    /// Uscita per fault dell'istruzione corrente (registri come prima):
    /// salva `pc` e `steps` e lascia decidere l'interprete.
    fn exit_fault(&mut self) {
        self.f.local_get(L_STATE);
        self.slow_args();
        self.f.call(F_SAVE);
        self.exit_fault_saved();
    }

    /// Uscita per fault con `pc` e `steps` già salvati (percorsi lenti).
    fn exit_fault_saved(&mut self) {
        self.f.i32_const(FAULT as i32).local_set(L_EXIT_CODE);
        let depth = self.f.depth;
        self.f.br(depth - 1);
    }

    /// Salto condizionato a `target` con la condizione (i32) in cima allo
    /// stack; chiude sempre il blocco base: preso va a `target`, altrimenti
    /// all'istruzione successiva (dentro la regione o con un'uscita).
    fn cond_branch(&mut self, target: u64) {
        let target = self.target(target);
        let next = self.pc.wrapping_add(4);
        let done = self.index + 1;
        self.f.if_(BLOCK_EMPTY);
        self.jump(target, done, false);
        self.f.end();
        self.jump(next, done, true);
    }

    /// Chiude il blocco dopo un salto: `pc` in cima allo stack.
    fn exit_branch(&mut self) {
        let done = self.index + 1;
        self.exit(NEXT, PcSrc::Stack, done);
    }

    // --- aritmetica ---------------------------------------------------

    /// `AddWithCarry(x, y, carry)` con x in `t64(0)`, y in `t64(1)`:
    /// risultato (troncato se `!sf`) in `t64(2)`; se `flags`, NZCV nella
    /// variabile dei flag. `carry`: `Some(c)` costante, `None` = flag C.
    fn add_with_carry(&mut self, sf: bool, carry: Option<bool>, flags: bool) {
        let (x, y, r) = (t64(0), t64(1), t64(2));
        if carry.is_none() {
            self.materialize();
        }
        let f = &mut self.f;
        if !sf {
            f.local_get(x).i64_const(0xffff_ffff).op(op::I64_AND).local_set(x);
            f.local_get(y).i64_const(0xffff_ffff).op(op::I64_AND).local_set(y);
        }
        f.local_get(x).local_get(y).op(op::I64_ADD);
        match carry {
            Some(false) => {}
            Some(true) => {
                f.i64_const(1).op(op::I64_ADD);
            }
            None => {
                self.read |= 1 << B_NZCV;
                let f = &mut self.f;
                f.local_get(L_NZCV).i32_const(29).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
                f.op(op::I64_EXTEND_I32_U).op(op::I64_ADD);
            }
        }
        let f = &mut self.f;
        if sf {
            f.local_set(r);
        } else {
            // wide in t64(3), r = wide & 0xffffffff
            f.local_tee(t64(3)).i64_const(0xffff_ffff).op(op::I64_AND).local_set(r);
        }
        if !flags {
            return;
        }
        let bits = if sf { 63 } else { 31 };
        // N
        f.local_get(r).i64_const(bits).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
        f.i32_const(31).op(op::I32_SHL);
        // Z
        f.local_get(r).op(op::I64_EQZ).i32_const(30).op(op::I32_SHL).op(op::I32_OR);
        // C
        if sf {
            match carry {
                Some(false) => {
                    f.local_get(r).local_get(x).op(op::I64_LT_U);
                }
                Some(true) => {
                    f.local_get(r).local_get(x).op(op::I64_LE_U);
                }
                None => {
                    f.local_get(r).local_get(x).op(op::I64_LE_U);
                    f.local_get(r).local_get(x).op(op::I64_LT_U);
                    f.local_get(L_NZCV).i32_const(29).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
                    f.op(op::SELECT);
                }
            }
        } else {
            f.local_get(t64(3)).i64_const(32).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
        }
        f.i32_const(29).op(op::I32_SHL).op(op::I32_OR);
        // V = ((x ^ r) & (y ^ r)) >> bits
        f.local_get(x).local_get(r).op(op::I64_XOR);
        f.local_get(y).local_get(r).op(op::I64_XOR).op(op::I64_AND);
        f.i64_const(bits).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
        f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
        self.set_nzcv();
    }

    /// `add_sub(x, y, sub, setflags, sf)` con x in `t64(0)` e y in
    /// `t64(1)`: risultato in `t64(2)`.
    /// Con `setflags` i flag sono pigri (ADR 0024): operandi troncati in
    /// `L_FA`, `L_FB`, risultato in `L_FR`.
    fn add_sub(&mut self, sub: bool, setflags: bool, sf: bool) {
        let (x, y, r) = (t64(0), t64(1), t64(2));
        let o = if sub { op::I64_SUB } else { op::I64_ADD };
        if !setflags {
            self.f.local_get(x).local_get(y).op(o);
            self.trunc(sf);
            self.f.local_set(r);
            return;
        }
        self.f.local_get(x);
        self.trunc(sf);
        self.f.local_set(L_FA).local_get(y);
        self.trunc(sf);
        self.f.local_set(L_FB).local_get(L_FA).local_get(L_FB).op(o);
        self.trunc(sf);
        self.f.local_tee(L_FR).local_set(r);
        let k = match (sub, sf) {
            (false, true) => FK_ADD64,
            (true, true) => FK_SUB64,
            (false, false) => FK_ADD32,
            (true, false) => FK_SUB32,
        };
        self.set_lazy(k);
    }

    /// ADDS/SUBS con operandi (troncati) in `L_FA`, `L_FB`: risultato in
    /// `L_FR` e in `rd` (XZR scarta), flag pigri.
    fn flag_op(&mut self, sub: bool, sf: bool, rd: u8) {
        self.f.local_get(L_FA).local_get(L_FB).op(if sub { op::I64_SUB } else { op::I64_ADD });
        self.trunc(sf);
        if rd == 31 {
            self.f.local_set(L_FR);
        } else {
            self.f.local_tee(L_FR);
            self.set_x(rd);
        }
        let k = match (sub, sf) {
            (false, true) => FK_ADD64,
            (true, true) => FK_SUB64,
            (false, false) => FK_ADD32,
            (true, false) => FK_SUB32,
        };
        self.set_lazy(k);
    }

    /// Flag di AND/BIC con risultato (troncato) in cima allo stack, che
    /// resta.
    fn logic_flags(&mut self, sf: bool) {
        self.f.local_tee(L_FR);
        self.set_lazy(if sf { FK_LOGIC64 } else { FK_LOGIC32 });
    }

    /// `shift_reg(top, shift, amount, sf)`: valore in cima allo stack.
    fn shift_reg(&mut self, shift: Shift, amount: u8, sf: bool) {
        if amount == 0 {
            // Ogni shift di 0 è l'identità (a 32 bit tronca).
            self.trunc(sf);
            return;
        }
        let a = amount as i64;
        let f = &mut self.f;
        if sf {
            match shift {
                Shift::Lsl => f.i64_const(a).op(op::I64_SHL),
                Shift::Lsr => f.i64_const(a).op(op::I64_SHR_U),
                Shift::Asr => f.i64_const(a).op(op::I64_SHR_S),
                Shift::Ror => f.i64_const(a).op(op::I64_ROTR),
            };
        } else {
            f.op(op::I32_WRAP_I64);
            match shift {
                Shift::Lsl => f.i32_const(a as i32).op(op::I32_SHL),
                Shift::Lsr => f.i32_const(a as i32).op(op::I32_SHR_U),
                Shift::Asr => f.i32_const(a as i32).op(op::I32_SHR_S),
                Shift::Ror => f.i32_const(a as i32).op(op::I32_ROTR),
            };
            f.op(op::I64_EXTEND_I32_U);
        }
    }

    /// `extend_reg(top, extend, shift)` a 64 bit.
    fn extend_reg(&mut self, extend: u8, shift: u8) {
        let f = &mut self.f;
        match extend {
            0 => f.i64_const(0xff).op(op::I64_AND),
            1 => f.i64_const(0xffff).op(op::I64_AND),
            2 => f.i64_const(0xffff_ffff).op(op::I64_AND),
            3 | 7 => f,
            4 => f.op(op::I64_EXTEND8_S),
            5 => f.op(op::I64_EXTEND16_S),
            _ => f.op(op::I64_EXTEND32_S),
        };
        if shift != 0 {
            f.i64_const(shift as i64).op(op::I64_SHL);
        }
    }

    /// Lascia sullo stack (i32) `ConditionHolds(cond)`. Con i flag pigri
    /// di tipo noto la condizione si calcola dagli operandi.
    fn cond(&mut self, cond: u8) {
        if cond >> 1 == 7 {
            self.f.i32_const(1);
            return;
        }
        if let Fl::Lazy(k) = self.fl {
            self.cond_lazy(k, cond >> 1);
        } else {
            self.materialize();
            self.cond_nzcv(cond >> 1);
        }
        if cond & 1 == 1 {
            self.f.op(op::I32_EQZ);
        }
    }

    /// Condizione `c` (senza il bit di negazione) dai bit di `L_NZCV`.
    fn cond_nzcv(&mut self, c: u8) {
        let f = &mut self.f;
        let flag = |f: &mut Func, sh: i32| {
            f.local_get(L_NZCV).i32_const(sh).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
        };
        match c {
            0 => flag(f, 30),
            1 => flag(f, 29),
            2 => flag(f, 31),
            3 => flag(f, 28),
            4 => {
                flag(f, 29);
                flag(f, 30);
                f.i32_const(1).op(op::I32_XOR).op(op::I32_AND);
            }
            5 => {
                flag(f, 31);
                flag(f, 28);
                f.op(op::I32_EQ);
            }
            _ => {
                flag(f, 31);
                flag(f, 28);
                f.op(op::I32_EQ);
                flag(f, 30);
                f.i32_const(1).op(op::I32_XOR).op(op::I32_AND);
            }
        }
    }

    /// Condizione `c` (senza il bit di negazione) dai flag pigri di tipo
    /// `k`: `L_FA`, `L_FB`, `L_FR` (troncati a 32 bit per i tipi a 32).
    fn cond_lazy(&mut self, k: i32, c: u8) {
        let w32 = matches!(k, FK_ADD32 | FK_SUB32 | FK_LOGIC32);
        let f = &mut self.f;
        // Bit del segno di L_FR (N).
        let n = |f: &mut Func| {
            if w32 {
                f.local_get(L_FR).op(op::I32_WRAP_I64).i32_const(0).op(op::I32_LT_S);
            } else {
                f.local_get(L_FR).i64_const(0).op(op::I64_LT_S);
            }
        };
        // Confronto con segno di L_FA e L_FB.
        let scmp = |f: &mut Func, o32: u8, o64: u8| {
            if w32 {
                f.local_get(L_FA).op(op::I32_WRAP_I64).local_get(L_FB).op(op::I32_WRAP_I64).op(o32);
            } else {
                f.local_get(L_FA).local_get(L_FB).op(o64);
            }
        };
        // V dalla formula (somma se `add`).
        let v = |f: &mut Func, add: bool| {
            f.local_get(L_FA).local_get(L_FB).op(op::I64_XOR);
            if add {
                f.i64_const(-1).op(op::I64_XOR);
            }
            f.local_get(L_FA).local_get(L_FR).op(op::I64_XOR).op(op::I64_AND);
            f.i64_const(if w32 { 31 } else { 63 }).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
            f.i32_const(1).op(op::I32_AND);
        };
        match k {
            FK_SUB64 | FK_SUB32 => match c {
                0 => {
                    f.local_get(L_FA).local_get(L_FB).op(op::I64_EQ);
                }
                1 => {
                    f.local_get(L_FA).local_get(L_FB).op(op::I64_GE_U);
                }
                2 => n(f),
                3 => v(f, false),
                4 => {
                    f.local_get(L_FA).local_get(L_FB).op(op::I64_GT_U);
                }
                5 => scmp(f, op::I32_GE_S, op::I64_GE_S),
                _ => scmp(f, op::I32_GT_S, op::I64_GT_S),
            },
            FK_LOGIC64 | FK_LOGIC32 => match c {
                // C = V = 0.
                0 => {
                    f.local_get(L_FR).op(op::I64_EQZ);
                }
                1 | 3 | 4 => {
                    f.i32_const(0);
                }
                2 => n(f),
                5 => {
                    n(f);
                    f.op(op::I32_EQZ);
                }
                _ => {
                    f.local_get(L_FR).op(op::I64_EQZ);
                    n(f);
                    f.op(op::I32_OR).op(op::I32_EQZ);
                }
            },
            _ => match c {
                // Somma: C = r < a.
                0 => {
                    f.local_get(L_FR).op(op::I64_EQZ);
                }
                1 => {
                    f.local_get(L_FR).local_get(L_FA).op(op::I64_LT_U);
                }
                2 => n(f),
                3 => v(f, true),
                4 => {
                    f.local_get(L_FR).local_get(L_FA).op(op::I64_LT_U);
                    f.local_get(L_FR).op(op::I64_EQZ).op(op::I32_EQZ).op(op::I32_AND);
                }
                5 => {
                    n(f);
                    v(f, true);
                    f.op(op::I32_EQ);
                }
                _ => {
                    n(f);
                    v(f, true);
                    f.op(op::I32_EQ);
                    f.local_get(L_FR).op(op::I64_EQZ).op(op::I32_EQZ).op(op::I32_AND);
                }
            },
        }
    }

    // --- memoria ------------------------------------------------------

    /// Load di `bytes` byte dall'indirizzo in `addr`: valore esteso a zero
    /// sullo stack; esce con FAULT se l'accesso fallisce. In modalità
    /// sistema passa dalla TLB software (`rt.ld<el>_<n>`), altrimenti
    /// dall'host (`rt.ld_slow`).
    fn ld(&mut self, addr: u32, bytes: u32) {
        if let Some(sys) = self.sys
            && self.inline_tlb
        {
            // Colpo nella TLB degli accessi allineati: in linea; altrimenti
            // `rt.ld<el>_<n>` (TLB non allineata, host).
            let tlb = area::tlb(sys.el, false);
            self.tlb_hit(addr, bytes, tlb);
            self.f.if_(ValType::I64 as u8);
            self.f.local_get(t32(2)).i64_load(tlb + 8).local_get(addr).op(op::I64_ADD);
            self.f.op(op::I32_WRAP_I64).i64_load_n(bytes, 0);
            self.f.else_();
            self.f.local_get(L_STATE).local_get(addr);
            self.slow_args();
            self.f.call(f_tlb(F_LD_TLB, sys.el, bytes)).if_(BLOCK_EMPTY);
            self.exit_fault_saved();
            self.f.end();
            self.f.end();
            return;
        }
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.slow_args();
                self.f.call(f_tlb(F_LD_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32);
                self.slow_args();
                self.f.call(F_LD_SLOW);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Store di `bytes` byte del valore in `val` all'indirizzo in `addr`
    /// (DC ZVA con `bytes` = [`ZVA_BYTES`], sempre dall'host); se è un
    /// fault esce con FAULT. Una scrittura su codice sorvegliato (STOP): con
    /// `stop` = `None` esce subito dopo l'istruzione (store unico, senza
    /// altro da fare dopo), altrimenti lo segna in `t32(stop)` (0 se no)
    /// per [`stop_after`](Self::stop_after).
    fn st(&mut self, addr: u32, bytes: u32, val: u32, stop: Option<u32>) {
        if let Some(sys) = self.sys
            && self.inline_tlb
            && bytes != ZVA_BYTES
        {
            let tlb = area::tlb(sys.el, true);
            self.tlb_hit(addr, bytes, tlb);
            self.f.if_(BLOCK_EMPTY);
            self.f.local_get(t32(2)).i64_load(tlb + 8).local_get(addr).op(op::I64_ADD);
            self.f.op(op::I32_WRAP_I64).local_get(val).i64_store_n(bytes, 0);
            if let Some(s) = stop {
                self.f.i32_const(0).local_set(t32(s));
            }
            self.f.else_();
            self.f.local_get(L_STATE).local_get(addr).local_get(val);
            self.slow_args();
            self.f.call(f_tlb(F_ST_TLB, sys.el, bytes));
            self.st_result(stop);
            self.f.end();
            return;
        }
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) if bytes != ZVA_BYTES => {
                self.f.local_get(val);
                self.slow_args();
                self.f.call(f_tlb(F_ST_TLB, sys.el, bytes));
            }
            _ => {
                self.f.i32_const(bytes as i32).local_get(val);
                self.slow_args();
                self.f.call(F_ST_SLOW);
            }
        }
        self.st_result(stop);
    }

    /// Lascia sullo stack (i32) il colpo nella TLB software `tlb` per un
    /// accesso di `bytes` byte all'indirizzo in `addr`, allineato: il tag
    /// della voce è la pagina di `addr` e i bit bassi sotto `bytes` sono
    /// zero. L'indirizzo della voce resta in `t32(2)`.
    fn tlb_hit(&mut self, addr: u32, bytes: u32, tlb: u32) {
        let f = &mut self.f;
        f.local_get(addr).i64_const(8).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
        f.i32_const(((area::TLB_ENTRIES - 1) << 4) as i32).op(op::I32_AND);
        f.local_get(L_STATE).op(op::I32_ADD).local_tee(t32(2)).i64_load(tlb);
        f.local_get(addr).i64_const((!0xfffu64 | (bytes as u64 - 1)) as i64).op(op::I64_AND);
        f.op(op::I64_EQ);
    }

    /// Esito di uno store (0, FAULT o STOP) sullo stack: fault → uscita;
    /// STOP → uscita dopo l'istruzione (`stop` = `None`) o segnato in
    /// `t32(stop)`.
    fn st_result(&mut self, stop: Option<u32>) {
        let s = stop.unwrap_or(0);
        self.f.local_tee(t32(s));
        self.f.i32_const(FAULT as i32).op(op::I32_EQ).if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
        if stop.is_none() {
            self.stop_after(&[0]);
        }
    }

    /// Coppia di load di `bytes` byte (4 o 8) da `addr` e `addr + bytes`:
    /// i due valori sullo stack (esce con FAULT se uno fallisce).
    fn ld_pair(&mut self, addr: u32, bytes: u32) {
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.slow_args();
                self.f.call(f_pair(F_LDP_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32);
                self.slow_args();
                self.f.call(F_LDP_SLOW);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Coppia di store dei valori in `v1` e `v2`, come [`st`](Self::st).
    fn st_pair(&mut self, addr: u32, bytes: u32, v1: u32, v2: u32, stop: Option<u32>) {
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.f.local_get(v1).local_get(v2);
                self.slow_args();
                self.f.call(f_pair(F_STP_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32).local_get(v1).local_get(v2);
                self.slow_args();
                self.f.call(F_STP_SLOW);
            }
        }
        self.st_result(stop);
    }

    /// Modalità sistema, base SP: con SP non allineato a 16 esce con FAULT
    /// e l'interprete decide (SCTLR_EL1.SA/SA0, `CheckSPAlignment`).
    /// Una volta per blocco base finché SP non cambia in modo che possa
    /// perdere l'allineamento (`sp_ok`).
    fn sp_check(&mut self, rn: u8) {
        if self.sys.is_none() || rn != 31 || self.sp_ok {
            return;
        }
        self.get_xsp(31);
        self.f.op(op::I32_WRAP_I64).i32_const(15).op(op::I32_AND).if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
        self.sp_ok = true;
    }

    /// Dopo il writeback della base SP di `offset` byte: SP resta allineato
    /// se lo era e `offset` è multiplo di 16.
    fn sp_writeback(&mut self, rn: u8, was_ok: bool, offset: i64) {
        if rn == 31 && was_ok && offset % 16 == 0 {
            self.sp_ok = true;
        }
    }

    /// Indirizzo di un salto preso con destinazione costante.
    fn target(&self, t: u64) -> u64 {
        match self.sys {
            Some(s) => s.branch_addr(t),
            None => t,
        }
    }

    /// `AArch64.BranchAddr` sulla destinazione in `t64(3)` (modalità
    /// sistema con TBI), che resta in `t64(3)`.
    fn branch_addr_dyn(&mut self) {
        let Some(s) = self.sys else { return };
        let f = &mut self.f;
        let bit55 = |f: &mut Func| {
            f.local_get(t64(3)).i64_const(55).op(op::I64_SHR_U).i64_const(1).op(op::I64_AND);
            f.op(op::I32_WRAP_I64);
        };
        match (s.tbi0, s.tbi1) {
            (false, false) => return,
            (true, true) => {
                f.local_get(t64(3)).i64_const(8).op(op::I64_SHL).i64_const(8).op(op::I64_SHR_S);
            }
            (true, false) => {
                f.local_get(t64(3)).i64_const(0x00ff_ffff_ffff_ffff).op(op::I64_AND);
                f.local_get(t64(3));
                bit55(f);
                f.op(op::I32_EQZ).op(op::SELECT);
            }
            (false, true) => {
                f.local_get(t64(3)).i64_const(0xff00_0000_0000_0000u64 as i64).op(op::I64_OR);
                f.local_get(t64(3));
                bit55(f);
                f.op(op::SELECT);
            }
        }
        f.local_set(t64(3));
    }

    /// Dopo gli store dell'istruzione corrente (e il writeback): se uno ha
    /// chiesto di fermarsi, esce con STOP dopo l'istruzione.
    fn stop_after(&mut self, stops: &[u32]) {
        for (i, &s) in stops.iter().enumerate() {
            self.f.local_get(t32(s));
            if i > 0 {
                self.f.op(op::I32_OR);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        self.f.i32_const(EXIT_STOP_SAVED as i32).local_set(L_EXIT_CODE);
        let depth = self.f.depth;
        self.f.br(depth - 1);
        self.f.end();
    }

    // --- SIMD (ADR 0024) ------------------------------------------------

    /// Offset in `JitState` della metà bassa (`hi` = falso) o alta di Vr.
    fn v_off(r: u8, hi: bool) -> u32 {
        off::V + 16 * r as u32 + if hi { 8 } else { 0 }
    }

    /// Metà di Vr (i64) sullo stack.
    fn get_v(&mut self, r: u8, hi: bool) {
        self.simd = true;
        self.f.local_get(L_STATE).i64_load(Self::v_off(r, hi));
    }

    /// Metà di Vr = locale `l` (i64).
    fn set_v(&mut self, r: u8, hi: bool, l: u32) {
        self.simd = true;
        self.f.local_get(L_STATE).local_get(l).i64_store(Self::v_off(r, hi));
    }

    /// Metà di Vr = costante.
    fn set_v_const(&mut self, r: u8, hi: bool, v: i64) {
        self.simd = true;
        self.f.local_get(L_STATE).i64_const(v).i64_store(Self::v_off(r, hi));
    }

    /// Load di 16 byte (Q) da `addr`: metà bassa e alta sullo stack (esce
    /// con FAULT se l'accesso fallisce, o se va rifatto dall'interprete).
    fn ld_q(&mut self, addr: u32) {
        self.f.local_get(L_STATE).local_get(addr);
        self.slow_args();
        self.f.call(match self.sys {
            Some(s) => F_LDQ_TLB + s.el as u32,
            None => F_LDQ_SLOW,
        });
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Store di 16 byte (Q) delle metà in `lo` e `hi`, come [`st`](Self::st).
    fn st_q(&mut self, addr: u32, lo: u32, hi: u32, stop: Option<u32>) {
        self.f.local_get(L_STATE).local_get(addr).local_get(lo).local_get(hi);
        self.slow_args();
        self.f.call(match self.sys {
            Some(s) => F_STQ_TLB + s.el as u32,
            None => F_STQ_SLOW,
        });
        self.st_result(stop);
    }

    /// Load/store dei registri V (`VecMemInsn::Reg` e `Pair`), come
    /// `simd::ldst::exec`: indirizzo, accessi, registri scritti solo dopo
    /// l'ultimo accesso, poi il writeback della base.
    fn vec_mem(&mut self, m: VecMemInsn) {
        match m {
            VecMemInsn::Reg { scale, load, addr, rt, rn } => {
                self.sp_check(rn);
                let was_ok = self.sp_ok;
                // indirizzo in t64(4), base nuova in t64(6)
                let (writeback, wb_off) = match addr {
                    AddrMode::Imm { offset, index } => {
                        self.get_xsp(rn);
                        match index {
                            Index::Offset => {
                                if offset != 0 {
                                    self.f.i64_const(offset).op(op::I64_ADD);
                                }
                                self.f.local_set(t64(4));
                                (false, 0)
                            }
                            Index::Pre => {
                                self.f.i64_const(offset).op(op::I64_ADD).local_tee(t64(4)).local_set(t64(6));
                                (true, offset)
                            }
                            Index::Post => {
                                self.f.local_tee(t64(4)).i64_const(offset).op(op::I64_ADD).local_set(t64(6));
                                (true, offset)
                            }
                        }
                    }
                    AddrMode::Reg { rm, extend, shift } => {
                        self.get_xsp(rn);
                        self.get_x(rm);
                        self.extend_reg(extend, shift);
                        self.f.op(op::I64_ADD).local_set(t64(4));
                        (false, 0)
                    }
                };
                let stop = writeback.then_some(0);
                if scale <= 3 {
                    let bytes = 1u32 << scale;
                    if load {
                        self.ld(t64(4), bytes);
                        self.f.local_set(t64(5));
                        self.set_v(rt, false, t64(5));
                        self.set_v_const(rt, true, 0);
                    } else {
                        self.get_v(rt, false);
                        self.f.local_set(t64(7));
                        self.st(t64(4), bytes, t64(7), stop);
                    }
                } else if load {
                    self.ld_q(t64(4));
                    self.f.local_set(t64(3)).local_set(t64(5));
                    self.set_v(rt, false, t64(5));
                    self.set_v(rt, true, t64(3));
                } else {
                    self.get_v(rt, false);
                    self.f.local_set(t64(7));
                    self.get_v(rt, true);
                    self.f.local_set(t64(3));
                    self.st_q(t64(4), t64(7), t64(3), stop);
                }
                if writeback {
                    self.f.local_get(t64(6));
                    self.set_xsp(rn);
                    self.sp_writeback(rn, was_ok, wb_off);
                    if !load {
                        self.stop_after(&[0]);
                    }
                }
            }
            VecMemInsn::Pair { scale, load, index, offset, rt, rt2, rn } => {
                self.sp_check(rn);
                let was_ok = self.sp_ok;
                self.get_xsp(rn);
                match index {
                    Index::Offset => {
                        self.f.i64_const(offset).op(op::I64_ADD).local_set(t64(4));
                    }
                    Index::Pre => {
                        self.f.i64_const(offset).op(op::I64_ADD).local_tee(t64(4)).local_set(t64(6));
                    }
                    Index::Post => {
                        self.f.local_tee(t64(4)).i64_const(offset).op(op::I64_ADD).local_set(t64(6));
                    }
                }
                let writeback = index != Index::Offset;
                if scale <= 3 {
                    let bytes = 1u32 << scale;
                    if load {
                        self.ld_pair(t64(4), bytes);
                        self.f.local_set(t64(3)).local_set(t64(5));
                        self.set_v(rt, false, t64(5));
                        self.set_v_const(rt, true, 0);
                        self.set_v(rt2, false, t64(3));
                        self.set_v_const(rt2, true, 0);
                    } else {
                        self.get_v(rt, false);
                        self.f.local_set(t64(7));
                        self.get_v(rt2, false);
                        self.f.local_set(t64(3));
                        self.st_pair(t64(4), bytes, t64(7), t64(3), writeback.then_some(0));
                    }
                } else {
                    // Due Q: a `addr` e a `addr + 16`.
                    self.f.local_get(t64(4)).i64_const(16).op(op::I64_ADD).local_set(t64(8));
                    if load {
                        self.ld_q(t64(4));
                        self.f.local_set(t64(1)).local_set(t64(0));
                        self.ld_q(t64(8));
                        self.f.local_set(t64(3)).local_set(t64(2));
                        self.set_v(rt, false, t64(0));
                        self.set_v(rt, true, t64(1));
                        self.set_v(rt2, false, t64(2));
                        self.set_v(rt2, true, t64(3));
                    } else {
                        self.get_v(rt, false);
                        self.f.local_set(t64(0));
                        self.get_v(rt, true);
                        self.f.local_set(t64(1));
                        self.get_v(rt2, false);
                        self.f.local_set(t64(2));
                        self.get_v(rt2, true);
                        self.f.local_set(t64(3));
                        self.st_q(t64(4), t64(0), t64(1), Some(0));
                        self.st_q(t64(8), t64(2), t64(3), Some(1));
                    }
                }
                if writeback {
                    self.f.local_get(t64(6));
                    self.set_xsp(rn);
                    self.sp_writeback(rn, was_ok, offset);
                }
                if !load {
                    match (scale, writeback) {
                        (4, _) => self.stop_after(&[0, 1]),
                        (_, true) => self.stop_after(&[0]),
                        _ => {}
                    }
                }
            }
            other => unreachable!("load/store SIMD non tradotto: {other:?}"),
        }
    }

    /// Elemento `index` di `es` bit di Vn sullo stack (esteso a zero).
    fn v_elem(&mut self, rn: u8, index: u8, es: u32) {
        let bit = index as u32 * es;
        self.get_v(rn, bit >= 64);
        let sh = bit % 64;
        if sh != 0 {
            self.f.i64_const(sh as i64).op(op::I64_SHR_U);
        }
        if es < 64 {
            self.f.i64_const(((1u64 << es) - 1) as i64).op(op::I64_AND);
        }
    }

    /// Vd.<es>[index] = valore in `l` (i64), il resto invariato.
    fn v_insert(&mut self, rd: u8, index: u8, es: u32, l: u32) {
        let bit = index as u32 * es;
        let (hi, sh) = (bit >= 64, bit % 64);
        let m: u64 = if es == 64 { u64::MAX } else { ((1u64 << es) - 1) << sh };
        self.f.local_get(L_STATE);
        self.get_v(rd, hi);
        self.f.i64_const(!m as i64).op(op::I64_AND);
        self.f.local_get(l);
        if sh != 0 {
            self.f.i64_const(sh as i64).op(op::I64_SHL);
        }
        self.f.i64_const(m as i64).op(op::I64_AND).op(op::I64_OR);
        self.f.i64_store(Self::v_off(rd, hi));
    }

    /// Istruzioni SIMD intere tradotte (`simd::int::exec`).
    fn vec_int(&mut self, i: IntInsn) {
        match i {
            IntInsn::Copy { op: cop, scalar, q, esize, index, index2, rn, rd } => {
                let es = esize as u32;
                // Replica di `x` (t64(0)) su 64 bit.
                let rep = |t: &mut Tx| {
                    t.f.local_get(t64(0));
                    let m: u64 = match es {
                        8 => 0x0101_0101_0101_0101,
                        16 => 0x0001_0001_0001_0001,
                        32 => 0x0000_0001_0000_0001,
                        _ => 1,
                    };
                    if m != 1 {
                        t.f.i64_const(m as i64).op(op::I64_MUL);
                    }
                    t.f.local_set(t64(1));
                };
                match cop {
                    CopyOp::DupElem => {
                        self.v_elem(rn, index, es);
                        self.f.local_set(t64(0));
                        if scalar {
                            self.set_v(rd, false, t64(0));
                            self.set_v_const(rd, true, 0);
                        } else {
                            rep(self);
                            self.set_v(rd, false, t64(1));
                            if q { self.set_v(rd, true, t64(1)) } else { self.set_v_const(rd, true, 0) }
                        }
                    }
                    CopyOp::DupGen => {
                        self.get_x(rn);
                        if es < 64 {
                            self.f.i64_const(((1u64 << es) - 1) as i64).op(op::I64_AND);
                        }
                        self.f.local_set(t64(0));
                        rep(self);
                        self.simd = true;
                        self.set_v(rd, false, t64(1));
                        if q { self.set_v(rd, true, t64(1)) } else { self.set_v_const(rd, true, 0) }
                    }
                    CopyOp::InsGen => {
                        self.get_x(rn);
                        self.f.local_set(t64(0));
                        self.v_insert(rd, index, es, t64(0));
                    }
                    CopyOp::InsElem => {
                        self.v_elem(rn, index2, es);
                        self.f.local_set(t64(0));
                        self.v_insert(rd, index, es, t64(0));
                    }
                    CopyOp::Smov => {
                        self.v_elem(rn, index, es);
                        match es {
                            8 => self.f.op(op::I64_EXTEND8_S),
                            16 => self.f.op(op::I64_EXTEND16_S),
                            _ => self.f.op(op::I64_EXTEND32_S),
                        };
                        self.trunc(q);
                        self.set_x(rd);
                    }
                    CopyOp::Umov => {
                        self.v_elem(rn, index, es);
                        self.set_x(rd);
                    }
                }
            }
            IntInsn::MovImm { q, op: mop, imm, rd } => {
                let imm = imm as i64;
                let half = |t: &mut Tx, hi: bool| {
                    if hi && !q {
                        t.set_v_const(rd, true, 0);
                        return;
                    }
                    match mop {
                        MovImmOp::Movi => t.set_v_const(rd, hi, imm),
                        MovImmOp::Mvni => t.set_v_const(rd, hi, !imm),
                        MovImmOp::Orr | MovImmOp::Bic => {
                            t.f.local_get(L_STATE);
                            t.get_v(rd, hi);
                            if mop == MovImmOp::Orr {
                                t.f.i64_const(imm).op(op::I64_OR);
                            } else {
                                t.f.i64_const(!imm).op(op::I64_AND);
                            }
                            t.f.i64_store(Self::v_off(rd, hi));
                        }
                    }
                };
                half(self, false);
                half(self, true);
            }
            other => unreachable!("istruzione SIMD non tradotta: {other:?}"),
        }
    }

    /// Istruzione SIMD/FP senza memoria eseguita dall'interprete dalla
    /// regione (`rt.simd` → `env.simd`, [`crate::helper`]): passa il
    /// registro generale letto e NZCV, e scrive il registro generale o NZCV
    /// restituito.
    fn simd_helper(&mut self, s: &SimdInsn) {
        let io = crate::helper::io(s);
        self.simd = true;
        self.f.local_get(L_STATE).i32_const(self.word as i32);
        match io.x_in {
            Some(rn) => self.get_x(rn),
            None => {
                self.f.i64_const(0);
            }
        }
        if io.nzcv_in {
            self.get_nzcv();
        } else {
            self.f.i32_const(0);
        }
        self.f.call(F_SIMD);
        match io.out {
            crate::helper::Out::None => {
                self.f.op(op::DROP);
            }
            crate::helper::Out::X(rd) => self.set_x(rd),
            crate::helper::Out::Nzcv => {
                self.f.op(op::I32_WRAP_I64);
                self.set_nzcv();
            }
        }
    }

    /// Se la cima dello stack (i32) non è zero (interrupt smascherati), esce
    /// con YIELD dopo l'istruzione corrente.
    fn yield_if(&mut self) {
        self.f.if_(BLOCK_EMPTY);
        let (next, done) = (self.pc.wrapping_add(4), self.index + 1);
        self.exit_const(YIELD, next, done);
        self.f.end();
    }

    /// Estende un valore caricato di `1 << size` byte come chiede `op`.
    fn extend_load(&mut self, size: u8, op_: MemOp) {
        if let MemOp::Load { signed: true, dst64 } = op_ {
            let f = &mut self.f;
            match size {
                0 => f.op(op::I64_EXTEND8_S),
                1 => f.op(op::I64_EXTEND16_S),
                2 => f.op(op::I64_EXTEND32_S),
                _ => f,
            };
            if !dst64 {
                f.i64_const(0xffff_ffff).op(op::I64_AND);
            }
        }
    }

    // --- istruzioni ---------------------------------------------------

    fn insn(&mut self, insn: &Insn) {
        let pc = self.pc;
        match *insn {
            Insn::AddSubImm { sf, sub, setflags: false, imm, rn, rd } => {
                self.get_xsp(rn);
                self.f.i64_const(imm as i64).op(if sub { op::I64_SUB } else { op::I64_ADD });
                self.trunc(sf);
                self.set_xsp(rd);
            }
            Insn::AddSubImm { sf, sub, setflags: true, imm, rn, rd } => {
                self.get_xsp(rn);
                self.trunc(sf);
                self.f.local_set(L_FA).i64_const(imm as i64).local_set(L_FB);
                self.flag_op(sub, sf, rd);
            }
            Insn::LogicalImm { sf, op: lop, imm, rn, rd } => {
                self.get_x(rn);
                self.f.i64_const(imm as i64);
                self.f.op(match lop {
                    LogicOp::And | LogicOp::Ands => op::I64_AND,
                    LogicOp::Orr => op::I64_OR,
                    LogicOp::Eor => op::I64_XOR,
                });
                self.trunc(sf);
                if lop == LogicOp::Ands {
                    self.logic_flags(sf);
                    self.set_x(rd);
                } else {
                    self.set_xsp(rd);
                }
            }
            Insn::MoveWide { sf, op: mop, shift, imm16, rd } => {
                let imm = (imm16 as u64) << shift;
                match mop {
                    MovOp::Movz => {
                        self.f.i64_const(imm as i64);
                    }
                    MovOp::Movn => {
                        self.f.i64_const(!imm as i64);
                    }
                    MovOp::Movk => {
                        self.get_x(rd);
                        self.f.i64_const(!(0xffffu64 << shift) as i64).op(op::I64_AND);
                        self.f.i64_const(imm as i64).op(op::I64_OR);
                    }
                }
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Adr { page, imm, rd } => {
                let base = if page { pc & !0xfff } else { pc };
                self.f.i64_const(base.wrapping_add(imm as u64) as i64);
                self.set_x(rd);
            }
            // UBFM/SBFM nelle forme più comuni (LSL, LSR, ASR, UBFX, SBFX,
            // UBFIZ, SBFIZ, estensioni): shift e maschere dirette.
            Insn::Bitfield { sf, op: bop @ (BfOp::Ubfm | BfOp::Sbfm), r, s, rn, rd, .. } => {
                let (d, r, s) = (if sf { 64i64 } else { 32 }, r as i64, s as i64);
                self.get_x(rn);
                let f = &mut self.f;
                match (bop, sf) {
                    (BfOp::Ubfm, _) => {
                        let (sh, width, left) =
                            if s >= r { (r, s - r + 1, false) } else { (d - r, s + 1, true) };
                        let mask = if width == 64 { -1 } else { (1i64 << width) - 1 };
                        if left {
                            f.i64_const(mask).op(op::I64_AND).i64_const(sh).op(op::I64_SHL);
                        } else {
                            // La maschera (al più 32 - r bit a 32 bit) toglie
                            // anche i bit alti di Wn.
                            if sh != 0 {
                                f.i64_const(sh).op(op::I64_SHR_U);
                            }
                            f.i64_const(mask).op(op::I64_AND);
                        }
                    }
                    (_, true) => {
                        // Estensione del segno dei bit [s:r] (o [s:0] poi a sinistra).
                        f.i64_const(63 - s).op(op::I64_SHL);
                        if s >= r {
                            f.i64_const(63 - s + r).op(op::I64_SHR_S);
                        } else {
                            f.i64_const(63 - s).op(op::I64_SHR_S).i64_const(64 - r).op(op::I64_SHL);
                        }
                    }
                    (_, false) => {
                        f.op(op::I32_WRAP_I64).i32_const((31 - s) as i32).op(op::I32_SHL);
                        if s >= r {
                            f.i32_const((31 - s + r) as i32).op(op::I32_SHR_S);
                        } else {
                            f.i32_const((31 - s) as i32).op(op::I32_SHR_S);
                            f.i32_const((32 - r) as i32).op(op::I32_SHL);
                        }
                        f.op(op::I64_EXTEND_I32_U);
                    }
                }
                self.set_x(rd);
            }
            Insn::Bitfield { sf, op: bop, r, s, wmask, tmask, rn, rd } => {
                // src in t64(0), dst in t64(1)
                self.get_x(rn);
                self.f.local_set(t64(0));
                if bop == BfOp::Bfm {
                    self.get_x(rd);
                } else {
                    self.f.i64_const(0);
                }
                self.f.local_set(t64(1));
                let f = &mut self.f;
                // bot = (dst & !wmask) | (ror(src, r, n) & wmask)
                f.local_get(t64(1)).i64_const(!wmask as i64).op(op::I64_AND);
                if sf {
                    f.local_get(t64(0)).i64_const(r as i64).op(op::I64_ROTR);
                } else {
                    f.local_get(t64(0)).op(op::I32_WRAP_I64).i32_const(r as i32).op(op::I32_ROTR);
                    f.op(op::I64_EXTEND_I32_U);
                }
                f.i64_const(wmask as i64).op(op::I64_AND).op(op::I64_OR).local_set(t64(2));
                // top
                if bop == BfOp::Sbfm {
                    f.i64_const(0).local_get(t64(0)).i64_const(s as i64).op(op::I64_SHR_U);
                    f.i64_const(1).op(op::I64_AND).op(op::I64_SUB);
                } else {
                    f.local_get(t64(1));
                }
                f.i64_const(!tmask as i64).op(op::I64_AND);
                f.local_get(t64(2)).i64_const(tmask as i64).op(op::I64_AND).op(op::I64_OR);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Extract { sf, lsb, rm, rn, rd } => {
                if sf {
                    if lsb == 0 {
                        self.get_x(rm);
                    } else {
                        self.get_x(rm);
                        self.f.i64_const(lsb as i64).op(op::I64_SHR_U);
                        self.get_x(rn);
                        self.f.i64_const(64 - lsb as i64).op(op::I64_SHL).op(op::I64_OR);
                    }
                } else {
                    self.get_x(rn);
                    self.f.i64_const(32).op(op::I64_SHL);
                    self.get_x(rm);
                    self.f.i64_const(0xffff_ffff).op(op::I64_AND).op(op::I64_OR);
                    self.f.i64_const(lsb as i64).op(op::I64_SHR_U);
                    self.trunc(false);
                }
                self.set_x(rd);
            }
            Insn::LogicalReg { sf, op: LogicOp::Orr, invert: false, shift: _, amount: 0, rm, rn: 31, rd } => {
                self.get_x(rm);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::LogicalReg { sf, op: lop, invert, shift, amount, rm, rn, rd } => {
                self.get_x(rn);
                self.get_x(rm);
                if !sf {
                    self.trunc(false);
                }
                self.shift_reg(shift, amount, sf);
                if invert {
                    self.f.i64_const(-1).op(op::I64_XOR);
                }
                self.f.op(match lop {
                    LogicOp::And | LogicOp::Ands => op::I64_AND,
                    LogicOp::Orr => op::I64_OR,
                    LogicOp::Eor => op::I64_XOR,
                });
                self.trunc(sf);
                if lop == LogicOp::Ands {
                    self.logic_flags(sf);
                }
                self.set_x(rd);
            }
            Insn::AddSubReg { sf, sub, setflags: false, shift, amount, rm, rn, rd } => {
                self.get_x(rn);
                self.get_x(rm);
                self.shift_reg(shift, amount, sf);
                self.f.op(if sub { op::I64_SUB } else { op::I64_ADD });
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::AddSubReg { sf, sub, setflags: true, shift, amount, rm, rn, rd } => {
                self.get_x(rn);
                self.trunc(sf);
                self.f.local_set(L_FA);
                self.get_x(rm);
                self.shift_reg(shift, amount, sf);
                self.f.local_set(L_FB);
                self.flag_op(sub, sf, rd);
            }
            Insn::AddSubExt { sf, sub, setflags: false, extend, amount, rm, rn, rd } => {
                self.get_xsp(rn);
                self.get_x(rm);
                self.extend_reg(extend, amount);
                self.f.op(if sub { op::I64_SUB } else { op::I64_ADD });
                self.trunc(sf);
                self.set_xsp(rd);
            }
            Insn::AddSubExt { sf, sub, setflags: true, extend, amount, rm, rn, rd } => {
                self.get_xsp(rn);
                self.trunc(sf);
                self.f.local_set(L_FA);
                self.get_x(rm);
                self.extend_reg(extend, amount);
                self.trunc(sf);
                self.f.local_set(L_FB);
                self.flag_op(sub, sf, rd);
            }
            Insn::AddSubCarry { sf, sub, setflags, rm, rn, rd } => {
                self.get_x(rn);
                self.f.local_set(t64(0));
                self.get_x(rm);
                if sub {
                    self.f.i64_const(-1).op(op::I64_XOR);
                }
                self.f.local_set(t64(1));
                self.add_with_carry(sf, None, setflags);
                self.f.local_get(t64(2));
                self.set_x(rd);
            }
            Insn::CondCmp { sf, sub, operand, cond, nzcv, rn } => {
                self.cond(cond);
                self.f.if_(BLOCK_EMPTY);
                self.get_x(rn);
                self.f.local_set(t64(0));
                match operand {
                    CcmpOperand::Reg(rm) => self.get_x(rm),
                    CcmpOperand::Imm(i) => {
                        self.f.i64_const(i as i64);
                    }
                }
                self.f.local_set(t64(1));
                self.add_sub(sub, true, sf);
                self.f.else_();
                self.f.i32_const(((nzcv as u32) << 28) as i32);
                self.set_nzcv();
                self.f.end();
                self.fl = Fl::Unknown;
            }
            Insn::CondSel { sf, op: cop, cond, rm, rn, rd } => {
                self.get_x(rn);
                match cop {
                    CselOp::Csel => self.get_x(rm),
                    CselOp::Csinc => {
                        self.get_x(rm);
                        self.f.i64_const(1).op(op::I64_ADD);
                    }
                    CselOp::Csinv => {
                        self.get_x(rm);
                        self.f.i64_const(-1).op(op::I64_XOR);
                    }
                    CselOp::Csneg => {
                        self.f.i64_const(0);
                        self.get_x(rm);
                        self.f.op(op::I64_SUB);
                    }
                }
                self.cond(cond);
                self.f.op(op::SELECT);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Dp1 { sf, op: dop, rn, rd } => {
                self.get_x(rn);
                self.dp1(sf, dop);
                self.set_x(rd);
            }
            Insn::Dp2 { sf, op: dop, rm, rn, rd } => {
                self.get_x(rn);
                self.trunc(sf);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.trunc(sf);
                self.f.local_set(t64(1));
                self.dp2(sf, dop);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Dp3 { sf, op: dop, ra, rm, rn, rd } => {
                self.get_x(ra);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.f.local_set(t64(1));
                self.get_x(rn);
                self.f.local_set(t64(2));
                self.dp3(dop);
                self.trunc(sf);
                self.set_x(rd);
            }

            Insn::B { link, offset } => {
                if link {
                    self.f.i64_const(pc.wrapping_add(4) as i64);
                    self.set_x(30);
                }
                let t = self.target(pc.wrapping_add(offset as u64));
                self.jump(t, self.index + 1, true);
            }
            Insn::BCond { cond, offset } => {
                self.cond(cond);
                self.cond_branch(pc.wrapping_add(offset as u64));
            }
            Insn::Cbz { sf, nonzero, rt, offset } => {
                // (x != 0) == nonzero
                self.get_x(rt);
                self.trunc(sf);
                self.f.op(op::I64_EQZ);
                if nonzero {
                    self.f.op(op::I32_EQZ);
                }
                self.cond_branch(pc.wrapping_add(offset as u64));
            }
            Insn::Tbz { nonzero, bit, rt, offset } => {
                self.get_x(rt);
                self.f.i64_const(bit as i64).op(op::I64_SHR_U).i64_const(1).op(op::I64_AND);
                self.f.op(op::I32_WRAP_I64);
                if !nonzero {
                    self.f.op(op::I32_EQZ);
                }
                self.cond_branch(pc.wrapping_add(offset as u64));
            }
            Insn::BranchReg { op: bop, rn } => {
                self.get_x(rn);
                self.f.local_set(t64(3));
                self.branch_addr_dyn();
                if bop == BrOp::Blr {
                    self.f.i64_const(pc.wrapping_add(4) as i64);
                    self.set_x(30);
                }
                self.f.local_get(t64(3));
                self.exit_branch();
            }

            Insn::Nop | Insn::Barrier | Insn::CacheMaint | Insn::Wfi | Insn::Wfe => {}
            Insn::Mrs { reg: SysReg::Nzcv, rt } => {
                self.get_nzcv();
                self.f.op(op::I64_EXTEND_I32_U);
                self.set_x(rt);
            }
            Insn::Msr { reg: SysReg::Nzcv, rt } => {
                self.get_x(rt);
                self.f.op(op::I32_WRAP_I64).i32_const(0xf000_0000u32 as i32).op(op::I32_AND);
                self.set_nzcv();
            }

            Insn::Mrs { reg, rt } => {
                let s = self.sys.expect("MRS di sistema solo in modalità sistema");
                match sys_mrs(reg, s).expect("classificato da kind_in") {
                    MrsSrc::State(o) => {
                        self.f.local_get(L_STATE).i64_load(o);
                    }
                    MrsSrc::State32(o) => {
                        self.f.local_get(L_STATE).i32_load(o).op(op::I64_EXTEND_I32_U);
                    }
                    MrsSrc::Const(v) => {
                        self.f.i64_const(v as i64);
                    }
                }
                self.set_x(rt);
            }
            Insn::Msr { reg, rt } => {
                let s = self.sys.expect("MSR di sistema solo in modalità sistema");
                let o = sys_msr(reg, s).expect("classificato da kind_in");
                if reg == SysReg::Daif {
                    // Come `sysreg_write`: DAIF = Xt & DAIF_ALL; se un bit passa
                    // da 1 a 0 esce con YIELD dopo l'istruzione.
                    self.f.local_get(L_STATE).i32_load(off::DAIF).local_set(t32(0));
                    self.get_x(rt);
                    self.f.op(op::I32_WRAP_I64).i32_const(DAIF_ALL as i32).op(op::I32_AND).local_set(t32(1));
                    self.f.local_get(L_STATE).local_get(t32(1)).i32_store(off::DAIF);
                    self.f.local_get(t32(0)).local_get(t32(1)).i32_const(-1).op(op::I32_XOR).op(op::I32_AND);
                    self.yield_if();
                } else {
                    self.f.local_get(L_STATE);
                    self.get_x(rt);
                    self.f.i64_store(o);
                }
            }
            Insn::MsrImm { field, imm } => {
                // DAIFSet / DAIFClr a EL1 (kind_in), come `step_system`.
                let bits = (imm as u32 & 0xf) << 6;
                self.f.local_get(L_STATE).local_get(L_STATE).i32_load(off::DAIF).local_tee(t32(0));
                if field == PstateField::DaifSet {
                    self.f.i32_const(bits as i32).op(op::I32_OR).i32_store(off::DAIF);
                } else {
                    self.f.i32_const(!bits as i32).op(op::I32_AND).i32_store(off::DAIF);
                    self.f.local_get(t32(0)).i32_const(bits as i32).op(op::I32_AND);
                    self.yield_if();
                }
            }
            Insn::Exclusive { size, load, pair, rs, rt, rt2, rn } => {
                self.exclusive(size, load, pair, rs, rt, rt2, rn)
            }
            Insn::DcZva { rt } => {
                if self.sys.is_some_and(|s| s.el == 0) {
                    // DCZID_EL0.DZP (SCTLR_EL1.DZE a 0): trap nell'interprete.
                    self.f.local_get(L_STATE).i64_load(off::DCZID).i64_const(16).op(op::I64_AND);
                    self.f.op(op::I32_WRAP_I64).if_(BLOCK_EMPTY);
                    self.exit_fault();
                    self.f.end();
                }
                self.get_x(rt);
                self.f.i64_const(!63).op(op::I64_AND).local_set(t64(4));
                self.f.i64_const(0).local_set(t64(7));
                // Sempre dall'host: 64 byte, e fault di allineamento su
                // memoria Device come `zero_block`.
                self.st(t64(4), ZVA_BYTES, t64(7), None);
            }
            Insn::LdSt { size, op: mop, addr, rt, rn, unpriv: _ } => {
                if mop == MemOp::Prefetch {
                    return;
                }
                self.sp_check(rn);
                let was_ok = self.sp_ok;
                let wb_off = if let AddrMode::Imm { offset, .. } = addr { offset } else { 0 };
                // address in t64(4), writeback in t64(6)
                let writeback = match addr {
                    AddrMode::Imm { offset, index } => {
                        self.get_xsp(rn);
                        match index {
                            Index::Offset => {
                                if offset != 0 {
                                    self.f.i64_const(offset).op(op::I64_ADD);
                                }
                                self.f.local_set(t64(4));
                                false
                            }
                            Index::Pre => {
                                self.f.i64_const(offset).op(op::I64_ADD).local_tee(t64(4)).local_set(t64(6));
                                true
                            }
                            Index::Post => {
                                self.f.local_tee(t64(4)).i64_const(offset).op(op::I64_ADD).local_set(t64(6));
                                true
                            }
                        }
                    }
                    AddrMode::Reg { rm, extend, shift } => {
                        self.get_xsp(rn);
                        self.get_x(rm);
                        self.extend_reg(extend, shift);
                        self.f.op(op::I64_ADD).local_set(t64(4));
                        false
                    }
                };
                let bytes = 1u32 << size;
                match mop {
                    MemOp::Store => {
                        self.get_x(rt);
                        self.f.local_set(t64(7));
                        if writeback {
                            self.st(t64(4), bytes, t64(7), Some(0));
                            self.f.local_get(t64(6));
                            self.set_xsp(rn);
                            self.sp_writeback(rn, was_ok, wb_off);
                            self.stop_after(&[0]);
                        } else {
                            self.st(t64(4), bytes, t64(7), None);
                        }
                    }
                    MemOp::Load { .. } => {
                        self.ld(t64(4), bytes);
                        self.extend_load(size, mop);
                        self.set_x(rt);
                        if writeback {
                            self.f.local_get(t64(6));
                            self.set_xsp(rn);
                            self.sp_writeback(rn, was_ok, wb_off);
                        }
                    }
                    MemOp::Prefetch => unreachable!(),
                }
            }
            Insn::LdLiteral { size, op: mop, offset, rt } => {
                if mop == MemOp::Prefetch {
                    return;
                }
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64).local_set(t64(4));
                self.ld(t64(4), 1 << size);
                self.extend_load(size, mop);
                self.set_x(rt);
            }
            Insn::LdStPair { size, load, signed, index, offset, rt, rt2, rn } => {
                // indirizzo in t64(4), base nuova (writeback) in t64(6)
                self.sp_check(rn);
                let was_ok = self.sp_ok;
                self.get_xsp(rn);
                match index {
                    Index::Offset => {
                        self.f.i64_const(offset).op(op::I64_ADD).local_set(t64(4));
                    }
                    Index::Pre => {
                        self.f.i64_const(offset).op(op::I64_ADD).local_tee(t64(4)).local_set(t64(6));
                    }
                    Index::Post => {
                        self.f.local_tee(t64(4)).i64_const(offset).op(op::I64_ADD).local_set(t64(6));
                    }
                }
                let bytes = 1u32 << size;
                let writeback = index != Index::Offset;
                if load {
                    let lop = MemOp::Load { signed, dst64: true };
                    self.ld_pair(t64(4), bytes);
                    self.f.local_set(t64(3));
                    self.extend_load(size, lop);
                    self.set_x(rt);
                    self.f.local_get(t64(3));
                    self.extend_load(size, lop);
                    self.set_x(rt2);
                    if writeback {
                        self.f.local_get(t64(6));
                        self.set_xsp(rn);
                        self.sp_writeback(rn, was_ok, offset);
                    }
                } else {
                    self.get_x(rt);
                    self.f.local_set(t64(7));
                    self.get_x(rt2);
                    self.f.local_set(t64(3));
                    if writeback {
                        self.st_pair(t64(4), bytes, t64(7), t64(3), Some(0));
                        self.f.local_get(t64(6));
                        self.set_xsp(rn);
                        self.sp_writeback(rn, was_ok, offset);
                        self.stop_after(&[0]);
                    } else {
                        self.st_pair(t64(4), bytes, t64(7), t64(3), None);
                    }
                }
            }
            Insn::LoadAcquire { size, rt, rn } => {
                self.sp_check(rn);
                self.get_xsp(rn);
                self.f.local_set(t64(4));
                self.misaligned_fault(size);
                self.ld(t64(4), 1 << size);
                self.set_x(rt);
            }
            Insn::StoreRelease { size, rt, rn } => {
                self.sp_check(rn);
                self.get_xsp(rn);
                self.f.local_set(t64(4));
                self.misaligned_fault(size);
                self.get_x(rt);
                self.f.local_set(t64(7));
                self.st(t64(4), 1 << size, t64(7), None);
            }
            Insn::Simd(SimdInsn::Mem(m)) => self.vec_mem(m),
            Insn::Simd(SimdInsn::Int(i @ (IntInsn::Copy { .. } | IntInsn::MovImm { .. }))) => self.vec_int(i),
            Insn::Simd(SimdInsn::Int(i)) if self.vec_int_inline(i) => {}
            Insn::Simd(SimdInsn::Fp(f)) if self.fp_inline(f) => {}
            Insn::Simd(s) => self.simd_helper(&s),
            other => unreachable!("istruzione non traducibile: {other:?}"),
        }
    }

    /// LDXR/LDAXR/STXR/STLXR e le coppie, con il monitor in `JitState`,
    /// come `Cpu::execute`: allineamento all'accesso intero (altrimenti
    /// FAULT e l'interprete dà l'eccezione), il load attiva il monitor, lo
    /// store riesce se il monitor è per lo stesso indirizzo e la stessa
    /// dimensione e la memoria ha ancora il valore letto; il monitor si
    /// spegne dopo lo store (anche fallito). Nessuno stato cambia prima
    /// dell'ultimo accesso che può fallire.
    #[allow(clippy::too_many_arguments)]
    fn exclusive(&mut self, size: u8, load: bool, pair: bool, rs: u8, rt: u8, rt2: u8, rn: u8) {
        self.sp_check(rn);
        self.get_xsp(rn);
        self.f.local_set(t64(4));
        let elem = 1u32 << size;
        let total = if pair { elem * 2 } else { elem };
        self.misaligned_fault(total.trailing_zeros() as u8);
        if total == 16 {
            self.f.local_get(t64(4)).i64_const(8).op(op::I64_ADD).local_set(t64(8));
        }
        // Valore di `total` byte in (lo, hi): da `ld` o nuovo.
        let load_pair = |t: &mut Tx, lo: u32, hi: u32| {
            if total <= 8 {
                t.ld(t64(4), total);
                t.f.local_set(lo).i64_const(0).local_set(hi);
            } else {
                t.ld(t64(4), 8);
                t.f.local_set(lo);
                t.ld(t64(8), 8);
                t.f.local_set(hi);
            }
        };
        let (lo, hi) = (t64(7), t64(3));
        if load {
            load_pair(self, lo, hi);
            let f = &mut self.f;
            f.local_get(L_STATE).i32_const(1).i32_store(off::MON_VALID);
            f.local_get(L_STATE).i32_const(total as i32).i32_store(off::MON_BYTES);
            f.local_get(L_STATE).local_get(t64(4)).i64_store(off::MON_ADDR);
            f.local_get(L_STATE).local_get(lo).i64_store(off::MON_LO);
            f.local_get(L_STATE).local_get(hi).i64_store(off::MON_HI);
            if pair && elem == 4 {
                self.f.local_get(lo).i64_const(0xffff_ffff).op(op::I64_AND);
                self.set_x(rt);
                self.f.local_get(lo).i64_const(32).op(op::I64_SHR_U);
                self.set_x(rt2);
            } else {
                self.f.local_get(lo);
                self.set_x(rt);
                if pair {
                    self.f.local_get(hi);
                    self.set_x(rt2);
                }
            }
            return;
        }
        // Nuovo valore.
        if pair && elem == 4 {
            self.get_x(rt);
            self.f.i64_const(0xffff_ffff).op(op::I64_AND);
            self.get_x(rt2);
            self.f.i64_const(32).op(op::I64_SHL).op(op::I64_OR).local_set(lo);
            self.f.i64_const(0).local_set(hi);
        } else {
            self.get_x(rt);
            self.f.local_set(lo);
            if pair {
                self.get_x(rt2);
            } else {
                self.f.i64_const(0);
            }
            self.f.local_set(hi);
        }
        // ok (t32(3)) = monitor per questo accesso e memoria invariata.
        self.f.i32_const(0).local_set(t32(3));
        self.f.i32_const(0).local_set(t32(0)).i32_const(0).local_set(t32(1));
        let f = &mut self.f;
        f.local_get(L_STATE).i32_load(off::MON_VALID);
        f.local_get(L_STATE).i64_load(off::MON_ADDR).local_get(t64(4)).op(op::I64_EQ).op(op::I32_AND);
        f.local_get(L_STATE).i32_load(off::MON_BYTES).i32_const(total as i32).op(op::I32_EQ).op(op::I32_AND);
        f.if_(BLOCK_EMPTY);
        load_pair(self, t64(6), t64(2));
        let f = &mut self.f;
        f.local_get(t64(6)).local_get(L_STATE).i64_load(off::MON_LO).op(op::I64_EQ);
        f.local_get(t64(2)).local_get(L_STATE).i64_load(off::MON_HI).op(op::I64_EQ).op(op::I32_AND);
        f.local_set(t32(3));
        f.end();
        self.f.local_get(t32(3)).if_(BLOCK_EMPTY);
        self.st(t64(4), total.min(8), lo, Some(0));
        if total == 16 {
            self.st(t64(8), 8, hi, Some(1));
        }
        self.f.end();
        self.f.local_get(L_STATE).i32_const(0).i32_store(off::MON_VALID);
        self.f.local_get(t32(3)).op(op::I32_EQZ).op(op::I64_EXTEND_I32_U);
        self.set_x(rs);
        if total == 16 { self.stop_after(&[0, 1]) } else { self.stop_after(&[0]) }
    }

    /// Se l'indirizzo in `t64(4)` non è allineato a `1 << size`, esce con
    /// FAULT: l'interprete riesegue e dà l'eccezione di allineamento.
    fn misaligned_fault(&mut self, size: u8) {
        if size == 0 {
            return;
        }
        self.f.local_get(t64(4)).i64_const((1i64 << size) - 1).op(op::I64_AND);
        self.f.op(op::I64_EQZ).op(op::I32_EQZ).if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
    }

    /// Dp1 sul valore in cima allo stack.
    fn dp1(&mut self, sf: bool, dop: Dp1Op) {
        let f = &mut self.f;
        // passo di scambio: ((v >> s) & m) | ((v & m) << s)
        let swap64 = |f: &mut Func, s: i64, m: u64| {
            f.local_tee(t64(0)).i64_const(s).op(op::I64_SHR_U).i64_const(m as i64).op(op::I64_AND);
            f.local_get(t64(0)).i64_const(m as i64).op(op::I64_AND).i64_const(s).op(op::I64_SHL);
            f.op(op::I64_OR);
        };
        let swap32 = |f: &mut Func, s: i32, m: u32| {
            f.local_tee(t32(0)).i32_const(s).op(op::I32_SHR_U).i32_const(m as i32).op(op::I32_AND);
            f.local_get(t32(0)).i32_const(m as i32).op(op::I32_AND).i32_const(s).op(op::I32_SHL);
            f.op(op::I32_OR);
        };
        if sf {
            match dop {
                Dp1Op::Rbit => {
                    swap64(f, 1, 0x5555_5555_5555_5555);
                    swap64(f, 2, 0x3333_3333_3333_3333);
                    swap64(f, 4, 0x0f0f_0f0f_0f0f_0f0f);
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                    f.i64_const(32).op(op::I64_ROTL);
                }
                Dp1Op::Rev16 => swap64(f, 8, 0x00ff_00ff_00ff_00ff),
                Dp1Op::Rev32 => {
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                }
                Dp1Op::Rev64 => {
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                    f.i64_const(32).op(op::I64_ROTL);
                }
                Dp1Op::Clz => {
                    f.op(op::I64_CLZ);
                }
                Dp1Op::Cls => {
                    // clz((x ^ (x >> 1)) & (MAX >> 1)) - 1
                    f.local_tee(t64(0)).local_get(t64(0)).i64_const(1).op(op::I64_SHR_U).op(op::I64_XOR);
                    f.i64_const((u64::MAX >> 1) as i64).op(op::I64_AND).op(op::I64_CLZ);
                    f.i64_const(1).op(op::I64_SUB);
                }
            }
        } else {
            f.op(op::I32_WRAP_I64);
            match dop {
                Dp1Op::Rbit => {
                    swap32(f, 1, 0x5555_5555);
                    swap32(f, 2, 0x3333_3333);
                    swap32(f, 4, 0x0f0f_0f0f);
                    swap32(f, 8, 0x00ff_00ff);
                    f.i32_const(16).op(op::I32_ROTL);
                }
                Dp1Op::Rev16 => swap32(f, 8, 0x00ff_00ff),
                Dp1Op::Rev32 | Dp1Op::Rev64 => {
                    swap32(f, 8, 0x00ff_00ff);
                    f.i32_const(16).op(op::I32_ROTL);
                }
                Dp1Op::Clz => {
                    f.op(op::I32_CLZ);
                }
                Dp1Op::Cls => {
                    f.local_tee(t32(0)).local_get(t32(0)).i32_const(1).op(op::I32_SHR_U).op(op::I32_XOR);
                    f.i32_const((u32::MAX >> 1) as i32).op(op::I32_AND).op(op::I32_CLZ);
                    f.i32_const(1).op(op::I32_SUB);
                }
            }
            f.op(op::I64_EXTEND_I32_U);
        }
    }

    /// Dp2 con x in `t64(0)` e y in `t64(1)` (già troncati): risultato
    /// sullo stack.
    fn dp2(&mut self, sf: bool, dop: Dp2Op) {
        let (x, y) = (t64(0), t64(1));
        let f = &mut self.f;
        match dop {
            Dp2Op::Udiv => {
                f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                f.i64_const(0).else_();
                f.local_get(x).local_get(y).op(op::I64_DIV_U).end();
            }
            Dp2Op::Sdiv => {
                if sf {
                    f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                    f.i64_const(0).else_();
                    // MIN / -1 = MIN (wrapping_div)
                    f.local_get(x).i64_const(i64::MIN).op(op::I64_EQ);
                    f.local_get(y).i64_const(-1).op(op::I64_EQ).op(op::I32_AND);
                    f.if_(ValType::I64 as u8).local_get(x).else_();
                    f.local_get(x).local_get(y).op(op::I64_DIV_S).end();
                    f.end();
                } else {
                    f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                    f.i64_const(0).else_();
                    f.local_get(x).op(op::I64_EXTEND32_S).local_get(y).op(op::I64_EXTEND32_S);
                    f.op(op::I64_DIV_S).end();
                }
            }
            Dp2Op::Lslv | Dp2Op::Lsrv | Dp2Op::Asrv | Dp2Op::Rorv => {
                // Il conteggio modulo la dimensione coincide con la maschera
                // del conteggio delle istruzioni WASM.
                if sf {
                    f.local_get(x).local_get(y);
                    f.op(match dop {
                        Dp2Op::Lslv => op::I64_SHL,
                        Dp2Op::Lsrv => op::I64_SHR_U,
                        Dp2Op::Asrv => op::I64_SHR_S,
                        _ => op::I64_ROTR,
                    });
                } else {
                    f.local_get(x).op(op::I32_WRAP_I64).local_get(y).op(op::I32_WRAP_I64);
                    f.op(match dop {
                        Dp2Op::Lslv => op::I32_SHL,
                        Dp2Op::Lsrv => op::I32_SHR_U,
                        Dp2Op::Asrv => op::I32_SHR_S,
                        _ => op::I32_ROTR,
                    });
                    f.op(op::I64_EXTEND_I32_U);
                }
            }
            Dp2Op::Crc32 { bytes, c } => {
                // Come `exec::crc32`: un byte alla volta, un bit alla volta.
                let poly: u32 = if c { 0x82F6_3B78 } else { 0xEDB8_8320 };
                let (crc, k, n) = (t32(0), t32(1), t32(3));
                f.local_get(x).op(op::I32_WRAP_I64).local_set(crc);
                f.i32_const(bytes as i32).local_set(n);
                f.loop_(BLOCK_EMPTY);
                f.local_get(crc).local_get(y).op(op::I32_WRAP_I64).i32_const(0xff).op(op::I32_AND);
                f.op(op::I32_XOR).local_set(crc);
                f.local_get(y).i64_const(8).op(op::I64_SHR_U).local_set(y);
                f.i32_const(8).local_set(k);
                f.loop_(BLOCK_EMPTY);
                f.local_get(crc).i32_const(1).op(op::I32_SHR_U);
                f.i32_const(0).local_get(crc).i32_const(1).op(op::I32_AND).op(op::I32_SUB);
                f.i32_const(poly as i32).op(op::I32_AND).op(op::I32_XOR).local_set(crc);
                f.local_get(k).i32_const(1).op(op::I32_SUB).local_tee(k).br_if(0);
                f.end();
                f.local_get(n).i32_const(1).op(op::I32_SUB).local_tee(n).br_if(0);
                f.end();
                f.local_get(crc).op(op::I64_EXTEND_I32_U);
            }
        }
    }

    /// Dp3 con a in `t64(0)`, m in `t64(1)`, n in `t64(2)`: risultato
    /// sullo stack (non troncato).
    fn dp3(&mut self, dop: Dp3Op) {
        let (a, m, n) = (t64(0), t64(1), t64(2));
        match dop {
            Dp3Op::Madd | Dp3Op::Msub => {
                let f = &mut self.f;
                f.local_get(a).local_get(n).local_get(m).op(op::I64_MUL);
                f.op(if dop == Dp3Op::Madd { op::I64_ADD } else { op::I64_SUB });
            }
            Dp3Op::Smaddl | Dp3Op::Smsubl | Dp3Op::Umaddl | Dp3Op::Umsubl => {
                let signed = matches!(dop, Dp3Op::Smaddl | Dp3Op::Smsubl);
                let f = &mut self.f;
                f.local_get(a);
                for r in [n, m] {
                    f.local_get(r);
                    if signed {
                        f.op(op::I64_EXTEND32_S);
                    } else {
                        f.i64_const(0xffff_ffff).op(op::I64_AND);
                    }
                }
                f.op(op::I64_MUL);
                f.op(if matches!(dop, Dp3Op::Smaddl | Dp3Op::Umaddl) { op::I64_ADD } else { op::I64_SUB });
            }
            Dp3Op::Umulh => self.umulh(n, m),
            Dp3Op::Smulh => {
                // smulh = umulh - (n < 0 ? m : 0) - (m < 0 ? n : 0)
                self.umulh(n, m);
                let f = &mut self.f;
                f.local_get(m).local_get(n).i64_const(63).op(op::I64_SHR_S).op(op::I64_AND).op(op::I64_SUB);
                f.local_get(n).local_get(m).i64_const(63).op(op::I64_SHR_S).op(op::I64_AND).op(op::I64_SUB);
            }
        }
    }

    /// Parte alta del prodotto senza segno a 128 bit di `a` e `b`, sullo
    /// stack. Usa i temporanei 3..=8.
    fn umulh(&mut self, a: u32, b: u32) {
        let (a0, a1, b0, b1, mid, p) = (t64(3), t64(4), t64(5), t64(6), t64(7), t64(8));
        let f = &mut self.f;
        let lo = |f: &mut Func, v: u32, d: u32| {
            f.local_get(v).i64_const(0xffff_ffff).op(op::I64_AND).local_set(d);
        };
        let hi = |f: &mut Func, v: u32, d: u32| {
            f.local_get(v).i64_const(32).op(op::I64_SHR_U).local_set(d);
        };
        lo(f, a, a0);
        hi(f, a, a1);
        lo(f, b, b0);
        hi(f, b, b1);
        // mid = (a0*b0 >> 32) + lo(a0*b1) + lo(a1*b0)
        f.local_get(a0).local_get(b0).op(op::I64_MUL).i64_const(32).op(op::I64_SHR_U);
        f.local_get(a0).local_get(b1).op(op::I64_MUL).local_tee(p).i64_const(0xffff_ffff).op(op::I64_AND);
        f.op(op::I64_ADD);
        f.local_get(a1).local_get(b0).op(op::I64_MUL).local_tee(mid).i64_const(0xffff_ffff).op(op::I64_AND);
        f.op(op::I64_ADD);
        // stack: mid ; p = a0*b1, mid(local) = a1*b0
        // hi = a1*b1 + (p >> 32) + (a1*b0 >> 32) + (mid >> 32)
        f.i64_const(32).op(op::I64_SHR_U);
        f.local_get(a1).local_get(b1).op(op::I64_MUL).op(op::I64_ADD);
        f.local_get(p).i64_const(32).op(op::I64_SHR_U).op(op::I64_ADD);
        f.local_get(mid).i64_const(32).op(op::I64_SHR_U).op(op::I64_ADD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_cpu::decode;

    fn validate(bytes: &[u8]) {
        wasmparser::Validator::new().validate_all(bytes).expect("modulo WASM non valido");
    }

    /// Ogni istruzione traducibile produce un modulo valido. Le codifiche
    /// coprono tutte le varianti del decoder per le classi tradotte.
    #[test]
    fn every_translatable_encoding_validates() {
        // Scorre codifiche pseudo-casuali: per ognuna traducibile, un blocco
        // con quella sola istruzione (più una di chiusura) deve validare.
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut count = 0;
        let mut blocks = Vec::new();
        while count < 4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = seed as u32;
            let insn = decode(w);
            if kind(&insn) == Kind::Unsupported {
                continue;
            }
            count += 1;
            blocks.push(Region::linear(0x40_0000 + 4 * count, vec![w], None));
            if blocks.len() == 64 {
                let mem = MemoryImport { min: 1, shared_max: None };
                validate(&module(&blocks, mem));
                // Gli stessi in modalità sistema, nella tabella.
                for i in 0..4 {
                    let sys = SysTarget {
                        el: (i & 1) as u8,
                        tbi0: i & 1 != 0,
                        tbi1: i & 2 != 0,
                        spsel: i != 2,
                        fp: i != 3,
                    };
                    let sb: Vec<Region> = blocks
                        .iter()
                        .filter(|b| kind_in(&b.bbs[0].insns[0], Some(sys)) != Kind::Unsupported)
                        .map(|b| Region { sys: Some(sys), ..b.clone() })
                        .collect();
                    validate(&module(&sb, mem));
                }
                blocks.clear();
            }
        }
    }

    /// Le istruzioni SIMD/FP (in linea o con `env.simd`, ADR 0026): ogni
    /// codifica valida produce una regione valida, da sola.
    #[test]
    fn simd_encodings_validate() {
        let mut seed = 0x0bad_cafe_1234_5678u64;
        let mut count = 0;
        let mem = MemoryImport { min: 1, shared_max: None };
        while count < 20000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // Classi SIMD/FP: bit 27:25 = x111.
            let w = (seed as u32 & !(7 << 25)) | 7 << 25;
            let insn = decode(w);
            if !matches!(insn, Insn::Simd(_)) || kind(&insn) == Kind::Unsupported {
                continue;
            }
            count += 1;
            let r = Region::linear(0x40_0000, vec![w], None);
            let bytes = module(std::slice::from_ref(&r), mem);
            if let Err(e) = wasmparser::Validator::new().validate_all(&bytes) {
                panic!("{w:#010x} {insn:?}: {e}");
            }
        }
    }

    #[test]
    fn max_steps_excludes_final_svc() {
        // nop; svc #0 (tools/a64asm.sh)
        let b = Region::linear(0, vec![0xd503201f, 0xd4000001], None);
        assert_eq!(b.max_steps(), 1);
        let b = Region::linear(0, vec![0xd503201f, 0xd503201f], None);
        assert_eq!(b.max_steps(), 2);
    }

    #[test]
    fn dispatcher_validates() {
        validate(&dispatcher(MemoryImport { min: 1, shared_max: None }));
        validate(&dispatcher(MemoryImport { min: 17, shared_max: Some(16384) }));
    }

    #[test]
    fn branch_addr_come_la_cpu() {
        let t = 0x5a00_0000_0040_1000u64;
        let n = 0x5a80_0000_0040_1000u64;
        let s = |tbi0, tbi1| SysTarget { el: 0, tbi0, tbi1, spsel: false, fp: true };
        assert_eq!(s(false, false).branch_addr(t), t);
        assert_eq!(s(true, false).branch_addr(t), 0x0000_0000_0040_1000);
        assert_eq!(s(true, false).branch_addr(n), n);
        assert_eq!(s(false, true).branch_addr(n), 0xff80_0000_0040_1000);
        assert_eq!(s(true, true).branch_addr(t), 0x0000_0000_0040_1000);
    }

    /// In modalità sistema WFI, LDTR/STTR e le manutenzioni delle cache a
    /// EL0 restano all'interprete.
    #[test]
    fn istruzioni_solo_interprete_in_modalita_sistema() {
        let el0 = Some(SysTarget { el: 0, tbi0: false, tbi1: false, spsel: false, fp: true });
        let el1 = Some(SysTarget { el: 1, tbi0: false, tbi1: false, spsel: true, fp: true });
        assert_eq!(kind_in(&Insn::Wfi, None), Kind::Linear);
        assert_eq!(kind_in(&Insn::Wfi, el1), Kind::Unsupported);
        assert_eq!(kind_in(&Insn::CacheMaint, el1), Kind::Linear);
        assert_eq!(kind_in(&Insn::CacheMaint, el0), Kind::Unsupported);
        let ldtr = decode(0xf8400820); // ldtr x0, [x1]
        assert!(matches!(ldtr, Insn::LdSt { unpriv: true, .. }), "{ldtr:?}");
        assert_eq!(kind_in(&ldtr, None), Kind::Linear);
        assert_eq!(kind_in(&ldtr, el1), Kind::Unsupported);
    }

    /// Dimensione del codice generato (ADR 0024): la compilazione pesa in
    /// V8 in proporzione ai byte. Istruzioni tipiche del kernel a EL1, una
    /// regione ciascuna; il limite ferma le regressioni.
    #[test]
    fn codice_compatto() {
        let words = [
            0xf9400420u32, // ldr x0, [x1, #8]
            0xf9000420,    // str x0, [x1, #8]
            0xa9410820,    // ldp x0, x2, [x1, #16]
            0xa9bf0be0,    // stp x0, x2, [sp, #-16]!
            0xeb02001f,    // cmp x0, x2
            0x54000201,    // b.ne .+64
            0x91000420,    // add x0, x1, #1
            0xaa0403e3,    // mov x3, x4
            0xb4000200,    // cbz x0, .+64
            0x38626820,    // ldrb w0, [x1, x2]
            0x6b020020,    // subs w0, w1, w2
            0x9a820020,    // csel x0, x1, x2, eq
            0xd65f03c0,    // ret
            0x94000020,    // bl .+128
        ];
        let sys = Some(SysTarget { el: 1, tbi0: false, tbi1: true, spsel: true, fp: true });
        let pc = 0xffff_8000_1234_5000u64;
        let r = Region::linear(pc, words.to_vec(), sys);
        let m = module(std::slice::from_ref(&r), MemoryImport { min: 1, shared_max: None });
        validate(&m);
        let body: usize = r.bbs.len();
        let per = function(&r).code.len() / words.len();
        // Prima delle regioni (ADR 0012-0013) erano circa 130.
        assert!(per <= 80, "{per} byte per istruzione ({body} blocchi base)");
    }

    /// Regioni: un ciclo resta nella funzione, i salti fuori pagina e i
    /// ritorni escono, ogni blocco base con passi è un ingresso.
    #[test]
    fn regione_con_ciclo() {
        // mov x0, #10; l: subs x0, x0, #1; b.ne l; ret (tools/a64asm.sh)
        let words = [0xd2800140u32, 0xf1000400, 0x54ffffe1, 0xd65f03c0];
        let (r, sig) =
            discover(0x1000, None, MAX_REGION, |a| words.get(((a - 0x1000) / 4) as usize).copied()).unwrap();
        let starts: Vec<u64> = r.bbs.iter().map(|b| b.pc).collect();
        assert_eq!(starts, [0x1000, 0x1004, 0x100c]);
        assert_eq!(
            r.entries().iter().map(|e| (e.0, e.2)).collect::<Vec<_>>(),
            [(0x1000, 1), (0x1004, 2), (0x100c, 1)]
        );
        assert_eq!(r.max_steps(), 1);
        assert_eq!(sig.len(), 3 + 4, "posizione e lunghezza di ogni blocco, poi le parole");
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
        // Un blocco che salta a sé (b .) ha il suo ciclo.
        let (r, _) = discover(0x2000, None, MAX_REGION, |a| (a == 0x2000).then_some(0x14000000)).unwrap();
        assert_eq!(r.bbs.len(), 1);
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
        // SVC in testa: niente regione.
        assert!(discover(0x3000, None, MAX_REGION, |_| Some(0xd4000001)).is_none());
    }

    #[test]
    fn ciclo_valida() {
        let words = [0xd2800140u32, 0xf1000400, 0x54ffffe1, 0xd65f03c0];
        let (r, _) =
            discover(0x1000, None, MAX_REGION, |a| words.get(((a - 0x1000) / 4) as usize).copied()).unwrap();
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
    }
}
