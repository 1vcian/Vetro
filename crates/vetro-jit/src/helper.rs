//! `env.simd` (ADR 0026): the SIMD/FP instructions without memory accesses
//! that the regions do not translate inline are executed by the interpreter
//! itself (`vetro_cpu::simd::exec_dp`) on the V registers, FPCR and FPSR of
//! `JitState`, without leaving the region. The semantics are therefore those
//! of the interpreter by construction (NaN, rounding, cumulative flags,
//! denormals, FZ, DN).
//!
//! General registers and NZCV stay in the region's variables: the caller
//! passes the general register read by the instruction (`x`) and NZCV
//! (`nzcv`), and receives the written general register or the new NZCV, as
//! [`io`] says.

use std::cell::RefCell;

use vetro_cpu::simd::{CopyOp, FpInsn, IntInsn, SimdInsn};
use vetro_cpu::{Cpu, Insn};

use crate::state::off;

/// What an instruction writes besides the V registers and FPSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    /// Nothing.
    None,
    /// The general register (31 = XZR, discarded): `env.simd` returns
    /// its value.
    X(u8),
    /// NZCV: `env.simd` returns it in bits 31:28.
    Nzcv,
}

/// General registers and NZCV of a SIMD/FP instruction without memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Io {
    /// General register read (31 = XZR).
    pub x_in: Option<u8>,
    /// Reads NZCV (condition of FCCMP/FCSEL).
    pub nzcv_in: bool,
    pub out: Out,
}

/// General registers and NZCV read and written by `i`.
pub fn io(i: &SimdInsn) -> Io {
    let none = Io { x_in: None, nzcv_in: false, out: Out::None };
    match *i {
        SimdInsn::Fp(f) => match f {
            FpInsn::FromInt { rn, .. } | FpInsn::MovFromGp { rn, .. } => Io { x_in: Some(rn), ..none },
            FpInsn::ToInt { rd, .. } | FpInsn::MovToGp { rd, .. } => Io { out: Out::X(rd), ..none },
            FpInsn::Cmp { .. } => Io { out: Out::Nzcv, ..none },
            FpInsn::CondCmp { .. } => Io { nzcv_in: true, out: Out::Nzcv, ..none },
            FpInsn::CondSel { .. } => Io { nzcv_in: true, ..none },
            _ => none,
        },
        SimdInsn::Int(IntInsn::Copy { op, rn, rd, .. }) => match op {
            CopyOp::DupGen | CopyOp::InsGen => Io { x_in: Some(rn), ..none },
            CopyOp::Umov | CopyOp::Smov => Io { out: Out::X(rd), ..none },
            _ => none,
        },
        _ => none,
    }
}

/// V0..V31 from the `JitState` format (little-endian, 16 bytes each).
#[inline]
fn copy_v_in(v: &mut [u128; 32], b: &[u8]) {
    assert_eq!(b.len(), 512);
    if cfg!(target_endian = "little") {
        // SAFETY: `v` is 512 bytes; on a little-endian host a u128 has in
        // memory the same bytes as the `JitState` format.
        unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr().cast::<u8>(), 512) };
    } else {
        for (r, d) in v.iter_mut().enumerate() {
            *d = u128::from_le_bytes(b[16 * r..16 * r + 16].try_into().unwrap());
        }
    }
}

/// V0..V31 in the `JitState` format.
#[inline]
fn copy_v_out(b: &mut [u8], v: &[u128; 32]) {
    assert_eq!(b.len(), 512);
    if cfg!(target_endian = "little") {
        // SAFETY: as in `copy_v_in`.
        unsafe { core::ptr::copy_nonoverlapping(v.as_ptr().cast::<u8>(), b.as_mut_ptr(), 512) };
    } else {
        for (r, s) in v.iter().enumerate() {
            b[16 * r..16 * r + 16].copy_from_slice(&s.to_le_bytes());
        }
    }
}

thread_local! {
    /// Scratch CPU: only the V registers, FPCR, FPSR, NZCV and the general
    /// register read are copied in and out.
    static SCRATCH: RefCell<Cpu> = RefCell::new(Cpu::new());
    /// Calls to [`exec`] from this thread.
    static CALLS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Calls to `env.simd` made so far by this thread: the tests use it to
/// check that the fast inline and runtime paths really work.
pub fn calls() -> u64 {
    CALLS.get()
}

thread_local! {
    /// With [`profile`] active: calls per instruction class.
    static PROFILE: RefCell<Option<crate::profile::Profile>> = const { RefCell::new(None) };
}

/// Counts per class the instructions executed by `env.simd` (for
/// measurements, `VETRO_JIT_PROFILE=1`).
pub fn profile(on: bool) {
    PROFILE.with_borrow_mut(|p| *p = on.then(crate::profile::Profile::default));
}

/// Report of the most frequent classes executed by `env.simd`.
pub fn profile_report(n: usize) -> Option<String> {
    PROFILE.with_borrow(|p| p.as_ref().map(|p| p.report(n)))
}

/// `env.simd(state, word, x, nzcv) -> value`: executes instruction `word`
/// (SIMD/FP without memory) on the `JitState` at `mem[at..]`, whose V
/// registers must be valid (`v_valid`). Returns the written general register
/// or NZCV ([`Out`]), otherwise 0.
pub fn exec(mem: &mut [u8], at: usize, word: u32, x: u64, nzcv: u32) -> u64 {
    let Insn::Simd(i) = vetro_cpu::decode(word) else {
        panic!("env.simd with a non-SIMD instruction: {word:#010x}");
    };
    CALLS.set(CALLS.get() + 1);
    PROFILE.with_borrow_mut(|p| {
        if let Some(p) = p {
            p.note(word, None);
        }
    });
    let io = io(&i);
    let st = &mut mem[at..at + off::SIZE];
    let rd32 = |st: &[u8], o: u32| u32::from_le_bytes(st[o as usize..o as usize + 4].try_into().unwrap());
    SCRATCH.with_borrow_mut(|cpu| {
        let v = off::V as usize;
        copy_v_in(&mut cpu.v, &st[v..v + 512]);
        cpu.fpcr = rd32(st, off::FPCR);
        cpu.fpsr = rd32(st, off::FPSR);
        cpu.nzcv = nzcv;
        if let Some(rn) = io.x_in {
            cpu.set_x(rn, x);
        }
        vetro_cpu::simd::exec_dp(cpu, i);
        copy_v_out(&mut st[v..v + 512], &cpu.v);
        let f = off::FPSR as usize;
        st[f..f + 4].copy_from_slice(&cpu.fpsr.to_le_bytes());
        match io.out {
            Out::None => 0,
            Out::X(rd) => cpu.xr(rd),
            Out::Nzcv => cpu.nzcv as u64,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::JitState;

    fn run(cpu: &Cpu, word: u32, x: u64) -> (JitState, u64) {
        let mut s = JitState::from_cpu(cpu);
        for (d, v) in s.v.iter_mut().zip(&cpu.v) {
            *d = [*v as u64, (*v >> 64) as u64];
        }
        let mut mem = vec![0u8; 16 + off::SIZE];
        s.store(&mut mem, 16);
        let r = exec(&mut mem, 16, word, x, cpu.nzcv);
        (JitState::load(&mem, 16), r)
    }

    /// Same registers and flags as the interpreter (encodings from tools/a64asm.sh).
    #[test]
    fn same_as_interpreter() {
        let mut cpu = Cpu::new();
        cpu.v[1] = 0x3ff0_0000_0000_0000; // 1.0
        cpu.v[2] = 0x3fb9_9999_9999_999a; // 0.1
        cpu.x[3] = 7;
        // fadd d0, d1, d2 (inexact: IXC); fcmp d1, d2; scvtf d4, x3;
        // fcvtzs x5, d1
        for (w, x) in [(0x1e622820u32, 0), (0x1e622020, 0), (0x9e620064, 7), (0x9e780025, 0)] {
            let (s, r) = run(&cpu, w, x);
            let mut want = cpu.clone();
            let Insn::Simd(i) = vetro_cpu::decode(w) else { unreachable!() };
            vetro_cpu::simd::exec_dp(&mut want, i);
            for (a, b) in s.v.iter().zip(&want.v) {
                assert_eq!(a[0] as u128 | (a[1] as u128) << 64, *b, "{w:#x}");
            }
            assert_eq!(s.fpsr, want.fpsr, "{w:#x}");
            match io(&i).out {
                Out::None => assert_eq!(r, 0),
                Out::X(rd) => assert_eq!(r, want.xr(rd)),
                Out::Nzcv => assert_eq!(r as u32, want.nzcv),
            }
        }
    }
}
