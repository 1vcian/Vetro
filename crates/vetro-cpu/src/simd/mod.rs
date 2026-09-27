//! AdvSIMD and floating point (Arm ARM C4.1.95 ff., "Data Processing --
//! Scalar Floating-Point and Advanced SIMD" and load/store with V=1).
//!
//! The decoder produces an already validated [`SimdInsn`]; execution lives in the
//! submodules. Everything that is valid on ARMv8.0/Cortex-A53 but not yet
//! written stays `Unimplemented`.

mod crypto;
pub mod fp;
mod fpinsn;
mod int;
mod ldst;
pub mod vreg;

pub use crypto::CryptoInsn;
pub use fpinsn::{FpInsn, MovKind};
pub use int::{CopyOp, IntInsn, MovImmOp};
pub use ldst::{Post, VecMemInsn};

use crate::decode::Insn;

/// Decoded SIMD/FP instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimdInsn {
    Mem(VecMemInsn),
    Int(IntInsn),
    Fp(FpInsn),
    Crypto(CryptoInsn),
}

/// Decodes a load/store with V=1 (called by the main decoder).
pub fn decode_ldst(w: u32) -> Insn {
    ldst::decode(w)
}

/// Decodes the "Data Processing -- Scalar FP and Advanced SIMD" class.
pub fn decode_dp(w: u32) -> Insn {
    // The FP classes have bit 30 = 0 and 28:24 = 11110/11111; the scalar
    // AdvSIMD ones have 31:30 = 01; the vector ones 31 = 0 and 28 = 0.
    let bit = |n: u32| (w >> n) & 1 != 0;
    if bit(28) && !bit(30) {
        return fpinsn::decode(w);
    }
    int::decode(w)
}

pub(crate) fn exec_mem<M: crate::mem::Memory>(
    cpu: &mut crate::state::Cpu,
    i: VecMemInsn,
    mem: &mut M,
) -> Result<(), crate::exec::Exception> {
    ldst::exec(cpu, i, mem)
}

pub(crate) fn exec_int(cpu: &mut crate::state::Cpu, i: IntInsn) {
    int::exec(cpu, i)
}

pub(crate) fn exec_crypto(cpu: &mut crate::state::Cpu, i: CryptoInsn) {
    crypto::exec(cpu, i)
}

pub(crate) fn exec_fp(cpu: &mut crate::state::Cpu, i: FpInsn) {
    fpinsn::exec(cpu, i)
}

/// Executes a SIMD/FP instruction without memory accesses (integer, FP or
/// cryptographic) like the interpreter: for the JIT, which calls it from
/// regions (`env.simd`, ADR 0026). Reads and writes only `v`, `x`, `nzcv`,
/// `fpcr` and `fpsr`. A load/store (`SimdInsn::Mem`) is a caller
/// error.
pub fn exec_dp(cpu: &mut crate::state::Cpu, i: SimdInsn) {
    match i {
        SimdInsn::Int(i) => int::exec(cpu, i),
        SimdInsn::Fp(f) => fpinsn::exec(cpu, f),
        SimdInsn::Crypto(c) => crypto::exec(cpu, c),
        SimdInsn::Mem(m) => panic!("exec_dp with a SIMD load/store: {m:?}"),
    }
}
