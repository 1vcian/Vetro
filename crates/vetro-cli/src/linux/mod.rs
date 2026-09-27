//! arm64 Linux kernel emulated in user mode.
//!
//! Several processes and threads run in the same emulator, taking turns (quanta of
//! instructions). Blocking syscalls do not block the host: the task stops
//! with `pc` on the SVC and re-executes it when the condition can be
//! satisfied (child exited, data in a pipe, deadline reached...).
//!
//! Time is virtual by default (one instruction = one nanosecond, jumps
//! forward when everyone sleeps): repeatable runs, as the determinism rule
//! requires (CLAUDE.md). `getrandom`, AT_RANDOM and
//! /dev/urandom are deterministic too.

pub mod abi;
mod fs;
mod ipc;
mod loader;
mod locks;
mod mm;
mod process;
mod procfs;
mod signal;
mod syscall;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use vetro_cpu::{Cpu, Exception};
use vetro_jit::{JitConfig, JitCpu, JitStats};
use vetro_jit_native::NativeEngine;

pub use fs::Console;
use fs::FdTable;
use mm::Mm;
use signal::{SigHand, SigState};

pub type Pid = i32;

pub const RLIM_NLIMITS: usize = 16;
const INF: u64 = u64::MAX;
/// Initial limits, the typical Linux ones; RLIMIT_CORE at 0: no core.
pub const DEFAULT_RLIMITS: [(u64, u64); RLIM_NLIMITS] = [
    (INF, INF),         // CPU
    (INF, INF),         // FSIZE
    (INF, INF),         // DATA
    (8 << 20, INF),     // STACK
    (0, INF),           // CORE
    (INF, INF),         // RSS
    (INF, INF),         // NPROC
    (1024, 4096),       // NOFILE
    (8 << 20, 8 << 20), // MEMLOCK
    (INF, INF),         // AS
    (INF, INF),         // LOCKS
    (31_805, 31_805),   // SIGPENDING
    (819_200, 819_200), // MSGQUEUE
    (0, 0),             // NICE
    (0, 0),             // RTPRIO
    (INF, INF),         // RTTIME
];

/// Virtual time per instruction: a nominal 100 MHz CPU, close to the
/// interpreter's real speed (so sleep and alarm cost little).
pub const NS_PER_STEP: u64 = 10;

/// Virtual time of a syscall: on a real kernel it costs about one
/// microsecond, and a loop of nothing but syscalls must still make time pass.
pub const NS_PER_SYSCALL: u64 = 1000;

/// Identity of a futex: (space, offset). For shared memory the space
/// is the shared buffer (like the physical page for Linux), otherwise the
/// process address space, and the offset is the virtual address.
pub type FutexKey = (usize, u64);

/// File mapped with MAP_SHARED: path on the host and common buffer.
pub type SharedFile = (std::path::PathBuf, Rc<RefCell<Vec<u8>>>);

/// Linux signal numbers.
pub mod sig {
    pub const SIGHUP: i32 = 1;
    pub const SIGINT: i32 = 2;
    pub const SIGQUIT: i32 = 3;
    pub const SIGILL: i32 = 4;
    pub const SIGTRAP: i32 = 5;
    pub const SIGABRT: i32 = 6;
    pub const SIGBUS: i32 = 7;
    pub const SIGFPE: i32 = 8;
    pub const SIGKILL: i32 = 9;
    pub const SIGUSR1: i32 = 10;
    pub const SIGSEGV: i32 = 11;
    pub const SIGUSR2: i32 = 12;
    pub const SIGPIPE: i32 = 13;
    pub const SIGALRM: i32 = 14;
    pub const SIGTERM: i32 = 15;
    pub const SIGCHLD: i32 = 17;
    pub const SIGCONT: i32 = 18;
    pub const SIGSTOP: i32 = 19;
    pub const SIGTSTP: i32 = 20;
    pub const SIGIO: i32 = 29;
    pub const SIGSYS: i32 = 31;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMode {
    /// Deterministic virtual time (default).
    Virtual,
    /// Host clock.
    Host,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Copies the guest's stdout/stderr to the host's.
    pub echo: bool,
    /// Prints the syscalls to stderr, like strace.
    pub strace: bool,
    pub clock: ClockMode,
    /// Total instruction limit (all tasks).
    pub max_steps: u64,
    /// Content of stdin.
    pub stdin: Vec<u8>,
    /// Initial working directory of the guest.
    pub cwd: String,
    /// Like `qemu -L`: absolute paths are looked up here first.
    pub sysroot: Option<String>,
    /// CPUs visible to the guest (sched_getaffinity). QEMU user mode shows
    /// the host's; the fixed default keeps the run reproducible.
    pub cpus: usize,
    /// Kernel version in uname (QEMU user mode reports the host's).
    pub release: String,
    /// Runs with the JIT (M4, ADR 0012): same results and same clock
    /// as the interpreter.
    pub jit: bool,
    /// Executions of a block with the interpreter before compiling it (0 =
    /// immediately; the parity tests use it to translate even code
    /// executed only once).
    pub jit_threshold: u32,
}

impl Config {
    /// For the test harnesses: `VETRO_JIT=1` turns on the JIT and
    /// `VETRO_JIT_THRESHOLD=N` sets its threshold (0 = translates everything from the
    /// first execution).
    pub fn jit_from_env(mut self) -> Self {
        if std::env::var("VETRO_JIT").is_ok_and(|v| v == "1") {
            self.jit = true;
        }
        if let Some(n) = std::env::var("VETRO_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()) {
            self.jit_threshold = n;
        }
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            echo: false,
            strace: false,
            clock: ClockMode::Virtual,
            max_steps: 20_000_000_000,
            stdin: Vec::new(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into()),
            sysroot: None,
            cpus: 1,
            release: "6.6.0-vetro".into(),
            jit: false,
            jit_threshold: DEFAULT_JIT_THRESHOLD,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The initial process exited with this code.
    Code(i32),
    /// The initial process was killed by a signal. `cause` is
    /// the CPU exception, if the signal originated there.
    Signal {
        signo: i32,
        cause: Option<Exception>,
        pc: u64,
    },
    /// Syscall not implemented yet: our limitation, not the guest's.
    UnsupportedSyscall {
        nr: u64,
        pc: u64,
    },
    /// Valid instruction but not implemented yet.
    Unimplemented {
        raw: u32,
        what: &'static str,
        pc: u64,
    },
    StepLimit,
    /// All tasks are blocked and nothing can wake them.
    Deadlock,
}

/// Why a task is stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    /// wait4/waitid: a child matching `pid` (like wait4).
    Child { pid: i32 },
    /// Read from an empty pipe or write to a full one.
    Pipe,
    /// Sleep until `until` (ns of monotonic time).
    Sleep { until: u64 },
    /// FUTEX_WAIT on `addr`, with optional deadline.
    Futex { key: FutexKey, until: Option<u64> },
    /// The parent of a vfork waits for the child to exec or exit.
    Vfork { child: Pid },
    /// pause/rt_sigsuspend: only a signal wakes it.
    Signal,
    /// ppoll/pselect: a ready pipe or the deadline.
    Poll { until: Option<u64> },
    /// futex_waitv: the first wake on one of the keys, or the deadline.
    FutexV { keys: Vec<FutexKey>, until: Option<u64> },
    /// rt_sigtimedwait: a signal of `set` pending (even if blocked) or
    /// the deadline.
    SigWait { set: u64, until: Option<u64> },
    /// Condition to recheck on every round (F_SETLKW): the syscall is
    /// re-executed until it succeeds.
    Retry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Runnable,
    Blocked(Wait),
    /// Thread exited; for the group leader the exit status remains
    /// until the parent reaps it.
    Zombie {
        status: i32,
    },
    Dead,
}

pub struct Task {
    pub tid: Pid,
    /// Process id (thread group).
    pub tgid: Pid,
    pub ppid: Pid,
    pub pgid: Pid,
    pub cpu: Cpu,
    pub mm: Rc<RefCell<Mm>>,
    pub files: Rc<RefCell<FdTable>>,
    pub cwd: Rc<RefCell<String>>,
    pub sighand: Rc<RefCell<SigHand>>,
    pub sig: SigState,
    pub state: State,
    pub clear_child_tid: u64,
    /// Signal to send to the parent on exit (SIGCHLD for fork).
    pub exit_signal: i32,
    /// Woken by FUTEX_WAKE: the next re-executed FUTEX_WAIT returns 0.
    pub futex_woken: bool,
    /// For futex_waitv: the index of the key that woke the task.
    pub futex_index: usize,
    /// For vfork: the parent to unblock on exec/exit.
    pub vfork_parent: Option<Pid>,
    /// Synchronous signal (fault) generated by the last instruction.
    pub fault: Option<(i32, Exception)>,
    pub comm: String,
    pub exe: String,
    pub umask: u32,
    /// Absolute deadline of the blocking syscall in progress (nanosleep, futex
    /// with timeout): set on the first block, so re-executing the SVC
    /// does not move it.
    pub deadline: Option<u64>,
    /// /proc/<pid>/oom_score_adj.
    pub oom_score_adj: i32,
    /// Resource limits (getrlimit/setrlimit): (current, maximum).
    pub rlimits: [(u64, u64); RLIM_NLIMITS],
    /// personality(2): execution domain and flags (UNAME26, ...).
    pub personality: u32,
}

pub struct Kernel {
    pub cfg: Config,
    pub tasks: Vec<Task>,
    pub console: Rc<RefCell<Console>>,
    next_pid: Pid,
    init: Pid,
    /// Virtual monotonic time in ns.
    clock_ns: u64,
    /// Lower bound of the next alarm/itimer deadline (MAX =
    /// none): avoids scanning the tasks on every instruction.
    pub(super) next_alarm: u64,
    steps: u64,
    run_queue: VecDeque<usize>,
    rng: u64,
    /// Outcome of the initial process, when it terminates.
    init_exit: Option<Exit>,
    /// Last CPU exception that killed a process (for the report).
    last_fault: Option<(Pid, Exception, u64)>,
    /// MAP_SHARED file mappings: (device, inode) → (path, buffer).
    pub shared_files: std::collections::HashMap<(u64, u64), SharedFile>,
    /// POSIX file locks.
    locks: locks::LockTable,
    /// IPC System V.
    ipc: ipc::Ipc,
    /// File system FIFOs, by (device, inode): opening them on the host
    /// would block the emulator.
    fifos: std::collections::HashMap<(u64, u64), fs::Fifo>,
    /// The JIT, if `cfg.jit`: one for the whole kernel (the block cache
    /// is per address space).
    jit: Option<Box<JitCpu<NativeEngine>>>,
}

/// Default JIT threshold (see [`Config::jit_threshold`]).
pub const DEFAULT_JIT_THRESHOLD: u32 = 16;

/// Instructions per scheduling quantum.
const QUANTUM: u64 = 20_000;
/// Initial "realtime" instant of virtual time: 2026-01-01T00:00:00Z.
const EPOCH: u64 = 1_767_225_600;

impl Kernel {
    pub fn new(cfg: Config) -> Self {
        let console = Rc::new(RefCell::new(Console::new(cfg.stdin.clone(), cfg.echo)));
        let jit = cfg.jit.then(|| {
            let profile = std::env::var("VETRO_JIT_PROFILE").is_ok_and(|v| v == "1");
            let jc = JitConfig { hot_threshold: cfg.jit_threshold, profile, ..JitConfig::default() };
            Box::new(JitCpu::new(NativeEngine::new(), jc))
        });
        Kernel {
            cfg,
            tasks: Vec::new(),
            console,
            next_pid: 100,
            init: 0,
            clock_ns: 0,
            next_alarm: u64::MAX,
            steps: 0,
            run_queue: VecDeque::new(),
            rng: 0x5eed_0000_0000_0001,
            init_exit: None,
            last_fault: None,
            shared_files: std::collections::HashMap::new(),
            locks: locks::LockTable::default(),
            ipc: ipc::Ipc::default(),
            fifos: std::collections::HashMap::new(),
            jit,
        }
    }

    /// Instructions executed so far (all tasks).
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// JIT counters, if enabled.
    pub fn jit_stats(&self) -> Option<JitStats> {
        self.jit.as_ref().map(|j| j.stats)
    }

    /// Interpreter instructions by class (`VETRO_JIT_PROFILE=1`).
    pub fn jit_profile(&self) -> Option<&vetro_jit::Profile> {
        self.jit.as_ref().and_then(|j| j.profile.as_ref())
    }

    pub fn stdout(&self) -> Vec<u8> {
        self.console.borrow().stdout.clone()
    }

    pub fn stderr(&self) -> Vec<u8> {
        self.console.borrow().stderr.clone()
    }

    fn alloc_pid(&mut self) -> Pid {
        let p = self.next_pid;
        self.next_pid += 1;
        p
    }

    /// Creates the initial process from an ELF image.
    pub fn spawn(
        &mut self,
        image: &[u8],
        argv: &[Vec<u8>],
        envp: &[Vec<u8>],
        exe: &str,
    ) -> Result<Pid, String> {
        let pid = self.alloc_pid();
        let random = self.random_bytes(16);
        let img = loader::load(image, argv, envp, exe, &random).map_err(|e| e.to_string())?;
        let mut cpu = Cpu::new();
        cpu.pc = img.entry;
        cpu.sp = img.sp;
        let files = FdTable::with_console(&self.console);
        let task = Task {
            tid: pid,
            tgid: pid,
            ppid: 1,
            pgid: pid,
            cpu,
            mm: Rc::new(RefCell::new(img.mm)),
            files: Rc::new(RefCell::new(files)),
            cwd: Rc::new(RefCell::new(self.cfg.cwd.clone())),
            sighand: Rc::new(RefCell::new(SigHand::default())),
            sig: SigState::default(),
            state: State::Runnable,
            clear_child_tid: 0,
            exit_signal: sig::SIGCHLD,
            futex_woken: false,
            futex_index: 0,
            vfork_parent: None,
            fault: None,
            comm: comm_of(exe),
            exe: exe.into(),
            umask: 0o022,
            deadline: None,
            oom_score_adj: 0,
            rlimits: DEFAULT_RLIMITS,
            personality: 0,
        };
        self.tasks.push(task);
        if self.init == 0 {
            self.init = pid;
        }
        Ok(pid)
    }

    /// Key of the futex at address `addr` in the space `mm`.
    pub fn futex_key(&self, mm: &Rc<RefCell<Mm>>, addr: u64, private: bool) -> FutexKey {
        if !private && let Some(k) = mm.borrow().mem.shared_key(addr) {
            return k;
        }
        (Rc::as_ptr(mm) as usize, addr)
    }

    /// Writes the content of the shared mappings back to the files.
    /// Writes to the file the content of its MAP_SHARED (if any), before
    /// a descriptor reads or writes it.
    pub fn flush_shared_one(&self, key: (u64, u64)) {
        use std::os::unix::fs::FileExt;
        if let Some((path, buf)) = self.shared_files.get(&key)
            && let Ok(f) = std::fs::OpenOptions::new().write(true).open(path)
        {
            let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
            let b = buf.borrow();
            let _ = f.write_at(&b[..len.min(b.len())], 0);
        }
    }

    /// Reloads the MAP_SHARED from the file after a write or an ftruncate through a
    /// descriptor: the mappings see the new content and the new end.
    pub fn reload_shared_one(&self, key: (u64, u64)) {
        if let Some((path, buf)) = self.shared_files.get(&key)
            && let Ok(content) = std::fs::read(path)
        {
            *buf.borrow_mut() = content;
        }
    }

    pub fn flush_shared(&self) {
        use std::os::unix::fs::FileExt;
        for (path, buf) in self.shared_files.values() {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
                let b = buf.borrow();
                let n = len.min(b.len());
                let _ = f.write_at(&b[..n], 0);
            }
        }
    }

    pub fn find(&self, tid: Pid) -> Option<usize> {
        self.tasks.iter().position(|t| t.tid == tid && t.state != State::Dead)
    }

    /// Monotonic time in ns.
    pub fn now(&self) -> u64 {
        match self.cfg.clock {
            ClockMode::Virtual => self.clock_ns,
            ClockMode::Host => {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
            }
        }
    }

    /// "Realtime" time in ns since the Unix epoch.
    pub fn realtime(&self) -> u64 {
        match self.cfg.clock {
            ClockMode::Virtual => EPOCH * 1_000_000_000 + self.clock_ns,
            ClockMode::Host => self.now(),
        }
    }

    /// Deterministic pseudorandom bytes (getrandom, AT_RANDOM, /dev/urandom).
    pub fn random_bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.rng;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.extend_from_slice(&z.to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// Runs until the initial process terminates.
    pub fn run(&mut self) -> Exit {
        loop {
            if let Some(e) = self.init_exit {
                return e;
            }
            if self.steps >= self.cfg.max_steps {
                return Exit::StepLimit;
            }
            let Some(t) = self.pick() else {
                // Nothing runnable: advance time to the next
                // deadline, if any.
                match self.next_deadline() {
                    Some(d) if self.cfg.clock == ClockMode::Virtual => {
                        self.clock_ns = self.clock_ns.max(d);
                        // An expired alarm wakes whoever is waiting for a signal.
                        self.check_alarms();
                        continue;
                    }
                    Some(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        self.check_alarms();
                        continue;
                    }
                    None => return Exit::Deadlock,
                }
            };
            if let Some(e) = self.run_task(t) {
                return e;
            }
        }
    }

    /// Next runnable task (round robin), waking those that can resume.
    fn pick(&mut self) -> Option<usize> {
        let n = self.tasks.len();
        if n == 0 {
            return None;
        }
        let start = self.run_queue.pop_front().unwrap_or(0);
        for k in 0..n {
            let i = (start + k) % n;
            if self.ready(i) {
                self.run_queue.push_back((i + 1) % n);
                return Some(i);
            }
        }
        None
    }

    /// True if task `i` can run (waking it if needed).
    fn ready(&mut self, i: usize) -> bool {
        let wake = match &self.tasks[i].state {
            State::Runnable => return true,
            State::Zombie { .. } | State::Dead => return false,
            State::Blocked(w) => {
                let w = w.clone();
                // Retry is always "satisfied" (the syscall rechecks by itself): a
                // deliverable signal interrupts it first, like Linux's F_SETLKW
                // (EINTR, or restart with SA_RESTART).
                if matches!(w, Wait::Retry) && self.signal_wakes(i) {
                    self.tasks[i].sig.interrupted = Some(w);
                    true
                } else if self.wait_satisfied(i, &w) {
                    true
                } else if self.signal_wakes(i) {
                    self.tasks[i].sig.interrupted = Some(w);
                    true
                } else {
                    false
                }
            }
        };
        if wake {
            self.tasks[i].state = State::Runnable;
        }
        wake
    }

    fn wait_satisfied(&self, i: usize, w: &Wait) -> bool {
        match w {
            Wait::Child { pid } => {
                self.has_waitable_child(self.tasks[i].tgid, *pid) || !self.has_child(self.tasks[i].tgid, *pid)
            }
            Wait::Pipe => self.tasks[i].files.borrow().any_pipe_ready(),
            Wait::Sleep { until } => self.now() >= *until,
            Wait::Futex { until, .. } | Wait::FutexV { until, .. } => {
                self.tasks[i].futex_woken || until.is_some_and(|u| self.now() >= u)
            }
            Wait::Vfork { child } => self.find(*child).is_none_or(|c| self.tasks[c].vfork_parent.is_none()),
            Wait::Signal => false,
            Wait::Retry => true,
            Wait::Poll { until } => {
                self.tasks[i].files.borrow().any_pipe_ready() || until.is_some_and(|u| self.now() >= u)
            }
            Wait::SigWait { set, until } => {
                self.tasks[i].sig.pending & set != 0 || until.is_some_and(|u| self.now() >= u)
            }
        }
    }

    fn next_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        for t in &self.tasks {
            let d = match &t.state {
                State::Blocked(Wait::Sleep { until }) => Some(*until),
                State::Blocked(Wait::Futex { until: Some(u), .. }) => Some(*u),
                State::Blocked(Wait::Poll { until: Some(u) }) => Some(*u),
                State::Blocked(Wait::SigWait { until: Some(u), .. }) => Some(*u),
                State::Blocked(Wait::FutexV { until: Some(u), .. }) => Some(*u),
                _ => None,
            };
            let d = match (d, t.sig.alarm) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if let Some(d) = d {
                best = Some(best.map_or(d, |b: u64| b.min(d)));
            }
        }
        best
    }

    /// Runs task `t` for one quantum. Returns the final outcome if the
    /// run is over.
    fn run_task(&mut self, t: usize) -> Option<Exit> {
        let mut n = 0;
        while n < QUANTUM {
            self.check_alarms();
            if self.tasks[t].state != State::Runnable {
                break;
            }
            if self.deliver_signals(t) {
                // the task may have died
                if self.tasks[t].state != State::Runnable {
                    break;
                }
            }
            let mm = self.tasks[t].mm.clone();
            let budget = if self.jit.is_some() { self.jit_budget(t, QUANTUM - n) } else { 1 };
            let (k, res) = match self.jit.as_mut() {
                Some(jit) => {
                    let mut mm = mm.borrow_mut();
                    jit.run(&mut self.tasks[t].cpu, &mut mm.mem, budget)
                }
                None => {
                    let mut mm = mm.borrow_mut();
                    (1, self.tasks[t].cpu.step(&mut mm.mem))
                }
            };
            n += k;
            self.steps += k;
            if self.cfg.clock == ClockMode::Virtual {
                self.clock_ns += k * NS_PER_STEP;
            }
            match res {
                Ok(()) => {}
                Err(Exception::Svc(_)) => {
                    if self.cfg.clock == ClockMode::Virtual {
                        self.clock_ns += NS_PER_SYSCALL;
                    }
                    if let Some(e) = self.syscall(t) {
                        return Some(e);
                    }
                    if self.init_exit.is_some() {
                        return self.init_exit;
                    }
                }
                Err(Exception::Unimplemented { raw, what }) => {
                    let pc = self.tasks[t].cpu.pc;
                    return Some(Exit::Unimplemented { raw, what, pc });
                }
                // Access just below a MAP_GROWSDOWN region: it is extended and
                // the instruction is re-executed (stack_guard_gap = 256 pages).
                Err(Exception::DataAbort { addr, .. })
                    if self.tasks[t].mm.borrow_mut().mem.grow_down(addr, 256 * 4096) => {}
                Err(e) => {
                    let beyond = matches!(e, Exception::DataAbort { addr, .. }
                        if self.tasks[t].mm.borrow().mem.beyond_eof(addr));
                    let signo = match e {
                        _ if beyond => sig::SIGBUS,
                        Exception::Undefined(_) => sig::SIGILL,
                        Exception::Breakpoint(_) => sig::SIGTRAP,
                        Exception::Alignment { .. } | Exception::PcAlignment { .. } => sig::SIGBUS,
                        _ => sig::SIGSEGV,
                    };
                    self.tasks[t].fault = Some((signo, e));
                    self.force_signal(t, signo);
                    if self.init_exit.is_some() {
                        return self.init_exit;
                    }
                }
            }
            if self.steps >= self.cfg.max_steps {
                return Some(Exit::StepLimit);
            }
        }
        None
    }
}

impl Kernel {
    /// Steps the JIT can execute in a row without the step-by-step loop of
    /// [`run_task`](Self::run_task) behaving
    /// differently: within the quantum and the instruction limit, before the
    /// next alarm deadline (with virtual time), and only one if there is
    /// a signal to deliver (one is delivered per step).
    fn jit_budget(&self, t: usize, quantum_left: u64) -> u64 {
        let mut b = quantum_left.min(self.cfg.max_steps.saturating_sub(self.steps)).max(1);
        if self.signal_wakes(t) {
            return 1;
        }
        if self.cfg.clock == ClockMode::Virtual && self.next_alarm != u64::MAX {
            // check_alarms before step j (j ≥ 1) sees clock + j·NS_PER_STEP:
            // it must stay below the deadline.
            let left = self.next_alarm.saturating_sub(self.clock_ns);
            b = b.min(left.div_ceil(NS_PER_STEP).max(1));
        }
        b
    }
}

fn comm_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.chars().take(15).collect()
}
