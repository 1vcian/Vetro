//! Record & replay (M10, ADR 0019, `docs/specs/replay.md`): the host
//! inputs and the log that records them.
//!
//! The machine is deterministic (time = instructions, ADR 0011; stopped time
//! on disks, ADR 0014): two runs from the same state with the same
//! inputs at the same instruction numbers are identical. Host inputs
//! are therefore the only thing to record, and they all go through a
//! single point, [`Machine::input`](crate::Machine::input) with an [`Input`].
//!
//! The [`Log`] holds: the configuration, the fingerprint ([`Digest`]) of the
//! starting state, the events (input with the instruction number, plus a
//! check of the registers and the console at that point), periodic
//! snapshots ([`Keyframe`], ADR 0015) for jumping to an instruction, and
//! the fingerprint of the final state. The file is a `vetro_snapshot`
//! container with magic [`LOG_MAGIC`] and version [`LOG_VERSION`].

use core::fmt;

use vetro_net::ConnId;
use vetro_platform::virtio::{InputEvent, VsockConn, VsockError};
use vetro_snapshot::{Error, Reader, Writer};

use crate::MachineConfig;

/// First 8 bytes of a recording log.
pub const LOG_MAGIC: [u8; 8] = *b"VETROREC";

/// Log format version: it changes with every change to what is
/// written (a log of another version is rejected).
pub const LOG_VERSION: u32 = 1;

/// A host input to the guest: the only way, during a
/// recording, to change what the guest sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Bytes on the PL011 console, as if from the terminal keyboard.
    Console(Vec<u8>),
    /// virtio-input keyboard events (with their SYN_REPORT).
    Keyboard(Vec<InputEvent>),
    /// virtio-input tablet or touchscreen events.
    Pointer(Vec<InputEvent>),
    /// Level of a PL061 GPIO input line (line 3 is the power
    /// key).
    Gpio { line: u32, level: bool },
    /// Resolution requested for a virtio-gpu scanout (0x0 = off),
    /// like resizing the window.
    Display { scanout: u32, width: u32, height: u32 },
    /// A host Ethernet frame for the guest, delivered by virtio-net
    /// before the network stack's frames.
    NetFrame(Vec<u8>),
    /// virtio-net link up (true) or down.
    NetLink(bool),
    /// A host operation on virtio-vsock.
    Vsock(VsockOp),
    /// A host operation on one of its TCP connections to the guest
    /// (port forwarding, `Stack::host_*` of `vetro-net`).
    HostNet(HostNetOp),
}

/// Host operations on the connections to the guest (the
/// `Stack::host_*` methods). Reads are inputs too: they free space and
/// reopen the guest's TCP window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostNetOp {
    /// `host_connect(guest port)`.
    Connect(u16),
    /// `host_send(connection, bytes)`.
    Send(ConnId, Vec<u8>),
    /// `host_recv(connection, at most this many bytes)`.
    Recv(ConnId, u64),
    Shutdown(ConnId),
    Abort(ConnId),
    Release(ConnId),
}

/// Host operations on virtio-vsock (the methods of `VirtioVsock`). Reads
/// are inputs too: they free credit, and the guest sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VsockOp {
    Listen(u32),
    Unlisten(u32),
    Accept(u32),
    Connect(u32),
    Send(VsockConn, Vec<u8>),
    Recv(VsockConn, u64),
    ShutdownSend(VsockConn),
    Close(VsockConn),
    Reset(VsockConn),
    Release(VsockConn),
    TransportReset,
}

/// The machine's reply to an [`Input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Applied.
    Done,
    /// The device is not there: nothing changed.
    NoDevice,
    /// Recording in progress and machine stopped on a disk
    /// ([`Stop::Blocked`](crate::Stop::Blocked)): the input is applied
    /// (and recorded) at the end of the first quantum after unblocking.
    Deferred,
    /// Replay in progress: inputs come from the log, the host's are
    /// ignored.
    Ignored,
    /// Outcome of `listen`/`send`.
    Vsock(Result<(), VsockError>),
    /// Connection accepted (`accept`) or requested (`connect`).
    Conn(Option<VsockConn>),
    /// Bytes read (vsock `recv`, network `host_recv`).
    Data(Vec<u8>),
    /// Host connection opened (`host_connect`; `None` with no free ephemeral
    /// ports).
    HostConn(Option<ConnId>),
    /// Bytes accepted by `host_send`.
    Accepted(u64),
}

impl Input {
    /// A key pressed or released, with SYN_REPORT (like
    /// `VirtioInput::key`).
    pub fn key_events(code: u16, down: bool) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::EV_KEY;
        vec![InputEvent::new(EV_KEY, code, down.into()), InputEvent::syn()]
    }

    /// Absolute tablet position, with SYN_REPORT (like
    /// `VirtioInput::move_abs`).
    pub fn move_abs_events(x: u32, y: u32) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::{ABS_X, ABS_Y, EV_ABS};
        vec![
            InputEvent { ty: EV_ABS, code: ABS_X, value: x },
            InputEvent { ty: EV_ABS, code: ABS_Y, value: y },
            InputEvent::syn(),
        ]
    }

    /// A touchscreen contact (like `VirtioInput::touch`).
    pub fn touch_events(slot: u32, pos: Option<(u32, u32)>) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::{
            ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, BTN_TOUCH, EV_ABS, EV_KEY,
        };
        let mut ev = vec![
            InputEvent { ty: EV_ABS, code: ABS_MT_SLOT, value: slot },
            InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, if pos.is_some() { slot as i32 } else { -1 }),
        ];
        if let Some((x, y)) = pos {
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_X, value: x });
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_Y, value: y });
        }
        ev.push(InputEvent::new(EV_KEY, BTN_TOUCH, pos.is_some().into()));
        ev.push(InputEvent::syn());
        ev
    }

    fn save(&self, w: &mut Writer) {
        let events = |w: &mut Writer, ev: &[InputEvent]| {
            w.seq(ev, |w, e| {
                w.u16(e.ty);
                w.u16(e.code);
                w.u32(e.value);
            })
        };
        let conn = |w: &mut Writer, c: &VsockConn| {
            w.u32(c.host_port);
            w.u32(c.guest_port);
        };
        match self {
            Input::Console(b) => {
                w.u8(0);
                w.bytes(b);
            }
            Input::Keyboard(ev) => {
                w.u8(1);
                events(w, ev);
            }
            Input::Pointer(ev) => {
                w.u8(2);
                events(w, ev);
            }
            Input::Gpio { line, level } => {
                w.u8(3);
                w.u32(*line);
                w.bool(*level);
            }
            Input::Display { scanout, width, height } => {
                w.u8(4);
                w.u32(*scanout);
                w.u32(*width);
                w.u32(*height);
            }
            Input::NetFrame(f) => {
                w.u8(5);
                w.bytes(f);
            }
            Input::NetLink(up) => {
                w.u8(6);
                w.bool(*up);
            }
            Input::Vsock(op) => {
                w.u8(7);
                match op {
                    VsockOp::Listen(p) => {
                        w.u8(0);
                        w.u32(*p);
                    }
                    VsockOp::Unlisten(p) => {
                        w.u8(1);
                        w.u32(*p);
                    }
                    VsockOp::Accept(p) => {
                        w.u8(2);
                        w.u32(*p);
                    }
                    VsockOp::Connect(p) => {
                        w.u8(3);
                        w.u32(*p);
                    }
                    VsockOp::Send(c, d) => {
                        w.u8(4);
                        conn(w, c);
                        w.bytes(d);
                    }
                    VsockOp::Recv(c, max) => {
                        w.u8(5);
                        conn(w, c);
                        w.u64(*max);
                    }
                    VsockOp::ShutdownSend(c) => {
                        w.u8(6);
                        conn(w, c);
                    }
                    VsockOp::Close(c) => {
                        w.u8(7);
                        conn(w, c);
                    }
                    VsockOp::Reset(c) => {
                        w.u8(8);
                        conn(w, c);
                    }
                    VsockOp::Release(c) => {
                        w.u8(9);
                        conn(w, c);
                    }
                    VsockOp::TransportReset => w.u8(10),
                }
            }
            Input::HostNet(op) => {
                w.u8(8);
                match op {
                    HostNetOp::Connect(p) => {
                        w.u8(0);
                        w.u16(*p);
                    }
                    HostNetOp::Send(c, d) => {
                        w.u8(1);
                        w.u64(*c);
                        w.bytes(d);
                    }
                    HostNetOp::Recv(c, max) => {
                        w.u8(2);
                        w.u64(*c);
                        w.u64(*max);
                    }
                    HostNetOp::Shutdown(c) => {
                        w.u8(3);
                        w.u64(*c);
                    }
                    HostNetOp::Abort(c) => {
                        w.u8(4);
                        w.u64(*c);
                    }
                    HostNetOp::Release(c) => {
                        w.u8(5);
                        w.u64(*c);
                    }
                }
            }
        }
    }

    fn load(r: &mut Reader<'_>) -> vetro_snapshot::Result<Input> {
        let events = |r: &mut Reader<'_>| {
            r.seq(8, |r| Ok(InputEvent { ty: r.u16()?, code: r.u16()?, value: r.u32()? }))
        };
        let conn = |r: &mut Reader<'_>| -> vetro_snapshot::Result<VsockConn> {
            Ok(VsockConn { host_port: r.u32()?, guest_port: r.u32()? })
        };
        Ok(match r.u8()? {
            0 => Input::Console(r.vec()?),
            1 => Input::Keyboard(events(r)?),
            2 => Input::Pointer(events(r)?),
            3 => Input::Gpio { line: r.u32()?, level: r.bool()? },
            4 => Input::Display { scanout: r.u32()?, width: r.u32()?, height: r.u32()? },
            5 => Input::NetFrame(r.vec()?),
            6 => Input::NetLink(r.bool()?),
            7 => Input::Vsock(match r.u8()? {
                0 => VsockOp::Listen(r.u32()?),
                1 => VsockOp::Unlisten(r.u32()?),
                2 => VsockOp::Accept(r.u32()?),
                3 => VsockOp::Connect(r.u32()?),
                4 => VsockOp::Send(conn(r)?, r.vec()?),
                5 => VsockOp::Recv(conn(r)?, r.u64()?),
                6 => VsockOp::ShutdownSend(conn(r)?),
                7 => VsockOp::Close(conn(r)?),
                8 => VsockOp::Reset(conn(r)?),
                9 => VsockOp::Release(conn(r)?),
                10 => VsockOp::TransportReset,
                k => return Err(Error::invalid(format!("vsock operation {k}"))),
            }),
            8 => Input::HostNet(match r.u8()? {
                0 => HostNetOp::Connect(r.u16()?),
                1 => HostNetOp::Send(r.u64()?, r.vec()?),
                2 => HostNetOp::Recv(r.u64()?, r.u64()?),
                3 => HostNetOp::Shutdown(r.u64()?),
                4 => HostNetOp::Abort(r.u64()?),
                5 => HostNetOp::Release(r.u64()?),
                k => return Err(Error::invalid(format!("host network operation {k}"))),
            }),
            k => return Err(Error::invalid(format!("input type {k}"))),
        })
    }
}

/// What happened at a log event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A host input.
    Input(Input),
    /// A host access to a device that the log cannot describe
    /// (`Machine::device` and its derivatives, with a closure): if it changed
    /// something, replay cannot redo it. Replay stops here with
    /// [`Divergence::Opaque`].
    Opaque { slot: Option<u32> },
}

/// A log event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Instructions executed when the input arrived: replay
    /// applies it between two quanta at exactly this number.
    pub step: u64,
    /// `hash64` of the CPU registers (`Cpu` in the snapshot) right before
    /// the input: replay compares it.
    pub cpu: u64,
    /// Bytes output by the console up to that moment.
    pub console: u64,
    pub kind: EventKind,
}

/// Snapshot taken during recording (ADR 0015), to restart near
/// an instruction without redoing everything from the start. Events with the same
/// `step` come after the snapshot.
#[derive(Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub step: u64,
    /// Console up to this point: bytes and hash (see [`Digest`]).
    pub console_len: u64,
    pub console_hash: u64,
    /// `Machine::save` (with the console output already taken from the UART).
    pub snapshot: Vec<u8>,
}

impl fmt::Debug for Keyframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyframe")
            .field("step", &self.step)
            .field("console_len", &self.console_len)
            .field("snapshot", &format_args!("{} bytes", self.snapshot.len()))
            .finish()
    }
}

/// Fingerprint of the machine state at a point: what replay must
/// find identical.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    /// Instructions executed.
    pub steps: u64,
    /// `hash64` of the CPU (general registers, SIMD/FP, PSTATE, system
    /// registers, exclusive monitor).
    pub cpu: u64,
    /// `hash64` of the MMU with the TLB (with the JIT the TLB sees fewer accesses,
    /// ADR 0013: it is not compared if either of the two runs has the JIT).
    pub mmu: u64,
    /// `hash64` of the platform: timer, GIC, UART, RTC, GPIO and all the
    /// virtio devices with their internal backends (network stack, copy-on-write
    /// disks).
    pub platform: u64,
    /// `hash64` of the RAM.
    pub ram: u64,
    /// Bytes output by the console since the start of the recording.
    pub console_len: u64,
    /// Their hash (64-bit FNV-1a, incremental).
    pub console_hash: u64,
}

impl Digest {
    /// The first difference from `other`, if any (`tlb`: also compares the
    /// MMU).
    pub fn diff(&self, other: &Digest, tlb: bool) -> Option<&'static str> {
        if self.steps != other.steps {
            Some("instructions")
        } else if self.console_len != other.console_len || self.console_hash != other.console_hash {
            Some("console")
        } else if self.cpu != other.cpu {
            Some("CPU")
        } else if self.ram != other.ram {
            Some("RAM")
        } else if self.platform != other.platform {
            Some("devices")
        } else if tlb && self.mmu != other.mmu {
            Some("MMU and TLB")
        } else {
            None
        }
    }

    fn save(&self, w: &mut Writer) {
        for v in
            [self.steps, self.cpu, self.mmu, self.platform, self.ram, self.console_len, self.console_hash]
        {
            w.u64(v);
        }
    }

    fn load(r: &mut Reader<'_>) -> vetro_snapshot::Result<Digest> {
        Ok(Digest {
            steps: r.u64()?,
            cpu: r.u64()?,
            mmu: r.u64()?,
            platform: r.u64()?,
            ram: r.u64()?,
            console_len: r.u64()?,
            console_hash: r.u64()?,
        })
    }
}

/// A complete recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    /// `Machine::config_hash` of the recorded machine.
    pub config_hash: u64,
    /// Its configuration (to rebuild it).
    pub config: MachineConfig,
    /// Snapshot format version of the [`Keyframe`]s.
    pub snapshot_version: u32,
    /// The recording used the JIT (at some point).
    pub jit: bool,
    /// Instructions between two keyframes (0 = none).
    pub keyframe_every: u64,
    /// Starting state.
    pub start: Digest,
    /// Events in `step` order (non-decreasing).
    pub events: Vec<Event>,
    /// Keyframes in increasing `step` order.
    pub keyframes: Vec<Keyframe>,
    /// State at the end of the recording.
    pub end: Digest,
}

/// Why a log cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogError(pub Error);

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Error::BadMagic => write!(f, "not a Vetro recording (unknown header)"),
            Error::Version { found, expected } => write!(
                f,
                "recording in format version {found}, this version of Vetro only reads version {expected}"
            ),
            Error::Checksum => write!(f, "corrupted recording (wrong checksum)"),
            Error::Truncated => write!(f, "truncated recording"),
            e => write!(f, "invalid recording: {e}"),
        }
    }
}

impl std::error::Error for LogError {}

impl From<Error> for LogError {
    fn from(e: Error) -> Self {
        LogError(e)
    }
}

impl Log {
    /// The file: a `vetro_snapshot` container ([`LOG_MAGIC`], [`LOG_VERSION`],
    /// configuration hash) with the sections `HEAD`, `EVTS`, `KEYF`,
    /// `END `.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.section(b"HEAD", |w| {
            w.u32(self.snapshot_version);
            w.u64(self.config.ram_size);
            w.u64(self.config.now_secs);
            w.u64(self.config.seed);
            w.bool(self.jit);
            w.u64(self.keyframe_every);
            self.start.save(w);
            if self.config.cpus > 1 {
                w.u32(self.config.cpus);
            }
        });
        w.section(b"EVTS", |w| {
            w.seq(&self.events, |w, e| {
                w.u64(e.step);
                w.u64(e.cpu);
                w.u64(e.console);
                match &e.kind {
                    EventKind::Input(i) => {
                        w.u8(0);
                        i.save(w);
                    }
                    EventKind::Opaque { slot } => {
                        w.u8(1);
                        w.opt(*slot, Writer::u32);
                    }
                }
            })
        });
        w.section(b"KEYF", |w| {
            w.seq(&self.keyframes, |w, k| {
                w.u64(k.step);
                w.u64(k.console_len);
                w.u64(k.console_hash);
                w.bytes(&k.snapshot);
            })
        });
        w.section(b"END ", |w| self.end.save(w));
        vetro_snapshot::encode_container(&LOG_MAGIC, LOG_VERSION, self.config_hash, w.as_bytes())
    }

    /// Reads a file from [`Log::encode`]: magic, version, checksum,
    /// then the content (events in order, increasing keyframes).
    pub fn decode(bytes: &[u8]) -> Result<Log, LogError> {
        let (header, payload) = vetro_snapshot::decode_container(&LOG_MAGIC, LOG_VERSION, bytes)?;
        let mut r = Reader::new(payload);
        let mut s = r.section(b"HEAD")?;
        let snapshot_version = s.u32()?;
        let (ram_size, now_secs, seed) = (s.u64()?, s.u64()?, s.u64()?);
        let jit = s.bool()?;
        let keyframe_every = s.u64()?;
        let start = Digest::load(&mut s)?;
        // Cores (ADR 0042): only with more than one, so single-core logs keep
        // their bytes.
        let cpus = if s.remaining() > 0 { s.u32()? } else { 1 };
        let config = MachineConfig { ram_size, now_secs, seed, cpus };
        s.finish()?;
        let mut s = r.section(b"EVTS")?;
        let events = s.seq(26, |r| {
            let (step, cpu, console) = (r.u64()?, r.u64()?, r.u64()?);
            let kind = match r.u8()? {
                0 => EventKind::Input(Input::load(r)?),
                1 => EventKind::Opaque { slot: r.opt(|r| r.u32())? },
                k => return Err(Error::invalid(format!("event type {k}"))),
            };
            Ok(Event { step, cpu, console, kind })
        })?;
        s.finish()?;
        let mut s = r.section(b"KEYF")?;
        let keyframes = s.seq(32, |r| {
            Ok(Keyframe { step: r.u64()?, console_len: r.u64()?, console_hash: r.u64()?, snapshot: r.vec()? })
        })?;
        s.finish()?;
        let mut s = r.section(b"END ")?;
        let end = Digest::load(&mut s)?;
        s.finish()?;
        r.finish()?;
        if events.windows(2).any(|w| w[1].step < w[0].step)
            || events.first().is_some_and(|e| e.step < start.steps)
            || events.last().is_some_and(|e| e.step > end.steps)
        {
            return Err(Error::invalid("events out of order").into());
        }
        if keyframes.windows(2).any(|w| w[1].step <= w[0].step) {
            return Err(Error::invalid("keyframes out of order").into());
        }
        Ok(Log {
            config_hash: header.config_hash,
            config,
            snapshot_version,
            jit,
            keyframe_every,
            start,
            events,
            keyframes,
            end,
        })
    }

    /// The last keyframe not beyond instruction `step`.
    pub fn keyframe_before(&self, step: u64) -> Option<&Keyframe> {
        self.keyframes.iter().rev().find(|k| k.step <= step)
    }

    /// Bytes of the events in the file (without keyframes): the part that grows with
    /// the inputs.
    pub fn events_len(&self) -> usize {
        let mut l = self.clone();
        l.keyframes.clear();
        l.encode().len()
    }
}

/// Why a replay stopped before the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// The log does not apply to this machine: different configuration,
    /// different starting state, keyframes of another version.
    Start(String),
    /// At an event the registers or the console are not the recorded ones: an
    /// input escaped the log, or the machine is not deterministic.
    Event { index: usize, step: u64, what: &'static str },
    /// Execution went past an event's instruction without stopping there
    /// (or it stopped, idle or powered off, before getting there).
    Missed { index: usize, step: u64, at: u64 },
    /// An access that cannot be recorded ([`EventKind::Opaque`]).
    Opaque { index: usize, step: u64, slot: Option<u32> },
    /// At the end the state is not the recorded one.
    End { what: &'static str },
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Divergence::Start(why) => write!(f, "the log does not apply to this machine: {why}"),
            Divergence::Event { index, step, what } => write!(
                f,
                "event {index} at instruction {step}: {what} differ from the recording (did an input \
                 escape the log?)"
            ),
            Divergence::Missed { index, step, at } => write!(
                f,
                "event {index} expected at instruction {step}, the machine is at {at}: execution is not \
                 the recorded one"
            ),
            Divergence::Opaque { index, step, slot } => write!(
                f,
                "event {index} at instruction {step}: host access to device {slot:?} that the log \
                 does not describe, replay cannot continue"
            ),
            Divergence::End { what } => write!(f, "at the end of the recording: {what} differ"),
        }
    }
}

/// State of a replay ([`Machine::replay_status`](crate::Machine::replay_status)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayStatus {
    /// In progress: `next` is the next event to apply.
    Running { next: usize },
    /// Reached the end of the recording with the same state: from here
    /// the machine runs free.
    Finished,
    /// Stopped at a difference: the machine runs free from there.
    Diverged(Divergence),
}

/// Incremental hash of the console bytes (64-bit FNV-1a).
pub(crate) fn console_hash(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Initial value of [`console_hash`].
pub(crate) const CONSOLE_HASH_INIT: u64 = 0xcbf2_9ce4_8422_2325;

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::virtio::input::BTN_LEFT;

    /// Linux `KEY_A`.
    const KEY_A: u16 = 30;

    fn sample() -> Log {
        let c = VsockConn { host_port: 1025, guest_port: 7 };
        let inputs = vec![
            Input::Console(b"ls\n".to_vec()),
            Input::Keyboard(Input::key_events(KEY_A, true)),
            Input::Pointer(Input::move_abs_events(3, 4)),
            Input::Pointer(Input::touch_events(1, Some((5, 6)))),
            Input::Pointer(Input::touch_events(1, None)),
            Input::Gpio { line: 3, level: true },
            Input::Display { scanout: 0, width: 800, height: 600 },
            Input::NetFrame(vec![0xff; 60]),
            Input::NetLink(false),
            Input::Vsock(VsockOp::Listen(5000)),
            Input::Vsock(VsockOp::Unlisten(5000)),
            Input::Vsock(VsockOp::Accept(5000)),
            Input::Vsock(VsockOp::Connect(22)),
            Input::Vsock(VsockOp::Send(c, b"ciao".to_vec())),
            Input::Vsock(VsockOp::Recv(c, 99)),
            Input::Vsock(VsockOp::ShutdownSend(c)),
            Input::Vsock(VsockOp::Close(c)),
            Input::Vsock(VsockOp::Reset(c)),
            Input::Vsock(VsockOp::Release(c)),
            Input::Vsock(VsockOp::TransportReset),
            Input::HostNet(HostNetOp::Connect(5555)),
            Input::HostNet(HostNetOp::Send(1, b"CNXN".to_vec())),
            Input::HostNet(HostNetOp::Recv(1, 65536)),
            Input::HostNet(HostNetOp::Shutdown(1)),
            Input::HostNet(HostNetOp::Abort(2)),
            Input::HostNet(HostNetOp::Release(1)),
            Input::Keyboard(Input::key_events(BTN_LEFT, false)),
        ];
        let mut events: Vec<Event> = inputs
            .into_iter()
            .enumerate()
            .map(|(i, input)| Event {
                step: 10 + i as u64,
                cpu: i as u64 * 3,
                console: 1,
                kind: EventKind::Input(input),
            })
            .collect();
        events.push(Event { step: 90, cpu: 1, console: 2, kind: EventKind::Opaque { slot: Some(31) } });
        Log {
            config_hash: 0x1234,
            config: MachineConfig::default(),
            snapshot_version: vetro_snapshot::FORMAT_VERSION,
            jit: true,
            keyframe_every: 50,
            start: Digest { steps: 5, cpu: 1, mmu: 2, platform: 3, ram: 4, console_len: 0, console_hash: 7 },
            events,
            keyframes: vec![
                Keyframe { step: 5, console_len: 0, console_hash: 1, snapshot: vec![1, 2, 3] },
                Keyframe { step: 55, console_len: 9, console_hash: 2, snapshot: vec![4; 100] },
            ],
            end: Digest { steps: 100, ..Digest::default() },
        }
    }

    /// Every kind of input and event round-trips through the file.
    #[test]
    fn log_round_trip() {
        let log = sample();
        let bytes = log.encode();
        assert_eq!(Log::decode(&bytes).unwrap(), log);
        assert_eq!(log.keyframe_before(54).unwrap().step, 5);
        assert_eq!(log.keyframe_before(55).unwrap().step, 55);
        assert!(log.keyframe_before(4).is_none());
        assert!(log.events_len() < bytes.len());
    }

    /// A log of another version, or corrupted, truncated or inconsistent, is
    /// rejected with a message that states the reason.
    #[test]
    fn damaged_logs_rejected() {
        let bytes = sample().encode();
        let mut other = bytes.clone();
        other[8..12].copy_from_slice(&(LOG_VERSION + 1).to_le_bytes());
        let e = Log::decode(&other).unwrap_err();
        assert!(e.to_string().contains("format version"), "{e}");
        let mut bad = bytes.clone();
        let n = bad.len() - 3;
        bad[n] ^= 1;
        assert_eq!(Log::decode(&bad).unwrap_err().0, Error::Checksum);
        assert!(Log::decode(&bytes[..bytes.len() - 1]).is_err());
        let snap = vetro_snapshot::encode_file(1, b"");
        assert!(Log::decode(&snap).unwrap_err().to_string().contains("not a Vetro recording"));
        let mut disordered = sample();
        disordered.events.swap(0, 1);
        assert!(Log::decode(&disordered.encode()).is_err());
    }

    /// The virtio-input event helpers give exactly what the device's
    /// methods give (same saved state).
    #[test]
    fn events_like_virtio_input() {
        use vetro_platform::VirtioDevice;
        use vetro_platform::virtio::{InputConfig, VirtioInput};
        let state = |d: &VirtioInput| {
            let mut w = Writer::new();
            d.save_state(&mut w);
            w.into_bytes()
        };
        let (mut a, mut b) =
            (VirtioInput::new(InputConfig::multitouch()), VirtioInput::new(InputConfig::multitouch()));
        // Without an active driver the events count as dropped: enough to
        // compare how many come out.
        a.key(KEY_A, true);
        a.move_abs(1, 2);
        a.touch(2, Some((3, 4)));
        a.touch(2, None);
        for ev in [
            Input::key_events(KEY_A, true),
            Input::move_abs_events(1, 2),
            Input::touch_events(2, Some((3, 4))),
            Input::touch_events(2, None),
        ] {
            b.inject(&ev);
        }
        assert_eq!(a.dropped(), 2 + 3 + 6 + 4);
        assert_eq!(state(&a), state(&b));
    }
}
