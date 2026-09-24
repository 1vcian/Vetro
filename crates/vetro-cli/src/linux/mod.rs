//! Kernel Linux arm64 emulato in user mode.
//!
//! Più processi e thread girano nello stesso emulatore, a turno (quanti di
//! istruzioni). Le syscall bloccanti non bloccano l'host: il task si ferma
//! con `pc` sull'SVC e lo riesegue quando la condizione può essere
//! soddisfatta (figlio terminato, dati in una pipe, scadenza raggiunta...).
//!
//! Il tempo è virtuale per default (un'istruzione = un nanosecondo, salti in
//! avanti quando tutti dormono): esecuzioni ripetibili, come chiede la regola
//! del determinismo (CLAUDE.md). Anche `getrandom`, AT_RANDOM e
//! /dev/urandom sono deterministici.

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

pub use fs::Console;
use fs::FdTable;
use mm::Mm;
use signal::{SigHand, SigState};

pub type Pid = i32;

/// File mappato con MAP_SHARED: percorso sull'host e buffer comune.
pub type SharedFile = (std::path::PathBuf, Rc<RefCell<Vec<u8>>>);

/// Numeri di segnale Linux.
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
    pub const SIGSYS: i32 = 31;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMode {
    /// Tempo virtuale deterministico (default).
    Virtual,
    /// Orologio dell'host.
    Host,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Copia stdout/stderr del guest su quelli dell'host.
    pub echo: bool,
    /// Stampa le syscall su stderr, come strace.
    pub strace: bool,
    pub clock: ClockMode,
    /// Limite totale di istruzioni (tutti i task).
    pub max_steps: u64,
    /// Contenuto di stdin.
    pub stdin: Vec<u8>,
    /// Directory di lavoro iniziale del guest.
    pub cwd: String,
    /// Come `qemu -L`: i percorsi assoluti si cercano prima qui.
    pub sysroot: Option<String>,
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Il processo iniziale è uscito con questo codice.
    Code(i32),
    /// Il processo iniziale è stato terminato da un segnale. `cause` è
    /// l'eccezione della CPU, se il segnale è nato da lì.
    Signal {
        signo: i32,
        cause: Option<Exception>,
        pc: u64,
    },
    /// Syscall non ancora implementata: limite nostro, non del guest.
    UnsupportedSyscall {
        nr: u64,
        pc: u64,
    },
    /// Istruzione valida ma non ancora implementata.
    Unimplemented {
        raw: u32,
        what: &'static str,
        pc: u64,
    },
    StepLimit,
    /// Tutti i task sono bloccati e nulla può svegliarli.
    Deadlock,
}

/// Perché un task è fermo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    /// wait4/waitid: un figlio che corrisponde a `pid` (come wait4).
    Child { pid: i32 },
    /// Lettura da una pipe vuota o scrittura su una piena.
    Pipe,
    /// Dormire fino a `until` (ns di tempo monotono).
    Sleep { until: u64 },
    /// FUTEX_WAIT su `addr`, con scadenza facoltativa.
    Futex { addr: u64, until: Option<u64> },
    /// Il genitore di un vfork aspetta che il figlio esegua exec o esca.
    Vfork { child: Pid },
    /// pause/rt_sigsuspend: solo un segnale sveglia.
    Signal,
    /// Condizione da ricontrollare a ogni giro (F_SETLKW): la syscall si
    /// riesegue finché non riesce.
    Retry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Runnable,
    Blocked(Wait),
    /// Thread terminato; per il leader del gruppo resta lo stato d'uscita
    /// finché il genitore non lo raccoglie.
    Zombie {
        status: i32,
    },
    Dead,
}

pub struct Task {
    pub tid: Pid,
    /// Id del processo (thread group).
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
    /// Segnale da mandare al genitore all'uscita (SIGCHLD per fork).
    pub exit_signal: i32,
    /// Svegliato da FUTEX_WAKE: il prossimo FUTEX_WAIT rieseguito restituisce 0.
    pub futex_woken: bool,
    /// Per vfork: il genitore da sbloccare a exec/exit.
    pub vfork_parent: Option<Pid>,
    /// Segnale sincrono (fault) generato dall'ultima istruzione.
    pub fault: Option<(i32, Exception)>,
    pub comm: String,
    pub exe: String,
    pub umask: u32,
    /// Scadenza assoluta della syscall bloccante in corso (nanosleep, futex
    /// con timeout): fissata al primo blocco, così la riesecuzione dell'SVC
    /// non la sposta.
    pub deadline: Option<u64>,
}

pub struct Kernel {
    pub cfg: Config,
    pub tasks: Vec<Task>,
    pub console: Rc<RefCell<Console>>,
    next_pid: Pid,
    init: Pid,
    /// Tempo monotono virtuale in ns.
    clock_ns: u64,
    steps: u64,
    run_queue: VecDeque<usize>,
    rng: u64,
    /// Esito del processo iniziale, quando termina.
    init_exit: Option<Exit>,
    /// Ultima eccezione della CPU che ha ucciso un processo (per il report).
    last_fault: Option<(Pid, Exception, u64)>,
    /// Mappature MAP_SHARED di file: (dispositivo, inode) → (percorso, buffer).
    pub shared_files: std::collections::HashMap<(u64, u64), SharedFile>,
    /// Lock POSIX sui file.
    locks: locks::LockTable,
    /// IPC System V.
    ipc: ipc::Ipc,
}

/// Istruzioni per quanto di scheduling.
const QUANTUM: u64 = 20_000;
/// Istante "realtime" iniziale del tempo virtuale: 2026-01-01T00:00:00Z.
const EPOCH: u64 = 1_767_225_600;

impl Kernel {
    pub fn new(cfg: Config) -> Self {
        let console = Rc::new(RefCell::new(Console::new(cfg.stdin.clone(), cfg.echo)));
        Kernel {
            cfg,
            tasks: Vec::new(),
            console,
            next_pid: 100,
            init: 0,
            clock_ns: 0,
            steps: 0,
            run_queue: VecDeque::new(),
            rng: 0x5eed_0000_0000_0001,
            init_exit: None,
            last_fault: None,
            shared_files: std::collections::HashMap::new(),
            locks: locks::LockTable::default(),
            ipc: ipc::Ipc::default(),
        }
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

    /// Crea il processo iniziale da un'immagine ELF.
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
            vfork_parent: None,
            fault: None,
            comm: comm_of(exe),
            exe: exe.into(),
            umask: 0o022,
            deadline: None,
        };
        self.tasks.push(task);
        if self.init == 0 {
            self.init = pid;
        }
        Ok(pid)
    }

    /// Riscrive sui file il contenuto delle mappature condivise.
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

    /// Tempo monotono in ns.
    pub fn now(&self) -> u64 {
        match self.cfg.clock {
            ClockMode::Virtual => self.clock_ns,
            ClockMode::Host => {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
            }
        }
    }

    /// Tempo "realtime" in ns dall'epoca Unix.
    pub fn realtime(&self) -> u64 {
        match self.cfg.clock {
            ClockMode::Virtual => EPOCH * 1_000_000_000 + self.clock_ns,
            ClockMode::Host => self.now(),
        }
    }

    /// Byte pseudocasuali deterministici (getrandom, AT_RANDOM, /dev/urandom).
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

    /// Esegue finché il processo iniziale non termina.
    pub fn run(&mut self) -> Exit {
        loop {
            if let Some(e) = self.init_exit {
                return e;
            }
            if self.steps >= self.cfg.max_steps {
                return Exit::StepLimit;
            }
            let Some(t) = self.pick() else {
                // Nessuno eseguibile: fai avanzare il tempo fino alla prossima
                // scadenza, se c'è.
                match self.next_deadline() {
                    Some(d) if self.cfg.clock == ClockMode::Virtual => {
                        self.clock_ns = self.clock_ns.max(d);
                        continue;
                    }
                    Some(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
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

    /// Prossimo task eseguibile (round robin), svegliando chi può ripartire.
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

    /// Vero se il task `i` può girare (eventualmente svegliandolo).
    fn ready(&mut self, i: usize) -> bool {
        let wake = match &self.tasks[i].state {
            State::Runnable => return true,
            State::Zombie { .. } | State::Dead => return false,
            State::Blocked(w) => {
                let w = w.clone();
                if self.wait_satisfied(i, &w) {
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
            Wait::Futex { until, .. } => self.tasks[i].futex_woken || until.is_some_and(|u| self.now() >= u),
            Wait::Vfork { child } => self.find(*child).is_none_or(|c| self.tasks[c].vfork_parent.is_none()),
            Wait::Signal => false,
            Wait::Retry => true,
        }
    }

    fn next_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        for t in &self.tasks {
            let d = match &t.state {
                State::Blocked(Wait::Sleep { until }) => Some(*until),
                State::Blocked(Wait::Futex { until: Some(u), .. }) => Some(*u),
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

    /// Esegue il task `t` per un quanto. Restituisce l'esito finale se la
    /// corsa è finita.
    fn run_task(&mut self, t: usize) -> Option<Exit> {
        let mut n = 0;
        while n < QUANTUM {
            self.check_alarms();
            if self.tasks[t].state != State::Runnable {
                break;
            }
            if self.deliver_signals(t) {
                // il task potrebbe essere morto
                if self.tasks[t].state != State::Runnable {
                    break;
                }
            }
            let mm = self.tasks[t].mm.clone();
            let res = {
                let mut mm = mm.borrow_mut();
                self.tasks[t].cpu.step(&mut mm.mem)
            };
            n += 1;
            self.steps += 1;
            if self.cfg.clock == ClockMode::Virtual {
                self.clock_ns += 1;
            }
            match res {
                Ok(()) => {}
                Err(Exception::Svc(_)) => {
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
                Err(e) => {
                    let signo = match e {
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

fn comm_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.chars().take(15).collect()
}
