//! More integer SIMD inline (ADR 0041): the widening, narrowing, by-element
//! and saturating-doubling forms that Android's media and graphics code runs
//! (they went through `env.simd`: the interpreter's element loops on
//! 128-bit integers, about a tenth of an Android profile).
//!
//! Semantics of `vetro_cpu::simd::int` (`three_diff_exec`, `indexed_exec`,
//! `shift_imm_exec`, `three_same_exec`), exact by construction:
//!
//! - widening products and sums with `extmul` and `extend` (exact);
//! - rounding shifts as `(x >> s) + ((x >> (s - 1)) & 1)`, which is
//!   `(x + 2^(s-1)) >> s` without the overflow;
//! - saturating narrows with WASM's saturating `narrow` (the unsigned
//!   sources clamped to the signed maximum first: `narrow_u` reads its input
//!   as signed), QC when the result widened back differs from the input;
//! - SQDMULH/SQRDMULH: 16 bits with `q15mulr_sat_s` or the exact 32-bit
//!   product, 32 bits with the exact 64-bit product; the only saturating case
//!   (both operands the most negative value) sets QC.
//!
//! Scalar forms stay with `env.simd`.

use super::*;
use crate::wasm::v;
use vec::{ADD, MAX_S, MAX_U, MIN_S, MIN_U, MUL, SHL, SHR_S, SHR_U, SUB};

/// `extend_{low,high}` from narrow size `s` (0..3), signed or not.
fn extend(s: usize, signed: bool, high: bool) -> Option<u32> {
    Some(match (s, signed, high) {
        (0, true, false) => v::I16X8_EXTEND_LOW_I8X16_S,
        (0, true, true) => v::I16X8_EXTEND_HIGH_I8X16_S,
        (0, false, false) => v::I16X8_EXTEND_LOW_I8X16_U,
        (0, false, true) => v::I16X8_EXTEND_HIGH_I8X16_U,
        (1, true, false) => v::I32X4_EXTEND_LOW_I16X8_S,
        (1, true, true) => v::I32X4_EXTEND_HIGH_I16X8_S,
        (1, false, false) => v::I32X4_EXTEND_LOW_I16X8_U,
        (1, false, true) => v::I32X4_EXTEND_HIGH_I16X8_U,
        (2, true, false) => v::I64X2_EXTEND_LOW_I32X4_S,
        (2, true, true) => v::I64X2_EXTEND_HIGH_I32X4_S,
        (2, false, false) => v::I64X2_EXTEND_LOW_I32X4_U,
        (2, false, true) => v::I64X2_EXTEND_HIGH_I32X4_U,
        _ => return None,
    })
}

/// `extmul_{low,high}` from narrow size `s`.
fn extmul(s: usize, signed: bool, high: bool) -> Option<u32> {
    Some(match (s, signed, high) {
        (0, true, false) => v::I16X8_EXTMUL_LOW_I8X16_S,
        (0, true, true) => v::I16X8_EXTMUL_HIGH_I8X16_S,
        (0, false, false) => v::I16X8_EXTMUL_LOW_I8X16_U,
        (0, false, true) => v::I16X8_EXTMUL_HIGH_I8X16_U,
        (1, true, false) => v::I32X4_EXTMUL_LOW_I16X8_S,
        (1, true, true) => v::I32X4_EXTMUL_HIGH_I16X8_S,
        (1, false, false) => v::I32X4_EXTMUL_LOW_I16X8_U,
        (1, false, true) => v::I32X4_EXTMUL_HIGH_I16X8_U,
        (2, true, false) => v::I64X2_EXTMUL_LOW_I32X4_S,
        (2, true, true) => v::I64X2_EXTMUL_HIGH_I32X4_S,
        (2, false, false) => v::I64X2_EXTMUL_LOW_I32X4_U,
        (2, false, true) => v::I64X2_EXTMUL_HIGH_I32X4_U,
        _ => return None,
    })
}

/// A v128 constant with every lane of size `s` equal to `x`.
fn splat_const(s: usize, x: u64) -> (u64, u64) {
    let bits = 8u32 << s;
    let x = if bits == 64 { x } else { x & ((1u64 << bits) - 1) };
    let mut w = 0u64;
    let mut at = 0;
    while at < 64 {
        w |= x << at;
        at += bits;
    }
    (w, w)
}

/// Shuffle lanes: the low 8 bytes of the first operand, then the low 8 of
/// the second (a narrow result into the upper half, "2" forms).
const UPPER_FROM_SECOND: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23];

/// Lanes of size `eb` bytes taking element `2e + 1` (the high narrow half of
/// each wide element) into the low half.
fn high_halves(eb: usize) -> [u8; 16] {
    core::array::from_fn(|j| {
        let (e, b) = (j / eb, j % eb);
        let e = e % (8 / eb);
        ((2 * e + 1) * eb + b) as u8
    })
}

/// Lanes of size `eb` bytes taking element `2e` (the low narrow half of
/// each wide element, truncation) into the low half.
fn low_halves(eb: usize) -> [u8; 16] {
    core::array::from_fn(|j| {
        let (e, b) = (j / eb, j % eb);
        let e = e % (8 / eb);
        (2 * e * eb + b) as u8
    })
}

impl Tx {
    /// A mask (v128, lanes all ones or zeros) on the stack: if any lane is set
    /// (only in the low half with `!q`), FPSR.QC = 1.
    fn qc_if_any(&mut self, q: bool) {
        if !q {
            self.f.v128_const(u64::MAX, 0).v(v::AND);
        }
        self.f.v(v::ANY_TRUE).if_(BLOCK_EMPTY);
        self.f.local_get(L_STATE).local_get(L_STATE).i32_load(off::FPSR);
        self.f.i32_const(1 << 27).op(op::I32_OR).i32_store(off::FPSR);
        self.f.end();
    }

    /// Vm's element `index` in every lane (size `s`), on the stack.
    fn splat_elem(&mut self, rm: u8, index: u8, s: usize) {
        let eb = 1usize << s;
        let l: [u8; 16] = core::array::from_fn(|j| (index as usize * eb + j % eb) as u8);
        self.vld(rm);
        self.vld(rm);
        self.f.shuffle(l);
    }

    /// `(x + 2^(sh-1)) >> sh` (or `x >> sh` without `round`) of the v128 on
    /// the stack, lanes of size `s`, `1 <= sh < 8 << s`, arithmetic if
    /// `signed`, `s` <= 3. Uses `L_V0 + 5`.
    fn shift_right(&mut self, s: usize, sh: u32, signed: bool, round: bool) {
        let shr = if signed { SHR_S[s] } else { SHR_U[s] }.expect("shifts at every size");
        let (shr_u, add) = (SHR_U[s].expect("shifts at every size"), ADD[s].expect("adds at every size"));
        let t = L_V0 + 5;
        self.f.local_tee(t).i32_const(sh as i32).v(shr);
        if round {
            let (lo, hi) = splat_const(s, 1);
            self.f.local_get(t).i32_const(sh as i32 - 1).v(shr_u).v128_const(lo, hi).v(v::AND).v(add);
        }
    }

    /// Integer SIMD forms of this module; false if there is none (`env.simd`).
    pub(super) fn vec_more(&mut self, i: IntInsn) -> bool {
        match i {
            IntInsn::ThreeDiff { scalar: false, q, u, size, opcode, rm, rn, rd } => {
                self.three_diff(q, u, size as usize, opcode, rm, rn, rd)
            }
            IntInsn::Indexed { scalar: false, q, u, size, index, opcode, rm, rn, rd } => {
                self.indexed(q, u, size as usize, index, opcode, rm, rn, rd)
            }
            IntInsn::ShiftImm { scalar: false, q, u, esize, shift, opcode, rn, rd } => {
                self.shift_more(q, u, esize as u32, shift as u32, opcode, rn, rd)
            }
            IntInsn::ThreeSame { scalar: false, q, u, size, opcode, rm, rn, rd } => {
                self.three_same_more(q, u, size as usize, opcode, rm, rn, rd)
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn three_diff(&mut self, q: bool, u: bool, s: usize, opcode: u8, rm: u8, rn: u8, rd: u8) -> bool {
        let signed = !u;
        if s > 2 {
            return false;
        }
        let w = s + 1;
        let (Some(add), Some(sub)) = (ADD[w], SUB[w]) else { return false };
        match opcode {
            0b0000..=0b0011 => {
                // [SU]ADDL, [SU]ADDW, [SU]SUBL, [SU]SUBW (2)
                let Some(ext) = extend(s, signed, q) else { return false };
                self.vst_begin();
                self.vld(rn);
                if opcode & 1 == 0 {
                    self.f.v(ext);
                }
                self.vld(rm);
                self.f.v(ext);
                self.f.v(if opcode < 0b0010 { add } else { sub });
                self.vst_end(rd, true);
                true
            }
            0b0101 | 0b0111 => {
                // [SU]ABAL, [SU]ABDL (2): |a - b| of the narrow halves, widened
                let (mx, mn) = if u { (MAX_U, MIN_U) } else { (MAX_S, MIN_S) };
                let (Some(mx), Some(mn), Some(nsub), Some(ext)) = (mx[s], mn[s], SUB[s], extend(s, false, q))
                else {
                    return false;
                };
                self.vst_begin();
                if opcode == 0b0101 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.vld(rm);
                self.f.v(mx);
                self.vld(rn);
                self.vld(rm);
                self.f.v(mn).v(nsub).v(ext);
                if opcode == 0b0101 {
                    self.f.v(add);
                }
                self.vst_end(rd, true);
                true
            }
            0b1000 | 0b1010 | 0b1100 => {
                // [SU]MLAL, [SU]MLSL, [SU]MULL (2)
                let Some(mul) = extmul(s, signed, q) else { return false };
                self.vst_begin();
                if opcode != 0b1100 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.vld(rm);
                self.f.v(mul);
                if opcode != 0b1100 {
                    self.f.v(if opcode == 0b1000 { add } else { sub });
                }
                self.vst_end(rd, true);
                true
            }
            0b0100 | 0b0110 => {
                // ADDHN, SUBHN, RADDHN, RSUBHN (2): the high halves of the wide sums
                let eb = 1usize << s;
                self.vst_begin();
                if q {
                    self.vld(rd);
                }
                self.vld(rn);
                self.vld(rm);
                self.f.v(if opcode == 0b0100 { add } else { sub });
                if u {
                    let (lo, hi) = splat_const(w, 1u64 << ((8 << s) - 1));
                    self.f.v128_const(lo, hi).v(add);
                }
                self.f.local_tee(L_V0).local_get(L_V0).shuffle(high_halves(eb));
                if q {
                    self.f.shuffle(UPPER_FROM_SECOND);
                }
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn indexed(&mut self, q: bool, u: bool, s: usize, index: u8, opcode: u8, rm: u8, rn: u8, rd: u8) -> bool {
        if !(1..=2).contains(&s) {
            return false;
        }
        match (u, opcode) {
            (false, 0b1000) | (true, 0b0000) | (true, 0b0100) => {
                // MUL, MLA, MLS by element
                let (Some(mul), Some(add), Some(sub)) = (MUL[s], ADD[s], SUB[s]) else { return false };
                self.vst_begin();
                if opcode != 0b1000 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.splat_elem(rm, index, s);
                self.f.v(mul);
                match opcode {
                    0b0000 => {
                        self.f.v(add);
                    }
                    0b0100 => {
                        self.f.v(sub);
                    }
                    _ => {}
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b1010 | 0b0010 | 0b0110) => {
                // [SU]MULL, [SU]MLAL, [SU]MLSL by element (2)
                let (Some(mul), Some(add), Some(sub)) = (extmul(s, !u, q), ADD[s + 1], SUB[s + 1]) else {
                    return false;
                };
                self.vst_begin();
                if opcode != 0b1010 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.splat_elem(rm, index, s);
                self.f.v(mul);
                match opcode {
                    0b0010 => {
                        self.f.v(add);
                    }
                    0b0110 => {
                        self.f.v(sub);
                    }
                    _ => {}
                }
                self.vst_end(rd, true);
                true
            }
            (false, 0b1100 | 0b1101) => {
                // SQDMULH, SQRDMULH by element
                self.vld(rn);
                self.f.local_set(L_V0);
                self.splat_elem(rm, index, s);
                self.f.local_set(L_V0 + 1);
                self.sqdmulh_to(s, opcode == 0b1101, L_V0, L_V0 + 1, q, rd)
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn three_same_more(&mut self, q: bool, u: bool, s: usize, opcode: u8, rm: u8, rn: u8, rd: u8) -> bool {
        match opcode {
            0b10110 if (1..=2).contains(&s) => {
                // SQDMULH (u = 0), SQRDMULH (u = 1)
                self.vld(rn);
                self.f.local_set(L_V0);
                self.vld(rm);
                self.f.local_set(L_V0 + 1);
                self.sqdmulh_to(s, u, L_V0, L_V0 + 1, q, rd)
            }
            0b00000 | 0b00010 if s <= 2 => {
                // [SU]HADD: (a & b) + ((a ^ b) >> 1); [SU]RHADD: (a | b) - ((a ^ b) >> 1)
                let (Some(sh), Some(add), Some(sub)) = (if u { SHR_U[s] } else { SHR_S[s] }, ADD[s], SUB[s])
                else {
                    return false;
                };
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.v(if opcode == 0 { v::AND } else { v::OR });
                self.vld(rn);
                self.vld(rm);
                self.f.v(v::XOR).i32_const(1).v(sh);
                self.f.v(if opcode == 0 { add } else { sub });
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }

    /// Vd = SQDMULH(a, b), or SQRDMULH with `round`, of the v128 in locals `a`
    /// (`L_V0`) and `b` (`L_V0 + 1`), lanes of 16 or 32 bits (`s` 1, 2); QC
    /// if it saturated (both operands the most negative value, the only case).
    fn sqdmulh_to(&mut self, s: usize, round: bool, a: u32, b: u32, q: bool, rd: u8) -> bool {
        let (mask, r, lo, hi) = (L_V0 + 2, L_V0 + 3, L_V0 + 4, L_V0 + 5);
        let min = if s == 1 { 0x8000u64 } else { 0x8000_0000 };
        let (mlo, mhi) = splat_const(s, min);
        let eq = if s == 1 { v::I16X8_EQ } else { v::I32X4_EQ };
        self.f.local_get(a).v128_const(mlo, mhi).v(eq);
        self.f.local_get(b).v128_const(mlo, mhi).v(eq).v(v::AND).local_set(mask);
        match (s, round) {
            (1, true) => {
                // (a*b + 2^14) >> 15, saturated: exactly SQRDMULH.
                self.f.local_get(a).local_get(b).v(v::I16X8_Q15MULR_SAT_S).local_set(r);
            }
            (1, false) => {
                // (a*b) >> 15 of the exact 32-bit products, saturating narrow.
                self.f.local_get(a).local_get(b).v(v::I32X4_EXTMUL_LOW_I16X8_S);
                self.f.i32_const(15).v(v::I32X4_SHR_S);
                self.f.local_get(a).local_get(b).v(v::I32X4_EXTMUL_HIGH_I16X8_S);
                self.f.i32_const(15).v(v::I32X4_SHR_S);
                self.f.v(v::I16X8_NARROW_I32X4_S).local_set(r);
            }
            (2, _) => {
                // (a*b [+ 2^30]) >> 31 of the exact 64-bit products, their low
                // 32 bits; the saturated lanes replaced by the maximum.
                for (dst, op_) in [(lo, v::I64X2_EXTMUL_LOW_I32X4_S), (hi, v::I64X2_EXTMUL_HIGH_I32X4_S)] {
                    self.f.local_get(a).local_get(b).v(op_);
                    if round {
                        self.f.v128_const(1 << 30, 1 << 30).v(v::I64X2_ADD);
                    }
                    self.f.i32_const(31).v(v::I64X2_SHR_S).local_set(dst);
                }
                self.f.v128_const(0x7fff_ffff_7fff_ffff, 0x7fff_ffff_7fff_ffff);
                self.f.local_get(lo).local_get(hi);
                self.f.shuffle(core::array::from_fn(|j| ((j / 4) * 8 + j % 4) as u8));
                self.f.local_get(mask).v(v::BITSELECT).local_set(r);
            }
            _ => return false,
        }
        self.vst_begin();
        self.f.local_get(r);
        self.vst_end(rd, q);
        self.f.local_get(mask);
        self.qc_if_any(q);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn shift_more(&mut self, q: bool, u: bool, esize: u32, shift: u32, opcode: u8, rn: u8, rd: u8) -> bool {
        let s = esize.trailing_zeros() as usize - 3;
        match (u, opcode) {
            (_, 0b00100 | 0b00110) => {
                // [SU]RSHR, [SU]RSRA #shift (1..=esize)
                let (Some(add), Some(shr_u)) = (ADD[s], SHR_U[s]) else { return false };
                self.vst_begin();
                if opcode == 0b00110 {
                    self.vld(rd);
                }
                if shift == esize {
                    // (x + 2^(esize-1)) >> esize: 0 if signed, the top bit if not.
                    if u {
                        self.vld(rn);
                        self.f.i32_const(esize as i32 - 1).v(shr_u);
                    } else {
                        self.f.v128_const(0, 0);
                    }
                } else {
                    self.vld(rn);
                    self.shift_right(s, shift, !u, true);
                }
                if opcode == 0b00110 {
                    self.f.v(add);
                }
                self.vst_end(rd, q);
                true
            }
            (true, 0b01000) | (true, 0b01010) => {
                // SRI #shift (1..=esize), SLI #shift (0..esize): insert
                let mk = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
                let sri = opcode == 0b01000;
                let ins = if sri { if shift >= esize { 0 } else { mk >> shift } } else { (mk << shift) & mk };
                let (Some(shl), Some(shr_u)) = (SHL[s], SHR_U[s]) else { return false };
                self.vst_begin();
                if ins == 0 {
                    self.vld(rd);
                } else {
                    self.vld(rn);
                    self.f.i32_const(shift as i32).v(if sri { shr_u } else { shl });
                    self.vld(rd);
                    let (lo, hi) = splat_const(s, ins);
                    self.f.v128_const(lo, hi).v(v::BITSELECT);
                }
                self.vst_end(rd, q);
                true
            }
            (false, 0b10001) if s <= 1 => {
                // RSHRN(2) #shift: rounded at the source width, truncated
                let eb = 1usize << s;
                self.vst_begin();
                if q {
                    self.vld(rd);
                }
                self.vld(rn);
                self.shift_right(s + 1, shift, false, true);
                self.f.local_tee(L_V0).local_get(L_V0).shuffle(low_halves(eb));
                if q {
                    self.f.shuffle(UPPER_FROM_SECOND);
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b10000..=0b10011) if s <= 1 && (u || opcode >= 0b10010) => {
                // SQSHRN, SQRSHRN, UQSHRN, UQRSHRN, SQSHRUN, SQRSHRUN (2)
                let round = opcode & 1 == 1;
                let src_signed = !(u && opcode >= 0b10010);
                let dst_signed = !u;
                let (narrow, eq) = if s == 0 {
                    (if dst_signed { v::I8X16_NARROW_I16X8_S } else { v::I8X16_NARROW_I16X8_U }, v::I16X8_EQ)
                } else {
                    (if dst_signed { v::I16X8_NARROW_I32X4_S } else { v::I16X8_NARROW_I32X4_U }, v::I32X4_EQ)
                };
                let (Some(back), Some(min_u)) = (
                    extend(s, dst_signed, false),
                    if s == 0 { Some(v::I16X8_MIN_U) } else { Some(v::I32X4_MIN_U) },
                ) else {
                    return false;
                };
                // v' = the shifted source (L_V0)
                self.vld(rn);
                self.shift_right(s + 1, shift, src_signed, round);
                self.f.local_set(L_V0);
                // narrowed (L_V0 + 1); an unsigned source clamped to the signed maximum
                self.f.local_get(L_V0);
                if !src_signed {
                    let (lo, hi) = splat_const(s + 1, (1u64 << ((16 << s) - 1)) - 1);
                    self.f.v128_const(lo, hi).v(min_u);
                }
                self.f.local_tee(L_V0 + 2).local_get(L_V0 + 2).v(narrow).local_set(L_V0 + 1);
                // QC: the narrowed value widened back differs from v'
                self.f.local_get(L_V0 + 1).v(back).local_get(L_V0).v(eq).v(v::NOT);
                self.qc_if_any(true);
                self.vst_begin();
                if q {
                    self.vld(rd);
                    self.f.local_get(L_V0 + 1).shuffle(UPPER_FROM_SECOND);
                } else {
                    self.f.local_get(L_V0 + 1);
                }
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_helpers() {
        assert_eq!(splat_const(1, 0x80), (0x0080_0080_0080_0080, 0x0080_0080_0080_0080));
        assert_eq!(splat_const(3, 1 << 63), (1 << 63, 1 << 63));
        assert_eq!(high_halves(1)[..8], [1, 3, 5, 7, 9, 11, 13, 15]);
        assert_eq!(low_halves(2)[..8], [0, 1, 4, 5, 8, 9, 12, 13]);
    }
}
