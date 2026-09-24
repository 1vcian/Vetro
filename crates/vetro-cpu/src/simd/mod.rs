//! AdvSIMD e virgola mobile (Arm ARM C4.1.95 ss., "Data Processing --
//! Scalar Floating-Point and Advanced SIMD" e load/store con V=1).
//!
//! Il decoder produce una [`SimdInsn`] già validata; l'esecuzione sta nei
//! sottomoduli. Tutto ciò che è valido su ARMv8.0/Cortex-A53 ma non ancora
//! scritto resta `Unimplemented`.

mod crypto;
pub mod fp;
mod fpinsn;
mod int;
mod ldst;
pub mod vreg;

pub use crypto::CryptoInsn;
pub use fpinsn::FpInsn;
pub use int::IntInsn;
pub use ldst::VecMemInsn;

use crate::decode::Insn;

/// Istruzione SIMD/FP decodificata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimdInsn {
    Mem(VecMemInsn),
    Int(IntInsn),
    Fp(FpInsn),
    Crypto(CryptoInsn),
}

/// Decodifica un load/store con V=1 (chiamato dal decoder principale).
pub fn decode_ldst(w: u32) -> Insn {
    ldst::decode(w)
}

/// Decodifica la classe "Data Processing -- Scalar FP and Advanced SIMD".
pub fn decode_dp(w: u32) -> Insn {
    // Le classi FP hanno bit 30 = 0 e 28:24 = 11110/11111; le AdvSIMD
    // scalari hanno 31:30 = 01; le vettoriali 31 = 0 e 28 = 0.
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
