//! `SysJit`: the system-mode JIT (ADR 0012, ADR 0013).
//!
//! # Contract
//! [`SysJit::run`] executes only translated blocks, at most `budget` steps,
//! and stops before anything the interpreter has to do: untranslated
//! instructions, faults (including accesses outside RAM, i.e. MMIO),
//! SVC. Every block executed gives the same result that its steps would
//! have given with `Cpu::step_system`, provided that:
//! - the caller does not ask it to run when the interpreter would take
//!   an interrupt (unmasked IRQ, FIQ or SError), with PSTATE.IL or with
//!   a misaligned PC;
//! - `budget` does not exceed the steps until the next platform event
//!   (timer deadline): inside the blocks nothing changes the interrupt
//!   lines, because no block touches MMIO or the system registers.
//!
//! So the clock (the number of instructions) and the points where
//! interrupts arrive are identical to the interpreter.
//!
//! # Regions (ADR 0024)
//! - A region is looked up by (`pc`, EL, TBI0, TBI1, SPSel, FP) and is valid for
//!   the physical address it was read from: on every entry from the host
//!   `pc` is translated again with the MMU (as a fetch, with the permissions of EL: the
//!   same translation as the interpreter, including the cache of recent
//!   translations) and the region of that physical page is used. Cold code
//!   (below the threshold) is not even translated.
//! - Every base block of a compiled region is also one of its entries: a
//!   `pc` already inside a region does not cause another one to be translated.
//! - MSR DAIF/DAIFClr that unmask interrupts exit with `YIELD`:
//!   `run` returns to the caller, which checks interrupts again.
//! - Every physical page with blocks is watched ([`SysPhys::watch_code`]):
//!   any write (CPU, device DMA, loading) marks it
//!   dirty, and its blocks are discarded before the next run. A
//!   store by a block to a watched page makes it exit right after
//!   (`STOP`).
//!
//! # Chaining
//! The blocks live in a function table shared among the modules; the
//! dispatcher (a generated module, [`translate::dispatcher`]) goes from one
//! block to the next without returning to the host, as long as the jump cache
//! ([`area::JC`]) has an entry for the new `pc` with the current context. The
//! entries are written only by the host, after checking the translation of the
//! fetch; the context (`ctx`) is an epoch that changes with the translation
//! registers, with every TLBI and with every block invalidation, plus the
//! region parameters (EL, TBI, SPSel, FP).
//!
//! # Software TLB
//! The blocks read and write RAM directly when the page is
//! in the software TLB ([`area::tlb`]) of their EL and the access is aligned,
//! or in the TLB of unaligned ones ([`area::tlb_u`], filled only after a
//! successful unaligned access: Normal memory and SCTLR_EL1.A at 0) and
//! the access stays within the page; otherwise they call `ld`/`st`. The host
//! fills the TLB only after a
//! successful access (same permissions, same page), only for RAM pages
//! that the engine can reach ([`Engine::host_address`]) and, for
//! writes, only for pages without blocks. It flushes it with the translation
//! registers and with TLBIs, like the MMU's TLB.
//!
//! The MMU's TLB sees fewer accesses than with the interpreter (no fetches
//! inside the blocks, no accesses from the fast path): the result
//! changes only for a guest that modifies the page tables without
//! TLBI, which the architecture leaves unpredictable and Linux does not do.

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use vetro_cpu::sys::{AccessReq, SysBus, TranslationRegs, cntkctl, cpacr, sctlr};
use vetro_cpu::{Access, Cpu};
use vetro_mmu::{BusError, Inval, Mmu, MmuBus, PhysMemory, tcr};

use crate::engine::{Engine, Host, TABLE_SIZE};
use crate::profile::Profile;
use crate::state::{self, JitState, area, off};
use crate::translate::{self, MAX_REGION, Region, SysTarget, ZVA_BYTES};
use crate::wasm::MemoryImport;
use crate::{FAULT, NEXT, STOP, SVC, YIELD};

/// Physical memory as seen by the system-mode JIT.
pub trait SysPhys: PhysMemory {
    /// Read from RAM only, with no side effects: false if `[pa, pa+len)` is not
    /// all RAM.
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool;
    /// Write to RAM only: `None` if it is not RAM, otherwise
    /// `Some(true)` if it touched a watched page (now dirty).
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool>;
    /// Watches physical page `page` (`pa >> 12`): false if it is not RAM.
    fn watch_code(&mut self, page: u64) -> bool;
    /// True if the page is watched.
    fn is_watched(&self, page: u64) -> bool;
    /// Appends to `out` the watched pages written since the last call
    /// (which are no longer watched).
    fn take_code_dirty(&mut self, out: &mut Vec<u64>);
    /// After [`watch_code`](Self::watch_code) of a page: true if its code can
    /// be translated now. With cores running in parallel (ADR 0042) the others
    /// must first stop writing to the page directly; until then the JIT leaves
    /// it to the interpreter and reads it again later.
    fn watch_ready(&mut self, _page: u64) -> bool {
        true
    }
    /// RAM as a contiguous host block: physical start address,
    /// pointer and length. Used by the blocks' software TLB.
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        None
    }
}

/// Physical memory reduced to RAM (for the host's walks and accesses):
/// everything else is a decode error, which for the JIT means "the
/// interpreter does it".
struct RamOnly<'a>(&'a mut dyn SysPhys);

impl PhysMemory for RamOnly<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.0.ram_read(pa, buf) { Ok(()) } else { Err(BusError::Decode) }
    }
    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        self.0.ram_write(pa, data).map(|_| ()).ok_or(BusError::Decode)
    }
}

/// CPU translation registers (like `Cpu::translation_regs`).
pub fn translation_regs(cpu: &Cpu) -> TranslationRegs {
    let s = &cpu.sys;
    TranslationRegs {
        sctlr: s.sctlr_el1,
        tcr: s.tcr_el1,
        ttbr0: s.ttbr0_el1,
        ttbr1: s.ttbr1_el1,
        mair: s.mair_el1,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SysJitConfig {
    /// Entries (with the interpreter) at the start of a block before
    /// translating it.
    pub hot_threshold: u32,
    /// Blocks to collect before compiling them into a single module: until
    /// then the hot blocks stay with the interpreter. 1 = immediately. The
    /// module is compiled earlier if the waiting blocks are requested
    /// `batch` times in total (a hot loop does not wait for the others).
    pub batch: usize,
    /// How the modules import the memory.
    pub memory: MemoryImport,
    /// Address of `JitState` (and of the area that follows it, [`area::SIZE`]
    /// bytes) in the engine's memory, aligned to 16.
    pub state_addr: u32,
    /// Counts the interpreter's instructions per class
    /// ([`SysJit::profile_step`], [`Profile`]).
    pub profile: bool,
    /// Names every region function `r<el>_<pc>` in the modules' `name`
    /// section, so that a V8 CPU profile attributes time to guest code (ADR
    /// 0041). Measurement only: the code is the same.
    pub names: bool,
}

impl Default for SysJitConfig {
    fn default() -> Self {
        SysJitConfig {
            hot_threshold: 64,
            batch: 16,
            memory: MemoryImport { min: 1, shared_max: None },
            state_addr: 16,
            profile: false,
            names: false,
        }
    }
}

/// Counters, for measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SysJitStats {
    /// Steps executed by the blocks.
    pub jit_steps: u64,
    /// Dispatcher runs (entries from the host).
    pub runs: u64,
    /// Jump cache entries requested by the dispatcher from the host.
    pub resolves: u64,
    /// Calls to [`SysJit::run`].
    pub calls: u64,
    /// Blocks and modules compiled.
    pub blocks: u64,
    pub modules: u64,
    /// Blocks reused (same instructions at the same address) without
    /// recompiling.
    pub reused: u64,
    /// Pages with blocks invalidated.
    pub invalidated_pages: u64,
    /// Exits due to faults (including MMIO accesses) and to SVC.
    pub faults: u64,
    pub svcs: u64,
    /// Exits due to stores to code.
    pub stops: u64,
    /// Exits after unmasking interrupts (`YIELD`).
    pub yields: u64,
    /// New chaining epochs and software TLB flushes.
    pub epochs: u64,
    pub tlb_flushes: u64,
    /// Entries written into the software TLB.
    pub tlb_fills: u64,
    /// Engine resets.
    pub resets: u64,
    /// Accesses that reached the host (`env.ld`/`env.st`: software TLB miss,
    /// fault, MMIO).
    pub host_lds: u64,
    pub host_sts: u64,
    /// Why the epoch changed: translation registers, TLBI, code
    /// invalidation.
    pub epochs_regs: u64,
    pub epochs_tlbi: u64,
    pub epochs_code: u64,
    /// Changes of the table bases (TTBR0/TTBR1 without the ASID) between runs.
    pub base_switches: u64,
    /// TLBIs by VA handled without a new epoch (runs that saw some).
    pub tlbi_partial: u64,
    /// Entries from the host found in the jump cache (no lookup).
    pub jc_probes: u64,
    /// Lookups that reused a remembered fetch translation (ADR 0041).
    pub memo_hits: u64,
    /// MSR TTBR0/TTBR1 in a region followed within the same run (ADR 0041).
    pub regime_switches: u64,
    /// Evictions of unused modules instead of a reset, and modules evicted
    /// (ADR 0041).
    pub evictions: u64,
    pub evicted_modules: u64,
    /// Region calls by the dispatcher, with `SysJitConfig::names` (ADR 0041).
    pub dispatches: u64,
    /// Bytes of WebAssembly compiled (modules and dispatcher).
    pub wasm_bytes: u64,
}

/// Outcome of [`SysJit::run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysRun {
    /// Steps executed by the blocks.
    pub steps: u64,
    /// What to do next.
    pub next: Next,
    /// Of `steps`, those the regions already added to the shared clock
    /// (`Clock::shared`); 0 otherwise.
    pub flushed: u64,
}

/// After a JIT run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Steps exhausted: the JIT can be called again.
    Jit,
    /// The next instruction goes to the interpreter (fault, MMIO access, SVC,
    /// untranslated instruction, block longer than the remaining steps); afterwards
    /// the JIT can be called again.
    One,
    /// Cold code (or waiting for compilation): better to interpret
    /// up to the next jump before calling the JIT again.
    Cold,
}

/// Hash for integer keys (addresses, pages): the lookup happens on every
/// entry from the host.
#[derive(Default)]
struct U64Hasher(u64);

impl Hasher for U64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, v: u64) {
        let h = (self.0 ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<U64Hasher>>;

/// Key of a block: `pc` and the parameters of [`SysTarget`] in one byte.
type Key = (u64, u8);

fn flags(sys: SysTarget) -> u8 {
    sys.el
        | (sys.tbi0 as u8) << 1
        | (sys.tbi1 as u8) << 2
        | (sys.spsel as u8) << 3
        | (sys.fp as u8) << 4
        | (sys.cntk & 3) << 5
}

/// VA[55:30]: the 1 GiB region (the largest block with 4 KiB granules)
/// that a TLBI by VA may have changed.
const RANGE: u64 = ((1 << 56) - 1) & !((1 << 30) - 1);

/// ASID field of TTBR0_EL1/TTBR1_EL1.
const TTBR_ASID: u64 = 0xffff << 48;

/// Half of the address space of `va`: 1 for TTBR1 (bit 55, which selects
/// the table with and without TBI; with TBI off an address whose top bits
/// differ from bit 55 does not translate at all).
fn half(va: u64) -> u8 {
    (va >> 55 & 1) as u8
}

/// Software TLB entries filled for one (EL, half of the address space),
/// with the table base they were filled under.
#[derive(Default)]
struct TlbGroup {
    key: Option<u64>,
    /// Offsets in the engine's memory of the entries filled.
    filled: Vec<u32>,
    /// Too many to track: flush by scanning.
    overflow: bool,
}

impl TlbGroup {
    /// Entries tracked at most before flushing by scanning.
    const MAX: usize = 1024;

    fn note(&mut self, e: u32) {
        if self.overflow {
            return;
        }
        if self.filled.len() >= Self::MAX {
            self.overflow = true;
            self.filled = Vec::new();
        } else {
            self.filled.push(e);
        }
    }
}

/// Bits of the region parameters in the context (`ctx = epoch << CTX_SHIFT
/// | parameters`).
const CTX_SHIFT: u32 = 7;

/// The translation parameters for the current CPU state (`yields` and
/// `parallel`: the JIT's settings, [`SysJit::set_yields`],
/// [`SysJit::set_parallel`]).
pub fn target(cpu: &Cpu, yields: bool, parallel: bool) -> SysTarget {
    let t = cpu.sys.tcr_el1;
    // CPACR_EL1.FPEN like `Cpu::fp_trapped`: 11 no trap, 01 EL0 only.
    let fp = match cpu.sys.cpacr_el1 >> cpacr::FPEN_SHIFT & 3 {
        0b11 => true,
        0b01 => cpu.sys.el == 1,
        _ => false,
    };
    // CNTKCTL_EL1: counts only at EL0 (at EL1 CNTPCT/CNTVCT can always be read).
    let k = cpu.sys.cntkctl_el1;
    let cntk = if cpu.sys.el == 0 {
        (k & cntkctl::EL0PCTEN != 0) as u8 | ((k & cntkctl::EL0VCTEN != 0) as u8) << 1
    } else {
        0
    };
    SysTarget {
        el: cpu.sys.el,
        tbi0: t & tcr::TBI0 != 0,
        tbi1: t & tcr::TBI1 != 0,
        spsel: cpu.sys.spsel,
        fp,
        cntk,
        yields,
        parallel,
    }
}

/// A compiled module and what eviction needs to know (ADR 0041).
struct Mod<M> {
    m: M,
    /// Looked up (entered through the host) since the last eviction.
    used: Cell<bool>,
}

/// Modules compiled between two jump cache refreshes (ADR 0041): after one,
/// every region entered is looked up again and marks its module, so the
/// marks say which modules were entered since the last eviction.
const SWEEP_MODULES: u32 = 1024;

/// An entry of a compiled region.
struct Compiled<M> {
    module: Rc<Mod<M>>,
    /// False while the engine is still compiling the module
    /// ([`Engine::ready`], ADR 0038): the region runs in the interpreter.
    ready: Rc<Cell<bool>>,
    /// Table entry.
    slot: u32,
    /// Maximum steps of the base entry block.
    max_steps: u8,
    /// Index of the base entry block (`JitState::entry`).
    bb: u8,
}

/// The entries of a compiled region: (address, entry); the first
/// is the start of the region.
type Entries<M> = Rc<[(u64, Rc<Compiled<M>>)]>;

/// A module the engine is still compiling and its readiness flag.
type Compiling<M> = (Rc<Mod<M>>, Rc<Cell<bool>>);

/// A block for a physical page: compiled, or `None` if the first
/// instruction is not translated.
struct Variant<M> {
    pa: u64,
    block: Option<Rc<Compiled<M>>>,
}

struct Entry<M> {
    /// Entries seen with the interpreter.
    seen: u32,
    variants: Vec<Variant<M>>,
    /// The fetch translation last checked for this `pc` (ADR 0041):
    /// (jump cache context, [`Cache::inval_gen`], physical address). Within a
    /// context it cannot change (the guarantee the jump cache relies on), so
    /// a lookup with the same context and no partial TLBI since skips the MMU.
    memo: Option<(u32, u32, u64)>,
    /// The compiled and ready variant last found (ADR 0041): its physical
    /// address and jump cache word, in the entry itself (a lookup is a chain
    /// of cache misses otherwise). Cleared whenever `variants` changes.
    hot: Option<(u64, u32)>,
    /// Looked up hot since the last eviction: the eviction marks the modules
    /// of its variants.
    used: bool,
}

impl<M> Default for Entry<M> {
    fn default() -> Self {
        Entry { seen: 0, variants: Vec::new(), memo: None, hot: None, used: false }
    }
}

/// The jump cache word of a compiled region entry (`area::JC`):
/// `entry << 26 | slot << 8 | maximum steps`.
fn jc_word<M>(c: &Compiled<M>) -> u32 {
    (c.bb as u32) << 26 | c.slot << 8 | c.max_steps as u32
}

/// A hot block waiting for compilation.
struct Pending {
    key: Key,
    pa: u64,
    words: Vec<u32>,
    block: Region,
}

/// What a dispatcher run means for the rest of [`SysJit::run`].
enum Flow {
    /// Go on from the new `pc`.
    Go,
    /// The translation regime changed (MSR TTBR): go on after
    /// [`SysJit::switch_regime`].
    Regime,
    /// The run ends: what comes next.
    End(Next),
}

enum Look {
    /// A ready region: its jump cache word.
    Hot(u32),
    Translate(u64),
    One,
    Cold,
}

/// The known blocks, separate from the engine: the host consults them
/// (`resolve`) while the engine runs the dispatcher.
struct Cache<M> {
    blocks: FastMap<Key, Entry<M>>,
    /// Physical page → keys with variants in that page.
    pages: FastMap<u64, Vec<Key>>,
    /// Modules compiled by (key, instructions): the same code at the
    /// same virtual address (another physical page, another
    /// process) is not recompiled.
    compiled: HashMap<(Key, Vec<u32>), Entries<M>>,
    pending: Vec<Pending>,
    /// Block requests pending since the last compilation.
    pending_hits: usize,
    /// Table chunks handed out so far (`batch` entries each), and the free
    /// ones (ADR 0041).
    next_chunk: u32,
    free_chunks: Vec<u32>,
    /// Live modules with their chunk, and the evicted ones not yet dropped
    /// (their chunk is free once nothing refers to them).
    modules: Vec<(std::rc::Weak<Mod<M>>, u32)>,
    retired: Vec<(std::rc::Weak<Mod<M>>, u32)>,
    /// Modules compiled since the last [`SysJit::refresh_marks`].
    since_sweep: u32,
    /// Context numbers of the jump cache: one per (EL, TTBR0 base, TTBR1
    /// base) since the last [`SysJit::new_epoch`], which clears them (ADR
    /// 0035).
    ids: FastMap<(u8, u64, u64), u32>,
    /// Last context number handed out.
    next_id: u32,
    /// Generation of the partial TLBIs (by VA): a remembered fetch
    /// translation ([`Entry::memo`]) is valid only in its own.
    inval_gen: u32,
    /// Table bases (TTBR0/TTBR1 without the ASID) of the current regime.
    lo: u64,
    hi: u64,
    /// Software TLB groups, index `el * 2 + half`.
    groups: [TlbGroup; 4],
    hot_threshold: u32,
    /// Address of `JitState` in the engine's memory.
    at: usize,
    stats: SysJitStats,
}

impl<M> Cache<M> {
    /// The EL0 software TLB groups were filled under the current table bases:
    /// their entries give what an access with EL0 permissions gives now, so
    /// LDTR/STTR at EL1 may use them and fill them (ADR 0041).
    fn utlb_ok(&self) -> bool {
        self.groups[0].key == Some(self.lo) && self.groups[1].key == Some(self.hi)
    }

    /// Context of the jump cache entries valid now for the
    /// parameters `fl` ([`flags`]: EL, TBI, SPSel, FP). The fetch
    /// translation depends on the table bases and on what
    /// [`SysJit::sync_regime`] compares: the number is the same whenever the
    /// regime comes back to the same bases, so the entries survive the
    /// TTBR0 switches of every kernel entry and exit (Linux's software PAN:
    /// the kernel runs with the reserved TTBR0, user code with its own).
    fn ctx(&mut self, fl: u8) -> u32 {
        let key = (fl & 1, self.lo, self.hi);
        let next = &mut self.next_id;
        let id = *self.ids.entry(key).or_insert_with(|| {
            *next += 1;
            *next
        });
        id << CTX_SHIFT | fl as u32
    }

    /// Looks up the block of `pc` for the physical page the CPU would
    /// read it from now (same translation as the interpreter's fetch) and,
    /// if `count`, counts the entries of untranslated ones.
    #[allow(clippy::too_many_arguments)]
    fn lookup(
        &mut self,
        pc: u64,
        fl: u8,
        ctx: u32,
        regs: &TranslationRegs,
        el: u8,
        mmu: &mut Mmu,
        phys: &mut dyn SysPhys,
        count: bool,
    ) -> Look {
        if !pc.is_multiple_of(4) {
            return Look::One;
        }
        // Cold code (no variant and below the threshold): the fetch
        // translation is not needed. Count as before (the interpreter will
        // execute the same instructions anyway, even if the fetch fails).
        // One hash lookup for everything (ADR 0041: the map is large and this
        // runs at every jump cache miss).
        let threshold = self.hot_threshold;
        let generation = self.inval_gen;
        let stats = &mut self.stats;
        let e = match self.blocks.entry((pc, fl)) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                if !count {
                    return Look::Cold;
                }
                if threshold > 1 {
                    v.insert(Entry { seen: 1, ..Entry::default() });
                    return Look::Cold;
                }
                v.insert(Entry::default())
            }
        };
        if e.variants.is_empty() {
            if !count {
                return Look::Cold;
            }
            if e.seen.saturating_add(1) < threshold {
                e.seen += 1;
                return Look::Cold;
            }
        }
        let pa = match e.memo.filter(|m| m.0 == ctx && m.1 == generation) {
            Some((_, _, pa)) => {
                stats.memo_hits += 1;
                pa
            }
            None => {
                let mut ram = RamOnly(phys);
                let mut bus = MmuBus::new(mmu, &mut ram);
                match bus.translate(regs, pc, AccessReq { access: Access::Fetch, el, aligned: true }) {
                    Ok(pa) => pa,
                    Err(_) => return Look::One,
                }
            }
        };
        e.memo = Some((ctx, generation, pa));
        if let Some((_, w)) = e.hot.filter(|h| h.0 == pa) {
            e.used = true;
            return Look::Hot(w);
        }
        if let Some(v) = e.variants.iter().find(|v| v.pa == pa) {
            return match &v.block {
                Some(c) if c.ready.get() => {
                    let w = jc_word(c);
                    e.hot = Some((pa, w));
                    e.used = true;
                    Look::Hot(w)
                }
                // Still compiling (ADR 0038): the interpreter runs it.
                Some(_) => Look::Cold,
                None => Look::One,
            };
        }
        if count {
            e.seen = e.seen.saturating_add(1);
        }
        if e.seen < threshold { Look::Cold } else { Look::Translate(pa) }
    }

    /// Maximum steps of the jump cache entry of `pc`, if it is valid for `ctx`.
    /// The two entries (ways) where `pc` can be: `i` and `i ^ 1` (ADR 0041).
    fn jc_ways(&self, pc: u64) -> (usize, usize) {
        let i = ((pc >> 2) & (area::JC_ENTRIES as u64 - 1)) as usize;
        let base = self.at + area::JC as usize;
        (base + i * 16, base + (i ^ 1) * 16)
    }

    fn probe_jc(&self, mem: &[u8], pc: u64, ctx: u32) -> Option<u8> {
        let (a, b) = self.jc_ways(pc);
        [a, b].into_iter().find_map(|e| {
            let hit = state::read_u64(mem, e, 0) == pc && state::read_u32(mem, e, 8) == ctx;
            hit.then(|| state::read_u32(mem, e, 12) as u8)
        })
    }

    /// Jump cache entry: `pc` → block, valid for `ctx`, in the first way; the
    /// entry it replaces, if for another `pc`, moves to the second (2-way,
    /// ADR 0041).
    fn install_jc(&self, mem: &mut [u8], pc: u64, ctx: u32, w: u32) {
        let (e, second) = self.jc_ways(pc);
        if state::read_u64(mem, e, 0) != pc {
            mem.copy_within(e..e + 16, second);
        }
        mem[e..e + 8].copy_from_slice(&pc.to_le_bytes());
        mem[e + 8..e + 12].copy_from_slice(&ctx.to_le_bytes());
        mem[e + 12..e + 16].copy_from_slice(&w.to_le_bytes());
    }

    /// Records the entries of a region of `key` at the physical page of
    /// `pa`: the start always, the other base blocks only where there is not
    /// already a compiled block (no duplicated code for returns and jumps
    /// inside the region).
    fn record_entries(&mut self, key: Key, pa: u64, entries: &Entries<M>) {
        for (i, (epc, c)) in entries.iter().enumerate() {
            let k = (*epc, key.1);
            let epa = pa.wrapping_add(epc.wrapping_sub(key.0));
            if i > 0
                && self
                    .blocks
                    .get(&k)
                    .is_some_and(|e| e.variants.iter().any(|v| v.pa == epa && v.block.is_some()))
            {
                continue;
            }
            self.record(k, epa, Some(c.clone()));
        }
    }

    fn record(&mut self, key: Key, pa: u64, block: Option<Rc<Compiled<M>>>) {
        let e = self.blocks.entry(key).or_default();
        e.variants.retain(|v| v.pa != pa);
        e.hot = block.as_ref().filter(|c| c.ready.get()).map(|c| (pa, jc_word(c)));
        e.variants.push(Variant { pa, block });
        self.pages.entry(pa >> 12).or_default().push(key);
    }
}

/// The system-mode JIT.
pub struct SysJit<E: Engine> {
    engine: E,
    cfg: SysJitConfig,
    dispatcher: Option<E::Module>,
    cache: Cache<E::Module>,
    /// SCTLR, TCR and MAIR of the last run: a change invalidates everything.
    common: Option<(u64, u64, u64)>,
    flushes: u64,
    /// RAM as seen by the engine: (start pa, address in `env.mem`, bytes).
    ram: Option<(u64, u32, u64)>,
    ram_key: Option<(u64, usize, usize)>,
    dirty: Vec<u64>,
    /// Modules the engine is still compiling, with their flag (ADR 0038).
    compiling: Vec<Compiling<E::Module>>,
    profile: Option<Profile>,
    /// Clock of the next run ([`SysJit::set_time`]).
    time: Option<Clock>,
    /// Virtual addresses that no region contains
    /// ([`SysJit::set_stops`], introspection hook points).
    stops: std::collections::BTreeSet<u64>,
    /// WFE and YIELD left to the interpreter ([`SysJit::set_yields`]).
    yields: bool,
    /// Code for cores running in parallel ([`SysJit::set_parallel`]).
    parallel: bool,
    /// Set by another core to end the current run ([`SysJit::set_abort`]).
    abort: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Steps of the current run the regions added to the shared clock.
    flushed: u64,
}

/// The guest clock for the regions (ADR 0026): instructions executed
/// by the machine and CNTVOFF. MRS of CNTPCT/CNTVCT in the regions gives the
/// same value as the interpreter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub steps: u64,
    pub cntvoff: u64,
    /// Several cores (ADR 0042): the counter is that of `steps / div`
    /// (1 = one core).
    pub div: u64,
    /// Cores in parallel: host address of the shared clock (a u64 the
    /// regions add their steps to atomically before reading it, so time never
    /// goes back from one core to the next), or 0. Used only if the engine
    /// reaches that address (`Engine::host_address`); otherwise MRS of the
    /// counter exits to the interpreter.
    pub shared: usize,
}

impl Clock {
    /// One core: the counter of `steps`.
    pub fn new(steps: u64, cntvoff: u64) -> Self {
        Clock { steps, cntvoff, div: 1, shared: 0 }
    }
}

/// The blocks' host: accesses through the MMU with the permissions of EL and the
/// system-mode rules; everything that is not a simple access
/// to RAM is a fault, and the interpreter redoes it. It also resolves missing
/// jump cache entries for the dispatcher.
struct SysHost<'a, M> {
    cache: &'a mut Cache<M>,
    mmu: &'a mut Mmu,
    phys: &'a mut dyn SysPhys,
    /// The SIMD/FP registers of the `Cpu` (for `vsync`).
    v: &'a [u128; 32],
    regs: TranslationRegs,
    el: u8,
    fl: u8,
    ram: Option<(u64, u32, u64)>,
}

impl<M> SysHost<'_, M> {
    /// Translation of a data access of `size` bytes (1, 2, 4, 8, or
    /// [`ZVA_BYTES`] for DC ZVA).
    /// Also returns whether the access was aligned.
    fn translate(&mut self, va: u64, size: u32, access: Access) -> Result<(u64, bool), ()> {
        // Half of a 16-byte access not aligned to 16 (`SIZE_PART_OF_MISALIGNED`):
        // the permissions and alignment are those of the whole access.
        let part = size & translate::SIZE_PART_OF_MISALIGNED != 0;
        // LDTR/STTR at EL1: the permissions of EL0.
        let el = if size & translate::SIZE_UNPRIV != 0 { 0 } else { self.el };
        let size = size & !(translate::SIZE_PART_OF_MISALIGNED | translate::SIZE_UNPRIV);
        let zva = size == ZVA_BYTES;
        let aligned = !zva && !part && va & (size as u64 - 1) == 0;
        // Like `SysMem::access`: big-endian not implemented, alignment
        // with SCTLR_EL1.A (not for DC ZVA). Crossing a page:
        // to the interpreter.
        let big = if self.el == 0 { sctlr::E0E } else { sctlr::EE };
        if self.regs.sctlr & big != 0
            || (!aligned && !zva && self.regs.sctlr & sctlr::A != 0)
            || (va & 0xfff) + size as u64 > 0x1000
        {
            return Err(());
        }
        let mut ram = RamOnly(&mut *self.phys);
        let mut bus = MmuBus::new(&mut *self.mmu, &mut ram);
        let pa = bus.translate(&self.regs, va, AccessReq { access, el, aligned }).map_err(|_| ())?;
        Ok((pa, aligned))
    }

    /// Software TLB entries for the page of `va` (translated to `pa`):
    /// after a successful unaligned access (`aligned` false) the page is
    /// Normal and SCTLR_EL1.A is 0, and it is valid for unaligned ones too.
    fn fill(&mut self, mem: &mut [u8], va: u64, pa: u64, write: bool, aligned: bool, el: u8) {
        let Some((base, addr, len)) = self.ram else { return };
        let page = pa & !0xfff;
        if page < base || page + 0x1000 > base + len || (write && self.phys.is_watched(pa >> 12)) {
            return;
        }
        let host = addr as u64 + (page - base);
        let vpage = va & !0xfff;
        let idx = ((va >> 12) & (area::TLB_ENTRIES as u64 - 1)) as usize * 16;
        let tables = [Some(area::tlb(el, write)), (!aligned).then(|| area::tlb_u(el, write))];
        let g = &mut self.cache.groups[(el * 2 + half(va)) as usize];
        for t in tables.into_iter().flatten() {
            let e = self.cache.at + t as usize + idx;
            mem[e..e + 8].copy_from_slice(&vpage.to_le_bytes());
            mem[e + 8..e + 16].copy_from_slice(&host.wrapping_sub(vpage).to_le_bytes());
            g.note(e as u32);
        }
        self.cache.stats.tlb_fills += 1;
    }
}

impl<M> Host for SysHost<'_, M> {
    fn ld(&mut self, mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()> {
        self.cache.stats.host_lds += 1;
        let (pa, aligned) = self.translate(va, size, Access::Read)?;
        let unpriv = size & translate::SIZE_UNPRIV != 0;
        let size = size & !(translate::SIZE_PART_OF_MISALIGNED | translate::SIZE_UNPRIV);
        let mut b = [0u8; 8];
        if !self.phys.ram_read(pa, &mut b[..size as usize]) {
            return Err(());
        }
        // Checked with the permissions of EL0: an entry of the EL0 tables,
        // if they belong to the current table bases (ADR 0041).
        if !unpriv {
            self.fill(mem, va, pa, false, aligned, self.el);
        } else if self.cache.utlb_ok() {
            self.fill(mem, va, pa, false, aligned, 0);
        }
        Ok(u64::from_le_bytes(b))
    }

    fn st(&mut self, mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()> {
        self.cache.stats.host_sts += 1;
        let (pa, aligned) = self.translate(va, size, Access::Write)?;
        let unpriv = size & translate::SIZE_UNPRIV != 0;
        let size = size & !(translate::SIZE_PART_OF_MISALIGNED | translate::SIZE_UNPRIV);
        if size == ZVA_BYTES {
            return self.phys.ram_write(pa, &[0u8; ZVA_BYTES as usize]).ok_or(());
        }
        match self.phys.ram_write(pa, &value.to_le_bytes()[..size as usize]) {
            None => Err(()),
            Some(true) => Ok(true),
            Some(false) => {
                if !unpriv {
                    self.fill(mem, va, pa, true, aligned, self.el);
                } else if self.cache.utlb_ok() {
                    self.fill(mem, va, pa, true, aligned, 0);
                }
                Ok(false)
            }
        }
    }

    fn vsync(&mut self, mem: &mut [u8], state: u32) {
        state::vsync_in(mem, state as usize, self.v);
    }

    /// Missing jump cache entry for the `pc` of `JitState`: if the
    /// block already exists (compiled, for the current physical page), writes it
    /// with the current context.
    fn resolve(&mut self, mem: &mut [u8]) -> bool {
        let at = self.cache.at;
        let pc = state::read_u64(mem, at, off::PC);
        let ctx = state::read_u32(mem, at, off::CTX);
        self.cache.stats.resolves += 1;
        match self.cache.lookup(pc, self.fl, ctx, &self.regs, self.el, self.mmu, self.phys, false) {
            Look::Hot(w) => {
                self.cache.install_jc(mem, pc, ctx, w);
                true
            }
            _ => false,
        }
    }
}

impl<E: Engine> SysJit<E> {
    pub fn new(mut engine: E, cfg: SysJitConfig) -> Self {
        assert!(cfg.state_addr.is_multiple_of(16), "JitState must be aligned to 16 bytes");
        assert!(cfg.batch >= 1);
        engine.reserve(cfg.state_addr as usize + area::SIZE as usize);
        engine.runtime(&translate::runtime(cfg.memory)).expect("JIT runtime rejected by the engine");
        let mut j = SysJit {
            engine,
            cfg,
            dispatcher: None,
            cache: Cache {
                blocks: FastMap::default(),
                pages: FastMap::default(),
                compiled: HashMap::new(),
                pending: Vec::new(),
                pending_hits: 0,
                next_chunk: 0,
                free_chunks: Vec::new(),
                modules: Vec::new(),
                retired: Vec::new(),
                since_sweep: 0,
                ids: FastMap::default(),
                next_id: 0,
                inval_gen: 0,
                lo: 0,
                hi: 0,
                groups: Default::default(),
                hot_threshold: cfg.hot_threshold,
                at: cfg.state_addr as usize,
                stats: SysJitStats::default(),
            },
            common: None,
            flushes: 0,
            ram: None,
            ram_key: None,
            dirty: Vec::new(),
            compiling: Vec::new(),
            time: None,
            stops: std::collections::BTreeSet::new(),
            yields: false,
            parallel: false,
            abort: None,
            flushed: 0,
            profile: cfg.profile.then(|| {
                crate::helper::profile(true);
                Profile::default()
            }),
        };
        j.init_area();
        j
    }

    /// Virtual addresses (of any space) that the regions do not
    /// contain: a block ends before, and a region does not start there.
    /// So the instruction at that address is always executed by the interpreter,
    /// which checks the introspection hook points there (ADR 0027).
    /// The result of the execution does not change. If the set changes
    /// all blocks (even the waiting ones) and the jump cache entries are
    /// forgotten (new epoch); the table entries stay
    /// occupied until the next reset, without touching the engine.
    pub fn set_stops(&mut self, stops: &[u64]) {
        let new: std::collections::BTreeSet<u64> = stops.iter().copied().collect();
        if new == self.stops {
            return;
        }
        self.stops = new;
        self.forget_blocks();
    }

    /// With `on`, regions never contain WFE or YIELD: the interpreter
    /// executes them and reports `SysEvent::Yield`, which ends a core's turn
    /// on a machine with several cores (ADR 0042). Without it (one core) they
    /// are NOPs inside the regions. A change forgets every block, like
    /// [`SysJit::set_stops`].
    pub fn set_yields(&mut self, on: bool) {
        if on != self.yields {
            self.yields = on;
            self.forget_blocks();
        }
    }

    /// Code for a core that runs in parallel with others on shared memory
    /// (ADR 0042): DMB/DSB/ISB become `atomic.fence`, LDAR/STLR atomic
    /// accesses, the exclusive loads are followed by a fence and the
    /// store-exclusives are left to the interpreter (an atomic
    /// compare-and-exchange). A change forgets every block.
    pub fn set_parallel(&mut self, on: bool) {
        if on != self.parallel {
            self.parallel = on;
            self.forget_blocks();
        }
    }

    /// A flag another core sets to end this core's run at the next region
    /// boundary (with the limit at zero, [`SysJit::limit_addr`]). Cleared by
    /// the owner before every run.
    pub fn set_abort(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.abort = Some(flag);
    }

    /// Host address of the step limit in `JitState` (an aligned u64): another
    /// core writes zero there to stop the dispatcher at the next block.
    pub fn limit_addr(&mut self) -> usize {
        let at = self.cache.at + off::LIMIT as usize;
        self.engine.memory()[at..].as_mut_ptr().addr()
    }

    /// Forgets the software TLB's write entries: another core now has
    /// translated code on a page this core may write directly (ADR 0042).
    pub fn flush_writes(&mut self) {
        self.flush_tlb(true);
    }

    /// Forgets all blocks (even the waiting ones) and the jump cache
    /// entries (new epoch).
    fn forget_blocks(&mut self) {
        let c = &mut self.cache;
        c.blocks.clear();
        c.pages.clear();
        c.compiled.clear();
        c.pending.clear();
        c.pending_hits = 0;
        self.new_epoch();
    }

    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    pub fn stats(&self) -> SysJitStats {
        self.cache.stats
    }

    /// With `cfg.profile`: counts the class of the instruction that the interpreter
    /// is about to execute at `cpu.pc` (read with a fetch translation,
    /// without effects on RAM; a fetch that fails is not counted).
    pub fn profile_step(&mut self, cpu: &Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys) {
        let Some(p) = &mut self.profile else { return };
        let regs = translation_regs(cpu);
        let el = cpu.sys.el;
        let pa = {
            let mut ram = RamOnly(phys);
            let mut bus = MmuBus::new(mmu, &mut ram);
            bus.translate(&regs, cpu.pc, AccessReq { access: Access::Fetch, el, aligned: true })
        };
        let mut w = [0u8; 4];
        if let Ok(pa) = pa
            && phys.ram_read(pa, &mut w)
        {
            let w = u32::from_le_bytes(w);
            p.note(w, Some(target(cpu, self.yields, self.parallel)));
            if let Some(r) = crate::profile::fp_reason(w, cpu, None) {
                p.note_fp(w, r);
            }
        }
    }

    /// The clock for the next [`run`](Self::run) (valid only for that one):
    /// without it, MRS of CNTPCT/CNTVCT in the regions exits to the interpreter.
    pub fn set_time(&mut self, c: Clock) {
        self.time = Some(c);
    }

    /// Interpreter instructions per class, if `cfg.profile`.
    pub fn profile(&self) -> Option<&Profile> {
        self.profile.as_ref()
    }

    /// Empty jump cache and empty software TLB.
    fn init_area(&mut self) {
        let at = self.cache.at;
        let m = self.engine.memory();
        m[at..at + area::SIZE as usize].fill(0);
        for i in 0..8 * area::TLB_ENTRIES as usize {
            let e = at + area::TLB as usize + i * 16;
            m[e..e + 8].copy_from_slice(&area::TLB_INVALID.to_le_bytes());
        }
        for g in &mut self.cache.groups {
            *g = TlbGroup::default();
        }
        self.cache.stats.tlb_flushes += 1;
    }

    /// Flushes the software TLB (only the write tables if `writes_only`).
    fn flush_tlb(&mut self, writes_only: bool) {
        for g in 0..4 {
            self.flush_group(g, writes_only);
        }
        self.cache.stats.tlb_flushes += 1;
    }

    /// Empties TLB group `g` (`el * 2 + half`): the entries it filled, or,
    /// if it lost count, every entry of its EL whose page is in its half.
    fn flush_group(&mut self, g: usize, writes_only: bool) {
        let at = self.cache.at;
        // In place: the list keeps its allocation (ADR 0041: with the software
        // PAN's TTBR0 switches followed within the run, this runs at every
        // kernel entry and exit).
        let group = &mut self.cache.groups[g];
        let m = self.engine.memory();
        let is_write = |e: usize| ((e - at - area::TLB as usize) / area::TLB_SIZE as usize) & 1 == 1;
        let invalid = area::TLB_INVALID.to_le_bytes();
        if group.overflow {
            let (el, h) = ((g / 2) as u8, (g % 2) as u8);
            for write in [false, true] {
                if writes_only && !write {
                    continue;
                }
                for t in [area::tlb(el, write), area::tlb_u(el, write)] {
                    for i in 0..area::TLB_ENTRIES as usize {
                        let e = at + t as usize + i * 16;
                        let tag = state::read_u64(m, e, 0);
                        if tag != area::TLB_INVALID && half(tag) == h {
                            m[e..e + 8].copy_from_slice(&invalid);
                        }
                    }
                }
            }
            // The read tables were not looked at: the count stays lost.
            group.overflow = writes_only;
            group.filled.clear();
        } else {
            group.filled.retain(|&e| {
                let e = e as usize;
                if writes_only && !is_write(e) {
                    true
                } else {
                    m[e..e + 8].copy_from_slice(&invalid);
                    false
                }
            });
        }
    }

    /// After TLBIs by VA: forgets the software TLB entries and the jump cache
    /// entries whose page is in one of `ranges` (VA[55:0] & [`RANGE`]).
    fn invalidate_ranges(&mut self, ranges: &[u64]) {
        // The fetch translations remembered by the lookups may be among them.
        self.cache.inval_gen = self.cache.inval_gen.wrapping_add(1);
        let at = self.cache.at;
        let m = self.engine.memory();
        let hit = |va: u64| ranges.contains(&(va & RANGE));
        for i in 0..8 * area::TLB_ENTRIES as usize {
            let e = at + area::TLB as usize + i * 16;
            let tag = state::read_u64(m, e, 0);
            if tag != area::TLB_INVALID && hit(tag) {
                m[e..e + 8].copy_from_slice(&area::TLB_INVALID.to_le_bytes());
            }
        }
        for i in 0..area::JC_ENTRIES as usize {
            let e = at + area::JC as usize + i * 16;
            // Context 0 is never handed out: a zeroed entry matches nothing.
            if state::read_u32(m, e, 8) != 0 && hit(state::read_u64(m, e, 0)) {
                m[e..e + 16].fill(0);
            }
        }
    }

    /// New epoch: the jump cache entries are no longer valid.
    fn new_epoch(&mut self) {
        self.cache.ids.clear();
        self.cache.stats.epochs += 1;
        if self.cache.next_id >= (1 << (32 - CTX_SHIFT)) - 1024 {
            let at = self.cache.at + area::JC as usize;
            self.engine.memory()[at..at + area::JC_ENTRIES as usize * 16].fill(0);
            self.cache.next_id = 0;
        }
    }

    /// Discards the blocks of the written pages.
    fn drain(&mut self, phys: &mut dyn SysPhys) {
        phys.take_code_dirty(&mut self.dirty);
        if self.dirty.is_empty() {
            return;
        }
        let mut any = false;
        let c = &mut self.cache;
        for &page in &self.dirty {
            c.pending.retain(|p| p.pa >> 12 != page);
            if let Some(keys) = c.pages.remove(&page) {
                for k in keys {
                    if let Some(e) = c.blocks.get_mut(&k) {
                        e.variants.retain(|v| v.pa >> 12 != page);
                        e.hot = None;
                    }
                }
                c.stats.invalidated_pages += 1;
                any = true;
            }
        }
        self.dirty.clear();
        if any {
            self.cache.stats.epochs_code += 1;
            self.new_epoch();
        }
    }

    /// The translation regime of this run (ADR 0036). SCTLR, TCR or MAIR
    /// changed, or a TLBI: new epoch and empty software TLB. The table bases
    /// (TTBR0 and TTBR1 without the ASID) only select, per half of the
    /// address space, the context numbers ([`Cache::ctx`]) and the TLB
    /// entries that are valid: the TLB groups of the current EL filled
    /// with another base are emptied. The ASID does not change what a walk
    /// from the same tables gives; like the MMU's TLB, entries become stale
    /// only if the guest changes the tables without a TLBI.
    fn sync_regime(&mut self, cpu: &Cpu, mmu: &Mmu) {
        let r = translation_regs(cpu);
        let f = mmu.tlb().flushes();
        let common = (r.sctlr, r.tcr, r.mair);
        if self.common == Some(common) && self.flushes != f {
            // Only TLBIs by VA: what they may have changed is within the
            // largest block around each address (1 GiB with 4 KiB granules).
            let granule_4k = r.tcr >> 14 & 3 == 0 && r.tcr >> 30 & 3 == 0b10;
            let ranges: Option<Vec<u64>> =
                mmu.tlb().invalidations_since(self.flushes).filter(|_| granule_4k).and_then(|it| {
                    it.map(|i| match i {
                        Inval::Va(va) => Some(va & RANGE),
                        Inval::All => None,
                    })
                    .collect()
                });
            if let Some(mut ranges) = ranges {
                ranges.sort_unstable();
                ranges.dedup();
                self.invalidate_ranges(&ranges);
                self.cache.stats.tlbi_partial += 1;
                self.flushes = f;
            }
        }
        if self.common != Some(common) || self.flushes != f {
            if self.common != Some(common) {
                self.cache.stats.epochs_regs += 1;
            } else {
                self.cache.stats.epochs_tlbi += 1;
            }
            self.common = Some(common);
            self.flushes = f;
            self.new_epoch();
            self.flush_tlb(false);
        }
        self.sync_bases(cpu, &r);
    }

    /// The table bases of the regime `r` (part of [`sync_regime`](Self::sync_regime),
    /// alone after a region's MSR TTBR: nothing else can have changed within
    /// the run).
    fn sync_bases(&mut self, cpu: &Cpu, r: &TranslationRegs) {
        let (lo, hi) = (r.ttbr0 & !TTBR_ASID, r.ttbr1 & !TTBR_ASID);
        if (lo, hi) != (self.cache.lo, self.cache.hi) {
            self.cache.stats.base_switches += 1;
            self.cache.lo = lo;
            self.cache.hi = hi;
        }
        let el = cpu.sys.el.min(1) as usize;
        for h in 0..2 {
            let g = el * 2 + h;
            let key = if h == 0 { lo } else { hi };
            if self.cache.groups[g].key != Some(key) {
                self.flush_group(g, false);
                self.cache.groups[g].key = Some(key);
            }
        }
    }

    /// RAM as seen by the engine (recomputed if it changes).
    fn sync_ram(&mut self, phys: &mut dyn SysPhys) {
        let Some((base, ptr, len)) = phys.ram_region() else {
            self.ram = None;
            return;
        };
        let key = (base, ptr as usize, len);
        if self.ram_key != Some(key) {
            self.ram_key = Some(key);
            self.ram = self.engine.host_address(ptr, len).map(|a| (base, a, len as u64));
        }
    }

    /// Executes translated blocks for at most `budget` steps (see the module
    /// contract).
    pub fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun {
        self.cache.stats.calls += 1;
        self.poll_compiling();
        self.drain(phys);
        self.sync_regime(cpu, mmu);
        self.sync_ram(phys);
        let mut regs = translation_regs(cpu);
        let sys = target(cpu, self.yields, self.parallel);
        let (el, fl) = (sys.el, flags(sys));
        let at = self.cache.at;
        let mut done = 0u64;
        let mut in_jit = false;
        let mut pc = cpu.pc;
        let time = self.time.take();
        let next = loop {
            if done >= budget
                || self.abort.as_ref().is_some_and(|a| a.load(std::sync::atomic::Ordering::Relaxed))
            {
                break Next::Jit;
            }
            // A valid jump cache entry for `pc` names the region the lookup
            // would find (same guarantee the dispatcher relies on): no
            // lookup, no fetch translation.
            if self.dispatcher.is_some() {
                let ctx = self.cache.ctx(fl);
                if let Some(max) = self.cache.probe_jc(self.engine.memory(), pc, ctx) {
                    if max as u64 > budget - done {
                        break Next::One;
                    }
                    let m = self.engine.memory();
                    if !in_jit {
                        let mut s = JitState::from_cpu_sys(cpu);
                        s.ctx = ctx;
                        s.limit = budget - done;
                        s.store(m, at);
                        in_jit = true;
                    } else {
                        state::write_u64(m, at, off::STEPS, 0);
                        state::write_u64(m, at, off::LIMIT, budget - done);
                        state::write_u32(m, at, off::CTX, ctx);
                        state::write_u32(m, at, off::EXIT_DETAIL, 0);
                    }
                    self.cache.stats.jc_probes += 1;
                    match self.dispatch(cpu, mmu, phys, &regs, el, fl, time, done, &mut pc) {
                        (steps, Flow::Go) => {
                            done += steps;
                            continue;
                        }
                        (steps, Flow::Regime) => {
                            done += steps;
                            regs = self.switch_regime(cpu);
                            continue;
                        }
                        (steps, Flow::End(n)) => {
                            done += steps;
                            break n;
                        }
                    }
                }
            }
            let ctx = self.cache.ctx(fl);
            let w = match self.cache.lookup(pc, fl, ctx, &regs, el, mmu, phys, true) {
                Look::Hot(w) => w,
                Look::One => break Next::One,
                Look::Cold => break Next::Cold,
                Look::Translate(pa) => {
                    // Compiling may reset the engine and its
                    // memory: first the state goes back into the Cpu.
                    if in_jit {
                        JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
                        in_jit = false;
                    }
                    match self.install(pc, fl, sys, pa, phys) {
                        // Compiled in the background (ADR 0038): the
                        // interpreter runs it until the module is ready.
                        Ok(c) if !c.ready.get() => break Next::Cold,
                        Ok(c) => jc_word(&c),
                        Err(n) => break n,
                    }
                }
            };
            if (w & 0xff) as u64 > budget - done {
                break Next::One;
            }
            if self.dispatcher.is_none() {
                if in_jit {
                    JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
                    in_jit = false;
                }
                self.compile_dispatcher();
            }
            // The epoch may have changed (compilation, reset).
            let ctx = self.cache.ctx(fl);
            let m = self.engine.memory();
            self.cache.install_jc(m, pc, ctx, w);
            if !in_jit {
                let mut s = JitState::from_cpu_sys(cpu);
                s.ctx = ctx;
                s.limit = budget - done;
                s.store(m, at);
                in_jit = true;
            } else {
                state::write_u64(m, at, off::STEPS, 0);
                state::write_u64(m, at, off::LIMIT, budget - done);
                state::write_u32(m, at, off::CTX, ctx);
                state::write_u32(m, at, off::EXIT_DETAIL, 0);
            }
            match self.dispatch(cpu, mmu, phys, &regs, el, fl, time, done, &mut pc) {
                (steps, Flow::Go) => done += steps,
                (steps, Flow::Regime) => {
                    done += steps;
                    regs = self.switch_regime(cpu);
                }
                (steps, Flow::End(n)) => {
                    done += steps;
                    break n;
                }
            }
        };
        if in_jit {
            JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
        }
        SysRun { steps: done, next, flushed: core::mem::take(&mut self.flushed) }
    }

    /// After a region's MSR TTBR0/TTBR1 (YIELD without unmasked
    /// interrupts, ADR 0041): the run goes on in the new regime instead of
    /// returning to the machine. Nothing about interrupts changed; the table
    /// bases go from `JitState` into the `Cpu` (the rest stays in
    /// `JitState`) and the contexts and TLB groups follow them, as at the start
    /// of a run. Returns the new translation registers.
    fn switch_regime(&mut self, cpu: &mut Cpu) -> TranslationRegs {
        let m = self.engine.memory();
        cpu.sys.ttbr0_el1 = state::read_u64(m, self.cache.at, off::TTBR0);
        cpu.sys.ttbr1_el1 = state::read_u64(m, self.cache.at, off::TTBR1);
        self.cache.stats.regime_switches += 1;
        let r = translation_regs(cpu);
        self.sync_bases(cpu, &r);
        r
    }

    /// Runs the dispatcher from `pc` (the jump cache entry and `JitState` are
    /// ready): the steps it executed and whether the run goes on.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &mut self,
        cpu: &mut Cpu,
        mmu: &mut Mmu,
        phys: &mut dyn SysPhys,
        regs: &TranslationRegs,
        el: u8,
        fl: u8,
        time: Option<Clock>,
        done: u64,
        pc: &mut u64,
    ) -> (u64, Flow) {
        let at = self.cache.at;
        let utlb = el == 1 && self.cache.utlb_ok();
        let m = self.engine.memory();
        // LDTR/STTR at EL1 through the EL0 tables (ADR 0041).
        state::write_u32(m, at, off::UTLB, utlb as u32);
        // Clock: `steps` of JitState restarts from 0 on every run.
        let shared = match time {
            Some(c) if c.shared != 0 => self.engine.host_address(c.shared as *const u8, 8),
            _ => None,
        };
        let m = self.engine.memory();
        match (time, shared) {
            (Some(c), Some(addr)) => {
                state::write_u64(m, at, off::TIME_BASE, u64::from(addr));
                state::write_u64(m, at, off::CNTVOFF, c.cntvoff);
                state::write_u64(m, at, area::TIME_DIV, c.div.max(1));
                state::write_u64(m, at, area::TIME_FLUSHED, 0);
                state::write_u32(m, at, off::TIME_OK, 2);
            }
            (Some(c), None) if c.shared == 0 => {
                state::write_u64(m, at, off::TIME_BASE, c.steps + done);
                state::write_u64(m, at, off::CNTVOFF, c.cntvoff);
                state::write_u64(m, at, area::TIME_DIV, c.div.max(1));
                state::write_u32(m, at, off::TIME_OK, 1);
            }
            _ => state::write_u32(m, at, off::TIME_OK, 0),
        }
        let code = {
            let v = &cpu.v;
            let mut host =
                SysHost { cache: &mut self.cache, mmu, phys, v, regs: *regs, el, fl, ram: self.ram };
            let d = self.dispatcher.as_ref().expect("dispatcher compiled");
            self.engine.run(d, 0, self.cfg.state_addr, &mut host)
        };
        self.cache.stats.runs += 1;
        let m = self.engine.memory();
        if self.cfg.names {
            self.cache.stats.dispatches += state::read_u64(m, at, off::DISPATCHES);
            state::write_u64(m, at, off::DISPATCHES, 0);
        }
        let steps = state::read_u64(m, at, off::STEPS);
        *pc = state::read_u64(m, at, off::PC);
        self.cache.stats.jit_steps += steps;
        if shared.is_some() {
            self.flushed += state::read_u64(m, at, area::TIME_FLUSHED);
        }
        let next = match code {
            NEXT => Flow::Go,
            STOP => {
                self.cache.stats.stops += 1;
                self.drain(phys);
                Flow::Go
            }
            FAULT => {
                self.cache.stats.faults += 1;
                Flow::End(Next::One)
            }
            SVC => {
                self.cache.stats.svcs += 1;
                Flow::End(Next::One)
            }
            YIELD => {
                self.cache.stats.yields += 1;
                // MSR TTBR0/TTBR1 (`exit_detail` = 3): the run goes on in the
                // new regime. Otherwise interrupts were unmasked, and the
                // caller decides (`jit_budget`).
                if state::read_u32(m, at, off::EXIT_DETAIL) == state::DETAIL_REGIME {
                    Flow::Regime
                } else {
                    Flow::End(Next::Jit)
                }
            }
            other => panic!("unknown dispatcher exit code: {other}"),
        };
        (steps, next)
    }

    /// Reads and translates the block of `pc` at the physical page of `pa`;
    /// compiles it immediately or puts it among the waiting ones (`Err(Cold)`).
    /// `Err(One)`: the first instruction is not translated.
    fn install(
        &mut self,
        pc: u64,
        fl: u8,
        sys: SysTarget,
        pa: u64,
        phys: &mut dyn SysPhys,
    ) -> Result<Rc<Compiled<E::Module>>, Next> {
        let key = (pc, fl);
        if self.cache.pending.iter().any(|p| p.key == key && p.pa == pa) {
            self.cache.pending_hits += 1;
            if self.cache.pending_hits < self.cfg.batch {
                return Err(Next::Cold);
            }
            self.compile_pending();
            return self.compiled_block(key, pa);
        }
        let stops = &self.stops;
        let found = translate::discover(pc, Some(sys), MAX_REGION, |a| {
            if stops.contains(&a) {
                return None;
            }
            let mut w = [0u8; 4];
            phys.ram_read(pa.wrapping_add(a.wrapping_sub(pc)), &mut w).then(|| u32::from_le_bytes(w))
        });
        let page = pa >> 12;
        let was_watched = phys.is_watched(page);
        let Some((block, words)) = found.filter(|_| phys.watch_code(page)) else {
            self.cache.record(key, pa, None);
            return Err(Next::One);
        };
        if !was_watched {
            // No direct writes to a page that now has blocks.
            self.flush_tlb(true);
            if !phys.watch_ready(page) {
                // Other cores may still write it directly (ADR 0042): read it
                // again once they no longer can.
                return Err(Next::Cold);
            }
        }
        if let Some(es) = self.cache.compiled.get(&(key, words.clone())) {
            let es = es.clone();
            self.cache.stats.reused += 1;
            self.cache.record_entries(key, pa, &es);
            return Ok(es[0].1.clone());
        }
        self.cache.pending.push(Pending { key, pa, words, block });
        if self.cache.pending.len() < self.cfg.batch {
            // The waiting blocks stay watched: a write removes them
            // from `pending` (`drain`).
            self.cache.pages.entry(page).or_default().push(key);
            return Err(Next::Cold);
        }
        self.compile_pending();
        self.compiled_block(key, pa)
    }

    fn compiled_block(&self, key: Key, pa: u64) -> Result<Rc<Compiled<E::Module>>, Next> {
        self.cache
            .blocks
            .get(&key)
            .and_then(|e| e.variants.iter().find(|v| v.pa == pa))
            .and_then(|v| v.block.clone())
            .ok_or(Next::Cold)
    }

    /// Compiles the waiting blocks into one module.
    fn compile_pending(&mut self) {
        let n = self.cache.pending.len() as u32;
        self.cache.pending_hits = 0;
        if n == 0 {
            return;
        }
        assert!(n as usize <= self.cfg.batch, "a module has at most `batch` regions");
        let blocks: Vec<Region> = self.cache.pending.iter().map(|p| p.block.clone()).collect();
        let wasm = translate::module_with(&blocks, self.cfg.memory, self.cfg.names);
        self.cache.stats.wasm_bytes += wasm.len() as u64;
        // Table full or engine full (the browser's code budget, wasmtime's
        // instances per store): first the modules not entered since the last
        // eviction go (ADR 0041), then, if that is not enough, everything.
        let mut chunk = self.alloc_chunk();
        if chunk.is_none() && self.evict() {
            chunk = self.alloc_chunk();
        }
        let mut compiled = chunk.and_then(|_| self.engine.compile(&wasm).ok());
        if chunk.is_some() && compiled.is_none() && self.evict() {
            compiled = self.engine.compile(&wasm).ok();
        }
        let (module, chunk) = match (compiled, chunk) {
            (Some(m), Some(k)) => (m, k),
            _ => {
                self.reset();
                let k = self.alloc_chunk().expect("an empty table has chunks");
                match self.engine.compile(&wasm) {
                    Ok(m) => (m, k),
                    Err(e) => panic!("JIT module of {} bytes rejected by the engine: {e}", wasm.len()),
                }
            }
        };
        let base = chunk * self.cfg.batch as u32;
        self.engine.place(&module, n, base);
        self.cache.stats.modules += 1;
        let ready = Rc::new(Cell::new(self.engine.ready(&module)));
        // Marked when one of its regions is looked up (not the first entry,
        // which follows the compilation).
        let module = Rc::new(Mod { m: module, used: Cell::new(false) });
        self.cache.modules.push((Rc::downgrade(&module), chunk));
        self.cache.since_sweep += 1;
        if self.cache.since_sweep >= SWEEP_MODULES {
            self.refresh_marks();
        }
        if !ready.get() {
            self.compiling.push((module.clone(), ready.clone()));
        }
        let pending = std::mem::take(&mut self.cache.pending);
        for (i, p) in pending.into_iter().enumerate() {
            let slot = base + i as u32;
            let first = p.block.entry_index();
            let mut es: Vec<(u64, Rc<Compiled<E::Module>>)> = p
                .block
                .entries()
                .into_iter()
                .map(|(epc, bb, max)| {
                    let c = Compiled {
                        module: module.clone(),
                        ready: ready.clone(),
                        slot,
                        max_steps: max as u8,
                        bb: bb as u8,
                    };
                    (epc, Rc::new(c))
                })
                .collect();
            let at = es.iter().position(|e| e.1.bb as u32 == first).expect("region entry");
            es.swap(0, at);
            let es: Entries<E::Module> = es.into();
            self.cache.stats.blocks += 1;
            self.cache.compiled.insert((p.key, p.words), es.clone());
            self.cache.record_entries(p.key, p.pa, &es);
        }
    }

    /// Marks the modules the engine finished compiling (ADR 0038). Readiness
    /// only changes between runs, so no jump cache entry of the run can
    /// name a module that is not ready.
    fn poll_compiling(&mut self) {
        if self.compiling.is_empty() {
            return;
        }
        let engine = &mut self.engine;
        self.compiling.retain(|(m, r)| {
            if engine.ready(&m.m) {
                r.set(true);
                false
            } else {
                true
            }
        });
    }

    fn compile_dispatcher(&mut self) {
        let wasm = translate::dispatcher_with(self.cfg.memory, self.cfg.names);
        let d = match self.engine.compile(&wasm) {
            Ok(d) => d,
            Err(_) => {
                self.reset();
                self.engine.compile(&wasm).expect("JIT dispatcher rejected by the engine")
            }
        };
        self.dispatcher = Some(d);
    }

    /// A free chunk of `batch` table entries: one never used, or one of an
    /// evicted module nothing refers to any more.
    fn alloc_chunk(&mut self) -> Option<u32> {
        let c = &mut self.cache;
        if c.free_chunks.is_empty() {
            let mut i = 0;
            while i < c.retired.len() {
                if c.retired[i].0.strong_count() == 0 {
                    c.free_chunks.push(c.retired.swap_remove(i).1);
                } else {
                    i += 1;
                }
            }
        }
        if let Some(k) = c.free_chunks.pop() {
            return Some(k);
        }
        let per = self.cfg.batch as u32;
        if (c.next_chunk + 1) * per <= TABLE_SIZE {
            c.next_chunk += 1;
            return Some(c.next_chunk - 1);
        }
        None
    }

    /// The jump cache entries lose their context (new epoch, ADR 0041): every
    /// region entered from now on is looked up by the host at least once and
    /// marks its module, also the ones that never left the jump cache.
    fn refresh_marks(&mut self) {
        self.cache.since_sweep = 0;
        self.new_epoch();
    }

    /// Evicts the modules none of whose regions was entered since the last
    /// eviction (ADR 0041), or, if all of them were, the older half: their
    /// regions leave the cache (they are translated again when hot), the jump
    /// cache entries lose their context, and the engine frees each module
    /// when its last reference goes. The marks of the others are cleared.
    /// False if there was nothing to evict.
    fn evict(&mut self) -> bool {
        let c = &mut self.cache;
        c.modules.retain(|(w, k)| {
            let alive = w.strong_count() > 0;
            if !alive {
                c.free_chunks.push(*k);
            }
            alive
        });
        if c.modules.is_empty() {
            return false;
        }
        // The lookups mark their entries: now the modules.
        for e in c.blocks.values_mut() {
            if std::mem::take(&mut e.used) {
                for v in &e.variants {
                    if let Some(b) = &v.block {
                        b.module.used.set(true);
                    }
                }
            }
        }
        let unused = c.modules.iter().filter(|(w, _)| w.upgrade().is_some_and(|m| !m.used.get())).count();
        let old_half = if unused == 0 { c.modules.len().div_ceil(2) } else { 0 };
        let mut dead = std::collections::HashSet::new();
        let mut live = Vec::with_capacity(c.modules.len());
        for (i, (w, k)) in c.modules.drain(..).enumerate() {
            let m = w.upgrade().expect("alive");
            if i < old_half || (unused > 0 && !m.used.get()) {
                dead.insert(Rc::as_ptr(&m) as usize);
                c.retired.push((w, k));
            } else {
                live.push((w, k));
            }
        }
        c.modules = live;
        let is_dead = |m: &Rc<Mod<E::Module>>| dead.contains(&(Rc::as_ptr(m) as usize));
        for e in c.blocks.values_mut() {
            e.variants.retain(|v| v.block.as_ref().is_none_or(|b| !is_dead(&b.module)));
            e.hot = None;
        }
        // Without resets the cold entries would pile up: those below the
        // threshold with nothing compiled go (their count starts again), and
        // the page index is rebuilt from what remains.
        let threshold = c.hot_threshold;
        c.blocks.retain(|_, e| !e.variants.is_empty() || e.seen >= threshold);
        c.pages.clear();
        for (k, e) in &c.blocks {
            for v in &e.variants {
                c.pages.entry(v.pa >> 12).or_default().push(*k);
            }
        }
        for p in &c.pending {
            c.pages.entry(p.pa >> 12).or_default().push(p.key);
        }
        c.compiled.retain(|_, es| !is_dead(&es[0].1.module));
        self.compiling.retain(|(m, _)| !is_dead(m));
        c.stats.evictions += 1;
        c.stats.evicted_modules += dead.len() as u64;
        for (w, _) in &c.modules {
            if let Some(m) = w.upgrade() {
                m.used.set(false);
            }
        }
        self.refresh_marks();
        true
    }

    /// Discards all compiled code and resets the engine (table full or
    /// engine full). The pages stay watched: at most a few
    /// empty invalidations.
    fn reset(&mut self) {
        let c = &mut self.cache;
        c.blocks.clear();
        c.pages.clear();
        c.compiled.clear();
        c.next_chunk = 0;
        c.free_chunks.clear();
        c.modules.clear();
        c.retired.clear();
        c.since_sweep = 0;
        c.stats.resets += 1;
        self.compiling.clear();
        self.dispatcher = None;
        self.engine.reset();
        self.engine.reserve(self.cfg.state_addr as usize + area::SIZE as usize);
        self.ram_key = None;
        self.init_area();
        self.new_epoch();
        // The waiting blocks count again in the pages.
        let c = &mut self.cache;
        for i in 0..c.pending.len() {
            let (k, pa) = (c.pending[i].key, c.pending[i].pa);
            c.pages.entry(pa >> 12).or_default().push(k);
        }
    }
}

/// The system-mode JIT as an object (for `vetro-machine`, which
/// does not know the engine).
pub trait SysJitDyn {
    fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun;
    fn stats(&self) -> SysJitStats;
    /// True if it counts the interpreter's instructions ([`SysJit::profile_step`]).
    fn profiling(&self) -> bool {
        false
    }
    fn profile_step(&mut self, _cpu: &Cpu, _mmu: &mut Mmu, _phys: &mut dyn SysPhys) {}
    /// The clock for the next run ([`SysJit::set_time`]).
    fn set_time(&mut self, _c: Clock) {}
    /// Addresses that the regions do not contain ([`SysJit::set_stops`]).
    fn set_stops(&mut self, stops: &[u64]);
    /// WFE and YIELD left to the interpreter ([`SysJit::set_yields`]).
    fn set_yields(&mut self, on: bool);
    /// Code for cores in parallel ([`SysJit::set_parallel`]).
    fn set_parallel(&mut self, _on: bool) {}
    /// The flag that ends a run ([`SysJit::set_abort`]).
    fn set_abort(&mut self, _flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {}
    /// Address of the step limit ([`SysJit::limit_addr`]), 0 if none.
    fn limit_addr(&mut self) -> usize {
        0
    }
    /// Forgets the software TLB's write entries ([`SysJit::flush_writes`]).
    fn flush_writes(&mut self) {}
    fn profile(&self) -> Option<&Profile> {
        None
    }
}

impl<E: Engine> SysJitDyn for SysJit<E> {
    fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun {
        SysJit::run(self, cpu, mmu, phys, budget)
    }
    fn stats(&self) -> SysJitStats {
        self.cache.stats
    }
    fn profiling(&self) -> bool {
        self.profile.is_some()
    }
    fn set_time(&mut self, c: Clock) {
        SysJit::set_time(self, c)
    }
    fn set_stops(&mut self, stops: &[u64]) {
        SysJit::set_stops(self, stops)
    }
    fn set_yields(&mut self, on: bool) {
        SysJit::set_yields(self, on)
    }
    fn set_parallel(&mut self, on: bool) {
        SysJit::set_parallel(self, on)
    }
    fn set_abort(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        SysJit::set_abort(self, flag)
    }
    fn limit_addr(&mut self) -> usize {
        SysJit::limit_addr(self)
    }
    fn flush_writes(&mut self) {
        SysJit::flush_writes(self)
    }
    fn profile_step(&mut self, cpu: &Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys) {
        SysJit::profile_step(self, cpu, mmu, phys)
    }
    fn profile(&self) -> Option<&Profile> {
        SysJit::profile(self)
    }
}
