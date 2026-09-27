//! Per-instruction tests.
//!
//! Each case describes initial state, instructions and expected state. `run()`
//! runs the program on Vetro and checks the expectations; if the oracle is
//! available it also runs it on QEMU and demands an identical final
//! state, so even the hand-written expected values are verified.
//! From M4 the program also runs with the JIT, which must give the same outcome
//! as the interpreter (ADR 0012).
//!
//! The encodings come from `tools/a64asm.sh` (a real assembler); the comment
//! next to each word is the assembled instruction.
//!
//! Convention: x28 points to the middle of the memory block and the body must
//! not leave it modified; memory offsets are relative to x28.

use vetro_diff::harness::{
    BASE_PTR, Dump, MEM_SIZE, PROLOGUE_LEN, Program, Run, compare, run_qemu, run_vetro, run_vetro_jit,
};
use vetro_diff::qemu;

pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGBUS: i32 = 7;
pub const SIGSEGV: i32 = 11;

/// Address of the first instruction of the body (for ADR, BL, branches).
pub const BODY: u64 = vetro_diff::elf::BASE + vetro_diff::elf::CODE_OFFSET + PROLOGUE_LEN as u64 * 4;

/// Address corresponding to offset `off` relative to x28.
pub const fn at(off: i64) -> u64 {
    BASE_PTR.wrapping_add(off as u64)
}

pub struct Case {
    name: String,
    program: Program,
    want_x: Vec<(usize, u64)>,
    want_sp: Option<u64>,
    want_flags: Option<u32>,
    want_mem: Vec<(i64, Vec<u8>)>,
    want_signal: Option<i32>,
    want_v: Vec<(usize, u128)>,
    want_fpsr: Option<u32>,
}

pub fn case(name: &str, body: &[u32]) -> Case {
    Case {
        name: name.into(),
        program: Program::new(body.to_vec()),
        want_x: Vec::new(),
        want_sp: None,
        want_flags: None,
        want_mem: Vec::new(),
        want_signal: None,
        want_v: Vec::new(),
        want_fpsr: None,
    }
}

fn mem_index(off: i64, len: usize) -> usize {
    let i = (MEM_SIZE as i64 / 2 + off) as usize;
    assert!(i + len <= MEM_SIZE, "offset {off} outside the memory block");
    i
}

impl Case {
    pub fn x(mut self, r: usize, v: u64) -> Self {
        assert!(r != 28, "x28 is reserved for the memory base");
        self.program.x[r] = v;
        self
    }

    /// Initial value of a vector register.
    pub fn v(mut self, r: usize, val: u128) -> Self {
        self.program.v[r] = val;
        self
    }

    pub fn fpcr(mut self, val: u32) -> Self {
        self.program.fpcr = val;
        self
    }

    pub fn want_v(mut self, r: usize, val: u128) -> Self {
        self.want_v.push((r, val));
        self
    }

    /// Expected FPSR (cumulative flags).
    pub fn want_fpsr(mut self, val: u32) -> Self {
        self.want_fpsr = Some(val);
        self
    }

    /// Initial flags as a 4-bit NZCV (e.g. `0b0110` = Z and C).
    pub fn flags(mut self, nzcv: u32) -> Self {
        self.program.nzcv = nzcv << 28;
        self
    }

    pub fn mem(mut self, off: i64, bytes: &[u8]) -> Self {
        let i = mem_index(off, bytes.len());
        self.program.mem[i..i + bytes.len()].copy_from_slice(bytes);
        self
    }

    pub fn want_x(mut self, r: usize, v: u64) -> Self {
        self.want_x.push((r, v));
        self
    }

    pub fn want_sp(mut self, v: u64) -> Self {
        self.want_sp = Some(v);
        self
    }

    pub fn want_flags(mut self, nzcv: u32) -> Self {
        self.want_flags = Some(nzcv);
        self
    }

    pub fn want_mem(mut self, off: i64, bytes: &[u8]) -> Self {
        mem_index(off, bytes.len());
        self.want_mem.push((off, bytes.to_vec()));
        self
    }

    pub fn want_signal(mut self, sig: i32) -> Self {
        self.want_signal = Some(sig);
        self
    }

    pub fn run(self) {
        let image = self.program.build();
        let ours = run_vetro(&image);
        self.check(&ours);
        // M4: the same program with the JIT must give the same outcome.
        let jit = run_vetro_jit(&image);
        assert!(
            jit == ours,
            "{}: JIT and interpreter diverge (vetro = JIT, qemu = interpreter)\n{}",
            self.name,
            compare(&jit, &ours)
        );
        if let Some(q) = qemu::locate_or_skip(&self.name) {
            let slug: String =
                self.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let theirs = run_qemu(&q, &format!("isa-{slug}"), &image);
            let diff = compare(&ours, &theirs);
            assert!(diff.is_empty(), "{}: Vetro and QEMU diverge\n{diff}", self.name);
        }
    }

    fn check(&self, ours: &Run) {
        let name = &self.name;
        if let Some(sig) = self.want_signal {
            assert_eq!(ours, &Run::Signal(sig), "{name}: expected signal {sig}");
            return;
        }
        let d: &Dump = match ours {
            Run::Dump(d) => d,
            other => panic!("{name}: expected a dump, got {other:?}"),
        };
        for &(r, v) in &self.want_x {
            assert_eq!(d.x[r], v, "{name}: x{r} = {:#x}, expected {v:#x}", d.x[r]);
        }
        if let Some(sp) = self.want_sp {
            assert_eq!(d.sp, sp, "{name}: sp = {:#x}, expected {sp:#x}", d.sp);
        }
        if let Some(f) = self.want_flags {
            assert_eq!(d.nzcv >> 28, f, "{name}: NZCV = {:04b}, expected {f:04b}", d.nzcv >> 28);
        }
        for &(r, v) in &self.want_v {
            assert_eq!(d.v[r], v, "{name}: v{r} = {:#034x}, expected {v:#034x}", d.v[r]);
        }
        if let Some(f) = self.want_fpsr {
            assert_eq!(d.fpsr, f, "{name}: FPSR = {:#x}, expected {f:#x}", d.fpsr);
        }
        for (off, bytes) in &self.want_mem {
            let i = mem_index(*off, bytes.len());
            assert_eq!(&d.mem[i..i + bytes.len()], &bytes[..], "{name}: memory at x28{off:+}");
        }
        assert_eq!(d.x[28], BASE_PTR, "{name}: the body left x28 modified");
    }
}
