//! Processo Linux arm64 in user mode: CPU, memoria, syscall minime.

use crate::elf::{self, LoadError};
use vetro_cpu::{Cpu, Exception, Memory, Perm, UserMemory};

/// Cima dello stack iniziale e sua dimensione.
pub const STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const STACK_SIZE: u64 = 8 << 20;

/// Numeri di segnale Linux usati per riportare le eccezioni.
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGBUS: i32 = 7;
pub const SIGSEGV: i32 = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// `exit`/`exit_group` con questo codice.
    Code(i32),
    /// Il processo sarebbe terminato da un segnale (nessun gestore in M1).
    Signal {
        signo: i32,
        cause: Exception,
        pc: u64,
    },
    /// Syscall non ancora implementata: limite nostro, non del guest.
    UnsupportedSyscall {
        nr: u64,
        pc: u64,
    },
    StepLimit,
}

pub struct Process {
    pub cpu: Cpu,
    pub mem: UserMemory,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Se vero, stdout/stderr del guest vanno anche su quelli dell'host.
    pub echo: bool,
    pub steps: u64,
}

impl Process {
    pub fn load(image: &[u8], argv0: &str) -> Result<Self, LoadError> {
        let mut mem = UserMemory::new();
        let loaded = elf::load(image, &mut mem)?;
        mem.map(STACK_TOP - STACK_SIZE, vec![0; STACK_SIZE as usize], Perm::RW)
            .map_err(|e| LoadError::Overlap(e.base))?;
        let mut cpu = Cpu::new();
        cpu.pc = loaded.entry;
        cpu.sp = setup_stack(&mut mem, argv0, &loaded);
        Ok(Process { cpu, mem, stdout: Vec::new(), stderr: Vec::new(), echo: false, steps: 0 })
    }

    pub fn run(&mut self, max_steps: u64) -> Exit {
        while self.steps < max_steps {
            self.steps += 1;
            match self.cpu.step(&mut self.mem) {
                Ok(()) => {}
                Err(Exception::Svc(_)) => {
                    if let Some(exit) = self.syscall() {
                        return exit;
                    }
                }
                Err(cause) => {
                    let signo = match cause {
                        Exception::Undefined(_) | Exception::Unimplemented { .. } => SIGILL,
                        Exception::Breakpoint(_) => SIGTRAP,
                        Exception::Alignment { .. } | Exception::PcAlignment { .. } => SIGBUS,
                        _ => SIGSEGV,
                    };
                    return Exit::Signal { signo, cause, pc: self.cpu.pc };
                }
            }
        }
        Exit::StepLimit
    }

    fn syscall(&mut self) -> Option<Exit> {
        let nr = self.cpu.x[8];
        let a = self.cpu.x;
        let ret: i64 = match nr {
            64 => self.sys_write(a[0], a[1], a[2]),
            93 | 94 => return Some(Exit::Code(a[0] as i32)),
            _ => return Some(Exit::UnsupportedSyscall { nr, pc: self.cpu.pc - 4 }),
        };
        self.cpu.x[0] = ret as u64;
        None
    }

    fn sys_write(&mut self, fd: u64, buf: u64, len: u64) -> i64 {
        const EBADF: i64 = 9;
        const EFAULT: i64 = 14;
        let mut data = vec![0u8; len as usize];
        if self.mem.read(buf, &mut data).is_err() {
            return -EFAULT;
        }
        use std::io::Write;
        match fd {
            1 => {
                if self.echo {
                    let _ = std::io::stdout().write_all(&data);
                }
                self.stdout.extend_from_slice(&data);
            }
            2 => {
                if self.echo {
                    let _ = std::io::stderr().write_all(&data);
                }
                self.stderr.extend_from_slice(&data);
            }
            _ => return -EBADF,
        }
        len as i64
    }
}

/// Stack iniziale secondo l'ABI Linux: argc, argv, envp, auxv.
fn setup_stack(mem: &mut UserMemory, argv0: &str, loaded: &elf::Loaded) -> u64 {
    const AT_NULL: u64 = 0;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;

    let mut top = STACK_TOP;
    let mut push_bytes = |mem: &mut UserMemory, b: &[u8]| {
        top -= b.len() as u64;
        mem.write(top, b).expect("stack mappato");
        top
    };
    let mut s = argv0.as_bytes().to_vec();
    s.push(0);
    let argv0_addr = push_bytes(mem, &s);
    // AT_RANDOM: 16 byte fissi, per ora (determinismo; vedi CLAUDE.md).
    let random_addr = push_bytes(mem, &[0x5a; 16]);

    let words: Vec<u64> = vec![
        1,
        argv0_addr,
        0, // fine argv
        0, // envp vuoto
        AT_PHDR,
        loaded.phdr_addr,
        AT_PHENT,
        56,
        AT_PHNUM,
        loaded.phnum as u64,
        AT_PAGESZ,
        elf::PAGE,
        AT_ENTRY,
        loaded.entry,
        AT_RANDOM,
        random_addr,
        AT_NULL,
        0,
    ];
    let sp = (top - words.len() as u64 * 8) & !15;
    for (i, w) in words.iter().enumerate() {
        mem.write(sp + i as u64 * 8, &w.to_le_bytes()).expect("stack mappato");
    }
    sp
}
