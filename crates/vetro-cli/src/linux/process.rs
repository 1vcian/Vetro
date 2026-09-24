//! Processi e thread: clone, execve, exit, wait4.

use std::cell::RefCell;
use std::rc::Rc;

use super::abi::*;
use super::fs::FdTable;
use super::signal::{SA_NOCLDWAIT, SIG_IGN};
use super::{Exit, Kernel, Pid, State, Task, Wait, loader, sig};
use vetro_cpu::Cpu;

pub const CLONE_VM: u64 = 0x100;
pub const CLONE_FS: u64 = 0x200;
pub const CLONE_FILES: u64 = 0x400;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_PARENT: u64 = 0x8000;
pub const CLONE_THREAD: u64 = 0x10000;
pub const CLONE_SETTLS: u64 = 0x80000;
pub const CLONE_PARENT_SETTID: u64 = 0x100000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
pub const CLONE_CHILD_SETTID: u64 = 0x1000000;

const WNOHANG: u64 = 1;

impl Kernel {
    /// clone(flags, stack, ptid, tls, ctid) come su arm64.
    pub fn sys_clone(
        &mut self,
        t: usize,
        flags: u64,
        stack: u64,
        ptid: u64,
        tls: u64,
        ctid: u64,
    ) -> SysResult {
        if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0
            || flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0
        {
            return Err(EINVAL);
        }
        let tid = self.alloc_pid();
        let parent = &self.tasks[t];
        let mut cpu: Cpu = parent.cpu.clone();
        cpu.x[0] = 0;
        if stack != 0 {
            cpu.sp = stack;
        }
        if flags & CLONE_SETTLS != 0 {
            cpu.tpidr_el0 = tls;
        }
        cpu.monitor = None;
        let mm = if flags & CLONE_VM != 0 {
            parent.mm.clone()
        } else {
            Rc::new(RefCell::new(parent.mm.borrow().clone()))
        };
        let files = if flags & CLONE_FILES != 0 {
            parent.files.clone()
        } else {
            Rc::new(RefCell::new(parent.files.borrow().clone()))
        };
        let cwd = if flags & CLONE_FS != 0 {
            parent.cwd.clone()
        } else {
            Rc::new(RefCell::new(parent.cwd.borrow().clone()))
        };
        let sighand = if flags & CLONE_SIGHAND != 0 {
            parent.sighand.clone()
        } else {
            Rc::new(RefCell::new(parent.sighand.borrow().clone()))
        };
        let thread = flags & CLONE_THREAD != 0;
        let mut sigstate = parent.sig.clone();
        sigstate.pending = 0;
        sigstate.alarm = None;
        sigstate.interrupted = None;
        let child = Task {
            tid,
            tgid: if thread { parent.tgid } else { tid },
            ppid: if thread || flags & CLONE_PARENT != 0 { parent.ppid } else { parent.tgid },
            pgid: parent.pgid,
            cpu,
            mm,
            files,
            cwd,
            sighand,
            sig: sigstate,
            state: State::Runnable,
            clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 { ctid } else { 0 },
            exit_signal: if thread { 0 } else { (flags & 0xff) as i32 },
            futex_woken: false,
            vfork_parent: if flags & CLONE_VFORK != 0 { Some(parent.tid) } else { None },
            fault: None,
            comm: parent.comm.clone(),
            exe: parent.exe.clone(),
            umask: parent.umask,
            deadline: None,
        };
        if flags & CLONE_PARENT_SETTID != 0 {
            write_u32(&mut self.tasks[t].mm.borrow_mut().mem, ptid, tid as u32)?;
        }
        if flags & CLONE_CHILD_SETTID != 0 {
            write_u32(&mut child.mm.borrow_mut().mem, ctid, tid as u32)?;
        }
        self.tasks.push(child);
        if flags & CLONE_VFORK != 0 {
            // Il genitore riprende quando il figlio esegue exec o esce; la
            // syscall restituisce comunque il tid del figlio.
            self.tasks[t].cpu.x[0] = tid as u64;
            self.tasks[t].state = State::Blocked(Wait::Vfork { child: tid });
        }
        Ok(tid as i64)
    }

    pub fn sys_execve(&mut self, t: usize, path: &str, argv: Vec<Vec<u8>>, envp: Vec<Vec<u8>>) -> SysResult {
        let image = std::fs::read(path).map_err(|e| host_errno(&e))?;
        if image.len() >= 2 && &image[..2] == b"#!" {
            // Script: #!interprete [argomento] percorso argv[1..]
            let line_end = image.iter().position(|&c| c == b'\n').unwrap_or(image.len()).min(256);
            let line = String::from_utf8_lossy(&image[2..line_end]).trim().to_string();
            let mut parts = line.splitn(2, char::is_whitespace);
            let interp = parts.next().unwrap_or("").to_string();
            let mut new_argv = vec![interp.clone().into_bytes()];
            if let Some(a) = parts.next().map(str::trim).filter(|a| !a.is_empty()) {
                new_argv.push(a.as_bytes().to_vec());
            }
            new_argv.push(path.as_bytes().to_vec());
            new_argv.extend(argv.into_iter().skip(1));
            return self.sys_execve(t, &interp, new_argv, envp);
        }
        let random = self.random_bytes(16);
        let img = loader::load(&image, &argv, &envp, path, &random).map_err(|_| ENOEXEC)?;
        let tgid = self.tasks[t].tgid;
        // Gli altri thread del processo spariscono.
        for i in 0..self.tasks.len() {
            if i != t && self.tasks[i].tgid == tgid && self.tasks[i].state != State::Dead {
                self.tasks[i].state = State::Dead;
            }
        }
        let task = &mut self.tasks[t];
        task.mm = Rc::new(RefCell::new(img.mm));
        let mut cpu = Cpu::new();
        cpu.pc = img.entry;
        cpu.sp = img.sp;
        task.cpu = cpu;
        task.files.borrow_mut().close_on_exec();
        // Le azioni con gestore tornano al default; restano quelle ignorate.
        let mut sh = task.sighand.borrow().clone();
        for a in sh.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = Default::default();
            }
        }
        task.sighand = Rc::new(RefCell::new(sh));
        task.sig.altstack = (0, 0, 0);
        task.clear_child_tid = 0;
        task.comm = super::comm_of(path);
        task.exe = path.to_string();
        task.vfork_parent = None;
        Ok(0)
    }

    /// Esito di wait4 per un figlio in stato zombie, se c'è.
    fn reap(&mut self, parent_tgid: Pid, pid: i32) -> Option<(Pid, i32)> {
        let i = self.tasks.iter().position(|c| {
            c.ppid == parent_tgid
                && c.tid == c.tgid
                && matches!(c.state, State::Zombie { .. })
                && self.matches(c, parent_tgid, pid)
        })?;
        let State::Zombie { status } = self.tasks[i].state else { unreachable!() };
        self.tasks[i].state = State::Dead;
        Some((self.tasks[i].tid, status))
    }

    fn matches(&self, c: &Task, parent_tgid: Pid, pid: i32) -> bool {
        match pid {
            -1 => true,
            0 => c.pgid == self.find(parent_tgid).map_or(0, |p| self.tasks[p].pgid),
            p if p > 0 => c.tgid == p,
            p => c.pgid == -p,
        }
    }

    pub fn has_waitable_child(&self, parent_tgid: Pid, pid: i32) -> bool {
        self.tasks.iter().any(|c| {
            c.ppid == parent_tgid
                && c.tid == c.tgid
                && matches!(c.state, State::Zombie { .. })
                && self.matches(c, parent_tgid, pid)
        })
    }

    pub fn has_child(&self, parent_tgid: Pid, pid: i32) -> bool {
        self.tasks.iter().any(|c| {
            c.ppid == parent_tgid
                && c.tid == c.tgid
                && !matches!(c.state, State::Dead)
                && c.exit_signal != 0
                && self.matches(c, parent_tgid, pid)
        })
    }

    /// wait4. `Ok(None)` = bloccati.
    pub fn sys_wait4(
        &mut self,
        t: usize,
        pid: i32,
        status_ptr: u64,
        options: u64,
    ) -> Result<Option<i64>, i64> {
        let me = self.tasks[t].tgid;
        if let Some((cpid, status)) = self.reap(me, pid) {
            if status_ptr != 0 {
                write_u32(&mut self.tasks[t].mm.borrow_mut().mem, status_ptr, status as u32)?;
            }
            return Ok(Some(cpid as i64));
        }
        if !self.has_child(me, pid) {
            return Err(ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok(Some(0));
        }
        Ok(None)
    }

    /// Termina il thread `t` (exit).
    pub fn exit_thread(&mut self, t: usize, status: i32) {
        let tgid = self.tasks[t].tgid;
        self.finish_thread(t);
        let alive = self
            .tasks
            .iter()
            .any(|x| x.tgid == tgid && matches!(x.state, State::Runnable | State::Blocked(_)));
        if !alive {
            self.finish_process(tgid, status);
        }
    }

    /// Termina tutti i thread del processo (exit_group o segnale fatale).
    pub fn exit_group(&mut self, tgid: Pid, status: i32) {
        for i in 0..self.tasks.len() {
            if self.tasks[i].tgid == tgid
                && matches!(self.tasks[i].state, State::Runnable | State::Blocked(_))
            {
                self.finish_thread(i);
            }
        }
        self.finish_process(tgid, status);
    }

    pub fn kill_process(&mut self, tgid: Pid, wstatus: i32) {
        if tgid == self.init
            && let Some(i) = self.find(tgid)
            && let Some((_, e)) = self.tasks[i].fault
        {
            self.last_fault = Some((tgid, e, self.tasks[i].cpu.pc));
        }
        self.exit_group(tgid, wstatus);
    }

    fn finish_thread(&mut self, t: usize) {
        let ctid = self.tasks[t].clear_child_tid;
        if ctid != 0 {
            let mm = self.tasks[t].mm.clone();
            let _ = write_u32(&mut mm.borrow_mut().mem, ctid, 0);
            self.futex_wake(&mm, ctid, 1);
        }
        self.tasks[t].vfork_parent = None;
        let task = &mut self.tasks[t];
        task.state = if task.tid == task.tgid { State::Zombie { status: 0 } } else { State::Dead };
    }

    fn finish_process(&mut self, tgid: Pid, status: i32) {
        let Some(leader) = self.tasks.iter().position(|x| x.tid == tgid) else { return };
        // Chiudere i descrittori libera le pipe (EOF per i lettori).
        let empty = Rc::new(RefCell::new(FdTable::default()));
        for task in self.tasks.iter_mut().filter(|x| x.tgid == tgid) {
            task.files = empty.clone();
            task.sig.alarm = None;
        }
        // Orfani al processo iniziale.
        let init = self.init;
        for task in self.tasks.iter_mut() {
            if task.ppid == tgid && task.tgid != tgid {
                task.ppid = init;
            }
        }
        if tgid == self.init {
            self.init_exit = Some(if status & 0x7f == 0 {
                Exit::Code((status >> 8) & 0xff)
            } else {
                let (cause, pc) = match self.last_fault {
                    Some((p, e, pc)) if p == tgid => (Some(e), pc),
                    _ => (None, self.tasks[leader].cpu.pc),
                };
                Exit::Signal { signo: status & 0x7f, cause, pc }
            });
        }
        let ppid = self.tasks[leader].ppid;
        let exit_signal = self.tasks[leader].exit_signal;
        let parent = self.find(ppid);
        let auto_reap = parent.is_none_or(|p| {
            let a = self.tasks[p].sighand.borrow().actions[sig::SIGCHLD as usize];
            a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0
        });
        self.tasks[leader].state =
            if auto_reap && tgid != self.init { State::Dead } else { State::Zombie { status } };
        if exit_signal != 0 && parent.is_some() {
            self.send_to_process(ppid, exit_signal, tgid, 1);
        }
    }

    pub fn futex_wake(&mut self, mm: &Rc<RefCell<super::mm::Mm>>, addr: u64, n: usize) -> usize {
        let mut woken = 0;
        for task in self.tasks.iter_mut() {
            if woken >= n {
                break;
            }
            if let State::Blocked(Wait::Futex { addr: a, .. }) = task.state
                && a == addr
                && Rc::ptr_eq(&task.mm, mm)
                && !task.futex_woken
            {
                task.futex_woken = true;
                woken += 1;
            }
        }
        woken
    }
}
