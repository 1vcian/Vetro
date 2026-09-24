//! Segnali: azioni, maschere, consegna e ritorno dal gestore.
//!
//! Il frame sullo stack del guest ha il layout di Linux arm64
//! (arch/arm64/kernel/signal.c): `struct rt_sigframe { siginfo; ucontext }`
//! con `sigcontext.__reserved` che contiene un `fpsimd_context` e il
//! terminatore, seguito da un frame record {x29, x30}.

#![allow(clippy::needless_range_loop)]

use super::abi::*;
use super::{Kernel, State, Wait, sig};
use vetro_cpu::{Exception, Memory};

pub const SA_NOCLDWAIT: u64 = 0x2;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// Azioni dei segnali, condivise tra i thread di un processo.
#[derive(Clone)]
pub struct SigHand {
    pub actions: [SigAction; 65],
}

impl Default for SigHand {
    fn default() -> Self {
        SigHand { actions: [SigAction::default(); 65] }
    }
}

/// Stato dei segnali di un thread.
#[derive(Clone, Debug)]
pub struct SigState {
    pub mask: u64,
    pub pending: u64,
    /// siginfo dei segnali in attesa: (mittente, codice, indirizzo).
    pub info: Vec<(i32, i32, u64)>,
    pub altstack: (u64, u64, u32),
    /// Scadenza di ITIMER_REAL (ns monotoni) e periodo.
    pub alarm: Option<u64>,
    pub alarm_interval: u64,
    /// Maschera da ripristinare dopo rt_sigsuspend.
    pub saved_mask: Option<u64>,
    /// Syscall bloccata interrotta da un segnale: EINTR o riavvio.
    pub interrupted: Option<Wait>,
}

impl Default for SigState {
    fn default() -> Self {
        SigState {
            mask: 0,
            pending: 0,
            info: vec![(0, 0, 0); 65],
            altstack: (0, 0, 0),
            alarm: None,
            alarm_interval: 0,
            saved_mask: None,
            interrupted: None,
        }
    }
}

#[inline]
pub fn bit(s: i32) -> u64 {
    1u64 << (s - 1)
}

/// Segnali che non si possono bloccare né catturare.
pub const UNBLOCKABLE: u64 = (1 << (sig::SIGKILL - 1)) | (1 << (sig::SIGSTOP - 1));

#[derive(PartialEq, Eq)]
enum Default_ {
    Term,
    Core,
    Ign,
    Stop,
    Cont,
}

fn default_action(s: i32) -> Default_ {
    use sig::*;
    match s {
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGSYS | 24 | 25 => Default_::Core,
        SIGCHLD | 23 | 28 => Default_::Ign,
        SIGSTOP | SIGTSTP | 21 | 22 => Default_::Stop,
        SIGCONT => Default_::Cont,
        _ => Default_::Term,
    }
}

// Offset nel frame.
const SIGINFO_SIZE: u64 = 128;
const UC_MCONTEXT: u64 = 176; // offsetof(ucontext, uc_mcontext)
const SC_RESERVED: u64 = 288; // offsetof(sigcontext, __reserved)
const FRAME_SIZE: u64 = SIGINFO_SIZE + UC_MCONTEXT + SC_RESERVED + 4096;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;

impl Kernel {
    /// Manda `s` al processo (thread group) `tgid`.
    pub fn send_to_process(&mut self, tgid: i32, s: i32, from: i32, code: i32) -> bool {
        // Preferisci un thread che non blocca il segnale.
        let threads: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| {
                self.tasks[i].tgid == tgid
                    && matches!(self.tasks[i].state, State::Runnable | State::Blocked(_))
            })
            .collect();
        let Some(&first) = threads.first() else { return false };
        let target = threads.iter().copied().find(|&i| self.tasks[i].sig.mask & bit(s) == 0).unwrap_or(first);
        self.send_to_thread(target, s, from, code, 0);
        true
    }

    pub fn send_to_thread(&mut self, t: usize, s: i32, from: i32, code: i32, addr: u64) {
        if s == 0 {
            return;
        }
        let act = self.tasks[t].sighand.borrow().actions[s as usize];
        // Un segnale ignorato non resta in attesa (salvo SIGCHLD con SIG_DFL,
        // che è comunque ignorato).
        if act.handler == SIG_IGN && s != sig::SIGKILL && s != sig::SIGSTOP {
            return;
        }
        if act.handler == SIG_DFL && default_action(s) == Default_::Ign {
            return;
        }
        let st = &mut self.tasks[t].sig;
        st.pending |= bit(s);
        st.info[s as usize] = (from, code, addr);
    }

    /// Segnale sincrono da un'eccezione della CPU: se è bloccato o ignorato
    /// si torna all'azione di default (come force_sig_fault).
    pub fn force_signal(&mut self, t: usize, s: i32) {
        let addr = match self.tasks[t].fault {
            Some((_, Exception::DataAbort { addr, .. }))
            | Some((_, Exception::InstructionAbort { addr }))
            | Some((_, Exception::Alignment { addr }))
            | Some((_, Exception::PcAlignment { addr })) => addr,
            _ => self.tasks[t].cpu.pc,
        };
        {
            let mut sh = self.tasks[t].sighand.borrow_mut();
            let a = &mut sh.actions[s as usize];
            if a.handler == SIG_IGN || self.tasks[t].sig.mask & bit(s) != 0 {
                a.handler = SIG_DFL;
            }
        }
        self.tasks[t].sig.mask &= !bit(s);
        let code = match s {
            sig::SIGSEGV => 1, // SEGV_MAPERR
            sig::SIGBUS => 1,  // BUS_ADRALN
            sig::SIGILL => 1,  // ILL_ILLOPC
            sig::SIGTRAP => 1, // TRAP_BRKPT
            _ => 0,
        };
        if self.cfg.strace {
            eprintln!(
                "[{}] --- signal {s} (si_addr={addr:#x}) a pc={:#x}: {:?} ---",
                self.tasks[t].tid, self.tasks[t].cpu.pc, self.tasks[t].fault
            );
        }
        let st = &mut self.tasks[t].sig;
        st.pending |= bit(s);
        st.info[s as usize] = (0, code, addr);
        self.deliver_signals(t);
    }

    /// Vero se il task bloccato ha un segnale che lo deve svegliare.
    pub fn signal_wakes(&self, t: usize) -> bool {
        let st = &self.tasks[t].sig;
        st.pending & !(st.mask & !UNBLOCKABLE) != 0
    }

    pub fn check_alarms(&mut self) {
        let now = self.now();
        for t in 0..self.tasks.len() {
            if let Some(d) = self.tasks[t].sig.alarm
                && now >= d
            {
                let iv = self.tasks[t].sig.alarm_interval;
                self.tasks[t].sig.alarm = if iv > 0 { Some(d + iv) } else { None };
                let tgid = self.tasks[t].tgid;
                self.send_to_process(tgid, sig::SIGALRM, 0, 0x80);
            }
        }
    }

    /// Consegna il primo segnale non bloccato. Vero se ne ha consegnato uno.
    pub fn deliver_signals(&mut self, t: usize) -> bool {
        let deliverable = {
            let st = &self.tasks[t].sig;
            st.pending & !(st.mask & !UNBLOCKABLE)
        };
        if deliverable == 0 {
            return false;
        }
        let s = deliverable.trailing_zeros() as i32 + 1;
        self.tasks[t].sig.pending &= !bit(s);
        let info = self.tasks[t].sig.info[s as usize];
        let act = self.tasks[t].sighand.borrow().actions[s as usize];
        if s == sig::SIGKILL || act.handler == SIG_DFL {
            match default_action(s) {
                Default_::Ign | Default_::Cont | Default_::Stop => {
                    // La syscall interrotta si riavvia da sola (pc sull'SVC).
                    self.tasks[t].sig.interrupted = None;
                    return false;
                }
                _ => {
                    // Il bit "core dumped" (0x80) si accende solo se il core
                    // viene scritto davvero: con RLIMIT_CORE = 0 mai.
                    let tgid = self.tasks[t].tgid;
                    self.kill_process(tgid, s);
                    return true;
                }
            }
        }
        if act.handler == SIG_IGN {
            self.tasks[t].sig.interrupted = None;
            return false;
        }
        // Una syscall bloccata (pc sull'SVC) viene riavviata con SA_RESTART,
        // altrimenti restituisce EINTR.
        if let Some(w) = self.tasks[t].sig.interrupted.take() {
            let restart = act.flags & SA_RESTART != 0 && !matches!(w, Wait::Signal | Wait::Sleep { .. });
            if !restart {
                let cpu = &mut self.tasks[t].cpu;
                cpu.x[0] = (-EINTR) as u64;
                cpu.pc += 4;
                self.tasks[t].deadline = None;
                self.tasks[t].futex_woken = false;
            }
        }
        if let Err(()) = self.setup_frame(t, s, act, info) {
            // Stack del gestore non scrivibile: SIGSEGV forzato.
            let tgid = self.tasks[t].tgid;
            self.kill_process(tgid, sig::SIGSEGV);
            return true;
        }
        if act.flags & SA_RESETHAND != 0 {
            self.tasks[t].sighand.borrow_mut().actions[s as usize].handler = SIG_DFL;
        }
        // Un task fermo in attesa di un segnale riparte.
        if matches!(self.tasks[t].state, State::Blocked(_)) {
            self.tasks[t].state = State::Runnable;
        }
        true
    }

    fn setup_frame(
        &mut self,
        t: usize,
        s: i32,
        act: super::signal::SigAction,
        info: (i32, i32, u64),
    ) -> Result<(), ()> {
        let task = &self.tasks[t];
        let cpu = task.cpu.clone();
        let old_mask = task.sig.saved_mask.unwrap_or(task.sig.mask);
        let (ss_sp, ss_size, ss_flags) = task.sig.altstack;
        let on_alt =
            act.flags & SA_ONSTACK != 0 && ss_size != 0 && !(cpu.sp > ss_sp && cpu.sp <= ss_sp + ss_size);
        let top = if on_alt { ss_sp + ss_size } else { cpu.sp };
        let record = (top - 16) & !15;
        let frame = (record - FRAME_SIZE) & !15;

        let mut b = vec![0u8; FRAME_SIZE as usize];
        let mut put = |off: u64, v: &[u8]| b[off as usize..off as usize + v.len()].copy_from_slice(v);
        // siginfo
        put(0, &s.to_le_bytes());
        put(8, &info.1.to_le_bytes());
        if matches!(s, sig::SIGSEGV | sig::SIGBUS | sig::SIGILL | sig::SIGTRAP | sig::SIGFPE) && info.0 == 0 {
            put(16, &info.2.to_le_bytes());
        } else {
            put(16, &info.0.to_le_bytes());
            put(20, &0u32.to_le_bytes());
        }
        // ucontext
        let uc = SIGINFO_SIZE;
        put(uc + 16, &ss_sp.to_le_bytes());
        let flags = if ss_size == 0 { 2 } else { ss_flags } | if on_alt { 1 } else { 0 };
        put(uc + 24, &flags.to_le_bytes());
        put(uc + 32, &ss_size.to_le_bytes());
        put(uc + 40, &old_mask.to_le_bytes());
        // sigcontext
        let sc = uc + UC_MCONTEXT;
        let fault_addr = if matches!(s, sig::SIGSEGV | sig::SIGBUS) { info.2 } else { 0 };
        put(sc, &fault_addr.to_le_bytes());
        for r in 0..31 {
            put(sc + 8 + 8 * r as u64, &cpu.x[r].to_le_bytes());
        }
        put(sc + 256, &cpu.sp.to_le_bytes());
        put(sc + 264, &cpu.pc.to_le_bytes());
        put(sc + 272, &(cpu.nzcv as u64).to_le_bytes());
        // fpsimd_context
        let fp = sc + SC_RESERVED;
        put(fp, &FPSIMD_MAGIC.to_le_bytes());
        put(fp + 4, &FPSIMD_SIZE.to_le_bytes());
        put(fp + 8, &cpu.fpsr.to_le_bytes());
        put(fp + 12, &cpu.fpcr.to_le_bytes());
        for r in 0..32 {
            put(fp + 16 + 16 * r as u64, &cpu.v[r].to_le_bytes());
        }
        // terminatore: magic 0, size 0 (già a zero)

        {
            let mm = self.tasks[t].mm.clone();
            let mut mm = mm.borrow_mut();
            mm.mem.write(frame, &b).map_err(|_| ())?;
            let mut rec = [0u8; 16];
            rec[..8].copy_from_slice(&cpu.x[29].to_le_bytes());
            rec[8..].copy_from_slice(&cpu.x[30].to_le_bytes());
            mm.mem.write(record, &rec).map_err(|_| ())?;
        }

        let task = &mut self.tasks[t];
        task.cpu.x[0] = s as u64;
        task.cpu.x[1] = frame;
        task.cpu.x[2] = frame + SIGINFO_SIZE;
        task.cpu.x[29] = record;
        // Senza SA_RESTORER Linux torna al sigtramp del vDSO.
        task.cpu.x[30] = if act.flags & SA_RESTORER != 0 { act.restorer } else { super::loader::SIGTRAMP };
        task.cpu.sp = frame;
        task.cpu.pc = act.handler;
        task.sig.saved_mask = None;
        let mut mask = old_mask | act.mask;
        if act.flags & SA_NODEFER == 0 {
            mask |= bit(s);
        }
        task.sig.mask = mask & !UNBLOCKABLE;
        Ok(())
    }

    /// rt_sigreturn: ripristina lo stato dal frame a `sp`.
    pub(super) fn sigreturn(&mut self, t: usize) -> Result<(), ()> {
        let frame = self.tasks[t].cpu.sp;
        let mut b = vec![0u8; FRAME_SIZE as usize];
        {
            let mm = self.tasks[t].mm.clone();
            let mut mm = mm.borrow_mut();
            mm.mem.read(frame, &mut b).map_err(|_| ())?;
        }
        let u64_at = |off: u64| u64::from_le_bytes(b[off as usize..off as usize + 8].try_into().unwrap());
        let u32_at = |off: u64| u32::from_le_bytes(b[off as usize..off as usize + 4].try_into().unwrap());
        let uc = SIGINFO_SIZE;
        let sc = uc + UC_MCONTEXT;
        let task = &mut self.tasks[t];
        task.sig.mask = u64_at(uc + 40) & !UNBLOCKABLE;
        for r in 0..31 {
            task.cpu.x[r] = u64_at(sc + 8 + 8 * r as u64);
        }
        task.cpu.sp = u64_at(sc + 256);
        task.cpu.pc = u64_at(sc + 264);
        task.cpu.nzcv = (u64_at(sc + 272) as u32) & 0xf000_0000;
        let fp = sc + SC_RESERVED;
        if u32_at(fp) == FPSIMD_MAGIC {
            task.cpu.fpsr = u32_at(fp + 8) & vetro_cpu::state::FPSR_MASK;
            task.cpu.fpcr = u32_at(fp + 12) & vetro_cpu::state::FPCR_MASK;
            for r in 0..32 {
                let off = (fp + 16 + 16 * r as u64) as usize;
                task.cpu.v[r] = u128::from_le_bytes(b[off..off + 16].try_into().unwrap());
            }
        }
        Ok(())
    }

    /// Syscall rt_sigaction.
    pub fn sys_sigaction(&mut self, t: usize, s: i64, act: u64, oact: u64) -> SysResult {
        if !(1..=64).contains(&s) || (act != 0 && (s as i32 == sig::SIGKILL || s as i32 == sig::SIGSTOP)) {
            return Err(EINVAL);
        }
        let mm = self.tasks[t].mm.clone();
        let mut mm = mm.borrow_mut();
        let old = self.tasks[t].sighand.borrow().actions[s as usize];
        if act != 0 {
            let new = SigAction {
                handler: read_u64(&mut mm.mem, act)?,
                flags: read_u64(&mut mm.mem, act + 8)?,
                restorer: read_u64(&mut mm.mem, act + 16)?,
                mask: read_u64(&mut mm.mem, act + 24)? & !UNBLOCKABLE,
            };
            self.tasks[t].sighand.borrow_mut().actions[s as usize] = new;
            // Impostare SIG_IGN scarta i segnali in attesa.
            if new.handler == SIG_IGN {
                let tgid = self.tasks[t].tgid;
                for task in self.tasks.iter_mut().filter(|x| x.tgid == tgid) {
                    task.sig.pending &= !bit(s as i32);
                }
            }
        }
        if oact != 0 {
            write_u64(&mut mm.mem, oact, old.handler)?;
            write_u64(&mut mm.mem, oact + 8, old.flags)?;
            write_u64(&mut mm.mem, oact + 16, old.restorer)?;
            write_u64(&mut mm.mem, oact + 24, old.mask)?;
        }
        Ok(0)
    }

    pub fn sys_sigprocmask(&mut self, t: usize, how: u64, set: u64, oset: u64) -> SysResult {
        let mm = self.tasks[t].mm.clone();
        let mut mm = mm.borrow_mut();
        let old = self.tasks[t].sig.mask;
        if set != 0 {
            let s = read_u64(&mut mm.mem, set)?;
            let new = match how {
                0 => old | s,
                1 => old & !s,
                2 => s,
                _ => return Err(EINVAL),
            };
            self.tasks[t].sig.mask = new & !UNBLOCKABLE;
        }
        if oset != 0 {
            write_u64(&mut mm.mem, oset, old)?;
        }
        Ok(0)
    }

    /// Il task è in attesa di un segnale (pause, sigsuspend).
    pub fn block_for_signal(&mut self, t: usize) {
        self.tasks[t].state = State::Blocked(Wait::Signal);
    }
}
