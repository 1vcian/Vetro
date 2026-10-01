//! Translator: from a region of decoded instructions (the base blocks of
//! a page, ADR 0024) to a WASM function with the ABI of docs/specs/jit.md,
//! plus the runtime module ([`runtime`]) and the dispatcher ([`dispatcher`]).
//!
//! The semantics of every translated instruction are those of
//! `vetro_cpu::exec` (same flags, same 32-bit truncations, same
//! XZR/SP cases); parity is checked with the tests of `vetro-jit-native` and of
//! `tests/diff`.
//!
//! Inside the region the registers live in local variables: those read or
//! written are loaded at the start and written back to `JitState` in the
//! tail shared by all exits. A fault exit (after a failed
//! access) leaves the registers of the preceding instructions, and `pc` and `steps`
//! of the instruction that failed: the state is the one the interpreter
//! would have before executing it. The NZCV flags are lazy: operands and kind
//! of the last instruction that writes them, computed only if needed.
//!
//! The code is compact (in V8 compilation costs in proportion to the
//! bytes): the slow paths and the pair, Q and unaligned accesses live
//! in the runtime, compiled once; the software TLB fast path
//! for aligned accesses stays in the region.

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
use vetro_cpu::sysreg::EnvReg;

mod crypto;
mod fp;
mod vec;
mod vec2;

/// PSTATE.{D,A,I,F} in bits 9:6 (like `SysState::daif`).
const DAIF_ALL: u32 = 0x3c0;

/// Inline TLB fast path in the regions (experiment).
const INLINE_TLB: bool = true;

/// Maximum instructions per block (ADR 0012).
pub const MAX_BLOCK: usize = 64;

/// `size` of `st` for DC ZVA: zeroes the 64 (aligned) bytes at the address,
/// with the rules of `zero_block` (alignment fault on Device memory).
pub const ZVA_BYTES: u32 = 64;

/// How an instruction behaves for the translator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Translated, the block continues. Conditional branches (B.cond,
    /// CBZ/CBNZ, TBZ/TBNZ) are of this kind: when taken they leave the block
    /// ("side exit"), otherwise the block goes on.
    Linear,
    /// Translated, and closes the block (unconditional branch).
    Branch,
    /// Closes the block with the `SVC` code, without being executed:
    /// the interpreter executes it.
    Svc,
    /// Not translated: the block ends before it and the interpreter executes it.
    Unsupported,
}

/// Classification of an instruction.
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
        // Exclusives with the monitor in JitState (also in user mode, ADR
        // 0026).
        | Insn::Exclusive { .. }
        | Insn::BCond { .. }
        | Insn::Cbz { .. }
        | Insn::Tbz { .. } => Linear,
        Insn::Dp2 { .. } => Linear,
        Insn::Mrs { reg: SysReg::Nzcv, .. } | Insn::Msr { reg: SysReg::Nzcv, .. } => Linear,
        Insn::LdSt { .. } => Linear,
        // SIMD (ADR 0024): loads/stores of single and paired V registers,
        // DUP/INS/UMOV/SMOV, immediate MOVI/MVNI/ORR/BIC.
        Insn::Simd(SimdInsn::Mem(VecMemInsn::Reg { .. } | VecMemInsn::Pair { .. })) => Linear,
        // LD1/ST1 of one or more integer registers, LD1R, LD1/ST1 of one lane,
        // LD2..LD4/ST2..ST4 of multiple structures (ADR 0026); the interleaved
        // single structures (LD2 of one lane, LD2R...) no.
        Insn::Simd(SimdInsn::Mem(VecMemInsn::Multi { .. } | VecMemInsn::Single { selem: 1, .. })) => Linear,
        // All SIMD/FP instructions without memory (ADR 0026): those
        // without an inline form are executed by the interpreter from the region
        // (`env.simd`, [`crate::helper`]).
        Insn::Simd(SimdInsn::Int(_) | SimdInsn::Fp(_) | SimdInsn::Crypto(_)) => Linear,
        Insn::B { .. } | Insn::BranchReg { .. } => Branch,
        Insn::Svc { .. } => Svc,
        _ => Unsupported,
    }
}

/// Parameters of a system-mode block: the exception level and
/// the Top Byte Ignore of the two halves of the virtual space (TCR_EL1.TBI0 and
/// TBI1), which decides the branch address (`AArch64.BranchAddr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SysTarget {
    pub el: u8,
    pub tbi0: bool,
    pub tbi1: bool,
    /// PSTATE.SP (at EL1: SP_EL0 is read with MRS only if it is 1).
    pub spsel: bool,
    /// FP/SIMD instructions allowed at this EL (CPACR_EL1.FPEN): without it, only
    /// the interpreter handles them (trap).
    pub fp: bool,
    /// CNTKCTL_EL1.EL0PCTEN (bit 0) and EL0VCTEN (bit 1): at EL0 MRS of
    /// CNTPCT/CNTVCT is translated only if allowed (ADR 0026).
    pub cntk: u8,
}

/// Where a translated MRS reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MrsSrc {
    /// CNTPCT (`virt` false) or CNTVCT from the instruction count (ADR 0026).
    Counter {
        virt: bool,
    },
    /// Field of `JitState`.
    State(u32),
    /// 32-bit field of `JitState`.
    State32(u32),
    Const(u64),
}

/// MRS that a system-mode block executes by itself: the registers that
/// change only with translated MSRs (or between one run and the next), with the
/// permissions of `sysreg_access` for the block's level.
fn sys_mrs(reg: SysReg, s: SysTarget) -> Option<MrsSrc> {
    let el1 = s.el == 1;
    Some(match reg {
        SysReg::Env(EnvReg::CntpctEl0) if el1 || s.cntk & 1 != 0 => MrsSrc::Counter { virt: false },
        SysReg::Env(EnvReg::CntvctEl0) if el1 || s.cntk & 2 != 0 => MrsSrc::Counter { virt: true },
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
        SysReg::Ttbr0El1 if el1 => MrsSrc::State(off::TTBR0),
        SysReg::Ttbr1El1 if el1 => MrsSrc::State(off::TTBR1),
        SysReg::ContextidrEl1 if el1 => MrsSrc::State(off::CONTEXTIDR),
        SysReg::MidrEl1 if el1 => MrsSrc::Const(vetro_cpu::sys::id::MIDR_EL1),
        // FPCR/FPSR: the FP trap (CPACR_EL1.FPEN) is the region parameter.
        SysReg::Fpcr if s.fp => MrsSrc::State32(off::FPCR),
        SysReg::Fpsr if s.fp => MrsSrc::State32(off::FPSR),
        _ => return None,
    })
}

/// MSR that a system-mode block executes by itself: field of
/// `JitState` to write.
fn sys_msr(reg: SysReg, s: SysTarget) -> Option<u32> {
    let el1 = s.el == 1;
    Some(match reg {
        SysReg::TpidrEl0 => off::TPIDR_EL0,
        SysReg::TpidrroEl0 if el1 => off::TPIDRRO_EL0,
        SysReg::TpidrEl1 if el1 => off::TPIDR_EL1,
        SysReg::SpEl0 if el1 && s.spsel => off::SP_EL0,
        SysReg::ElrEl1 if el1 => off::ELR_EL1,
        SysReg::SpsrEl1 if el1 => off::SPSR_EL1,
        // MSR DAIF: separately (it may unmask interrupts).
        SysReg::Daif if el1 => off::DAIF,
        // TTBRs: separately (they change the regime: YIELD after them).
        SysReg::Ttbr0El1 if el1 => off::TTBR0,
        SysReg::Ttbr1El1 if el1 => off::TTBR1,
        SysReg::ContextidrEl1 if el1 => off::CONTEXTIDR,
        // FPSR: 32 bits with its mask, separately.
        SysReg::Fpsr if s.fp => off::FPSR,
        _ => return None,
    })
}

impl SysTarget {
    /// `AArch64.BranchAddr` like `Cpu::branch_addr`: with TBI active for the
    /// half of `t` the tag is removed by extending bit 55.
    pub fn branch_addr(&self, t: u64) -> u64 {
        let tbi = if t >> 55 & 1 != 0 { self.tbi1 } else { self.tbi0 };
        if tbi { ((t << 8) as i64 >> 8) as u64 } else { t }
    }
}

/// Classification of an instruction in user mode (`sys = None`) or
/// system mode. In system mode WFI (waiting for
/// interrupts), LDTR/STTR (EL0 permissions) and cache maintenance
/// at EL0 (SCTLR_EL1.UCI) also stay with the interpreter; in addition the exclusives (the
/// monitor is in `JitState`), DC ZVA and some MRS/MSR ([`sys_mrs`],
/// [`sys_msr`]) are translated.
pub fn kind_in(insn: &Insn, sys: Option<SysTarget>) -> Kind {
    if let Some(s) = sys {
        match *insn {
            Insn::Wfi => return Kind::Unsupported,
            // CPACR_EL1.FPEN trap: to the interpreter.
            Insn::Simd(_) if !s.fp => return Kind::Unsupported,
            Insn::CacheMaint if s.el == 0 => return Kind::Unsupported,
            Insn::DcZva { .. } => return Kind::Linear,
            // DAIFSet/DAIFClr at EL1 (DAIFClr exits with YIELD).
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

/// A base block: consecutive (already decoded) instructions from `pc`.
/// It ends with a branch (conditional too), an SVC, or before
/// an instruction that is not translated, the start of another block of the
/// region, the end of the page or after [`MAX_BLOCK`] instructions.
#[derive(Clone, Debug)]
pub struct Bb {
    pub pc: u64,
    pub insns: Vec<Insn>,
    /// The instruction words (for `env.simd`).
    pub words: Vec<u32>,
}

impl Bb {
    /// Instructions executed at most by one run of the block (the possible
    /// final SVC is not executed).
    pub fn max_steps(&self) -> u64 {
        let n = self.insns.len();
        match self.insns.last() {
            Some(i) if kind(i) == Kind::Svc => (n - 1) as u64,
            _ => n as u64,
        }
    }
}

/// A region to translate into a function (ADR 0024): the base blocks of
/// a page reachable from entry `pc` with direct branches (loops
/// too), with the system-mode parameters. Branches between blocks
/// of the region stay inside the function; every other branch is an exit.
#[derive(Clone, Debug)]
pub struct Region {
    pub pc: u64,
    /// In address order; one starts at `pc`.
    pub bbs: Vec<Bb>,
    pub sys: Option<SysTarget>,
}

/// Maximum instructions of a region.
pub const MAX_REGION: usize = 64;

/// Base blocks of a region that can act as its entry.
pub const MAX_ENTRIES: usize = 64;

/// Conditional branches (B.cond, CBZ/CBNZ, TBZ/TBNZ): they close a base block.
pub fn is_cond_branch(insn: &Insn) -> bool {
    matches!(insn, Insn::BCond { .. } | Insn::Cbz { .. } | Insn::Tbz { .. })
}

/// Target of a direct branch (B, BL, conditional branches) at `pc`,
/// with `AArch64.BranchAddr` in system mode.
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
    /// A linear sequence of instructions from `pc`, split into base blocks
    /// after every branch and SVC (for the tests).
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

    /// The entry block.
    pub fn entry(&self) -> &Bb {
        self.bbs.iter().find(|b| b.pc == self.pc).expect("entry block")
    }

    /// Maximum steps of the entry block: a run that enters it executes
    /// at least one if the limit allows it (every other block checks
    /// the step limit by itself).
    pub fn max_steps(&self) -> u64 {
        self.entry().max_steps()
    }

    /// Entries of the region: (address, base block index, maximum
    /// steps of the block) for every base block that executes at least
    /// one instruction and has an index lower than [`MAX_ENTRIES`] (the jump
    /// cache keeps the index in 6 bits). Whoever calls the region writes
    /// the index into `JitState::entry`.
    pub fn entries(&self) -> Vec<(u64, u32, u64)> {
        self.bbs
            .iter()
            .enumerate()
            .filter(|(i, b)| *i < MAX_ENTRIES && b.max_steps() > 0)
            .map(|(i, b)| (b.pc, i as u32, b.max_steps()))
            .collect()
    }

    /// Index of the base entry block.
    pub fn entry_index(&self) -> u32 {
        self.bbs.iter().position(|b| b.pc == self.pc).expect("entry block") as u32
    }

    /// Instructions of the region.
    pub fn len(&self) -> usize {
        self.bbs.iter().map(|b| b.insns.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.bbs.is_empty()
    }
}

/// Discovers the region that starts at `pc`: from the entry it follows the code in
/// sequence and the direct branches (taken and not taken) that stay within the page
/// of `pc`, up to `max` instructions. `fetch(a)` gives the word at address
/// `a` (of the same page), or `None` if it cannot be read.
///
/// Returns the region and its signature (position and length of every
/// block, then the words: the same code at the same address has the
/// same signature), or `None` if the first instruction is not translated or is an
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
    // Block starts from the branches; the others (at most MAX_REGION / MAX_BLOCK)
    // are added by splitting the long blocks: all stay below
    // MAX_ENTRIES and every base block can act as an entry.
    const MAX_LEADERS: usize = MAX_ENTRIES - MAX_REGION / MAX_BLOCK;
    let mut decoded: BTreeMap<u64, (u32, Insn)> = BTreeMap::new();
    let mut leaders: BTreeSet<u64> = BTreeSet::new();
    let mut work = vec![pc];
    leaders.insert(pc);
    while let Some(s) = work.pop() {
        let mut a = s;
        while in_page(a) && decoded.len() < max {
            if a != s && decoded.contains_key(&a) {
                // Code already seen: a start (if there is room; otherwise the
                // base block repeats it).
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
                    let t = direct_target(&insn, a, sys).expect("direct branch");
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
    // Base blocks: from every start, up to a branch, another start or
    // a hole; at most MAX_BLOCK instructions (then a new start).
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

/// Local variables: 0 = pointer to `JitState`, 1..=31 x0..x30, 32 SP,
/// 33 NZCV (i32), then temporaries.
const L_STATE: u32 = 0;
const L_NZCV: u32 = 33;
const L_T64: u32 = 34;
const N_T64: u32 = 18;
/// `steps` at the start of the current base block.
const L_STEPS: u32 = L_T64 + 10;
/// `limit` of `JitState` (regions with several blocks).
const L_LIMIT: u32 = L_T64 + 13;
/// Start of the region's page: base of the `pc`s passed to the slow
/// paths and to the exits.
const L_PC0: u32 = L_T64 + 14;
/// Lazy flags (ADR 0024): operands and result of the last instruction that
/// writes NZCV, and its kind in `L_FK` (0 = NZCV already in `L_NZCV`).
const L_FA: u32 = L_T64 + 15;
const L_FB: u32 = L_T64 + 16;
const L_FR: u32 = L_T64 + 17;
/// Current exit: new `pc`, steps done and code (for the shared tail).
const L_EXIT_PC: u32 = L_T64 + 11;
const L_EXIT_DONE: u32 = L_T64 + 12;
const L_T32: u32 = L_T64 + N_T64;
const N_T32: u32 = 7;
const L_EXIT_CODE: u32 = L_T32 + 4;
/// Next base block (index) for the region's `br_table`.
const L_NEXT: u32 = L_T32 + 5;
const L_FK: u32 = L_T32 + 6;
/// v128 temporaries (inline SIMD, ADR 0026).
const L_V0: u32 = L_T32 + N_T32;
const N_V128: u32 = 6;

/// Internal exit code: STOP after the current instruction, whose
/// store has already saved `pc` and `steps` (the tail moves them to the next
/// instruction and exits with `STOP`).
const EXIT_STOP_SAVED: u32 = 0x10;
const _: () = assert!(EXIT_STOP_SAVED > crate::YIELD, "internal code distinct from the ABI ones");

/// Lazy flag kinds (`L_FK`): addition, subtraction, logical, 64 or 32 bits.
const FK_ADD64: i32 = crate::state::fk::ADD64 as i32;
const FK_SUB64: i32 = crate::state::fk::SUB64 as i32;
const FK_ADD32: i32 = crate::state::fk::ADD32 as i32;
const FK_SUB32: i32 = crate::state::fk::SUB32 as i32;
const FK_LOGIC64: i32 = crate::state::fk::LOGIC64 as i32;
const FK_LOGIC32: i32 = crate::state::fk::LOGIC32 as i32;

/// What the translator knows about NZCV at the current point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fl {
    /// Depends on the path: `L_FK` says whether it is lazy.
    Unknown,
    /// In `L_NZCV` (`L_FK` = 0).
    Materialized,
    /// Lazy, of the given kind (`L_FK` has the same value).
    Lazy(i32),
}

/// Register bits in the read/write masks: 0..=30 x, 31 SP,
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

/// Generates the WASM module with one block per element of `blocks`: the
/// function of block `i` is exported as `b<i>`. In system mode the
/// engine then places them in the dispatcher's table (`Engine::place`).
pub fn module(blocks: &[Region], memory: MemoryImport) -> Vec<u8> {
    module_with(blocks, memory, false)
}

/// Like [`module`]; with `names` every region function is also named
/// `r<el>_<pc>` in the `name` section, so a V8 CPU profile attributes the
/// time to guest code (measurement only: the code is the same).
pub fn module_with(blocks: &[Region], memory: MemoryImport, names: bool) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t_blk = m.ty(&[I32], &[I32]);
    m.import_memory("env", "mem", memory);
    // First the functions (which choose which `rt.fp<k>` to import), then the
    // imports: the runtime functions used, at the indices assigned by the
    // regions in order of first use (ADR 0026 for the fast paths, ADR 0041 for
    // all of them: fewer bytes and less work instantiating each module).
    let mut used = Vec::new();
    let funcs: Vec<Func> = blocks.iter().map(|b| function_with(b, &mut used)).collect();
    for id in used.iter().copied() {
        let (name, p, r) = rt_sig(id);
        let t = m.ty(&p, &r);
        m.import_func("rt", &name, t);
    }
    for (i, f) in funcs.into_iter().enumerate() {
        let idx = m.func(t_blk, f);
        m.export_func(&format!("b{i}"), idx);
        if names {
            let el = blocks[i].sys.map_or(0, |s| s.el);
            m.name_func(idx, &format!("r{el}_{:x}", blocks[i].pc));
        }
    }
    m.encode()
}

/// Functions of the runtime module (ADR 0024), imported by every region
/// module as `rt.<name>` at indices `0..N_RT` (in the same order):
/// the slow paths and the memory accesses outside the regions'
/// code, compiled only once.
const F_SAVE: u32 = 0;
const F_LD_SLOW: u32 = 1;
const F_ST_SLOW: u32 = 2;
const F_NZCV: u32 = 3;
/// System-mode accesses with the software TLB (fast and
/// slow path): per EL (0, 1) and size (1, 2, 4, 8 bytes).
const F_LD_TLB: u32 = 4;
const F_ST_TLB: u32 = 12;
/// Pairs (LDP/STP) with the software TLB: per EL and size (4, 8 bytes).
const F_LDP_TLB: u32 = 20;
const F_STP_TLB: u32 = 24;
/// Pairs without TLB (user mode).
const F_LDP_SLOW: u32 = 28;
const F_STP_SLOW: u32 = 29;
/// End of a region: `pc`, `steps` and exit code in `JitState`.
const F_FINISH: u32 = 30;
/// SIMD/FP registers into `JitState` if they are not there yet (`env.vsync`).
const F_VSYNC: u32 = 31;
/// Q accesses (16 bytes): per EL with the software TLB, and without.
const F_LDQ_TLB: u32 = 32;
const F_LDQ_SLOW: u32 = 34;
const F_STQ_TLB: u32 = 35;
const F_STQ_SLOW: u32 = 37;
/// 8-byte half of a Q aligned to 8 but not to 16: TLB of the unaligned
/// accesses (`area::tlb_u`), then the host (per EL).
const F_LDU: u32 = 38;
const F_STU: u32 = 40;
/// `simd(state, word, x, nzcv) -> value`: `env.simd` (ADR 0026).
const F_SIMD: u32 = 42;
/// Floating-point fast paths (`rt.fp<k>`, [`fp::rt_ops`]).
const F_FP0: u32 = 43;

/// Cryptographic operations (`rt.cr<k>`, [`crypto::OPS`]), after the FP ones.
fn f_cr0() -> u32 {
    F_FP0 + fp::rt_ops().len() as u32
}

/// LDTR/STTR at EL1 (`rt.ldt_<n>`, `rt.stt_<n>`, ADR 0041), after the
/// cryptographic ones.
fn f_unpriv0() -> u32 {
    f_cr0() + crypto::count()
}

fn f_unpriv(write: bool, bytes: u32) -> u32 {
    f_unpriv0() + write as u32 * 4 + bytes.trailing_zeros()
}

/// Runtime functions.
fn n_rt() -> u32 {
    f_unpriv0() + 8
}

/// Bit of `size` for `env.ld`/`env.st`: half of a 16-byte access not
/// aligned to 16. The host treats the half as unaligned (SCTLR_EL1.A,
/// Device memory), as the interpreter treats the whole access.
pub const SIZE_PART_OF_MISALIGNED: u32 = 0x80;

/// Bit of `size` for `env.ld`/`env.st` (system mode): LDTR/STTR at EL1, an
/// access with the permissions of EL0 (alignment and endianness still those
/// of EL1, like `SysMem::read_unpriv`). Never through the software TLB.
pub const SIZE_UNPRIV: u32 = 0x100;

fn f_tlb(base: u32, el: u8, bytes: u32) -> u32 {
    base + el as u32 * 4 + bytes.trailing_zeros()
}

fn f_pair(base: u32, el: u8, bytes: u32) -> u32 {
    base + el as u32 * 2 + (bytes == 8) as u32
}

/// Name and signature of runtime function `id`.
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
        _ if id >= F_FP0 && id < f_cr0() => fp::rt_sig((id - F_FP0) as usize),
        _ if id >= f_cr0() && id < f_unpriv0() => crypto::rt_sig((id - f_cr0()) as usize),
        _ if id >= f_unpriv0() && id < n_rt() => {
            let k = id - f_unpriv0();
            if k < 4 {
                (format!("ldt_{}", 1 << k), vec![I32, I64, I64, I64, I32], vec![I64, I32])
            } else {
                (format!("stt_{}", 1 << (k - 4)), vec![I32, I64, I64, I64, I64, I32], vec![I32])
            }
        }
        _ => unreachable!("unknown runtime function: {id}"),
    }
}

/// The software TLB paths of `rt.ld<el>_<n>`/`rt.st<el>_<n>` (params: state
/// 0, `va`, the value 2 for a store; `e` an i32 scratch local): returns from
/// the function on a hit in the aligned TLB of the EL, or in its unaligned
/// TLB for an access that stays within the page.
fn tlb_fast_paths(f: &mut Func, el: u8, write: bool, bytes: u32, va: u32, e: u32) {
    let tlb = area::tlb(el, write);
    // e = state + ((va >> 12) & 511) * 16; hit if tag == va & (!0xfff | (n - 1))
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
        // Unaligned: TLB of the unaligned accesses, if it does not
        // cross into the next page.
        let tu = area::tlb_u(el, write);
        f.local_get(va).op(op::I32_WRAP_I64).i32_const(bytes as i32 - 1).op(op::I32_AND);
        f.local_get(e).i64_load(tu).local_get(va).i64_const(!0xfff).op(op::I64_AND).op(op::I64_EQ);
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
}

/// The runtime module (ADR 0024): imports `env.mem`, `env.ld` and `env.st`
/// and exports the `rt.*` functions that the region modules import. The
/// engine instantiates it once ([`crate::Engine::runtime`]).
///
/// - `save(state, pc0, steps, packed)`: `pc` = `pc0 + (packed & 0xfff)` and
///   `steps` = `steps + (packed >> 12)` in `JitState` (before an access
///   that may fail: the spec wants them saved during `ld`/`st`);
/// - `ld_slow(state, va, size, pc0, steps, packed) -> (value, fault)`:
///   `save` and `env.ld`; `fault` is `exit_detail` (0 if successful);
/// - `st_slow(state, va, size, value, pc0, steps, packed) -> outcome`:
///   `save` and `env.st`; 0, `FAULT` or `STOP`;
/// - `nzcv(k, a, b, r, old) -> NZCV`: the flags of an instruction of kind
///   `k` (`FK_*`) with operands `a`, `b` and result `r` (truncated to 32 bits
///   for the 32-bit kinds), or `old` if `k` = 0;
/// - `ld<el>_<n>`, `st<el>_<n>`: like `ld_slow`/`st_slow` with the EL's
///   software TLB first (aligned accesses to pages in the TLB);
/// - `ldp<el>_<n>`, `stp<el>_<n>`, `ldp_slow`, `stp_slow`: the pairs, two
///   accesses at `va` and `va + n` (the second is not done if the first fails;
///   a pair load returns nothing if either of the two fails).
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
    // Index in the runtime of function `id` (after the `env.*` imports).
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

    // nzcv(k 0, a 1, b 2, r 3, old 4); local 5: sign bit (i64).
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
    // C << 29 | V << 28 (0 for logical)
    f.local_get(k).i32_const(FK_LOGIC64).op(op::I32_GE_U).if_(I32 as u8).i32_const(0).else_();
    f.local_get(k).i32_const(1).op(op::I32_AND).if_(I32 as u8);
    // addition: C = r < a; V = (!(a ^ b) & (a ^ r)) >> sh
    f.local_get(r).local_get(a).op(op::I64_LT_U).i32_const(29).op(op::I32_SHL);
    f.local_get(a).local_get(b).op(op::I64_XOR).i64_const(-1).op(op::I64_XOR);
    f.local_get(a).local_get(r).op(op::I64_XOR).op(op::I64_AND);
    f.local_get(sh).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
    f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
    f.else_();
    // subtraction: C = a >= b; V = ((a ^ b) & (a ^ r)) >> sh
    f.local_get(a).local_get(b).op(op::I64_GE_U).i32_const(29).op(op::I32_SHL);
    f.local_get(a).local_get(b).op(op::I64_XOR);
    f.local_get(a).local_get(r).op(op::I64_XOR).op(op::I64_AND);
    f.local_get(sh).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
    f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
    f.end();
    f.end();
    f.op(op::I32_OR);
    def(&mut m, F_NZCV, f);

    // ld<el>_<n>(state 0, va 1, pc0 2, steps 3, packed 4) and
    // st<el>_<n>(state 0, va 1, value 2, pc0 3, steps 4, packed 5); the same
    // for ldt_<n>/stt_<n> (LDTR/STTR at EL1, ADR 0041): the EL0 tables if
    // `JitState::utlb` says they belong to the current table bases, the
    // host with `SIZE_UNPRIV` otherwise (defined last, after the
    // cryptographic functions: `late`).
    let mut late = Vec::new();
    for unpriv in [false, true] {
        for write in [false, true] {
            for el in 0..2u8 {
                if unpriv && el == 1 {
                    continue;
                }
                for lg in 0..4u32 {
                    let bytes = 1u32 << lg;
                    let (va, e) = (1, if write { 6 } else { 5 });
                    let mut f = Func { locals: vec![(1, I32)], ..Func::default() };
                    if unpriv {
                        f.local_get(0).i32_load(off::UTLB).if_(BLOCK_EMPTY);
                    }
                    tlb_fast_paths(&mut f, el, write, bytes, va, e);
                    if unpriv {
                        f.end();
                    }
                    let size = bytes as i32 | if unpriv { SIZE_UNPRIV as i32 } else { 0 };
                    if write {
                        f.local_get(0).local_get(va).i32_const(size).local_get(2);
                        f.local_get(3).local_get(4).local_get(5).call(rt(F_ST_SLOW));
                    } else {
                        f.local_get(0).local_get(va).i32_const(size);
                        f.local_get(2).local_get(3).local_get(4).call(rt(F_LD_SLOW));
                    }
                    let id = match (unpriv, write) {
                        (true, _) => f_unpriv(write, bytes),
                        (false, true) => f_tlb(F_ST_TLB, el, bytes),
                        (false, false) => f_tlb(F_LD_TLB, el, bytes),
                    };
                    if unpriv {
                        late.push((id, f));
                    } else {
                        def(&mut m, id, f);
                    }
                }
            }
        }
    }

    // Pairs: parameters state 0, va 1, [size 2,] values (store), pc0,
    // steps, packed. The second access is at `va + n` (`n` = size if present).
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

    // finish(state 0, code 1, pc 2, steps 3) -> code: for FAULT `pc` and
    // `steps` are already saved; for EXIT_STOP_SAVED they are those of the
    // store, and move to the next instruction (code STOP).
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

    // vsync(state): the SIMD/FP registers from the host, once per run.
    let mut f = Func::default();
    f.local_get(0).i32_load(off::V_VALID).op(op::I32_EQZ).if_(BLOCK_EMPTY);
    f.local_get(0).call(vsync).end();
    def(&mut m, F_VSYNC, f);

    // Q accesses (16 bytes) as two of 8, with the rules of the whole access
    // of `SysMem::access`: crossing a page FAULT (the interpreter translates
    // all pages before writing); in system mode, if aligned
    // to 8 but not to 16, the halves go to the host marked as unaligned
    // (`SIZE_PART_OF_MISALIGNED`); otherwise the halves use the TLB (those
    // not aligned to 8 end up at the host anyway, as unaligned).
    // ldq(state 0, va 1, pc0 2, steps 3, packed 4) -> (low, high, fault)
    // stq(state 0, va 1, low 2, high 3, pc0 4, steps 5, packed 6) -> outcome
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

    // ldu<el>(state 0, va 1, pc0 2, steps 3, packed 4) -> (value, fault) and
    // stu<el>(state 0, va 1, value 2, pc0 3, steps 4, packed 5) -> outcome:
    // `va` aligned to 8, half of a Q not aligned to 16.
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

    // simd(state, word, x, nzcv) -> value: `env.simd`.
    let mut f = Func::default();
    f.local_get(0).local_get(1).local_get(2).local_get(3).call(simd);
    def(&mut m, F_SIMD, f);

    // Floating-point fast paths (ADR 0026).
    for k in 0..fp::rt_ops().len() {
        def(&mut m, F_FP0 + k as u32, fp::build(k, simd));
    }
    // Cryptographic extension (ADR 0041).
    for c in crypto::all() {
        def(&mut m, crypto::rt_id(c), crypto::build(c, |o| rt(crypto::rt_id(o))));
    }
    for (id, f) in late {
        def(&mut m, id, f);
    }
    m.encode()
}

/// The system-mode dispatcher, exported as `b0`: starting
/// from `pc` it looks up the block in the jump cache ([`area::JC`]) and calls it
/// from the table, as long as it finds blocks valid for `ctx` that fit within the
/// step limit and end with `NEXT`. A missing entry is requested
/// from the host (`env.resolve`). Returns `NEXT` (entry missing for the host too,
/// or limit) or the exit code of the block.
pub fn dispatcher(memory: MemoryImport) -> Vec<u8> {
    dispatcher_with(memory, false)
}

/// Like [`dispatcher`]; with `count` every region call also increments
/// `JitState::dispatches` (measurements, ADR 0041).
pub fn dispatcher_with(memory: MemoryImport, count: bool) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t = m.ty(&[I32], &[I32]);
    m.import_memory("env", "mem", memory);
    m.import_table("env", "tbl", TABLE_SIZE);
    let resolve = m.import_func("env", "resolve", t);
    // locals: 0 state, 1 pc (i64), 2 entry (i32), 3 w (i32), 4 code (i32),
    // 5 index (i32)
    let (s, pc, e, w, code, i) = (0, 1, 2, 3, 4, 5);
    let mut f = Func { locals: vec![(1, I64), (4, I32)], ..Func::default() };
    f.loop_(BLOCK_EMPTY);
    // i = (pc >> 2) & (JC_ENTRIES - 1); e = s + i * 16
    f.local_get(s).i64_load(off::PC).local_tee(pc);
    f.i64_const(2).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
    f.i32_const((area::JC_ENTRIES - 1) as i32).op(op::I32_AND).local_tee(i).i32_const(4).op(op::I32_SHL);
    f.local_get(s).op(op::I32_ADD).local_set(e);
    let miss = |f: &mut Func| {
        f.local_get(e).i64_load(area::JC).local_get(pc).op(op::I64_NE);
        f.local_get(e).i32_load(area::JC + 8).local_get(s).i32_load(off::CTX).op(op::I32_NE);
        f.op(op::I32_OR);
    };
    // Entry of another pc or of another context: the second way (entry
    // i ^ 1, where the host moves the entry it replaces, ADR 0041), then the
    // host (`env.resolve`), which writes it at i if the block exists;
    // otherwise to the host.
    miss(&mut f);
    f.if_(BLOCK_EMPTY);
    f.local_get(i).i32_const(1).op(op::I32_XOR).i32_const(4).op(op::I32_SHL);
    f.local_get(s).op(op::I32_ADD).local_set(e);
    miss(&mut f);
    f.if_(BLOCK_EMPTY);
    f.local_get(s).call(resolve).op(op::I32_EQZ);
    f.if_(BLOCK_EMPTY).i32_const(crate::NEXT as i32).op(op::RETURN).end();
    f.local_get(i).i32_const(4).op(op::I32_SHL).local_get(s).op(op::I32_ADD).local_set(e);
    f.end();
    f.end();
    // steps + maximum steps of the block > limit: to the host
    f.local_get(e).i32_load(area::JC + 12).local_set(w);
    f.local_get(s).i64_load(off::STEPS);
    f.local_get(w).i32_const(0xff).op(op::I32_AND).op(op::I64_EXTEND_I32_U).op(op::I64_ADD);
    f.local_get(s).i64_load(off::LIMIT).op(op::I64_GT_U);
    f.if_(BLOCK_EMPTY).i32_const(crate::NEXT as i32).op(op::RETURN).end();
    // entry = w >> 26; code = table[(w >> 8) & (TABLE_SIZE - 1)](s)
    if count {
        f.local_get(s)
            .local_get(s)
            .i64_load(off::DISPATCHES)
            .i64_const(1)
            .op(op::I64_ADD)
            .i64_store(off::DISPATCHES);
    }
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

/// WASM function of a region.
///
/// Structure: an outer block from which all exits leave towards the
/// shared tail; inside, with several base blocks, a `loop` with a `br_table`
/// on the index of the next base block (`L_NEXT`). The code of the base
/// blocks follows address order, so falling through to the next one
/// in memory is free; the other internal branches set `L_NEXT` and
/// return to the `loop`. Every base block checks before starting that its
/// steps fit within the limit (`limit`), otherwise it exits with `NEXT` at
/// its start: the instruction count stays exact even in loops.
pub fn function(r: &Region) -> Func {
    function_with(r, &mut Vec::new())
}

/// Like [`function`]; `used` are the `rt.fp<k>` functions (runtime
/// indices) that the module imports after the fixed ones, in order: the
/// region calls them with index `F_FP0 + position` and adds those
/// that are missing.
fn function_with(r: &Region, used: &mut Vec<u32>) -> Func {
    assert!(!r.bbs.is_empty() && r.len() <= MAX_REGION.max(MAX_BLOCK));
    // The `loop` is needed with several base blocks or with a block that jumps to itself.
    let multi = r.bbs.len() > 1
        || r.bbs[0].insns.last().and_then(|i| {
            let at = r.bbs[0].pc.wrapping_add(4 * (r.bbs[0].insns.len() as u64 - 1));
            direct_target(i, at, r.sys)
        }) == Some(r.bbs[0].pc);
    let mut body = Func::default();
    // All exits leave this block towards the shared tail.
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
        fp_used: std::mem::take(used),
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
    // Shared tail: all registers the region writes are written back
    // (those not yet written at the exit point have the entry
    // value), then `pc`, `steps` and the code.
    // For FAULT `pc` and `steps` have already been saved by the slow path (or
    // `exit_fault`).
    let all = t.written;
    t.flush();
    let finish = t.rt_opt(F_FINISH);
    let vsync = t.simd.then(|| t.rt_opt(F_VSYNC));
    *used = std::mem::take(&mut t.fp_used);
    let f = &mut t.f;
    // NEXT (the common case) without calls; the others with `rt.finish`.
    f.local_get(L_EXIT_CODE).if_(ValType::I32 as u8);
    f.local_get(L_STATE).local_get(L_EXIT_CODE).local_get(L_EXIT_PC);
    f.local_get(L_STEPS).local_get(L_EXIT_DONE).op(op::I64_ADD).call(finish);
    f.else_();
    f.local_get(L_STATE).local_get(L_EXIT_PC).i64_store(off::PC);
    f.local_get(L_STATE).local_get(L_STEPS).local_get(L_EXIT_DONE).op(op::I64_ADD).i64_store(off::STEPS);
    f.i32_const(NEXT as i32);
    f.end();
    // Prologue: `steps`, `limit`, the entry block and the registers read or
    // written.
    let load = t.read | all;
    let mut pro = Func::default();
    pro.local_get(L_STATE).i64_load(off::STEPS).local_set(L_STEPS);
    pro.i64_const(t.pc0 as i64).local_set(L_PC0);
    if let Some(vsync) = vsync {
        // SIMD/FP registers in JitState (once per run).
        pro.local_get(L_STATE).call(vsync);
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

/// Translator of a region.
struct Tx {
    f: Func,
    /// Registers read by the region (to load in the prologue).
    read: u64,
    /// Registers written by the region (to write back in the tail).
    written: u64,
    /// Address of the current instruction.
    pc: u64,
    /// Word of the current instruction.
    word: u32,
    /// Index of the current instruction in the base block.
    index: u64,
    /// Start of the page (value of `L_PC0`).
    pc0: u64,
    /// State of the flags at the current point.
    fl: Fl,
    /// SP already checked to be aligned to 16 in the base block, and not changed since
    /// (system mode).
    sp_ok: bool,
    /// System mode: accesses with the software TLB, SP alignment,
    /// TBI on branches.
    sys: Option<SysTarget>,
    /// Start of every base block → index.
    bb_index: std::collections::HashMap<u64, usize>,
    /// Current base block.
    cur: usize,
    /// Depth of the region's `loop` (0 if there is only one base block).
    loop_depth: u32,
    /// The region uses the SIMD/FP registers (`JitState::v`).
    simd: bool,
    /// Inline TLB fast path (otherwise always `rt.*`).
    inline_tlb: bool,
    /// `rt.fp<k>` and `rt.cr<k>` functions imported by the module, besides the
    /// fixed ones.
    fp_used: Vec<u32>,
}

impl Tx {
    /// Index in the module of runtime function `id` (imported in order of
    /// first use).
    fn rt_opt(&mut self, id: u32) -> u32 {
        let k = match self.fp_used.iter().position(|&u| u == id) {
            Some(k) => k,
            None => {
                self.fp_used.push(id);
                self.fp_used.len() - 1
            }
        };
        k as u32
    }

    /// Calls runtime function `id`.
    fn call_rt(&mut self, id: u32) {
        let k = self.rt_opt(id);
        self.f.call(k);
    }
}

/// Where the new `pc` of an exit comes from.
enum PcSrc {
    Const(u64),
    /// Value on top of the stack (i64), consumed.
    Stack,
}

impl Tx {
    /// Code of a base block.
    fn bb(&mut self, bb: &Bb, multi: bool) {
        // At the start of a base block the flags and SP alignment depend
        // on the path.
        self.fl = Fl::Unknown;
        self.sp_ok = false;
        let steps = bb.max_steps();
        if multi && steps > 0 {
            // Steps beyond the limit: exit at the start of the base block.
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
            assert!(k != Kind::Unsupported, "untranslatable instruction in the block: {insn:?}");
            if k == Kind::Svc {
                assert_eq!(i + 1, bb.insns.len(), "SVC not at the end of the block");
                self.exit_const(SVC, self.pc, self.index);
                return;
            }
            self.insn(insn);
            if k == Kind::Branch || is_cond_branch(insn) {
                assert_eq!(i + 1, bb.insns.len(), "branch not at the end of the block");
                return;
            }
        }
        // The base block continues at the next instruction.
        let n = bb.insns.len() as u64;
        self.index = n;
        self.jump(bb.pc.wrapping_add(4 * n), n, true);
    }

    /// Branch to `target` (constant) after `done` instructions of the base block:
    /// inside the region if `target` starts one of its base blocks (with no
    /// code if it is the next one and `fall`), otherwise exit with `NEXT`.
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

    // --- registers ----------------------------------------------------

    /// `xr(r)`: 31 is XZR.
    fn get_x(&mut self, r: u8) {
        if r == 31 {
            self.f.i64_const(0);
        } else {
            self.read |= 1 << r;
            self.f.local_get(1 + r as u32);
        }
    }

    /// `xsp(r)`: 31 is SP.
    fn get_xsp(&mut self, r: u8) {
        self.read |= 1 << r;
        self.f.local_get(1 + r as u32);
    }

    /// `set_x(r, top)`: 31 (XZR) discards.
    fn set_x(&mut self, r: u8) {
        if r == 31 {
            self.f.op(op::DROP);
        } else {
            self.written |= 1 << r;
            self.f.local_set(1 + r as u32);
        }
    }

    /// `set_xsp(r, top)`: 31 is SP.
    fn set_xsp(&mut self, r: u8) {
        self.written |= 1 << r;
        if r == 31 {
            self.sp_ok = false;
        }
        self.f.local_set(1 + r as u32);
    }

    /// NZCV (i32) on the stack, computed if lazy.
    fn get_nzcv(&mut self) {
        self.materialize();
        self.f.local_get(L_NZCV);
    }

    /// NZCV = top of the stack (i32): no longer lazy.
    fn set_nzcv(&mut self) {
        self.read |= 1 << B_NZCV;
        self.written |= 1 << B_NZCV;
        self.f.local_set(L_NZCV);
        self.f.i32_const(0).local_set(L_FK);
        self.fl = Fl::Materialized;
    }

    /// Brings NZCV into `L_NZCV` if it is (or may be) lazy.
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
        self.call_rt(F_NZCV);
        let f = &mut self.f;
        f.local_set(L_NZCV).i32_const(0).local_set(L_FK);
        if !known {
            f.end();
        }
        self.fl = Fl::Materialized;
    }

    /// Lazy flags of kind `k`: operands already in `L_FA`, `L_FB` and result
    /// in `L_FR`.
    fn set_lazy(&mut self, k: i32) {
        self.read |= 1 << B_NZCV;
        self.written |= 1 << B_NZCV;
        self.f.i32_const(k).local_set(L_FK);
        self.fl = Fl::Lazy(k);
    }

    /// Truncates to 32 bits (zero extension) if `!sf`.
    fn trunc(&mut self, sf: bool) {
        if !sf {
            self.f.i64_const(0xffff_ffff).op(op::I64_AND);
        }
    }

    // --- exits --------------------------------------------------------

    /// Writes back into `JitState` the registers written so far.
    fn flush(&mut self) {
        for r in 0..=B_SP {
            if self.written & (1 << r) != 0 {
                self.f.local_get(L_STATE).local_get(1 + r).i64_store(off::X + 8 * r);
            }
        }
        if self.written & (1 << B_NZCV) != 0 {
            // The lazy flags pass to the next region (and to the host) as
            // they are.
            let f = &mut self.f;
            f.local_get(L_STATE).local_get(L_NZCV).i32_store(off::NZCV);
            f.local_get(L_STATE).local_get(L_FK).i32_store(off::FK);
            f.local_get(L_STATE).local_get(L_FA).i64_store(off::FA);
            f.local_get(L_STATE).local_get(L_FB).i64_store(off::FB);
            f.local_get(L_STATE).local_get(L_FR).i64_store(off::FR);
        }
    }

    /// Exit with code `code`, new `pc` and `steps` increased by `done`
    /// (instructions done in the base block): jump to the shared tail (end
    /// of the function), which writes back the registers and `JitState`.
    fn exit(&mut self, code: u32, pc: PcSrc, done: u64) {
        if let PcSrc::Const(v) = pc {
            // Relative to the start of the page: shorter constants.
            let d = v.wrapping_sub(self.pc0) as i64;
            if d.unsigned_abs() < 1 << 20 {
                self.f.local_get(L_PC0).i64_const(d).op(op::I64_ADD);
            } else {
                self.f.i64_const(v as i64);
            }
        }
        self.f.local_set(L_EXIT_PC);
        // The locals start at zero and are written only before exiting:
        // `done` = 0 and `NEXT` (0) are not needed.
        if done != 0 {
            self.f.i64_const(done as i64).local_set(L_EXIT_DONE);
        }
        if code != NEXT {
            self.f.i32_const(code as i32).local_set(L_EXIT_CODE);
        }
        let depth = self.f.depth;
        debug_assert!(depth >= 1, "exit outside the shared block");
        self.f.br(depth - 1);
    }

    fn exit_const(&mut self, code: u32, pc: u64, done: u64) {
        self.exit(code, PcSrc::Const(pc), done);
    }

    /// Arguments `pc0, steps, packed` of the slow paths for the current
    /// instruction.
    fn slow_args(&mut self) {
        let packed = (self.pc & 0xfff) as i32 | (self.index as i32) << 12;
        self.f.local_get(L_PC0).local_get(L_STEPS).i32_const(packed);
    }

    /// Fault exit of the current instruction (registers as before):
    /// saves `pc` and `steps` and lets the interpreter decide.
    fn exit_fault(&mut self) {
        self.f.local_get(L_STATE);
        self.slow_args();
        self.call_rt(F_SAVE);
        self.exit_fault_saved();
    }

    /// Fault exit with `pc` and `steps` already saved (slow paths).
    fn exit_fault_saved(&mut self) {
        self.f.i32_const(FAULT as i32).local_set(L_EXIT_CODE);
        let depth = self.f.depth;
        self.f.br(depth - 1);
    }

    /// Conditional branch to `target` with the condition (i32) on top of the
    /// stack; it always closes the base block: taken it goes to `target`, otherwise
    /// to the next instruction (inside the region or with an exit).
    fn cond_branch(&mut self, target: u64) {
        let target = self.target(target);
        let next = self.pc.wrapping_add(4);
        let done = self.index + 1;
        self.f.if_(BLOCK_EMPTY);
        self.jump(target, done, false);
        self.f.end();
        self.jump(next, done, true);
    }

    /// Closes the block after a branch: `pc` on top of the stack.
    fn exit_branch(&mut self) {
        let done = self.index + 1;
        self.exit(NEXT, PcSrc::Stack, done);
    }

    // --- arithmetic ---------------------------------------------------

    /// `AddWithCarry(x, y, carry)` with x in `t64(0)`, y in `t64(1)`:
    /// result (truncated if `!sf`) in `t64(2)`; if `flags`, NZCV in the
    /// flags variable. `carry`: `Some(c)` constant, `None` = flag C.
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

    /// `add_sub(x, y, sub, setflags, sf)` with x in `t64(0)` and y in
    /// `t64(1)`: result in `t64(2)`.
    /// With `setflags` the flags are lazy (ADR 0024): truncated operands in
    /// `L_FA`, `L_FB`, result in `L_FR`.
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

    /// ADDS/SUBS with (truncated) operands in `L_FA`, `L_FB`: result in
    /// `L_FR` and in `rd` (XZR discards), lazy flags.
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

    /// Flags of AND/BIC with the (truncated) result on top of the stack, which
    /// stays there.
    fn logic_flags(&mut self, sf: bool) {
        self.f.local_tee(L_FR);
        self.set_lazy(if sf { FK_LOGIC64 } else { FK_LOGIC32 });
    }

    /// `shift_reg(top, shift, amount, sf)`: value on top of the stack.
    fn shift_reg(&mut self, shift: Shift, amount: u8, sf: bool) {
        if amount == 0 {
            // Every shift by 0 is the identity (at 32 bits it truncates).
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

    /// `extend_reg(top, extend, shift)` at 64 bits.
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

    /// Leaves on the stack (i32) `ConditionHolds(cond)`. With lazy flags
    /// of known kind the condition is computed from the operands.
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

    /// Condition `c` (without the negation bit) from the bits of `L_NZCV`.
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

    /// Condition `c` (without the negation bit) from the lazy flags of kind
    /// `k`: `L_FA`, `L_FB`, `L_FR` (truncated to 32 bits for the 32-bit kinds).
    fn cond_lazy(&mut self, k: i32, c: u8) {
        let w32 = matches!(k, FK_ADD32 | FK_SUB32 | FK_LOGIC32);
        let f = &mut self.f;
        // Sign bit of L_FR (N).
        let n = |f: &mut Func| {
            if w32 {
                f.local_get(L_FR).op(op::I32_WRAP_I64).i32_const(0).op(op::I32_LT_S);
            } else {
                f.local_get(L_FR).i64_const(0).op(op::I64_LT_S);
            }
        };
        // Signed comparison of L_FA and L_FB.
        let scmp = |f: &mut Func, o32: u8, o64: u8| {
            if w32 {
                f.local_get(L_FA).op(op::I32_WRAP_I64).local_get(L_FB).op(op::I32_WRAP_I64).op(o32);
            } else {
                f.local_get(L_FA).local_get(L_FB).op(o64);
            }
        };
        // V from the formula (addition if `add`).
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
                // Addition: C = r < a.
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

    // --- memory -------------------------------------------------------

    /// Load of `bytes` bytes from the address in `addr`: value zero-extended
    /// on the stack; exits with FAULT if the access fails. In system
    /// mode it goes through the software TLB (`rt.ld<el>_<n>`), otherwise
    /// through the host (`rt.ld_slow`).
    fn ld(&mut self, addr: u32, bytes: u32) {
        if let Some(sys) = self.sys
            && self.inline_tlb
        {
            // Hit in the TLB of aligned accesses: inline; otherwise
            // `rt.ld<el>_<n>` (unaligned TLB, host).
            let tlb = area::tlb(sys.el, false);
            self.tlb_hit(addr, bytes, tlb);
            self.f.if_(ValType::I64 as u8);
            self.f.local_get(t32(2)).i64_load(tlb + 8).local_get(addr).op(op::I64_ADD);
            self.f.op(op::I32_WRAP_I64).i64_load_n(bytes, 0);
            self.f.else_();
            self.f.local_get(L_STATE).local_get(addr);
            self.slow_args();
            self.call_rt(f_tlb(F_LD_TLB, sys.el, bytes));
            self.f.if_(BLOCK_EMPTY);
            self.exit_fault_saved();
            self.f.end();
            self.f.end();
            return;
        }
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.slow_args();
                self.call_rt(f_tlb(F_LD_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32);
                self.slow_args();
                self.call_rt(F_LD_SLOW);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Store of `bytes` bytes of the value in `val` at the address in `addr`
    /// (DC ZVA with `bytes` = [`ZVA_BYTES`], always through the host); if it is a
    /// fault it exits with FAULT. A write to watched code (STOP): with
    /// `stop` = `None` it exits right after the instruction (single store, with
    /// nothing else to do afterwards), otherwise it records it in `t32(stop)` (0 if not)
    /// for [`stop_after`](Self::stop_after).
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
            self.call_rt(f_tlb(F_ST_TLB, sys.el, bytes));
            self.st_result(stop);
            self.f.end();
            return;
        }
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) if bytes != ZVA_BYTES => {
                self.f.local_get(val);
                self.slow_args();
                self.call_rt(f_tlb(F_ST_TLB, sys.el, bytes));
            }
            _ => {
                self.f.i32_const(bytes as i32).local_get(val);
                self.slow_args();
                self.call_rt(F_ST_SLOW);
            }
        }
        self.st_result(stop);
    }

    /// Leaves on the stack (i32) the hit in software TLB `tlb` for an
    /// aligned access of `bytes` bytes at the address in `addr`: the tag
    /// of the entry is the page of `addr` and the low bits below `bytes` are
    /// zero. The entry's address stays in `t32(2)`.
    fn tlb_hit(&mut self, addr: u32, bytes: u32, tlb: u32) {
        let f = &mut self.f;
        f.local_get(addr).i64_const(8).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
        f.i32_const(((area::TLB_ENTRIES - 1) << 4) as i32).op(op::I32_AND);
        f.local_get(L_STATE).op(op::I32_ADD).local_tee(t32(2)).i64_load(tlb);
        f.local_get(addr).i64_const((!0xfffu64 | (bytes as u64 - 1)) as i64).op(op::I64_AND);
        f.op(op::I64_EQ);
    }

    /// Outcome of a store (0, FAULT or STOP) on the stack: fault → exit;
    /// STOP → exit after the instruction (`stop` = `None`) or recorded in
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

    /// Pair of loads of `bytes` bytes (4 or 8) from `addr` and `addr + bytes`:
    /// the two values on the stack (exits with FAULT if one fails).
    fn ld_pair(&mut self, addr: u32, bytes: u32) {
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.slow_args();
                self.call_rt(f_pair(F_LDP_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32);
                self.slow_args();
                self.call_rt(F_LDP_SLOW);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Pair of stores of the values in `v1` and `v2`, like [`st`](Self::st).
    fn st_pair(&mut self, addr: u32, bytes: u32, v1: u32, v2: u32, stop: Option<u32>) {
        self.f.local_get(L_STATE).local_get(addr);
        match self.sys {
            Some(sys) => {
                self.f.local_get(v1).local_get(v2);
                self.slow_args();
                self.call_rt(f_pair(F_STP_TLB, sys.el, bytes));
            }
            None => {
                self.f.i32_const(bytes as i32).local_get(v1).local_get(v2);
                self.slow_args();
                self.call_rt(F_STP_SLOW);
            }
        }
        self.st_result(stop);
    }

    /// System mode, SP base: with SP not aligned to 16 exits with FAULT
    /// and the interpreter decides (SCTLR_EL1.SA/SA0, `CheckSPAlignment`).
    /// Once per base block as long as SP does not change in a way that could
    /// lose the alignment (`sp_ok`).
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

    /// After the writeback of the SP base by `offset` bytes: SP stays aligned
    /// if it was and `offset` is a multiple of 16.
    fn sp_writeback(&mut self, rn: u8, was_ok: bool, offset: i64) {
        if rn == 31 && was_ok && offset % 16 == 0 {
            self.sp_ok = true;
        }
    }

    /// Address of a taken branch with a constant target.
    fn target(&self, t: u64) -> u64 {
        match self.sys {
            Some(s) => s.branch_addr(t),
            None => t,
        }
    }

    /// `AArch64.BranchAddr` on the target in `t64(3)` (system
    /// mode with TBI), which stays in `t64(3)`.
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

    /// After the stores of the current instruction (and the writeback): if one
    /// asked to stop, exits with STOP after the instruction.
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

    /// Offset in `JitState` of the low (`hi` = false) or high half of Vr.
    fn v_off(r: u8, hi: bool) -> u32 {
        off::V + 16 * r as u32 + if hi { 8 } else { 0 }
    }

    /// Half of Vr (i64) on the stack.
    fn get_v(&mut self, r: u8, hi: bool) {
        self.simd = true;
        self.f.local_get(L_STATE).i64_load(Self::v_off(r, hi));
    }

    /// Half of Vr = local `l` (i64).
    fn set_v(&mut self, r: u8, hi: bool, l: u32) {
        self.simd = true;
        self.f.local_get(L_STATE).local_get(l).i64_store(Self::v_off(r, hi));
    }

    /// Half of Vr = constant.
    fn set_v_const(&mut self, r: u8, hi: bool, v: i64) {
        self.simd = true;
        self.f.local_get(L_STATE).i64_const(v).i64_store(Self::v_off(r, hi));
    }

    /// Load of 16 bytes (Q) from `addr`: low and high half on the stack (exits
    /// with FAULT if the access fails, or if it must be redone by the interpreter).
    fn ld_q(&mut self, addr: u32) {
        self.f.local_get(L_STATE).local_get(addr);
        self.slow_args();
        self.call_rt(match self.sys {
            Some(s) => F_LDQ_TLB + s.el as u32,
            None => F_LDQ_SLOW,
        });
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault_saved();
        self.f.end();
    }

    /// Store of 16 bytes (Q) of the halves in `lo` and `hi`, like [`st`](Self::st).
    fn st_q(&mut self, addr: u32, lo: u32, hi: u32, stop: Option<u32>) {
        self.f.local_get(L_STATE).local_get(addr).local_get(lo).local_get(hi);
        self.slow_args();
        self.call_rt(match self.sys {
            Some(s) => F_STQ_TLB + s.el as u32,
            None => F_STQ_SLOW,
        });
        self.st_result(stop);
    }

    /// Load/store of the V registers (`VecMemInsn::Reg` and `Pair`), like
    /// `simd::ldst::exec`: address, accesses, registers written only after
    /// the last access, then the writeback of the base.
    fn vec_mem(&mut self, m: VecMemInsn) {
        match m {
            VecMemInsn::Reg { scale, load, addr, rt, rn } => {
                self.sp_check(rn);
                let was_ok = self.sp_ok;
                // address in t64(4), new base in t64(6)
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
                    // Two Qs: at `addr` and at `addr + 16`.
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
            VecMemInsn::Multi { .. } | VecMemInsn::Single { .. } => self.vec_struct(m),
            other => unreachable!("SIMD load/store not translated: {other:?}"),
        }
    }

    /// Element `index` of `es` bits of Vn on the stack (zero-extended).
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

    /// Vd.<es>[index] = value in `l` (i64), the rest unchanged.
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

    /// Translated integer SIMD instructions (`simd::int::exec`).
    fn vec_int(&mut self, i: IntInsn) {
        match i {
            IntInsn::Copy { op: cop, scalar, q, esize, index, index2, rn, rd } => {
                let es = esize as u32;
                // Replication of `x` (t64(0)) over 64 bits.
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
            other => unreachable!("SIMD instruction not translated: {other:?}"),
        }
    }

    /// SIMD/FP instruction without memory executed by the interpreter from the
    /// region (`rt.simd` → `env.simd`, [`crate::helper`]): passes the
    /// general register read and NZCV, and writes the general register or NZCV
    /// returned.
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
        self.call_rt(F_SIMD);
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

    /// CNTPCT (or CNTVCT if `virt`) of the current instruction on the stack
    /// (i64), like the machine (`Machine::counter`): the instruction count
    /// `s` = `time_base` + steps done, and CNTPCT = s / 8 × 5 + (s mod 8) × 5
    /// / 8 (62.5 MHz out of a nominal 100 MHz); CNTVCT = CNTPCT - CNTVOFF. Without
    /// `time_ok` (the host did not provide the clock) it exits and the
    /// interpreter reads it.
    fn counter(&mut self, virt: bool) {
        self.f.local_get(L_STATE).i32_load(off::TIME_OK).op(op::I32_EQZ).if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
        let s = t64(0);
        let f = &mut self.f;
        f.local_get(L_STATE).i64_load(off::TIME_BASE).local_get(L_STEPS).op(op::I64_ADD);
        if self.index != 0 {
            f.i64_const(self.index as i64).op(op::I64_ADD);
        }
        f.local_tee(s).i64_const(3).op(op::I64_SHR_U).i64_const(5).op(op::I64_MUL);
        f.local_get(s)
            .i64_const(7)
            .op(op::I64_AND)
            .i64_const(5)
            .op(op::I64_MUL)
            .i64_const(3)
            .op(op::I64_SHR_U);
        f.op(op::I64_ADD);
        if virt {
            f.local_get(L_STATE).i64_load(off::CNTVOFF).op(op::I64_SUB);
        }
    }

    /// If the top of the stack (i32) is not zero (interrupts unmasked), exits
    /// with YIELD after the current instruction.
    fn yield_if(&mut self) {
        self.f.if_(BLOCK_EMPTY);
        let (next, done) = (self.pc.wrapping_add(4), self.index + 1);
        self.exit_const(YIELD, next, done);
        self.f.end();
    }

    /// Extends a loaded value of `1 << size` bytes as `op` requires.
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

    // --- instructions -------------------------------------------------

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
            // UBFM/SBFM in the most common forms (LSL, LSR, ASR, UBFX, SBFX,
            // UBFIZ, SBFIZ, extensions): direct shifts and masks.
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
                            // The mask (at most 32 - r bits at 32 bits) also removes
                            // the high bits of Wn.
                            if sh != 0 {
                                f.i64_const(sh).op(op::I64_SHR_U);
                            }
                            f.i64_const(mask).op(op::I64_AND);
                        }
                    }
                    (_, true) => {
                        // Sign extension of bits [s:r] (or [s:0] then to the left).
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
                let s = self.sys.expect("system MRS only in system mode");
                match sys_mrs(reg, s).expect("classified by kind_in") {
                    MrsSrc::State(o) => {
                        self.f.local_get(L_STATE).i64_load(o);
                    }
                    MrsSrc::State32(o) => {
                        self.f.local_get(L_STATE).i32_load(o).op(op::I64_EXTEND_I32_U);
                    }
                    MrsSrc::Const(v) => {
                        self.f.i64_const(v as i64);
                    }
                    MrsSrc::Counter { virt } => self.counter(virt),
                }
                self.set_x(rt);
            }
            Insn::Msr { reg, rt } => {
                let s = self.sys.expect("system MSR only in system mode");
                let o = sys_msr(reg, s).expect("classified by kind_in");
                if reg == SysReg::Daif {
                    // Like `sysreg_write`: DAIF = Xt & DAIF_ALL; if a bit goes
                    // from 1 to 0 it exits with YIELD after the instruction.
                    self.f.local_get(L_STATE).i32_load(off::DAIF).local_set(t32(0));
                    self.get_x(rt);
                    self.f.op(op::I32_WRAP_I64).i32_const(DAIF_ALL as i32).op(op::I32_AND).local_set(t32(1));
                    self.f.local_get(L_STATE).local_get(t32(1)).i32_store(off::DAIF);
                    self.f.local_get(t32(0)).local_get(t32(1)).i32_const(-1).op(op::I32_XOR).op(op::I32_AND);
                    self.yield_if();
                } else if reg == SysReg::Fpsr {
                    // Like `sysreg_write`: FPSR = Xt & FPSR_MASK.
                    self.f.local_get(L_STATE);
                    self.get_x(rt);
                    self.f.op(op::I32_WRAP_I64).i32_const(vetro_cpu::state::FPSR_MASK as i32).op(op::I32_AND);
                    self.f.i32_store(o);
                } else if matches!(reg, SysReg::Ttbr0El1 | SysReg::Ttbr1El1) {
                    // The translation regime changes: the run ends right after
                    // (YIELD), and the host starts the next one in the new
                    // regime (contexts, software TLB).
                    self.f.local_get(L_STATE);
                    self.get_x(rt);
                    self.f.i64_store(o);
                    // `exit_detail` = 3: a regime YIELD, which the host follows
                    // within the run (ADR 0041).
                    self.f
                        .local_get(L_STATE)
                        .i32_const(crate::state::DETAIL_REGIME as i32)
                        .i32_store(off::EXIT_DETAIL);
                    self.f.i32_const(1);
                    self.yield_if();
                } else {
                    self.f.local_get(L_STATE);
                    self.get_x(rt);
                    self.f.i64_store(o);
                }
            }
            Insn::MsrImm { field, imm } => {
                // DAIFSet / DAIFClr at EL1 (kind_in), like `step_system`.
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
                    // DCZID_EL0.DZP (SCTLR_EL1.DZE at 0): trap in the interpreter.
                    self.f.local_get(L_STATE).i64_load(off::DCZID).i64_const(16).op(op::I64_AND);
                    self.f.op(op::I32_WRAP_I64).if_(BLOCK_EMPTY);
                    self.exit_fault();
                    self.f.end();
                }
                self.get_x(rt);
                self.f.i64_const(!63).op(op::I64_AND).local_set(t64(4));
                self.f.i64_const(0).local_set(t64(7));
                // Always through the host: 64 bytes, and alignment fault on
                // Device memory like `zero_block`.
                self.st(t64(4), ZVA_BYTES, t64(7), None);
            }
            Insn::LdSt { size, op: mop, addr, rt, rn, unpriv } => {
                if mop == MemOp::Prefetch {
                    return;
                }
                // LDTR/STTR at EL1: EL0 permissions, always through the host.
                // At EL0 they are ordinary accesses.
                let unpriv = unpriv && self.sys.is_some_and(|s| s.el == 1);
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
                        if unpriv {
                            debug_assert!(!writeback, "LDTR/STTR have no writeback");
                            let f = self.rt_opt(f_unpriv(true, bytes));
                            self.f.local_get(L_STATE).local_get(t64(4)).local_get(t64(7));
                            self.slow_args();
                            self.f.call(f);
                            self.st_result(None);
                        } else if writeback {
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
                        if unpriv {
                            let f = self.rt_opt(f_unpriv(false, bytes));
                            self.f.local_get(L_STATE).local_get(t64(4));
                            self.slow_args();
                            self.f.call(f).if_(BLOCK_EMPTY);
                            self.exit_fault_saved();
                            self.f.end();
                        } else {
                            self.ld(t64(4), bytes);
                        }
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
                // address in t64(4), new base (writeback) in t64(6)
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
            Insn::Simd(SimdInsn::Int(IntInsn::ThreeDiff {
                scalar: false,
                u: false,
                size: 3,
                opcode: 0b1110,
                q,
                rm,
                rn,
                rd,
            })) => self.pmull64(q, rm, rn, rd),
            Insn::Simd(SimdInsn::Crypto(c)) => self.crypto(c),
            Insn::Simd(SimdInsn::Int(i)) if self.vec_int_inline(i) || self.vec_more(i) => {}
            Insn::Simd(SimdInsn::Fp(f)) if self.fp_inline(f) => {}
            Insn::Simd(s) => self.simd_helper(&s),
            other => unreachable!("untranslatable instruction: {other:?}"),
        }
    }

    /// LDXR/LDAXR/STXR/STLXR and the pairs, with the monitor in `JitState`,
    /// like `Cpu::execute`: alignment to the whole access (otherwise
    /// FAULT and the interpreter raises the exception), the load activates the monitor, the
    /// store succeeds if the monitor is for the same address and the same
    /// size and memory still holds the value read; the monitor is
    /// turned off after the store (even a failed one). No state changes before
    /// the last access that can fail.
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
        // Value of `total` bytes in (lo, hi): from `ld` or new.
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
        // New value.
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
        // ok (t32(3)) = monitor for this access and memory unchanged.
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

    /// If the address in `t64(4)` is not aligned to `1 << size`, exits with
    /// FAULT: the interpreter re-executes and raises the alignment exception.
    fn misaligned_fault(&mut self, size: u8) {
        if size == 0 {
            return;
        }
        self.f.local_get(t64(4)).i64_const((1i64 << size) - 1).op(op::I64_AND);
        self.f.op(op::I64_EQZ).op(op::I32_EQZ).if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
    }

    /// Dp1 on the value on top of the stack.
    fn dp1(&mut self, sf: bool, dop: Dp1Op) {
        let f = &mut self.f;
        // swap step: ((v >> s) & m) | ((v & m) << s)
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

    /// Dp2 with x in `t64(0)` and y in `t64(1)` (already truncated): result
    /// on the stack.
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
                // The count modulo the size coincides with the mask
                // of the count of the WASM instructions.
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
                // Like `exec::crc32`: one byte at a time, one bit at a time.
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

    /// Dp3 with a in `t64(0)`, m in `t64(1)`, n in `t64(2)`: result
    /// on the stack (not truncated).
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

    /// High part of the unsigned 128-bit product of `a` and `b`, on the
    /// stack. Uses temporaries 3..=8.
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
        wasmparser::Validator::new().validate_all(bytes).expect("invalid WASM module");
    }

    /// Every translatable instruction produces a valid module. The encodings
    /// cover all decoder variants for the translated classes.
    #[test]
    fn every_translatable_encoding_validates() {
        // Walks pseudo-random encodings: for each translatable one, a block
        // with that single instruction (plus a closing one) must validate.
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
                // The same in system mode, in the table.
                for i in 0..4 {
                    let sys = SysTarget {
                        el: (i & 1) as u8,
                        tbi0: i & 1 != 0,
                        tbi1: i & 2 != 0,
                        spsel: i != 2,
                        fp: i != 3,
                        cntk: i as u8 & 3,
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

    /// The SIMD/FP instructions (inline or with `env.simd`, ADR 0026): every
    /// valid encoding produces a valid region, on its own.
    #[test]
    fn simd_encodings_validate() {
        let mut seed = 0x0bad_cafe_1234_5678u64;
        let mut count = 0;
        let mem = MemoryImport { min: 1, shared_max: None };
        while count < 20000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // SIMD/FP classes: bits 27:25 = x111.
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
    fn branch_addr_like_the_cpu() {
        let t = 0x5a00_0000_0040_1000u64;
        let n = 0x5a80_0000_0040_1000u64;
        let s = |tbi0, tbi1| SysTarget { el: 0, tbi0, tbi1, spsel: false, fp: true, cntk: 0 };
        assert_eq!(s(false, false).branch_addr(t), t);
        assert_eq!(s(true, false).branch_addr(t), 0x0000_0000_0040_1000);
        assert_eq!(s(true, false).branch_addr(n), n);
        assert_eq!(s(false, true).branch_addr(n), 0xff80_0000_0040_1000);
        assert_eq!(s(true, true).branch_addr(t), 0x0000_0000_0040_1000);
    }

    /// In system mode WFI and cache maintenance at EL0 stay with the
    /// interpreter; LDTR/STTR are translated (at EL1 through the host with
    /// the permissions of EL0, `SIZE_UNPRIV`).
    #[test]
    fn interpreter_only_insns_in_system_mode() {
        let el0 = Some(SysTarget { el: 0, tbi0: false, tbi1: false, spsel: false, fp: true, cntk: 0 });
        let el1 = Some(SysTarget { el: 1, tbi0: false, tbi1: false, spsel: true, fp: true, cntk: 0 });
        assert_eq!(kind_in(&Insn::Wfi, None), Kind::Linear);
        assert_eq!(kind_in(&Insn::Wfi, el1), Kind::Unsupported);
        assert_eq!(kind_in(&Insn::CacheMaint, el1), Kind::Linear);
        assert_eq!(kind_in(&Insn::CacheMaint, el0), Kind::Unsupported);
        let ldtr = decode(0xf8400820); // ldtr x0, [x1]
        assert!(matches!(ldtr, Insn::LdSt { unpriv: true, .. }), "{ldtr:?}");
        assert_eq!(kind_in(&ldtr, None), Kind::Linear);
        assert_eq!(kind_in(&ldtr, el1), Kind::Linear);
        assert_eq!(kind_in(&ldtr, el0), Kind::Linear);
    }

    /// Size of the generated code (ADR 0024): compilation in V8 weighs
    /// in proportion to the bytes. Typical kernel instructions at EL1, one
    /// region each; the limit stops regressions.
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
        let sys = Some(SysTarget { el: 1, tbi0: false, tbi1: true, spsel: true, fp: true, cntk: 0 });
        let pc = 0xffff_8000_1234_5000u64;
        let r = Region::linear(pc, words.to_vec(), sys);
        let m = module(std::slice::from_ref(&r), MemoryImport { min: 1, shared_max: None });
        validate(&m);
        let body: usize = r.bbs.len();
        let per = function(&r).code.len() / words.len();
        // Before regions (ADR 0012-0013) it was about 130.
        assert!(per <= 80, "{per} bytes per instruction ({body} base blocks)");
    }

    /// Regions: a loop stays in the function, off-page branches and
    /// returns exit, every base block with steps is an entry.
    #[test]
    fn region_with_loop() {
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
        assert_eq!(sig.len(), 3 + 4, "position and length of every block, then the words");
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
        // A block that jumps to itself (b .) has its own loop.
        let (r, _) = discover(0x2000, None, MAX_REGION, |a| (a == 0x2000).then_some(0x14000000)).unwrap();
        assert_eq!(r.bbs.len(), 1);
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
        // SVC at the head: no region.
        assert!(discover(0x3000, None, MAX_REGION, |_| Some(0xd4000001)).is_none());
    }

    #[test]
    fn loop_validates() {
        let words = [0xd2800140u32, 0xf1000400, 0x54ffffe1, 0xd65f03c0];
        let (r, _) =
            discover(0x1000, None, MAX_REGION, |a| words.get(((a - 0x1000) / 4) as usize).copied()).unwrap();
        validate(&module(&[r], MemoryImport { min: 1, shared_max: None }));
    }
}
