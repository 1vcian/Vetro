//! `JitCpu`: user-mode execution that alternates translated blocks and
//! interpreter steps (ADR 0012).
//!
//! # Contract
//! [`JitCpu::run`] executes at most `budget` steps and stops after the first
//! step that raises an exception. The result is identical to calling
//! `Cpu::step` the same number of times: same registers, same memory,
//! same exception (it is always produced by the interpreter: a block that faults
//! leaves the state as it was before the instruction and the interpreter retries it).
//! The caller (the emulated kernel) counts steps as it did with the
//! interpreter, so the guest clock does not change.
//!
//! # Cache and invalidation
//! - Blocks are looked up by (address space, `pc`): the space is
//!   [`UserMemory::space_id`], which changes on every copy (fork).
//! - A block lies within a 4 KiB page, which is watched with
//!   [`UserMemory::watch_code`]. Every change to the page (guest store,
//!   write by the emulated kernel, munmap, mprotect, mremap...) puts it
//!   among the dirty pages; before every block and after every interpreter
//!   step the dirty pages are collected and their blocks are
//!   discarded. A store by a block to a watched page makes the block exit
//!   after the instruction (`STOP`).
//! - Code in shared pages (MAP_SHARED) is not translated: it can
//!   change from another space.
//! - The same block (same `pc`, same words) in different spaces reuses the
//!   already compiled module: after a fork or an exec of the same binary nothing
//!   is recompiled.
//! - A block is compiled after `hot_threshold` executions of its start
//!   with the interpreter (0 = immediately, for the parity tests).
//!
//! # Chaining (ADR 0026)
//! As in system mode: the regions live in the engine's table
//! (`Engine::place`) and the dispatcher (`translate::dispatcher`) goes
//! from one to the other with the jump cache in `JitState` (`area::JC`),
//! without returning to the host. An entry is valid for the context of the
//! address space (`Space::ctx`), which changes on every invalidation of
//! its pages: a space's entries stay good when the scheduler comes back
//! to it. Missing entries are written by the host (`Host::resolve`) if the region
//! is already compiled.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use vetro_cpu::{Cpu, Exception, Memory, UserMemory};

use crate::engine::{Engine, Host, TABLE_SIZE};
use crate::profile::Profile;
use crate::state::{self, JitState, area, off};
use crate::translate::{self, MAX_REGION};
use crate::wasm::MemoryImport;
use crate::{FAULT, NEXT, STOP, SVC};

#[derive(Clone, Copy, Debug)]
pub struct JitConfig {
    /// Executions with the interpreter before compiling a block.
    pub hot_threshold: u32,
    /// How the modules import the memory (shared in the browser with
    /// threads).
    pub memory: MemoryImport,
    /// Address of `JitState` in the engine's memory (aligned to 16).
    pub state_addr: u32,
    /// Counts the interpreter's instructions per class ([`Profile`]).
    pub profile: bool,
}

impl Default for JitConfig {
    fn default() -> Self {
        JitConfig {
            hot_threshold: 16,
            memory: MemoryImport { min: 1, shared_max: None },
            state_addr: 16,
            profile: false,
        }
    }
}

/// Counters, for measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitStats {
    /// Steps executed by the translated blocks.
    pub jit_steps: u64,
    /// Steps executed by the interpreter.
    pub interp_steps: u64,
    /// Block runs.
    pub block_runs: u64,
    /// Modules compiled.
    pub compiled: u64,
    /// Blocks reused from another space without recompiling.
    pub reused: u64,
    /// Pages invalidated.
    pub invalidated_pages: u64,
    /// Exits due to faults and to stores to code.
    pub faults: u64,
    pub stops: u64,
    /// Engine resets (all compiled code discarded).
    pub resets: u64,
    /// Jump cache entries requested by the dispatcher from the host.
    pub resolves: u64,
}

/// Outcome of looking up a block.
enum Look<M> {
    Hot(Rc<Compiled<M>>),
    /// To translate now.
    Translate,
    /// To the interpreter (cold).
    Interp,
    /// To the interpreter: the first instruction is not translated (a region may
    /// start after it).
    One,
}

/// Hash for u64 keys (addresses, pages): the block lookup happens on
/// every run, SipHash would cost as much as the block.
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
        // Fibonacci multiplication and folding: the low bits (the ones
        // the table uses) depend on the whole address.
        let h = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type FastMap<V> = HashMap<u64, V, BuildHasherDefault<U64Hasher>>;

/// An entry of a compiled region.
struct Compiled<M> {
    _module: Rc<M>,
    /// Entry of the engine's table.
    slot: u32,
    /// Maximum steps of the base entry block.
    max_steps: u64,
    /// Index of the base entry block (`JitState::entry`).
    bb: u32,
}

/// Jump cache entry (`area::JC`): `pc` → region, valid for
/// `ctx` (format of `docs/specs/jit.md`).
fn install_jc<M>(mem: &mut [u8], at: usize, pc: u64, ctx: u32, c: &Compiled<M>) {
    let e = at + area::JC as usize + ((pc >> 2) & (area::JC_ENTRIES as u64 - 1)) as usize * 16;
    mem[e..e + 8].copy_from_slice(&pc.to_le_bytes());
    mem[e + 8..e + 12].copy_from_slice(&ctx.to_le_bytes());
    let w = c.bb << 26 | c.slot << 8 | c.max_steps as u32;
    mem[e + 12..e + 16].copy_from_slice(&w.to_le_bytes());
}

/// The entries of a compiled region; the first is its start.
type Entries<M> = Rc<[(u64, Rc<Compiled<M>>)]>;

enum Entry<M> {
    /// Seen `n` times, not yet compiled.
    Cold(u32),
    Hot(Rc<Compiled<M>>),
    /// The first instruction is not translated: the interpreter executes it.
    NoBlock,
}

struct Space<M> {
    blocks: FastMap<Entry<M>>,
    /// Page → starts of the compiled blocks that lie in it.
    pages: FastMap<Vec<u64>>,
    last_use: u64,
    /// Context of this space's jump cache entries.
    ctx: u32,
}

impl<M> Space<M> {
    fn new(ctx: u32) -> Self {
        Space { blocks: FastMap::default(), pages: FastMap::default(), last_use: 0, ctx }
    }

    fn invalidate(&mut self, page: u64) -> bool {
        match self.pages.remove(&page) {
            Some(pcs) => {
                for pc in pcs {
                    self.blocks.remove(&pc);
                }
                true
            }
            None => false,
        }
    }
}

/// Identity of a compiled block: address and instruction words.
type BlockKey = (u64, Vec<u32>);

/// Address spaces kept in cache (the others are discarded, least
/// recently used first).
const MAX_SPACES: usize = 64;

/// User-mode JIT executor.
pub struct JitCpu<E: Engine> {
    engine: E,
    cfg: JitConfig,
    spaces: FastMap<Space<E::Module>>,
    /// Already compiled modules, by (pc, block words).
    compiled: HashMap<BlockKey, Entries<E::Module>>,
    tick: u64,
    pub stats: JitStats,
    /// Interpreter instructions per class, if `cfg.profile`.
    pub profile: Option<Profile>,
    /// The dispatcher (compiled at the first run and after every reset).
    dispatcher: Option<E::Module>,
    /// Next free entry of the engine's table.
    next_slot: u32,
    /// Last context assigned to a space.
    ctx_seq: u32,
}

/// `Host` on top of user memory.
struct MemHost<'a, M> {
    mem: &'a mut UserMemory,
    /// The SIMD/FP registers of the `Cpu` (for `vsync`).
    v: &'a [u128; 32],
    /// The run's space (for `resolve`) and the address of `JitState`.
    space: &'a Space<M>,
    at: usize,
    resolves: &'a mut u64,
}

impl<M> Host for MemHost<'_, M> {
    #[inline]
    fn ld(&mut self, _mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()> {
        let mut b = [0u8; 8];
        self.mem.read(va, &mut b[..size as usize]).map_err(|_| ())?;
        Ok(u64::from_le_bytes(b))
    }

    #[inline]
    fn st(&mut self, _mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()> {
        self.mem.write(va, &value.to_le_bytes()[..size as usize]).map_err(|_| ())?;
        Ok(self.mem.code_dirty())
    }

    fn vsync(&mut self, mem: &mut [u8], state: u32) {
        state::vsync_in(mem, state as usize, self.v);
    }

    /// Missing jump cache entry: if the region of `pc` is already
    /// compiled in this space, writes it with the run's context.
    fn resolve(&mut self, mem: &mut [u8]) -> bool {
        let pc = state::read_u64(mem, self.at, off::PC);
        *self.resolves += 1;
        match self.space.blocks.get(&pc) {
            Some(Entry::Hot(c)) => {
                let ctx = state::read_u32(mem, self.at, off::CTX);
                install_jc(mem, self.at, pc, ctx, c);
                true
            }
            _ => false,
        }
    }
}

impl<E: Engine> JitCpu<E> {
    pub fn new(mut engine: E, cfg: JitConfig) -> Self {
        assert!(cfg.state_addr.is_multiple_of(16), "JitState must be aligned to 16 bytes");
        engine.reserve(cfg.state_addr as usize + area::SIZE as usize);
        engine.runtime(&translate::runtime(cfg.memory)).expect("JIT runtime rejected by the engine");
        let at = cfg.state_addr as usize;
        engine.memory()[at..at + area::SIZE as usize].fill(0);
        JitCpu {
            engine,
            cfg,
            spaces: FastMap::default(),
            compiled: HashMap::new(),
            tick: 0,
            stats: JitStats::default(),
            profile: cfg.profile.then(|| {
                crate::helper::profile(true);
                Profile::default()
            }),
            dispatcher: None,
            next_slot: 0,
            ctx_seq: 0,
        }
    }

    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    /// Discards the blocks of the changed pages.
    fn drain(&mut self, space: u64, mem: &mut UserMemory) {
        if !mem.code_dirty() {
            return;
        }
        let dirty = mem.take_code_dirty();
        let mut any = false;
        if let Some(s) = self.spaces.get_mut(&space) {
            for p in dirty {
                if s.invalidate(p) {
                    self.stats.invalidated_pages += 1;
                    any = true;
                }
            }
        }
        if any {
            // This space's jump cache entries are no longer valid.
            let ctx = self.next_ctx();
            if let Some(s) = self.spaces.get_mut(&space) {
                s.ctx = ctx;
            }
        }
    }

    /// New context for the jump cache entries. When the values run out
    /// the cache is emptied and everything starts over (the spaces get new
    /// contexts).
    fn next_ctx(&mut self) -> u32 {
        if self.ctx_seq == u32::MAX {
            let at = self.cfg.state_addr as usize;
            let jc = at + area::JC as usize;
            self.engine.memory()[jc..jc + area::JC_ENTRIES as usize * 16].fill(0);
            self.ctx_seq = 0;
            let ids: Vec<u64> = self.spaces.keys().copied().collect();
            for id in ids {
                self.ctx_seq += 1;
                let c = self.ctx_seq;
                self.spaces.get_mut(&id).expect("space").ctx = c;
            }
        }
        self.ctx_seq += 1;
        self.ctx_seq
    }

    fn space(&mut self, id: u64) -> &mut Space<E::Module> {
        self.tick += 1;
        if !self.spaces.contains_key(&id) && self.spaces.len() >= MAX_SPACES {
            let oldest = self.spaces.iter().min_by_key(|(_, s)| s.last_use).map(|(&k, _)| k);
            if let Some(k) = oldest {
                self.spaces.remove(&k);
            }
        }
        let tick = self.tick;
        if !self.spaces.contains_key(&id) {
            let ctx = self.next_ctx();
            self.spaces.insert(id, Space::new(ctx));
        }
        let s = self.spaces.get_mut(&id).expect("just inserted");
        s.last_use = tick;
        s
    }

    /// Executes at most `budget` steps (at least one if `budget > 0`); stops
    /// after the first step that raises an exception. Returns the steps executed
    /// (including the one with the exception) and the outcome of the last one.
    pub fn run(&mut self, cpu: &mut Cpu, mem: &mut UserMemory, budget: u64) -> (u64, Result<(), Exception>) {
        let id = mem.space_id();
        self.drain(id, mem);
        self.space(id);
        let at = self.cfg.state_addr as usize;
        let mut done = 0u64;
        // True if the updated state is in JitState and not in the Cpu.
        let mut in_jit = false;
        // `pc` of JitState when `in_jit` (read together with `steps`).
        let mut jit_pc = 0;
        while done < budget {
            let pc = if in_jit { jit_pc } else { cpu.pc };
            let mut one = false;
            let hot = match self.lookup(id, pc) {
                Look::Hot(c) => Some(c),
                Look::Interp => None,
                Look::One => {
                    one = true;
                    None
                }
                Look::Translate => {
                    // Compiling may reset the engine (and its
                    // memory): first the state goes back into the Cpu.
                    if in_jit {
                        JitState::load(self.engine.memory(), at).to_cpu(cpu);
                        in_jit = false;
                    }
                    let c = self.install(id, pc, mem);
                    one = c.is_none();
                    c
                }
            };
            if let Some(c) = &hot
                && c.max_steps <= budget - done
            {
                if self.dispatcher.is_none() {
                    if in_jit {
                        JitState::load(self.engine.memory(), at).to_cpu(cpu);
                        in_jit = false;
                    }
                    self.compile_dispatcher();
                    // Compiling may have reset the engine (and the
                    // spaces): look up again.
                    self.space(id);
                    continue;
                }
                let ctx = self.spaces.get(&id).expect("space of the run").ctx;
                let m = self.engine.memory();
                install_jc(m, at, pc, ctx, c);
                if !in_jit {
                    let mut s = JitState::from_cpu(cpu);
                    s.limit = budget - done;
                    s.ctx = ctx;
                    s.store(m, at);
                    in_jit = true;
                } else {
                    state::write_u64(m, at, off::STEPS, 0);
                    state::write_u64(m, at, off::LIMIT, budget - done);
                    state::write_u32(m, at, off::CTX, ctx);
                    state::write_u32(m, at, off::EXIT_DETAIL, 0);
                }
                let code = {
                    let space = self.spaces.get(&id).expect("space of the run");
                    let mut host = MemHost { mem, v: &cpu.v, space, at, resolves: &mut self.stats.resolves };
                    let d = self.dispatcher.as_ref().expect("dispatcher compiled");
                    self.engine.run(d, 0, self.cfg.state_addr, &mut host)
                };
                let m = self.engine.memory();
                let steps = state::read_u64(m, at, off::STEPS);
                jit_pc = state::read_u64(m, at, off::PC);
                debug_assert!(steps <= budget - done);
                done += steps;
                self.stats.jit_steps += steps;
                self.stats.block_runs += 1;
                match code {
                    NEXT => {}
                    STOP => {
                        self.stats.stops += 1;
                        self.drain(id, mem);
                    }
                    FAULT | SVC => {
                        if code == FAULT {
                            self.stats.faults += 1;
                        }
                        JitState::load(self.engine.memory(), at).to_cpu(cpu);
                        in_jit = false;
                        self.drain(id, mem);
                        if done < budget {
                            let r = self.interp_step(id, cpu, mem);
                            done += 1;
                            if r.is_err() {
                                return (done, r);
                            }
                        }
                    }
                    other => panic!("unknown block exit code: {other}"),
                }
                continue;
            }
            if in_jit {
                JitState::load(self.engine.memory(), at).to_cpu(cpu);
                in_jit = false;
            }
            // Interpreter until the end of the block (jump, page, budget).
            loop {
                let old = cpu.pc;
                let r = self.interp_step(id, cpu, mem);
                done += 1;
                if r.is_err() {
                    return (done, r);
                }
                if done >= budget || cpu.pc != old.wrapping_add(4) || cpu.pc >> 12 != old >> 12 {
                    break;
                }
                if hot.is_some() || one {
                    // Block compiled but longer than the remaining budget, or
                    // untranslatable instruction: one step, then look up
                    // again (a region may start right after).
                    break;
                }
            }
        }
        if in_jit {
            JitState::load(self.engine.memory(), at).to_cpu(cpu);
        }
        (done, Ok(()))
    }

    /// Discards all blocks of all spaces and resets the engine. The
    /// pages stay watched: at most a few empty invalidations.
    fn reset(&mut self) {
        self.spaces.clear();
        self.compiled.clear();
        self.dispatcher = None;
        self.next_slot = 0;
        self.engine.reset();
        let at = self.cfg.state_addr as usize;
        self.engine.reserve(at + area::SIZE as usize);
        self.engine.memory()[at..at + area::SIZE as usize].fill(0);
        self.ctx_seq = 0;
        self.stats.resets += 1;
    }

    fn compile_dispatcher(&mut self) {
        let wasm = translate::dispatcher(self.cfg.memory);
        let d = match self.engine.compile(&wasm) {
            Ok(d) => d,
            Err(_) => {
                self.reset();
                self.engine.compile(&wasm).expect("JIT dispatcher rejected by the engine")
            }
        };
        self.dispatcher = Some(d);
    }

    fn interp_step(&mut self, id: u64, cpu: &mut Cpu, mem: &mut UserMemory) -> Result<(), Exception> {
        if let Some(p) = &mut self.profile
            && let Ok(w) = mem.fetch(cpu.pc)
        {
            p.note(w, None);
        }
        let r = cpu.step(mem);
        self.stats.interp_steps += 1;
        self.drain(id, mem);
        r
    }

    /// Looks up the block of `pc` and counts the executions of cold ones.
    fn lookup(&mut self, id: u64, pc: u64) -> Look<E::Module> {
        let threshold = self.cfg.hot_threshold;
        let s = self.spaces.get_mut(&id).expect("space created by run");
        match s.blocks.get_mut(&pc) {
            Some(Entry::Hot(c)) => Look::Hot(c.clone()),
            Some(Entry::NoBlock) => Look::One,
            Some(Entry::Cold(n)) => {
                *n += 1;
                if *n < threshold { Look::Interp } else { Look::Translate }
            }
            None => {
                if threshold > 1 {
                    s.blocks.insert(pc, Entry::Cold(1));
                    Look::Interp
                } else {
                    Look::Translate
                }
            }
        }
    }

    /// Translates and installs the block of `pc` (or marks it untranslatable).
    fn install(&mut self, id: u64, pc: u64, mem: &mut UserMemory) -> Option<Rc<Compiled<E::Module>>> {
        let es = self.translate(pc, mem);
        // After an engine reset the space must be recreated.
        let s = self.space(id);
        match es {
            Some(es) => {
                // The start, and the other base blocks where there is not already a
                // compiled block.
                for (i, (epc, c)) in es.iter().enumerate() {
                    if i > 0 && matches!(s.blocks.get(epc), Some(Entry::Hot(_))) {
                        continue;
                    }
                    s.blocks.insert(*epc, Entry::Hot(c.clone()));
                    s.pages.entry(pc >> 12).or_default().push(*epc);
                }
                mem.watch_code(pc >> 12);
                Some(es[0].1.clone())
            }
            None => {
                s.blocks.insert(pc, Entry::NoBlock);
                None
            }
        }
    }

    /// Reads, decodes and compiles the block that starts at `pc`.
    fn translate(&mut self, pc: u64, mem: &mut UserMemory) -> Option<Entries<E::Module>> {
        let (block, words) = translate::discover(pc, None, MAX_REGION, |a| {
            if !mem.is_private(a) {
                return None;
            }
            mem.fetch(a).ok()
        })?;
        let key = (pc, words);
        if let Some(es) = self.compiled.get(&key) {
            self.stats.reused += 1;
            return Some(es.clone());
        }
        let wasm = translate::module(std::slice::from_ref(&block), self.cfg.memory);
        if self.next_slot + 1 > TABLE_SIZE {
            self.reset();
        }
        let module = match self.engine.compile(&wasm) {
            Ok(m) => m,
            Err(_) => {
                // Engine full (wasmtime: instances per store): all compiled
                // code is discarded and we retry once.
                self.reset();
                match self.engine.compile(&wasm) {
                    Ok(m) => m,
                    Err(e) => panic!("JIT module rejected by the engine at pc={pc:#x}: {e}"),
                }
            }
        };
        self.stats.compiled += 1;
        // In the dispatcher's table (after a possible reset).
        let slot = self.next_slot;
        self.engine.place(&module, 1, slot);
        self.next_slot += 1;
        let module = Rc::new(module);
        let first = block.entry_index();
        let mut es: Vec<(u64, Rc<Compiled<E::Module>>)> = block
            .entries()
            .into_iter()
            .map(|(epc, bb, max_steps)| {
                (epc, Rc::new(Compiled { _module: module.clone(), slot, max_steps, bb }))
            })
            .collect();
        let at = es.iter().position(|e| e.1.bb == first).expect("region entry");
        es.swap(0, at);
        let es: Entries<E::Module> = es.into();
        self.compiled.insert(key, es.clone());
        Some(es)
    }
}
