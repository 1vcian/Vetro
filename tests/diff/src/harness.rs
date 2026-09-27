//! Test programs with a known initial state and a dump of the final state.
//!
//! Program layout:
//! - **prologue**: SP = [`STACK_TOP`], V0–V31, FPCR, FPSR, NZCV, x0–x30 at the
//!   given values;
//! - **body**: the instructions under test;
//! - **epilogue**: writes x0–x30, SP, NZCV, V0–V31, FPCR and FPSR into the dump
//!   buffer and sends them to stdout together with the memory block, then `exit(0)`.
//!
//! Conventions for the body: x28 ([`BASE_REG`]) points to the middle of the memory
//! block and the epilogue uses it as a base, so the body must not leave it
//! modified. x27 ([`INDEX_REG`]) is a small index for
//! register addressing.

use crate::a64;
use crate::elf::{self, RW_BASE, Rw};

/// Bytes of the memory block at [`RW_BASE`], initialised by the program.
pub const MEM_SIZE: usize = 0x1000;
pub const BASE_REG: u32 = 28;
pub const INDEX_REG: u32 = 27;
/// Value of x28: middle of the memory block.
pub const BASE_PTR: u64 = RW_BASE + 0x800;
const DUMP_OFF: u32 = 0x800; // relative to BASE_PTR: RW_BASE + 0x1000
const DUMP_WORDS: usize = 33; // x0..x30, sp, nzcv
/// In the dump: V0–V31 from this offset, then FPCR and FPSR.
const DUMP_V: usize = 0x200;
const DUMP_FP: usize = 0x400;
const DUMP_LEN: usize = 0x410;
/// Initial values of V0–V31 in the RW segment.
const VINIT_OFF: u64 = 0x2000;
pub const STACK_TOP: u64 = RW_BASE + 0x4000;
const RW_MEMSZ: u64 = 0x4000;

#[derive(Clone, Debug)]
pub struct Program {
    pub x: [u64; 31],
    pub v: [u128; 32],
    /// Flags in bits 31:28.
    pub nzcv: u32,
    pub fpcr: u32,
    pub fpsr: u32,
    pub mem: Vec<u8>,
    pub body: Vec<u32>,
}

/// Prologue instructions: the body starts at this index.
pub const PROLOGUE_LEN: usize = 4 + 1 + 4 + 16 + 4 + 1 + 4 + 1 + 4 + 1 + 31 * 4;

impl Program {
    pub fn new(body: Vec<u32>) -> Self {
        let mut x = [0u64; 31];
        x[BASE_REG as usize] = BASE_PTR;
        Program { x, v: [0; 32], nzcv: 0, fpcr: 0, fpsr: 0, mem: vec![0; MEM_SIZE], body }
    }

    /// Index (in the image) of instruction `i` of the body.
    pub fn body_index(i: usize) -> usize {
        PROLOGUE_LEN + i
    }

    pub fn code(&self) -> Vec<u32> {
        let mut c = Vec::with_capacity(PROLOGUE_LEN + self.body.len() + 64);
        c.extend(a64::mov64(0, STACK_TOP));
        c.push(a64::add_imm(a64::SP, 0, 0));
        c.extend(a64::mov64(0, RW_BASE + VINIT_OFF));
        for r in (0..32).step_by(2) {
            c.push(a64::ldp_q(r, r + 1, 0, r * 16));
        }
        c.extend(a64::mov64(0, self.fpcr as u64));
        c.push(a64::msr_fpcr(0));
        c.extend(a64::mov64(0, self.fpsr as u64));
        c.push(a64::msr_fpsr(0));
        c.extend(a64::mov64(0, self.nzcv as u64));
        c.push(a64::msr_nzcv(0));
        for (r, v) in self.x.iter().enumerate() {
            c.extend(a64::mov64(r as u32, *v));
        }
        debug_assert_eq!(c.len(), PROLOGUE_LEN);
        c.extend_from_slice(&self.body);

        let b = BASE_REG;
        for r in 0..31 {
            c.push(a64::str_x(r, b, DUMP_OFF + 8 * r));
        }
        c.push(a64::add_imm(0, a64::SP, 0));
        c.push(a64::str_x(0, b, DUMP_OFF + 8 * 31));
        c.push(a64::mrs_nzcv(0));
        c.push(a64::str_x(0, b, DUMP_OFF + 8 * 32));
        for r in 0..32 {
            c.push(a64::str_q(r, b, DUMP_OFF + DUMP_V as u32 + 16 * r));
        }
        c.push(a64::mrs_fpcr(0));
        c.push(a64::str_x(0, b, DUMP_OFF + DUMP_FP as u32));
        c.push(a64::mrs_fpsr(0));
        c.push(a64::str_x(0, b, DUMP_OFF + DUMP_FP as u32 + 8));
        // write(1, dump, DUMP_LEN)
        c.extend([
            a64::movz(0, 1, 0),
            a64::add_imm(1, b, DUMP_OFF),
            a64::movz(2, DUMP_LEN as u16, 0),
            a64::movz(8, a64::sys::WRITE, 0),
            a64::svc(0),
        ]);
        // write(1, mem, MEM_SIZE)
        c.extend([
            a64::movz(0, 1, 0),
            a64::sub_imm(1, b, 0x800),
            a64::movz(2, MEM_SIZE as u16, 0),
            a64::movz(8, a64::sys::WRITE, 0),
            a64::svc(0),
        ]);
        c.extend([a64::movz(0, 0, 0), a64::movz(8, a64::sys::EXIT, 0), a64::svc(0)]);
        c
    }

    pub fn build(&self) -> Vec<u8> {
        assert_eq!(self.mem.len(), MEM_SIZE);
        let mut init = self.mem.clone();
        init.resize(VINIT_OFF as usize, 0);
        for v in &self.v {
            init.extend_from_slice(&v.to_le_bytes());
        }
        elf::build_with_rw(&self.code(), &[], Some(Rw { init: &init, memsz: RW_MEMSZ }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dump {
    pub x: [u64; 31],
    pub sp: u64,
    pub nzcv: u32,
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
    pub mem: Vec<u8>,
}

impl Dump {
    pub fn parse(stdout: &[u8]) -> Option<Dump> {
        if stdout.len() != DUMP_LEN + MEM_SIZE {
            return None;
        }
        let w = |i: usize| u64::from_le_bytes(stdout[i * 8..i * 8 + 8].try_into().unwrap());
        let mut x = [0u64; 31];
        for (i, r) in x.iter_mut().enumerate() {
            *r = w(i);
        }
        let mut v = [0u128; 32];
        for (i, r) in v.iter_mut().enumerate() {
            let o = DUMP_V + 16 * i;
            *r = u128::from_le_bytes(stdout[o..o + 16].try_into().unwrap());
        }
        let _ = DUMP_WORDS;
        Some(Dump {
            x,
            sp: w(31),
            nzcv: w(32) as u32,
            v,
            fpcr: w(DUMP_FP / 8) as u32,
            fpsr: w(DUMP_FP / 8 + 1) as u32,
            mem: stdout[DUMP_LEN..].to_vec(),
        })
    }

    /// Readable differences with respect to `other` (empty if identical).
    pub fn diff(&self, other: &Dump) -> String {
        let mut s = String::new();
        for r in 0..31 {
            if self.x[r] != other.x[r] {
                s += &format!("  x{r:<2} vetro={:#018x} qemu={:#018x}\n", self.x[r], other.x[r]);
            }
        }
        if self.sp != other.sp {
            s += &format!("  sp  vetro={:#018x} qemu={:#018x}\n", self.sp, other.sp);
        }
        if self.nzcv != other.nzcv {
            s += &format!("  nzcv vetro={:#x} qemu={:#x}\n", self.nzcv >> 28, other.nzcv >> 28);
        }
        for r in 0..32 {
            if self.v[r] != other.v[r] {
                s += &format!("  v{r:<2} vetro={:#034x} qemu={:#034x}\n", self.v[r], other.v[r]);
            }
        }
        if self.fpcr != other.fpcr {
            s += &format!("  fpcr vetro={:#010x} qemu={:#010x}\n", self.fpcr, other.fpcr);
        }
        if self.fpsr != other.fpsr {
            s += &format!("  fpsr vetro={:#010x} qemu={:#010x}\n", self.fpsr, other.fpsr);
        }
        for (i, (a, b)) in self.mem.iter().zip(&other.mem).enumerate() {
            if a != b {
                s += &format!("  mem[{:#x}] vetro={a:#04x} qemu={b:#04x}\n", RW_BASE + i as u64);
            }
        }
        s
    }
}

/// Outcome of a run, comparable between Vetro and QEMU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    Dump(Box<Dump>),
    Signal(i32),
    /// Any other outcome (unexpected exit, missing syscall, timeout).
    Other(String),
}

/// Runs on Vetro with the interpreter.
pub fn run_vetro(image: &[u8]) -> Run {
    run_vetro_with(image, false)
}

/// Runs on Vetro with the JIT (M4), translating each block from the first
/// execution: the test programs almost always pass only once.
pub fn run_vetro_jit(image: &[u8]) -> Run {
    run_vetro_with(image, true)
}

fn run_vetro_with(image: &[u8], jit: bool) -> Run {
    use vetro_cli::linux::{Config, Exit};
    let cfg = Config { max_steps: 1_000_000, jit, jit_threshold: 0, ..Config::default() };
    let out = match vetro_cli::run_elf(image, &["test"], &[], "/test", cfg) {
        Ok(o) => o,
        Err(e) => return Run::Other(format!("caricamento: {e}")),
    };
    match out.exit {
        Exit::Code(0) => match Dump::parse(&out.stdout) {
            Some(d) => Run::Dump(Box::new(d)),
            None => Run::Other(format!("stdout of {} bytes", out.stdout.len())),
        },
        Exit::Signal { signo, .. } => Run::Signal(signo),
        Exit::Unimplemented { .. } => Run::Signal(4),
        other => Run::Other(format!("{other:?}")),
    }
}

pub fn run_qemu(qemu: &std::path::Path, name: &str, image: &[u8]) -> Run {
    let path = match crate::qemu::write_temp_elf(name, image) {
        Ok(p) => p,
        Err(e) => return Run::Other(format!("scrittura ELF: {e}")),
    };
    let out = match crate::qemu::run(qemu, &path, crate::qemu::DEFAULT_TIMEOUT) {
        Ok(o) => o,
        Err(e) => return Run::Other(format!("qemu: {e}")),
    };
    if let Some(sig) = out.signal() {
        return Run::Signal(sig);
    }
    match (out.exit_code, Dump::parse(&out.stdout)) {
        (Some(0), Some(d)) => Run::Dump(Box::new(d)),
        _ => Run::Other(format!(
            "exit={:?} stdout={}B stderr={}",
            out.exit_code,
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Readable difference between two outcomes (empty if they match).
pub fn compare(vetro: &Run, qemu: &Run) -> String {
    match (vetro, qemu) {
        (Run::Dump(a), Run::Dump(b)) => a.diff(b),
        (a, b) if a == b => String::new(),
        (a, b) => format!("  vetro={}\n  qemu ={}\n", summary(a), summary(b)),
    }
}

fn summary(r: &Run) -> String {
    match r {
        Run::Dump(_) => "regular exit with dump".into(),
        Run::Signal(s) => format!("segnale {s}"),
        Run::Other(s) => s.clone(),
    }
}
