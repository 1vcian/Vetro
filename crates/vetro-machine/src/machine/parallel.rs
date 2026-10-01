//! Cores in parallel on host threads (ADR 0042, step 3 of ADR 0038).
//!
//! [`Machine::start_parallel`] splits a machine with several cores: core 0
//! keeps running in [`Machine::run`] on the host's thread, every other core
//! becomes a [`Core`] that the host runs on a thread of its own
//! ([`Core::run`]), each with its own registers, MMU (TLB) and JIT, all on the
//! same RAM. What they share:
//!
//! - **RAM**, without a lock (`board::Ram`): plain accesses, single-copy
//!   atomic for 1–8 aligned bytes; store-exclusives are atomic
//!   compare-and-exchanges against the value of the exclusive load.
//! - **Devices** behind the board's lock: MMIO, the GIC's CPU interfaces and
//!   the timers' system registers. The IRQ lines are computed under the lock
//!   whenever the GIC may have changed, and a core whose line rose is kicked
//!   (out of its JIT run, out of a WFI wait).
//! - **Time**: one clock, every core's instructions added to it (CNTPCT is
//!   that of `clock / n`, like the cores in turns); when every core waits in
//!   WFI time jumps to the earliest deadline.
//! - **TLB and code coherence**: a broadcast TLBI is applied by every other
//!   core (kicked, between two runs) before the issuer goes on; a page that
//!   gets translated code is first made unwritable through the other cores'
//!   software TLBs, then translated; written code pages reach every JIT.
//! - **Memory ordering**: the JIT's regions use fences for the barriers and
//!   atomic accesses for LDAR/STLR (`SysJit::set_parallel`); the interpreter
//!   ends every instruction with a sequentially consistent fence.
//!
//! What it gives up: determinism. The interleaving depends on the host, so
//! recording and replay, snapshots and introspection hooks need the cores
//! back in turns ([`Machine::stop_parallel`]), which is always possible
//! between two runs.

use core::sync::atomic::{Ordering, fence};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vetro_cpu::Cpu;
use vetro_cpu::sys::SysEvent;
use vetro_jit::{Next, SysJitDyn};
use vetro_mmu::{Mmu, MmuBus};

use super::{Machine, PA_BITS, Stop, counter, smp, steps_for};
use crate::board::{BoardCell, Cores, Env, Phys, Request};
use crate::psci::{self, Call};

/// Most instructions of one JIT run or interpreter stretch before the core
/// looks at its requests and the clock again.
const SLICE: u64 = 1 << 15;

/// Longest a core waits in WFI on the host's own thread (core 0) before
/// returning to the host, which must keep handling its inputs.
const MAIN_WAIT: Duration = Duration::from_millis(4);

/// One core's execution state while the cores run in parallel.
pub(crate) struct ParCore {
    pub idx: usize,
    pub cpu: Cpu,
    pub mmu: Mmu,
    pub jit: Option<Box<dyn SysJitDyn>>,
    pub interp: Next,
    pub wfi_pending: bool,
    /// Instructions executed and not yet added to the clock.
    pending: u64,
    /// Broadcasts posted by this core still to be acknowledged.
    waits: Vec<(usize, u64)>,
    /// The machine's cores.
    n: u64,
    /// Instructions executed (for measurements).
    pub executed: u64,
}

/// A core other than 0 running on a host thread of its own
/// ([`Machine::start_parallel`]): [`Core::set_jit`] once on that thread, then
/// [`Core::run`] in a loop until it returns [`Stop::PowerOff`] or
/// [`Stop::Reset`], or the host asks the cores to stop
/// ([`Machine::request_stop`], then [`Core::stopped`] is true).
pub struct Core {
    board: Arc<BoardCell>,
    core: ParCore,
}

// SAFETY: a `Core` is moved to its thread before its JIT exists (`set_jit`
// is called there) and the JIT is dropped there (`drop_jit`) before the core
// is handed back; everything else is plain data or `Arc`s of `Sync` data.
unsafe impl Send for Core {}

/// How a stretch of one core ended.
enum Event {
    /// Instructions (or the budget) ran out.
    Budget,
    /// WFI (or CPU_SUSPEND) with nothing to take: wait.
    Wfi,
    /// CPU_OFF.
    Off,
    /// The machine stops (power off or reset), or the host asked.
    Halt,
    Unimplemented {
        pc: u64,
        raw: u32,
        what: &'static str,
    },
}

/// How a WFI wait ended.
enum Wait {
    Woken,
    /// Still waiting, but the caller must return to the host.
    Later,
    /// Every core waits with nothing that can wake it: only the host can.
    Idle,
}

impl ParCore {
    fn new(idx: usize, cpu: Cpu, mmu: Mmu, wfi_pending: bool, n: u64) -> Self {
        ParCore {
            idx,
            cpu,
            mmu,
            jit: None,
            interp: Next::Jit,
            wfi_pending,
            pending: 0,
            waits: Vec::new(),
            n,
            executed: 0,
        }
    }

    /// Adds the pending instructions to the clock and wakes the cores whose
    /// WFI deadline it reached; returns the clock.
    fn flush(&mut self, cores: &Cores) -> u64 {
        let d = core::mem::take(&mut self.pending);
        let c = if d > 0 {
            cores.clock.fetch_add(d, Ordering::SeqCst) + d
        } else {
            cores.clock.load(Ordering::SeqCst)
        };
        if d > 0 && cores.idle.load(Ordering::SeqCst) > 0 {
            wake_due(cores, c);
        }
        c
    }

    /// CNTPCT at clock value `clock`.
    fn counter(&self, clock: u64) -> u64 {
        counter(clock / self.n)
    }

    /// The device side at the current time: the counter, the lines (its
    /// timer's included), devices touched since; its next timer deadline.
    fn sync(&mut self, cell: &BoardCell) {
        let c = self.flush(&cell.cores);
        let now = self.counter(c);
        let mut b = cell.borrow_mut();
        b.cntpct = b.cntpct.max(now);
        if b.virtio_dirty {
            b.service_virtio();
        }
        b.update_irqs();
    }

    /// Its generic timer's next deadline (CNTPCT), as of the last
    /// `update_irqs` by any core.
    fn deadline(&self, cores: &Cores) -> Option<u64> {
        let d = cores.slots[self.idx].deadline.load(Ordering::SeqCst);
        (d != u64::MAX).then_some(d)
    }

    /// Applies the other cores' requests (TLBIs, newly watched pages) and
    /// acknowledges them.
    fn service(&mut self, cores: &Cores) {
        let reqs = cores.take(self.idx);
        if reqs.is_empty() {
            return;
        }
        for r in &reqs {
            match *r {
                Request::Tlbi(op, xt) => self.mmu.tlbi(op, xt),
                Request::Watch(_) => {
                    if let Some(j) = self.jit.as_mut() {
                        j.flush_writes();
                    }
                }
            }
        }
        cores.done(self.idx, reqs.len());
    }

    /// Waits until every core this one broadcast to has applied the
    /// requests, applying its own meanwhile (two cores may wait for each
    /// other). Gives up only if the host stops the cores.
    fn wait_acks(&mut self, cores: &Cores) {
        let mut spins = 0u32;
        while !cores.acked(&self.waits) {
            self.service(cores);
            if cores.stop.load(Ordering::SeqCst) || cores.halt.load(Ordering::SeqCst) != 0 {
                return;
            }
            spins += 1;
            if spins < 64 {
                core::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        self.waits.clear();
    }

    /// IRQ line of this core (the lines are computed eagerly in parallel).
    fn irq(&self, cell: &BoardCell) -> bool {
        use vetro_cpu::sys::CpuEnv;
        Env::new(cell, self.idx).irq_line()
    }

    /// Instructions the JIT may run now (like `Machine::jit_budget`): none
    /// if an interrupt would be taken, otherwise up to `left` and its timer
    /// deadline.
    fn jit_budget(&self, cell: &BoardCell, left: u64) -> Option<u64> {
        let s = &self.cpu.sys;
        if s.il || self.cpu.pc & 3 != 0 {
            return None;
        }
        if s.daif & 1 << 7 == 0 && self.irq(cell) {
            return None;
        }
        if s.daif & 1 << 8 == 0 && s.serror_pending.is_some() {
            return None;
        }
        let mut limit = left;
        if let Some(d) = self.deadline(&cell.cores) {
            let at = steps_for(d).saturating_mul(self.n);
            let now = cell.cores.clock.load(Ordering::SeqCst) + self.pending;
            if at <= now {
                return None;
            }
            limit = limit.min(at - now);
        }
        Some(limit)
    }

    /// Runs up to `left` instructions (one stretch).
    fn stretch(&mut self, cell: &BoardCell, left: u64) -> (u64, Event) {
        let cores = &*cell.cores;
        let slot = &cores.slots[self.idx];
        let mut done = 0u64;
        while done < left {
            if slot.kick_seen() || cores.stop.load(Ordering::SeqCst) || cores.halt.load(Ordering::SeqCst) != 0
            {
                // Requests, an interrupt or a stop: back to the loop.
                return (
                    done,
                    if cores.halt.load(Ordering::SeqCst) != 0 { Event::Halt } else { Event::Budget },
                );
            }
            let now = self.counter(cores.clock.load(Ordering::SeqCst) + self.pending);
            if self.deadline(cores).is_some_and(|d| now >= d) {
                self.sync(cell);
            }
            if self.interp == Next::Jit
                && self.jit.is_some()
                && let Some(limit) = self.jit_budget(cell, left - done)
            {
                slot.abort.store(false, Ordering::SeqCst);
                let jit = self.jit.as_mut().expect("checked above");
                let mut phys = Phys { cell, core: self.idx, consumer: self.idx, waits: &mut self.waits };
                let r = jit.run(&mut self.cpu, &mut self.mmu, &mut phys, limit);
                done += r.steps;
                self.pending += r.steps;
                self.executed += r.steps;
                self.interp = if r.next == Next::Jit && r.steps == 0 { Next::One } else { r.next };
                self.flush(cores);
                if !self.waits.is_empty() {
                    self.wait_acks(cores);
                }
                if r.steps > 0 {
                    continue;
                }
            }
            // One instruction in the interpreter, on the current clock.
            let c = self.flush(cores);
            let now = self.counter(c);
            let old_pc = self.cpu.pc;
            let ev = {
                let mut phys = Phys { cell, core: self.idx, consumer: self.idx, waits: &mut self.waits };
                let mut bus = MmuBus::new(&mut self.mmu, &mut phys);
                let mut env = Env { cell, core: self.idx, now: Some(now) };
                self.cpu.step_system(&mut bus, &mut env)
            };
            // The interpreter's accesses are ordered like the guest's
            // strongest barrier: whatever the instruction was (DMB, LDAR,
            // STLR, an exclusive), other cores see it in order.
            fence(Ordering::SeqCst);
            done += 1;
            self.pending += 1;
            self.executed += 1;
            if !self.waits.is_empty() {
                self.wait_acks(cores);
            }
            if self.interp == Next::Cold {
                let next = old_pc.wrapping_add(4);
                if !(ev == SysEvent::Executed && self.cpu.pc == next && next >> 12 == old_pc >> 12) {
                    self.interp = Next::Jit;
                }
            } else {
                self.interp = Next::Jit;
            }
            match ev {
                SysEvent::Executed | SysEvent::Exception { .. } | SysEvent::Yield => {}
                SysEvent::WaitForInterrupt => return (done, Event::Wfi),
                SysEvent::Hvc(_) | SysEvent::Smc(_) => {
                    let x = [self.cpu.x[0], self.cpu.x[1], self.cpu.x[2], self.cpu.x[3]];
                    match psci::call(x) {
                        Call::Ret(v) => self.cpu.x[0] = v as u64,
                        Call::Suspend => {
                            self.cpu.x[0] = 0;
                            return (done, Event::Wfi);
                        }
                        Call::CpuOn { target, entry, context } => {
                            self.cpu.x[0] = cpu_on(cores, target, entry, context) as u64;
                        }
                        Call::AffinityInfo { target } => {
                            self.cpu.x[0] = match by_affinity(cores, target) {
                                None => psci::RET_INVALID_PARAMS,
                                Some(i) if cores.slots[i].on.load(Ordering::SeqCst) => psci::AFFINITY_ON,
                                Some(_) => psci::AFFINITY_OFF,
                            } as u64;
                        }
                        Call::CpuOff => return (done, Event::Off),
                        Call::SystemOff => {
                            halt(cores, 1);
                            return (done, Event::Halt);
                        }
                        Call::SystemReset => {
                            halt(cores, 2);
                            return (done, Event::Halt);
                        }
                    }
                }
                SysEvent::Unimplemented { raw, what } => {
                    done -= 1;
                    self.pending -= 1;
                    self.executed -= 1;
                    return (done, Event::Unimplemented { pc: self.cpu.pc, raw, what });
                }
            }
        }
        (done, Event::Budget)
    }

    /// WFI: waits for an interrupt, its timer deadline (on the clock the
    /// others advance), a kick or the host. When every core waits, time
    /// jumps to the earliest deadline.
    fn wait(&mut self, cell: &BoardCell, main: bool) -> Wait {
        let cores = &*cell.cores;
        let slot = &cores.slots[self.idx];
        self.sync(cell);
        if self.irq(cell) {
            return Wait::Woken;
        }
        let target = self.deadline(cores).map_or(u64::MAX, |d| steps_for(d).saturating_mul(self.n));
        let seen = slot.kick.load(Ordering::SeqCst);
        slot.wait_until.store(target, Ordering::SeqCst);
        let idle = cores.idle.fetch_add(1, Ordering::SeqCst) + 1;
        let on = cores.slots.iter().filter(|s| s.on.load(Ordering::SeqCst)).count();
        let start = Instant::now();
        let mut out = Wait::Later;
        if idle >= on {
            // Every core waits: time jumps to the earliest deadline.
            let t = cores
                .slots
                .iter()
                .filter(|s| s.on.load(Ordering::SeqCst))
                .map(|s| s.wait_until.load(Ordering::SeqCst))
                .filter(|&w| w != 0)
                .chain([cores.net_deadline.load(Ordering::SeqCst)])
                .min()
                .unwrap_or(u64::MAX);
            if t == u64::MAX {
                if main {
                    out = Wait::Idle;
                }
            } else {
                let c = cores.clock.fetch_max(t, Ordering::SeqCst).max(t);
                wake_due(cores, c);
            }
        }
        if !matches!(out, Wait::Idle) {
            loop {
                if cores.clock.load(Ordering::SeqCst) >= target {
                    out = Wait::Woken;
                    break;
                }
                if slot.kick.load(Ordering::SeqCst) != seen
                    || cores.stop.load(Ordering::SeqCst)
                    || cores.halt.load(Ordering::SeqCst) != 0
                {
                    // An interrupt, a request or the host: the caller looks.
                    out = Wait::Woken;
                    break;
                }
                if main && start.elapsed() >= MAIN_WAIT {
                    out = Wait::Later;
                    break;
                }
                let g = crate::board::lock_slot(&slot.sleep);
                if slot.kick.load(Ordering::SeqCst) == seen && cores.clock.load(Ordering::SeqCst) < target {
                    let _ = slot.wake.wait_timeout(g, Duration::from_millis(if main { 1 } else { 20 }));
                }
            }
        }
        slot.wait_until.store(0, Ordering::SeqCst);
        cores.idle.fetch_sub(1, Ordering::SeqCst);
        out
    }

    /// Runs up to `budget` instructions: what both [`Machine::run`] (core 0)
    /// and [`Core::run`] do.
    fn run(&mut self, cell: &BoardCell, budget: u64, main: bool) -> Stop {
        let cores = &*cell.cores;
        let slot = &cores.slots[self.idx];
        let mut left = budget;
        loop {
            match cores.halt.load(Ordering::SeqCst) {
                1 => return Stop::PowerOff,
                2 => return Stop::Reset,
                _ => {}
            }
            self.service(cores);
            if !self.waits.is_empty() {
                self.wait_acks(cores);
            }
            if cores.stop.load(Ordering::SeqCst) {
                self.flush(cores);
                return Stop::Budget;
            }
            slot.clear_kick();
            if !slot.on.load(Ordering::SeqCst) {
                // Off: waits for PSCI CPU_ON.
                if let Some((entry, context)) = crate::board::lock_slot(&slot.start).take() {
                    self.power_on(entry, context);
                    continue;
                }
                let seen = slot.kick.load(Ordering::SeqCst);
                let g = crate::board::lock_slot(&slot.sleep);
                if slot.kick.load(Ordering::SeqCst) == seen && !slot.on.load(Ordering::SeqCst) {
                    let _ =
                        slot.wake.wait_timeout(g, if main { MAIN_WAIT } else { Duration::from_millis(20) });
                }
                if main {
                    return Stop::Budget;
                }
                continue;
            }
            if let Some(start) = crate::board::lock_slot(&slot.start).take() {
                // A CPU_ON that raced with the core's own power-off.
                self.power_on(start.0, start.1);
            }
            if left == 0 {
                self.flush(cores);
                return Stop::Budget;
            }
            if self.wfi_pending {
                match self.wait(cell, main) {
                    Wait::Woken => self.wfi_pending = false,
                    Wait::Later => return Stop::Budget,
                    Wait::Idle => return Stop::Idle,
                }
                continue;
            }
            let (done, ev) = self.stretch(cell, left.min(SLICE));
            left -= done.min(left);
            match ev {
                Event::Budget => {}
                Event::Wfi => self.wfi_pending = true,
                Event::Off => {
                    slot.on.store(false, Ordering::SeqCst);
                    if cores.slots.iter().all(|s| !s.on.load(Ordering::SeqCst)) {
                        halt(cores, 1);
                    }
                }
                Event::Halt => {}
                Event::Unimplemented { pc, raw, what } => {
                    self.flush(cores);
                    return Stop::Unimplemented { pc, raw, what };
                }
            }
        }
    }

    /// PSCI CPU_ON reaches this core: the reset state, at `entry` with x0 =
    /// `context`, with an empty TLB.
    fn power_on(&mut self, entry: u64, context: u64) {
        let mut cpu = Cpu::new();
        cpu.reset_system(smp::sys_config(self.idx));
        cpu.pc = entry;
        cpu.x[0] = context;
        self.cpu = cpu;
        self.mmu.tlb_mut().flush_all();
        self.interp = Next::Jit;
        self.wfi_pending = false;
    }
}

/// Wakes the waiting cores whose deadline the clock `c` reached.
fn wake_due(cores: &Cores, c: u64) {
    for (i, s) in cores.slots.iter().enumerate() {
        let w = s.wait_until.load(Ordering::SeqCst);
        if w != 0 && w <= c {
            cores.kick(i);
        }
    }
}

fn halt(cores: &Cores, how: u8) {
    cores.halt.store(how, Ordering::SeqCst);
    for i in 0..cores.slots.len() {
        cores.kick(i);
    }
}

fn by_affinity(cores: &Cores, target: u64) -> Option<usize> {
    (0..cores.slots.len()).find(|&i| vetro_platform::gic::cpu_affinity(i) == target)
}

/// PSCI CPU_ON from a core in parallel: the target starts at its next look.
fn cpu_on(cores: &Cores, target: u64, entry: u64, context: u64) -> i64 {
    let Some(i) = by_affinity(cores, target) else {
        return psci::RET_INVALID_PARAMS;
    };
    let s = &cores.slots[i];
    let mut start = crate::board::lock_slot(&s.start);
    if s.on.load(Ordering::SeqCst) {
        return psci::RET_ALREADY_ON;
    }
    *start = Some((entry, context));
    s.on.store(true, Ordering::SeqCst);
    drop(start);
    cores.kick(i);
    psci::RET_SUCCESS
}

impl Core {
    /// Index of the core.
    pub fn index(&self) -> usize {
        self.core.idx
    }

    /// The core's JIT, created on the core's thread (the JS engine of a
    /// browser Worker belongs to that Worker); `None` = interpreter only.
    pub fn set_jit(&mut self, jit: Option<Box<dyn SysJitDyn>>) {
        let slot = &self.board.cores.slots[self.core.idx];
        slot.limit.store(0, Ordering::SeqCst);
        self.core.jit = jit.map(|mut j| {
            j.set_yields(false);
            j.set_parallel(true);
            j.set_abort(slot.abort.clone());
            slot.limit.store(j.limit_addr(), Ordering::SeqCst);
            j
        });
    }

    /// Drops the JIT on the core's thread, before the core is handed back
    /// ([`Machine::stop_parallel`]).
    pub fn drop_jit(&mut self) {
        self.board.cores.slots[self.core.idx].limit.store(0, Ordering::SeqCst);
        self.core.jit = None;
    }

    /// Runs up to `budget` instructions of this core (fewer if it waits, the
    /// host asks the cores to stop or the machine stops).
    pub fn run(&mut self, budget: u64) -> Stop {
        let board = self.board.clone();
        self.core.run(&board, budget, false)
    }

    /// True once the host asked the cores to stop ([`Machine::request_stop`]):
    /// the thread then hands the core back.
    pub fn stopped(&self) -> bool {
        let c = &self.board.cores;
        c.stop.load(Ordering::SeqCst) || c.halt.load(Ordering::SeqCst) != 0
    }

    /// Instructions this core executed since the cores went parallel.
    pub fn executed(&self) -> u64 {
        self.core.executed
    }

    /// Its registers (as of its last run).
    pub fn cpu(&self) -> &Cpu {
        &self.core.cpu
    }
}

impl Machine {
    /// True while the cores run in parallel ([`Machine::start_parallel`]).
    pub fn is_parallel(&self) -> bool {
        self.par.is_some()
    }

    /// Splits the machine for parallel execution (ADR 0042): returns the
    /// cores 1..n, to run on host threads of their own; core 0 keeps running
    /// in [`Machine::run`]. Refused with one core and during a recording or
    /// replay (parallel execution is not deterministic). The JIT, if any,
    /// stays with core 0; [`Core::set_jit`] gives the others theirs.
    pub fn start_parallel(&mut self) -> Result<Vec<Core>, &'static str> {
        if self.par.is_some() {
            return Err("the cores already run in parallel");
        }
        if self.smp.is_none() {
            return Err("one core");
        }
        if self.rr.recording() || self.rr.replaying() {
            return Err("recording or replay in progress");
        }
        if self.smp.as_ref().is_some_and(|s| s.cur != 0) {
            self.switch_to(0);
        }
        let n = self.ncpu;
        let board = self.board.clone();
        let cores = &board.cores;
        let smp = self.smp.as_ref().expect("several cores");
        cores.clock.store(self.steps, Ordering::SeqCst);
        cores.idle.store(0, Ordering::SeqCst);
        cores.stop.store(false, Ordering::SeqCst);
        cores.halt.store(0, Ordering::SeqCst);
        for (i, s) in cores.slots.iter().enumerate() {
            s.on.store(smp.vcpus[i].on, Ordering::SeqCst);
            *crate::board::lock_slot(&s.start) = None;
            s.wait_until.store(0, Ordering::SeqCst);
            cores.take(i);
            s.posted.store(0, Ordering::SeqCst);
            s.applied.store(0, Ordering::SeqCst);
        }
        board.ram().set_consumers(n as usize);
        cores.parallel.store(true, Ordering::SeqCst);
        {
            let mut b = board.borrow_mut();
            b.cntpct = b.cntpct.max(counter(self.steps / n));
            b.update_irqs();
        }
        let others: Vec<Core> = (1..n as usize)
            .map(|i| Core {
                board: board.clone(),
                core: ParCore::new(
                    i,
                    smp.vcpus[i].cpu.clone(),
                    Mmu::new(PA_BITS),
                    smp.vcpus[i].wfi_pending,
                    n,
                ),
            })
            .collect();
        let mut core0 = ParCore::new(
            0,
            core::mem::take(&mut self.cpu),
            core::mem::replace(&mut self.mmu, Mmu::new(PA_BITS)),
            self.wfi_pending,
            n,
        );
        core0.interp = self.interp;
        core0.jit = self.jit.take().map(|mut j| {
            j.set_yields(false);
            j.set_parallel(true);
            j.set_abort(cores.slots[0].abort.clone());
            cores.slots[0].limit.store(j.limit_addr(), Ordering::SeqCst);
            j
        });
        self.par = Some(Box::new(core0));
        Ok(others)
    }

    /// Asks the parallel cores to stop: their [`Core::run`] returns soon,
    /// and [`Core::stopped`] is true.
    pub fn request_stop(&self) {
        let cores = &self.board.cores;
        cores.stop.store(true, Ordering::SeqCst);
        for i in 0..cores.slots.len() {
            cores.kick(i);
        }
    }

    /// Back to the cores in turns (deterministic) with the cores handed back
    /// by their threads (after [`Machine::request_stop`] and with their JITs
    /// dropped, [`Core::drop_jit`]). The TLB is flushed: the cores' TLBs
    /// become one again.
    pub fn stop_parallel(&mut self, others: Vec<Core>) {
        let Some(core0) = self.par.take() else { return };
        let board = self.board.clone();
        let cores = &board.cores;
        let core0 = *core0;
        self.steps = cores.clock.load(Ordering::SeqCst);
        self.cpu = core0.cpu;
        self.mmu = core0.mmu;
        self.mmu.tlb_mut().flush_all();
        self.interp = Next::Jit;
        self.wfi_pending = core0.wfi_pending;
        self.jit = core0.jit.map(|mut j| {
            j.set_parallel(false);
            j.set_yields(true);
            j
        });
        cores.slots[0].limit.store(0, Ordering::SeqCst);
        let smp = self.smp.as_mut().expect("several cores");
        for c in others {
            let i = c.core.idx;
            smp.vcpus[i].cpu = c.core.cpu;
            smp.vcpus[i].wfi_pending = c.core.wfi_pending;
            smp.vcpus[i].interp = Next::Jit;
        }
        for (i, s) in cores.slots.iter().enumerate() {
            smp.vcpus[i].on = s.on.load(Ordering::SeqCst);
            // A core switched on and not yet started: it starts in turns.
            if let Some((entry, context)) = crate::board::lock_slot(&s.start).take() {
                let mut v = smp::Vcpu::off(i);
                v.cpu.pc = entry;
                v.cpu.x[0] = context;
                v.on = true;
                if i == 0 {
                    self.cpu = v.cpu.clone();
                }
                smp.vcpus[i] = v;
            }
            cores.take(i);
        }
        smp.cur = 0;
        smp.turn_end = self.steps.saturating_add(smp::QUANTUM);
        cores.parallel.store(false, Ordering::SeqCst);
        cores.stop.store(false, Ordering::SeqCst);
        board.ram().set_consumers(1);
        let mut b = board.borrow_mut();
        b.virt.set_current_cpu(0);
        b.lines_changed();
        b.irq_dirty = true;
    }

    /// [`Machine::run`] with the cores in parallel: core 0 for up to
    /// `budget` instructions, with the network stack serviced here.
    pub(super) fn run_parallel(&mut self, budget: u64) -> Stop {
        let board = self.board.clone();
        let cores = &board.cores;
        let mut left = budget;
        loop {
            // The network stack (core 0's duty) and the devices it feeds.
            let clock = cores.clock.load(Ordering::SeqCst);
            self.steps = clock;
            self.par_sync(counter(clock / self.ncpu));
            let mut core0 = self.par.take().expect("parallel");
            let step = match self.net_deadline {
                Some(d) => left.min(steps_for(d).saturating_mul(self.ncpu).saturating_sub(clock).max(1)),
                None => left,
            };
            let before = core0.executed;
            let stop = core0.run(&board, step, true);
            let done = core0.executed - before;
            self.par = Some(core0);
            self.steps = cores.clock.load(Ordering::SeqCst);
            left = left.saturating_sub(done.max(1));
            if stop != Stop::Budget || left == 0 || cores.stop.load(Ordering::SeqCst) {
                return stop;
            }
        }
    }

    /// The network stack and the devices with the cores in parallel, at
    /// counter value `now` (like `sync_irqs`).
    fn par_sync(&mut self, now: u64) {
        let mut b = self.board.borrow_mut();
        b.cntpct = b.cntpct.max(now);
        let now = b.cntpct;
        let net_due = self.net_deadline.is_some_and(|d| now >= d);
        if let Some(slot) = self.slots.net
            && (net_due || b.virtio_dirty)
        {
            let at = crate::net::micros(now);
            let link = b
                .virt
                .virtio_mut(slot)
                .and_then(|t| t.device_as_mut::<vetro_platform::virtio::VirtioNet>())
                .and_then(|d| d.backend_as_mut::<crate::net::NetLink>())
                .expect("virtio-net with NetLink in the network slot");
            link.now = at;
            if net_due {
                link.stack.poll(at);
                if link.stack.pending_frames() > 0 {
                    b.virtio_dirty = true;
                }
            }
            let serviced = b.virtio_dirty;
            if serviced {
                b.service_virtio();
            }
            let link = b
                .virt
                .virtio_mut(slot)
                .and_then(|t| t.device_as_mut::<vetro_platform::virtio::VirtioNet>())
                .and_then(|d| d.backend_as_mut::<crate::net::NetLink>())
                .expect("virtio-net with NetLink in the network slot");
            self.net_deadline = link.stack.next_deadline().map(|t| crate::net::counter_at(t).max(now + 1));
        } else if b.virtio_dirty {
            b.service_virtio();
        }
        b.update_irqs();
        let nd = self.net_deadline.map_or(u64::MAX, |d| steps_for(d).saturating_mul(self.ncpu));
        b.cores.net_deadline.store(nd, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::super::smp::tests_support::{machine, to_power_off, word};
    use super::*;

    /// Runs `m` with its cores in parallel on threads (interpreter) up to the
    /// power-off; back in turns afterwards.
    fn run_parallel(m: &mut Machine) -> Stop {
        let cores = m.start_parallel().expect("parallel");
        std::thread::scope(|s| {
            let handles: Vec<_> = cores
                .into_iter()
                .map(|mut c| {
                    s.spawn(move || {
                        c.set_jit(None);
                        while !c.stopped() {
                            match c.run(1 << 16) {
                                Stop::Budget | Stop::Idle => {}
                                _ => break,
                            }
                        }
                        c.drop_jit();
                        c
                    })
                })
                .collect();
            let stop = loop {
                match m.run(1 << 16) {
                    Stop::Budget | Stop::Idle => {}
                    other => break other,
                }
            };
            m.request_stop();
            let cores: Vec<Core> = handles.into_iter().map(|h| h.join().expect("core thread")).collect();
            m.stop_parallel(cores);
            stop
        })
    }

    /// The two-core probe of `smp.rs` with the cores on two host threads:
    /// the same results (every exclusive increment, CPU_ON, the SGI).
    #[test]
    fn two_cores_in_parallel_meet() {
        for _ in 0..20 {
            let mut m = machine(2);
            assert_eq!(run_parallel(&mut m), Stop::PowerOff);
            assert!(!m.is_parallel());
            assert_eq!(word(&m, 0x800), 40_000, "every exclusive increment of both cores");
            assert_eq!(word(&m, 0x808), 1, "flag from core 1");
            assert_eq!(word(&m, 0x810) as i64, 0, "CPU_ON: SUCCESS");
            assert_eq!(word(&m, 0x818) as i64, -4, "CPU_ON again: ALREADY_ON");
            assert_eq!(word(&m, 0x820), 0, "AFFINITY_INFO: ON");
            assert_eq!(word(&m, 0x828), 1, "SGI 1 from core 1");
            assert_eq!(word(&m, 0x840), 0x1234, "x0 = context of CPU_ON");
            assert_eq!(word(&m, 0x848), 0x8000_0001, "MPIDR_EL1 of core 1");
            // Back in turns, the machine goes on deterministically.
            let _ = to_power_off;
        }
    }
}
