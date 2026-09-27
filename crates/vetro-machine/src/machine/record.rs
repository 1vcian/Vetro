//! Record & replay in the machine (M10, ADR 0019, `docs/specs/replay.md`).
//!
//! - **Recording** ([`Machine::start_recording`]): every
//!   [`Machine::input`] ends up in the log with the instruction number, a hash
//!   of the registers and the bytes output by the console up to there; every
//!   opaque host access (`Machine::device`, `Machine::net` with a closure)
//!   as an opaque event. At the end of quanta, at fixed intervals, a
//!   snapshot (keyframe). An input that arrives while the machine is stopped
//!   on a disk ([`Stop::Blocked`]) is applied at the end of the first quantum
//!   after the unblock: this way its instant does not depend on when the
//!   disk data arrives, and the replay finds it again even with an
//!   always-ready disk.
//! - **Replay** ([`Machine::start_replay`], [`Machine::replay_from`],
//!   [`Machine::goto`]): `run` cuts quanta at the instants of the events (the
//!   JIT receives the end of the quantum as its limit, so it never goes past
//!   them), checks registers and console and applies the input; at the end of
//!   the recording it compares the state fingerprint ([`Digest`]).
//!
//! Quantum boundaries do not change the execution (ADR 0014, 0015): this is
//! why an input applied between two quanta at the same instruction number
//! gives the same result whatever the host's quantum.

use vetro_cpu::Access;
use vetro_mmu::{BusError, PhysMemory};
use vetro_platform::virtio::{VirtioGpu, VirtioInput, VirtioNet, VirtioVsock};
use vetro_snapshot::{Snapshot, Writer, hash64};

use super::{Machine, Stop};
use crate::record::{
    CONSOLE_HASH_INIT, Digest, Divergence, Event, EventKind, HostNetOp, Input, Keyframe, Log, ReplayStatus,
    Reply, VsockOp, console_hash,
};

/// Options of a recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordOptions {
    /// Instructions between two keyframes (snapshots for jumping to an
    /// instruction); 0 = none. With keyframes the first is taken at the
    /// start, and the log alone is enough to start again.
    pub keyframe_every: u64,
}

/// Console output taken from the UART: the bytes not yet given to the host
/// and the count (bytes, hash) since the start of the recording or replay.
pub(super) struct ConsoleTap {
    pub(super) buf: Vec<u8>,
    len: u64,
    hash: u64,
}

impl Default for ConsoleTap {
    fn default() -> Self {
        ConsoleTap { buf: Vec::new(), len: 0, hash: CONSOLE_HASH_INIT }
    }
}

pub(super) struct Recorder {
    log: Log,
    next_keyframe: u64,
    /// Inputs that arrived with the machine stopped on a disk.
    deferred: Vec<Input>,
}

pub(super) struct Replayer {
    /// The log without keyframes.
    log: Log,
    next: usize,
    /// The replay used the JIT.
    jit: bool,
}

pub(super) enum Rr {
    Off,
    Record(Box<Recorder>),
    Replay(Box<Replayer>),
}

impl Rr {
    pub(super) fn recording(&self) -> bool {
        matches!(self, Rr::Record(_))
    }
    pub(super) fn replaying(&self) -> bool {
        matches!(self, Rr::Replay(_))
    }
    pub(super) fn note_jit(&mut self) {
        match self {
            Rr::Record(r) => r.log.jit = true,
            Rr::Replay(p) => p.jit = true,
            Rr::Off => {}
        }
    }
}

/// Read-only physical memory, RAM only: for the debugger's translations,
/// which must not touch devices.
struct RamOnly<'a>(&'a crate::board::Ram);

impl PhysMemory for RamOnly<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.0.read(pa, buf) { Ok(()) } else { Err(BusError::Decode) }
    }
    fn write(&mut self, _pa: u64, _data: &[u8]) -> Result<(), BusError> {
        Err(BusError::Decode)
    }
}

impl Machine {
    // ---- Inputs --------------------------------------------------------------

    /// A host input: the only point through which, during a recording,
    /// what the guest sees changes. Recorded with the current instruction
    /// number; it reaches the guest before the next instruction. During a
    /// replay it is ignored ([`Reply::Ignored`]); during a recording with
    /// the machine stopped on a disk it is deferred
    /// ([`Reply::Deferred`]).
    pub fn input(&mut self, input: Input) -> Reply {
        match &mut self.rr {
            Rr::Replay(_) => return Reply::Ignored,
            Rr::Record(r) if self.board.borrow().host_wait => {
                r.deferred.push(input);
                return Reply::Deferred;
            }
            Rr::Record(_) => self.log_event(EventKind::Input(input.clone())),
            Rr::Off => {}
        }
        self.apply(&input)
    }

    fn log_event(&mut self, kind: EventKind) {
        let ev = Event { step: self.steps, cpu: self.cpu_hash(), console: self.console_total(), kind };
        if let Rr::Record(r) = &mut self.rr {
            r.log.events.push(ev);
        }
    }

    /// An access to a device with a closure: an opaque event during a
    /// recording.
    pub(super) fn note_opaque(&mut self, slot: Option<u32>) {
        if self.rr.recording() {
            self.log_event(EventKind::Opaque { slot });
        }
    }

    fn apply(&mut self, input: &Input) -> Reply {
        let done = |r: Option<()>| if r.is_some() { Reply::Done } else { Reply::NoDevice };
        match input {
            Input::Console(bytes) => {
                let mut b = self.board.borrow_mut();
                b.virt.uart_mut().push_input(bytes);
                b.irq_dirty = true;
                Reply::Done
            }
            Input::Keyboard(ev) => {
                done(self.device_raw(self.slots.keyboard, |k: &mut VirtioInput| k.inject(ev)))
            }
            Input::Pointer(ev) => {
                done(self.device_raw(self.slots.pointer, |k: &mut VirtioInput| k.inject(ev)))
            }
            Input::Gpio { line, level } => {
                self.board.borrow_mut().gpio_input(*line, *level);
                Reply::Done
            }
            Input::Display { scanout, width, height } => done(
                self.device_raw(self.slots.gpu, |g: &mut VirtioGpu| g.set_display(*scanout, *width, *height)),
            ),
            Input::NetFrame(f) => done(self.net_input_link(|l| l.host_rx.push_back(f.clone()))),
            Input::NetLink(up) => {
                done(self.device_raw(self.slots.net, |d: &mut VirtioNet| d.set_link_up(*up)))
            }
            Input::Vsock(op) => self
                .device_raw(self.slots.vsock, |v: &mut VirtioVsock| match op {
                    VsockOp::Listen(p) => Reply::Vsock(v.listen(*p)),
                    VsockOp::Unlisten(p) => {
                        v.unlisten(*p);
                        Reply::Done
                    }
                    VsockOp::Accept(p) => Reply::Conn(v.accept(*p)),
                    VsockOp::Connect(p) => Reply::Conn(Some(v.connect(*p))),
                    VsockOp::Send(c, d) => Reply::Vsock(v.send(*c, d)),
                    VsockOp::Recv(c, max) => {
                        Reply::Data(v.recv(*c, usize::try_from(*max).unwrap_or(usize::MAX)))
                    }
                    VsockOp::ShutdownSend(c) => {
                        v.shutdown_send(*c);
                        Reply::Done
                    }
                    VsockOp::Close(c) => {
                        v.close(*c);
                        Reply::Done
                    }
                    VsockOp::Reset(c) => {
                        v.reset(*c);
                        Reply::Done
                    }
                    VsockOp::Release(c) => {
                        v.release(*c);
                        Reply::Done
                    }
                    VsockOp::TransportReset => {
                        v.transport_reset();
                        Reply::Done
                    }
                })
                .unwrap_or(Reply::NoDevice),
            Input::HostNet(op) => self
                .net_raw(|s| match op {
                    HostNetOp::Connect(p) => Reply::HostConn(s.host_connect(*p)),
                    HostNetOp::Send(c, d) => Reply::Accepted(s.host_send(*c, d) as u64),
                    HostNetOp::Recv(c, max) => {
                        let mut buf = vec![0; usize::try_from(*max).unwrap_or(usize::MAX).min(1 << 24)];
                        let n = s.host_recv(*c, &mut buf);
                        buf.truncate(n);
                        Reply::Data(buf)
                    }
                    HostNetOp::Shutdown(c) => {
                        s.host_shutdown(*c);
                        Reply::Done
                    }
                    HostNetOp::Abort(c) => {
                        s.host_abort(*c);
                        Reply::Done
                    }
                    HostNetOp::Release(c) => {
                        s.host_release(*c);
                        Reply::Done
                    }
                })
                .unwrap_or(Reply::NoDevice),
        }
    }

    // ---- Console and fingerprints ----------------------------------------------

    /// Moves the UART output into the machine's buffer, counting it.
    pub(super) fn drain_console(&mut self) {
        let out = self.board.borrow_mut().virt.uart_mut().take_output();
        self.console.len += out.len() as u64;
        self.console.hash = console_hash(self.console.hash, &out);
        self.console.buf.extend(out);
    }

    /// Bytes output by the console, including those still in the UART: does
    /// not depend on when the host reads.
    fn console_total(&self) -> u64 {
        self.console.len + self.board.borrow().virt.uart().output().len() as u64
    }

    fn cpu_hash(&self) -> u64 {
        let mut w = Writer::with_capacity(2048);
        self.cpu.save(&mut w);
        hash64(w.as_bytes())
    }

    /// State fingerprint: instructions, CPU, MMU, platform, RAM and
    /// console. First it moves the UART output into the machine's buffer
    /// (the host finds it with [`Machine::console_output`]), so the
    /// fingerprint does not depend on when the host reads.
    pub fn digest(&mut self) -> Digest {
        self.drain_console();
        let hash = |s: &dyn Fn(&mut Writer)| {
            let mut w = Writer::new();
            s(&mut w);
            hash64(w.as_bytes())
        };
        let b = self.board.borrow();
        Digest {
            steps: self.steps,
            cpu: self.cpu_hash(),
            mmu: hash(&|w| self.mmu.save(w)),
            platform: hash(&|w| b.virt.save(w)),
            ram: b.ram.hash(),
            console_len: self.console.len,
            console_hash: self.console.hash,
        }
    }

    // ---- Recording ---------------------------------------------------------------

    /// Starts recording from here (after `load_linux`, after a restore,
    /// or at any time between two `run`s). A replay in progress ends.
    pub fn start_recording(&mut self, opts: RecordOptions) {
        self.rr = Rr::Off;
        self.drain_console();
        self.console.len = 0;
        self.console.hash = CONSOLE_HASH_INIT;
        let start = self.digest();
        let log = Log {
            config_hash: self.config_hash(),
            config: self.cfg.clone(),
            snapshot_version: vetro_snapshot::FORMAT_VERSION,
            jit: self.jit.is_some(),
            keyframe_every: opts.keyframe_every,
            start,
            events: Vec::new(),
            keyframes: Vec::new(),
            end: Digest::default(),
        };
        self.rr = Rr::Record(Box::new(Recorder { log, next_keyframe: self.steps, deferred: Vec::new() }));
        if opts.keyframe_every > 0 {
            self.keyframe();
        } else if let Rr::Record(r) = &mut self.rr {
            r.next_keyframe = u64::MAX;
        }
    }

    /// Recording in progress.
    pub fn is_recording(&self) -> bool {
        self.rr.recording()
    }

    /// Events recorded so far.
    pub fn recorded_events(&self) -> usize {
        match &self.rr {
            Rr::Record(r) => r.log.events.len(),
            _ => 0,
        }
    }

    /// Ends the recording and returns the log, with the fingerprint of the
    /// current state. Inputs deferred by a machine still stopped on a disk
    /// never reached the guest and are left out.
    pub fn stop_recording(&mut self) -> Option<Log> {
        let Rr::Record(r) = core::mem::replace(&mut self.rr, Rr::Off) else { return None };
        let mut log = r.log;
        log.end = self.digest();
        Some(log)
    }

    fn keyframe(&mut self) {
        self.drain_console();
        let snapshot = self.save();
        let (steps, len, hash) = (self.steps, self.console.len, self.console.hash);
        if let Rr::Record(r) = &mut self.rr {
            r.log.keyframes.push(Keyframe { step: steps, console_len: len, console_hash: hash, snapshot });
            r.next_keyframe = steps.saturating_add(r.log.keyframe_every);
        }
    }

    /// End of a recorded quantum: keyframe if it is time (never after an event
    /// at the same instant: the events of an instant come after its
    /// keyframe), then the deferred inputs if the machine is no longer stopped.
    pub(super) fn after_quantum(&mut self) {
        if self.blocked() {
            return;
        }
        let Rr::Record(r) = &mut self.rr else { return };
        let due = self.steps >= r.next_keyframe && r.log.events.last().is_none_or(|e| e.step < self.steps);
        let deferred = core::mem::take(&mut r.deferred);
        if due {
            self.keyframe();
        }
        for i in deferred {
            self.input(i);
        }
    }

    // ---- Replay -------------------------------------------------------------------

    /// Starts the replay of `log` from the current state, which must be
    /// the starting state of the recording (same configuration, same
    /// fingerprint: same kernel loaded, or same snapshot restored).
    pub fn start_replay(&mut self, log: &Log) -> Result<(), Divergence> {
        self.check_config(log)?;
        self.rr = Rr::Off;
        self.drain_console();
        self.console.len = 0;
        self.console.hash = CONSOLE_HASH_INIT;
        let now = self.digest();
        let tlb = !log.jit && self.jit.is_none();
        if let Some(what) = log.start.diff(&now, tlb) {
            return Err(self
                .refuse(Divergence::Start(format!("the starting state is not the recorded one ({what})"))));
        }
        self.begin_replay(log, 0);
        Ok(())
    }

    /// Starts the replay of `log` from the last keyframe not beyond
    /// instruction `step`; without usable keyframes, from the current state like
    /// [`Machine::start_replay`].
    pub fn replay_from(&mut self, log: &Log, step: u64) -> Result<(), Divergence> {
        let Some(k) = log.keyframe_before(step) else { return self.start_replay(log) };
        self.check_config(log)?;
        if log.snapshot_version != vetro_snapshot::FORMAT_VERSION {
            return Err(self.refuse(Divergence::Start(format!(
                "keyframes in snapshot format {}, this version reads {}",
                log.snapshot_version,
                vetro_snapshot::FORMAT_VERSION
            ))));
        }
        self.rr = Rr::Off;
        if let Err(e) = self.load_state(&k.snapshot) {
            return Err(self.refuse(Divergence::Start(format!("keyframe at {}: {e}", k.step))));
        }
        self.console = ConsoleTap { buf: Vec::new(), len: k.console_len, hash: k.console_hash };
        self.drain_console();
        // Jump in time: the introspection's in-progress syscalls are no longer valid.
        self.hooks.forget_pending();
        let first = log.events.partition_point(|e| e.step < k.step);
        self.begin_replay(log, first);
        Ok(())
    }

    /// Brings the machine to instruction `step` of the recording (at the first
    /// boundary between quanta with at least `step` instructions: a WFI can
    /// jump past it), restarting from the nearest keyframe and redoing the inputs.
    /// Returns the instruction count reached; from there registers
    /// (`Machine::cpu`) and memory ([`Machine::read_phys`],
    /// [`Machine::read_virt`]) can be read, and the replay can continue with `run`.
    pub fn goto(&mut self, log: &Log, step: u64) -> Result<u64, Divergence> {
        self.replay_from(log, step)?;
        while self.steps < step {
            let stop = self.run(step - self.steps);
            match &self.replay_status {
                Some(ReplayStatus::Diverged(d)) => return Err(d.clone()),
                Some(ReplayStatus::Finished) => break,
                _ => {}
            }
            if stop != Stop::Budget {
                break;
            }
        }
        Ok(self.steps)
    }

    /// State of the last replay (`None` if there has not been one).
    pub fn replay_status(&self) -> Option<&ReplayStatus> {
        self.replay_status.as_ref()
    }

    fn check_config(&mut self, log: &Log) -> Result<(), Divergence> {
        let here = self.config_hash();
        if log.config_hash != here {
            return Err(self.refuse(Divergence::Start(format!(
                "machine configured differently (hash {:016x}, this one {here:016x}): the same \
                 RAM, devices and seed are required",
                log.config_hash
            ))));
        }
        Ok(())
    }

    fn refuse(&mut self, d: Divergence) -> Divergence {
        self.replay_status = Some(ReplayStatus::Diverged(d.clone()));
        d
    }

    fn begin_replay(&mut self, log: &Log, next: usize) {
        // Without the (large) keyframes: the replay needs only the events.
        let l = Log {
            config_hash: log.config_hash,
            config: log.config.clone(),
            snapshot_version: log.snapshot_version,
            jit: log.jit,
            keyframe_every: log.keyframe_every,
            start: log.start,
            events: log.events.clone(),
            keyframes: Vec::new(),
            end: log.end,
        };
        self.replay_status = Some(ReplayStatus::Running { next });
        self.rr = Rr::Replay(Box::new(Replayer { log: l, next, jit: self.jit.is_some() }));
    }

    /// End of the replay because of a difference: the machine continues freely.
    fn diverge(&mut self, d: Divergence, stop: Stop) -> Stop {
        self.rr = Rr::Off;
        self.replay_status = Some(ReplayStatus::Diverged(d));
        stop
    }

    /// Reached the end of the recording: the fingerprint is compared.
    fn finish_replay(&mut self, stop: Stop) -> Stop {
        let Rr::Replay(p) = core::mem::replace(&mut self.rr, Rr::Off) else { return stop };
        let now = self.digest();
        let tlb = !p.log.jit && !p.jit;
        self.replay_status = Some(match p.log.end.diff(&now, tlb) {
            None => ReplayStatus::Finished,
            Some(what) => ReplayStatus::Diverged(Divergence::End { what }),
        });
        stop
    }

    pub(super) fn run_replay(&mut self, budget: u64) -> Stop {
        let end = self.steps.saturating_add(budget);
        loop {
            // The events of this instant.
            loop {
                let Rr::Replay(p) = &self.rr else { return Stop::Budget };
                let index = p.next;
                let Some(ev) = p.log.events.get(index) else { break };
                if ev.step > self.steps {
                    break;
                }
                let ev = ev.clone();
                if ev.step < self.steps {
                    let d = Divergence::Missed { index, step: ev.step, at: self.steps };
                    return self.diverge(d, Stop::Budget);
                }
                let what = if self.cpu_hash() != ev.cpu {
                    Some("registers")
                } else if self.console_total() != ev.console {
                    Some("console bytes")
                } else {
                    None
                };
                if let Some(what) = what {
                    return self.diverge(Divergence::Event { index, step: ev.step, what }, Stop::Budget);
                }
                match &ev.kind {
                    EventKind::Input(i) => {
                        self.apply(i);
                    }
                    EventKind::Opaque { slot } => {
                        let d = Divergence::Opaque { index, step: ev.step, slot: *slot };
                        return self.diverge(d, Stop::Budget);
                    }
                }
                if let Rr::Replay(p) = &mut self.rr {
                    p.next += 1;
                }
                self.replay_status = Some(ReplayStatus::Running { next: index + 1 });
            }
            let Rr::Replay(p) = &self.rr else { return Stop::Budget };
            let (next, index, last) = (p.log.events.get(p.next).map(|e| e.step), p.next, p.log.end.steps);
            if next.is_none() && self.steps == last {
                return self.finish_replay(Stop::Budget);
            }
            if self.steps > last {
                return self.diverge(Divergence::End { what: "instructions" }, Stop::Budget);
            }
            if self.steps >= end {
                return Stop::Budget;
            }
            let target = end.min(next.unwrap_or(last));
            let stop = self.run_quantum(target - self.steps);
            match stop {
                Stop::Budget => {}
                Stop::Blocked => return Stop::Blocked,
                // Stopped by itself: fine if the recording had an input here
                // (the machine was waiting for it) or its end.
                other => {
                    if next == Some(self.steps) && other == Stop::Idle {
                        continue;
                    }
                    if next.is_none() && self.steps == last {
                        return self.finish_replay(other);
                    }
                    let d = match next {
                        Some(step) => Divergence::Missed { index, step, at: self.steps },
                        None => Divergence::End { what: "instructions" },
                    };
                    return self.diverge(d, other);
                }
            }
        }
    }

    // ---- Reading the state ----------------------------------------------------------

    /// The registers as text (for `vetro boot --goto` and comparisons):
    /// instructions, PC, SP, NZCV, EL, X0–X30 and the EL1 system registers
    /// needed to read the kernel state.
    pub fn registers_text(&self) -> String {
        use core::fmt::Write;
        let c = &self.cpu;
        let s = &c.sys;
        let mut t = String::new();
        let _ = writeln!(
            t,
            "instructions {}\npc   {:016x}  sp   {:016x}  nzcv {:08x}  el {}  daif {:03x}",
            self.steps, c.pc, c.sp, c.nzcv, s.el, s.daif
        );
        for (row, regs) in c.x.chunks(4).enumerate() {
            let line: Vec<String> = regs
                .iter()
                .enumerate()
                .map(|(i, v)| format!("{:<4} {v:016x}", format!("x{}", row * 4 + i)))
                .collect();
            let _ = writeln!(t, "{}", line.join("  "));
        }
        let sys = [
            ("sp_el0", s.sp_el[0]),
            ("sp_el1", s.sp_el[1]),
            ("elr_el1", s.elr_el1),
            ("spsr_el1", s.spsr_el1),
            ("esr_el1", s.esr_el1),
            ("far_el1", s.far_el1),
            ("vbar_el1", s.vbar_el1),
            ("sctlr_el1", s.sctlr_el1),
            ("tcr_el1", s.tcr_el1),
            ("ttbr0_el1", s.ttbr0_el1),
            ("ttbr1_el1", s.ttbr1_el1),
            ("tpidr_el0", c.tpidr_el0),
        ];
        for pair in sys.chunks(3) {
            let line: Vec<String> = pair.iter().map(|(n, v)| format!("{n:<9} {v:016x}")).collect();
            let _ = writeln!(t, "{}", line.join("  "));
        }
        t
    }

    /// Reads RAM at physical address `pa`; false outside RAM.
    pub fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.board.borrow().ram.read(pa, buf)
    }

    /// Translates virtual address `va` with the current tables and the
    /// current EL (without TLB, without touching devices).
    pub fn translate(&self, va: u64) -> Option<u64> {
        let b = self.board.borrow();
        self.mmu.walk(&mut RamOnly(&b.ram), va, Access::Read, self.cpu.sys.el).ok().map(|t| t.pa)
    }

    /// Reads memory at virtual address `va` (translation like
    /// [`Machine::translate`], page by page). Error: the first
    /// unreadable virtual address.
    pub fn read_virt(&self, va: u64, buf: &mut [u8]) -> Result<(), u64> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = va.wrapping_add(done as u64);
            let n = (4096 - (at & 4095) as usize).min(buf.len() - done);
            let pa = self.translate(at).ok_or(at)?;
            if !self.read_phys(pa, &mut buf[done..done + n]) {
                return Err(at);
            }
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::tests::{IRQ, IRQ_AT, MAIN, R, SVC, cfg};
    use super::super::tests::{Gate, blk_machine};
    use super::*;
    use crate::{Devices, MachineConfig};
    use vetro_cpu::Cpu;
    use vetro_platform::virtio::VirtioBlk;

    /// The loop of the `snapshot.rs` probe plus the UART echo
    /// (`tools/a64asm.sh`): turns on the UART, then on every iteration reads UARTFR and, if there
    /// is a byte, reads it, mixes it into x21 and sends it back. This way a console
    /// input changes the registers, and an input arriving one instruction
    /// earlier or later changes the points interrupted by the timer (and the RAM).
    const ECHO: [u32; 14] = [
        0x52806025, // mov w5, #0x301 (UARTEN, TXE, RXE)
        0xb9003125, // str w5, [x9, #0x30] (UARTCR)
        0x91000442, // loop: add x2, x2, #0x1
        0xb9401923, // ldr w3, [x9, #0x18]
        0x372000a3, // tbnz w3, #0x4, idle
        0xb9400124, // ldr w4, [x9]
        0x12001c84, // and w4, w4, #0xff
        0xcad52495, // eor x21, x4, x21, ror #9
        0xb9000124, // str w4, [x9]
        0xf2402c5f, // idle: tst x2, #0xfff
        0x54ffff01, // b.ne loop
        0xd4000001, // svc #0
        0xd503207f, // wfi
        0x17fffff5, // b loop
    ];

    fn echo_probe() -> Machine {
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        {
            let mut b = m.board.borrow_mut();
            let main = &MAIN[..25]; // up to `mov x2, #0`
            for (base, code) in
                [(R, main), (R + 4 * 25, &ECHO[..]), (R + 0xa00, &SVC[..]), (IRQ_AT, &IRQ[..])]
            {
                for (i, w) in code.iter().enumerate() {
                    assert!(b.ram.write(base + 4 * i as u64, &w.to_le_bytes()));
                }
            }
        }
        m.cpu.pc = R;
        m
    }

    const END: u64 = 300_000;

    /// Host inputs: at the first boundary with at least `step` instructions.
    fn schedule() -> Vec<(u64, Input)> {
        vec![
            (1_000, Input::Console(b"ciao".to_vec())),
            (1_000, Input::Console(b"!".to_vec())),
            (47_111, Input::Gpio { line: 3, level: true }),
            (90_001, Input::Console(b"vetro\n".to_vec())),
            (150_000, Input::Gpio { line: 3, level: false }),
            (222_222, Input::Console((0..=255).collect())),
        ]
    }

    /// Outcome of a session: console, final fingerprint and states at the
    /// requested points (instructions, CPU, RAM hash).
    struct Session {
        out: Vec<u8>,
        end: Digest,
        at: Vec<(u64, Cpu, u64)>,
    }

    /// Runs the probe up to `END` in quanta of `q`, giving the inputs of
    /// `inputs` and stopping exactly at every point of `stops`.
    fn drive(m: &mut Machine, q: u64, mut inputs: Vec<(u64, Input)>, stops: &[u64]) -> Session {
        inputs.reverse();
        let mut out = Vec::new();
        let mut at = Vec::new();
        let mut stops: Vec<u64> = stops.iter().rev().copied().collect();
        while m.steps < END {
            while stops.last().is_some_and(|&s| s <= m.steps) {
                stops.pop();
                at.push((m.steps, m.cpu.clone(), m.board.borrow().ram.hash()));
            }
            while inputs.last().is_some_and(|i| i.0 <= m.steps) {
                let (_, i) = inputs.pop().unwrap();
                assert_eq!(m.input(i), Reply::Done);
            }
            let limit = [END, inputs.last().map_or(END, |i| i.0), stops.last().copied().unwrap_or(END)]
                .into_iter()
                .filter(|&l| l > m.steps)
                .min()
                .unwrap_or(END);
            let s = m.run(q.min(limit - m.steps));
            out.extend(m.console_output());
            assert_eq!(s, Stop::Budget);
        }
        let end = m.digest();
        out.extend(m.console_output());
        Session { out, end, at }
    }

    fn record(stops: &[u64]) -> (Log, Session) {
        let mut m = echo_probe();
        m.start_recording(RecordOptions { keyframe_every: 40_000 });
        let s = drive(&mut m, 5_000, schedule(), stops);
        let log = m.stop_recording().unwrap();
        assert!(!m.is_recording());
        assert_eq!(log.end, s.end);
        // The log makes a round trip through the file.
        let log = Log::decode(&log.encode()).unwrap();
        (log, s)
    }

    /// Replay to the end in quanta of `q`: console and final state.
    fn replay(m: &mut Machine, q: u64) -> (Vec<u8>, Digest) {
        let mut out = Vec::new();
        for _ in 0..1_000_000 {
            if !matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
                break;
            }
            assert_eq!(
                m.input(Input::Console(b"x".to_vec())),
                Reply::Ignored,
                "in replay the host does not get in"
            );
            m.run(q);
            out.extend(m.console_output());
        }
        let d = m.digest();
        out.extend(m.console_output());
        (out, d)
    }

    /// The M10 criterion on the probe: the recording, replayed with
    /// different quanta from the start or from a keyframe on a new machine,
    /// gives the same console, the same instructions and the same state; the
    /// echo proves that the inputs arrived.
    #[test]
    fn registra_e_riproduci_la_sonda() {
        let (log, rec) = record(&[]);
        let text = String::from_utf8_lossy(&rec.out);
        assert!(text.contains("ciao!") && text.contains("vetro\n"), "console echo: {text:?}");
        assert_eq!(log.events.len(), schedule().len());
        assert_eq!(log.events[0].step, log.events[1].step, "two inputs at the same instant");
        assert!(log.keyframes.len() >= 7, "{:?}", log.keyframes);
        assert_eq!(log.keyframes[0].step, 0);

        for q in [1, 7_919, 1 << 40] {
            let mut m = echo_probe();
            m.start_replay(&log).unwrap();
            let (out, end) = if q == 1 {
                // One instruction at a time for a stretch (boundaries
                // everywhere, events included), then in large quanta.
                for _ in 0..3_000 {
                    m.run(1);
                }
                let mut out = m.console_output();
                let (o, e) = replay(&mut m, 50_000);
                out.extend(o);
                (out, e)
            } else {
                replay(&mut m, q)
            };
            assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "quantum {q}");
            assert!(out == rec.out, "quantum {q}: console");
            assert_eq!(end, rec.end, "quantum {q}");
        }

        // From a keyframe, on a new machine without the probe in RAM.
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        m.replay_from(&log, 150_000).unwrap();
        assert_eq!(m.steps, log.keyframe_before(150_000).unwrap().step);
        let (_, end) = replay(&mut m, 3_001);
        assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(end, rec.end);
    }

    /// `goto` brings the machine back to an instruction with the registers and
    /// RAM of the recorded execution at that point, from any starting
    /// state, and from there the replay continues to the end.
    #[test]
    fn goto_come_l_esecuzione_diretta() {
        let targets = [0, 999, 1_000, 1_001, 40_000, 47_112, 123_457, 222_222, 299_999];
        let (log, rec) = record(&targets);
        assert_eq!(rec.at.len(), targets.len());
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        for (target, cpu, ram) in rec.at.iter().rev() {
            let reached = m.goto(&log, *target).unwrap();
            assert_eq!(reached, *target);
            assert_eq!(m.cpu, *cpu, "registers at {target}");
            assert_eq!(m.board.borrow().ram.hash(), *ram, "RAM at {target}");
        }
        // Reading memory at the point: the sum of the interrupted points.
        m.goto(&log, 123_457).unwrap();
        let mut w = [0u8; 8];
        assert!(m.read_phys(R + 0x4008, &mut w));
        let mut v = [0u8; 8];
        m.read_virt(R + 0x4008, &mut v).unwrap(); // MMU off: identity
        assert_eq!(w, v);
        assert!(u64::from_le_bytes(w) > 0);
        assert_eq!(m.read_virt(0x1_0000_0000_0000, &mut v), Err(0x1_0000_0000_0000));
        let (_, end) = replay(&mut m, 10_000);
        assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(end, rec.end);
    }

    /// An input that escapes the log (given to the UART without going through
    /// `Machine::input`) shows: the replay stops with a difference at the
    /// first event after the guest has read it (event 2 falls at the
    /// same instant as the smuggled byte, before the guest reads
    /// it: the registers are still the right ones).
    #[test]
    fn ingresso_sfuggito_si_vede() {
        let mut m = echo_probe();
        m.start_recording(RecordOptions::default());
        let mut inputs = schedule();
        inputs.truncate(4);
        let first = inputs.split_off(2);
        drive_until(&mut m, 60_000, inputs);
        // The smuggled byte arrives between two recorded inputs.
        m.board.borrow_mut().virt.uart_mut().push_input(b"?");
        drive(&mut m, 5_000, first, &[]);
        let log = m.stop_recording().unwrap();
        let mut n = echo_probe();
        n.start_replay(&log).unwrap();
        replay(&mut n, 10_000);
        match n.replay_status() {
            Some(ReplayStatus::Diverged(Divergence::Event { index: 3, step, what: "registers" })) => {
                assert_eq!(*step, log.events[3].step)
            }
            other => panic!("expected event 3 to differ: {other:?}"),
        }

        // An opaque access to a device: the replay stops there.
        let mut m = echo_probe();
        m.start_recording(RecordOptions::default());
        m.run(1_000);
        m.device::<VirtioBlk, _>(Some(0), |_| ());
        m.run(1_000);
        let log = m.stop_recording().unwrap();
        assert_eq!(log.events[0].kind, EventKind::Opaque { slot: Some(0) });
        let mut n = echo_probe();
        n.start_replay(&log).unwrap();
        n.run(10_000);
        assert_eq!(
            n.replay_status(),
            Some(&ReplayStatus::Diverged(Divergence::Opaque { index: 0, step: 1_000, slot: Some(0) }))
        );
        assert!(!n.rr.replaying(), "after the difference the machine is free");
    }

    /// Like `drive`, up to `end` instructions.
    fn drive_until(m: &mut Machine, end: u64, mut inputs: Vec<(u64, Input)>) {
        inputs.reverse();
        while m.steps < end {
            while inputs.last().is_some_and(|i| i.0 <= m.steps) {
                m.input(inputs.pop().unwrap().1);
            }
            let limit = inputs.last().map_or(end, |i| i.0.min(end));
            assert_eq!(m.run(5_000.min(limit - m.steps)), Stop::Budget);
        }
    }

    /// A log does not apply to a machine configured differently or in
    /// another starting state.
    #[test]
    fn log_di_un_altra_macchina_rifiutato() {
        let (log, _) = record(&[]);
        let mut other =
            Machine::with_devices(&MachineConfig { ram_size: 2 << 20, ..cfg() }, &Devices::none());
        assert!(matches!(other.start_replay(&log), Err(Divergence::Start(_))));
        let mut moved = echo_probe();
        moved.run(10);
        let e = moved.start_replay(&log).unwrap_err();
        assert!(e.to_string().contains("starting state"), "{e}");
        assert!(!moved.rr.replaying());
        let mut wrong = log.clone();
        wrong.snapshot_version += 1;
        let e = Machine::with_devices(&cfg(), &Devices::none()).replay_from(&wrong, 50_000).unwrap_err();
        assert!(e.to_string().contains("snapshot format"), "{e}");
    }

    /// An input that arrived while the machine is waiting for a disk
    /// (`Stop::Blocked`) is applied at the end of the first quantum after the
    /// unblock; the replay with an always-ready disk gives the same
    /// execution.
    #[test]
    fn ingresso_con_il_disco_in_attesa() {
        let (mut m, slot) = blk_machine(false);
        m.start_recording(RecordOptions { keyframe_every: 1 });
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.input(Input::Console(b"k".to_vec())), Reply::Deferred);
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.recorded_events(), 0, "deferred, not yet recorded");
        m.host_link::<VirtioBlk, _>(Some(slot), |b| b.backend_as_mut::<Gate>().unwrap().open = true).unwrap();
        assert_eq!(m.run(500), Stop::Budget);
        let at = m.steps;
        assert_eq!(m.recorded_events(), 1);
        m.run(500);
        let log = m.stop_recording().unwrap();
        assert_eq!(log.events[0].step, at, "recorded at the end of the quantum after the unblock");
        assert_eq!(log.events[0].kind, EventKind::Input(Input::Console(b"k".to_vec())));
        assert!(log.keyframes.iter().all(|k| k.step != 1), "no keyframe with the machine stopped");

        let (mut n, _) = blk_machine(true);
        n.start_replay(&log).unwrap();
        while matches!(n.replay_status(), Some(ReplayStatus::Running { .. })) {
            assert_eq!(n.run(333), Stop::Budget, "the ready disk does not stop the machine");
        }
        assert_eq!(n.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(n.digest(), log.end);
        assert_eq!(n.board.borrow().virt.uart().pending_input(), 1, "the byte reached the UART");
    }
}
