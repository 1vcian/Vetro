//! Integer SIMD inline with the 128-bit WASM instructions (ADR 0026).
//!
//! Only operations that are exact by construction (no flags: the saturating ones,
//! which write FPSR.QC, stay with `env.simd`), with the semantics of
//! `vetro_cpu::simd::int`: with Q = 0 the high half of the result is zero.
//! Whatever has no form here returns `false` and the interpreter executes it
//! from the region ([`Tx::simd_helper`]).

use super::*;
use crate::wasm::v;

/// WASM opcode per element size (8, 16, 32, 64 bits); `None` if
/// the operation does not exist at that size.
pub(super) type BySize = [Option<u32>; 4];

pub(super) const ADD: BySize =
    [Some(v::I8X16_ADD), Some(v::I16X8_ADD), Some(v::I32X4_ADD), Some(v::I64X2_ADD)];
pub(super) const SUB: BySize =
    [Some(v::I8X16_SUB), Some(v::I16X8_SUB), Some(v::I32X4_SUB), Some(v::I64X2_SUB)];
const EQ: BySize = [Some(v::I8X16_EQ), Some(v::I16X8_EQ), Some(v::I32X4_EQ), Some(v::I64X2_EQ)];
const GT_S: BySize = [Some(v::I8X16_GT_S), Some(v::I16X8_GT_S), Some(v::I32X4_GT_S), Some(v::I64X2_GT_S)];
const GT_U: BySize = [Some(v::I8X16_GT_U), Some(v::I16X8_GT_U), Some(v::I32X4_GT_U), None];
const GE_S: BySize = [Some(v::I8X16_GE_S), Some(v::I16X8_GE_S), Some(v::I32X4_GE_S), Some(v::I64X2_GE_S)];
const GE_U: BySize = [Some(v::I8X16_GE_U), Some(v::I16X8_GE_U), Some(v::I32X4_GE_U), None];
pub(super) const MAX_S: BySize = [Some(v::I8X16_MAX_S), Some(v::I16X8_MAX_S), Some(v::I32X4_MAX_S), None];
pub(super) const MAX_U: BySize = [Some(v::I8X16_MAX_U), Some(v::I16X8_MAX_U), Some(v::I32X4_MAX_U), None];
pub(super) const MIN_S: BySize = [Some(v::I8X16_MIN_S), Some(v::I16X8_MIN_S), Some(v::I32X4_MIN_S), None];
pub(super) const MIN_U: BySize = [Some(v::I8X16_MIN_U), Some(v::I16X8_MIN_U), Some(v::I32X4_MIN_U), None];
pub(super) const MUL: BySize = [None, Some(v::I16X8_MUL), Some(v::I32X4_MUL), None];
const ABS: BySize = [Some(v::I8X16_ABS), Some(v::I16X8_ABS), Some(v::I32X4_ABS), Some(v::I64X2_ABS)];
const NEG: BySize = [Some(v::I8X16_NEG), Some(v::I16X8_NEG), Some(v::I32X4_NEG), Some(v::I64X2_NEG)];
pub(super) const SHL: BySize =
    [Some(v::I8X16_SHL), Some(v::I16X8_SHL), Some(v::I32X4_SHL), Some(v::I64X2_SHL)];
pub(super) const SHR_S: BySize =
    [Some(v::I8X16_SHR_S), Some(v::I16X8_SHR_S), Some(v::I32X4_SHR_S), Some(v::I64X2_SHR_S)];
pub(super) const SHR_U: BySize =
    [Some(v::I8X16_SHR_U), Some(v::I16X8_SHR_U), Some(v::I32X4_SHR_U), Some(v::I64X2_SHR_U)];
pub(super) const SPLAT: BySize =
    [Some(v::I8X16_SPLAT), Some(v::I16X8_SPLAT), Some(v::I32X4_SPLAT), Some(v::I64X2_SPLAT)];

/// `i8x16.shuffle` indices for a result of `n` elements of `eb` bytes
/// in which element `e` comes from element `src(e)` of the concatenation
/// (first operand: elements 0..16/eb, second: the following ones). The elements
/// beyond `n` repeat the first (the caller zeroes them if needed).
fn lanes(n: usize, eb: usize, src: impl Fn(usize) -> usize) -> [u8; 16] {
    let mut l = [0u8; 16];
    for e in 0..16 / eb {
        let s = if e < n { src(e) } else { src(0) };
        for b in 0..eb {
            l[e * eb + b] = (s * eb + b) as u8;
        }
    }
    l
}

impl Tx {
    /// Vr (v128) on the stack.
    pub(super) fn vld(&mut self, r: u8) {
        self.simd = true;
        self.f.local_get(L_STATE).v128_load(off::V + 16 * r as u32);
    }

    /// Before computing a value to write into a V register: the address.
    pub(super) fn vst_begin(&mut self) {
        self.simd = true;
        self.f.local_get(L_STATE);
    }

    /// Writes the v128 on top of the stack into Vd (high half zeroed if `!q`).
    pub(super) fn vst_end(&mut self, rd: u8, q: bool) {
        if !q {
            self.f.v128_const(u64::MAX, 0).v(v::AND);
        }
        self.f.v128_store(off::V + 16 * rd as u32);
    }

    /// Two v128 on the stack: if they differ (only in the low half with
    /// `!q`), FPSR.QC = 1 (cumulative saturation flag).
    fn qc_if_differ(&mut self, q: bool) {
        self.f.v(v::XOR);
        if !q {
            self.f.v128_const(u64::MAX, 0).v(v::AND);
        }
        self.f.v(v::ANY_TRUE).if_(BLOCK_EMPTY);
        self.f.local_get(L_STATE).local_get(L_STATE).i32_load(off::FPSR);
        self.f.i32_const(1 << 27).op(op::I32_OR).i32_store(off::FPSR);
        self.f.end();
    }

    /// Vd = op(Vn, Vm) with a binary WASM instruction.
    fn vbin(&mut self, op_: u32, rn: u8, rm: u8, rd: u8, q: bool) {
        self.vst_begin();
        self.vld(rn);
        self.vld(rm);
        self.f.v(op_);
        self.vst_end(rd, q);
    }

    /// Inline integer SIMD instructions; false if there is no form for them (they
    /// are executed by `env.simd`).
    pub(super) fn vec_int_inline(&mut self, i: IntInsn) -> bool {
        match i {
            IntInsn::ThreeSame { scalar: false, q, u, size, opcode, rm, rn, rd } => {
                self.three_same(q, u, size, opcode, rm, rn, rd)
            }
            IntInsn::ThreeSame { scalar: true, u, size: 3, opcode: 0b10000, rm, rn, rd, .. } => {
                // ADD/SUB Dd, Dn, Dm
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.v(if u { v::I64X2_SUB } else { v::I64X2_ADD });
                self.vst_end(rd, false);
                true
            }
            IntInsn::TwoMisc { scalar: false, q, u, size, opcode, rn, rd } => {
                self.two_misc(q, u, size, opcode, rn, rd)
            }
            IntInsn::Across { q, u, size, opcode, rn, rd } => self.across(q, u, size, opcode, rn, rd),
            IntInsn::AddpScalar { rn, rd } => {
                // Dd = Vn.D[0] + Vn.D[1]
                self.vst_begin();
                self.vld(rn);
                self.vld(rn);
                self.vld(rn);
                self.f.shuffle(lanes(2, 8, |_| 1));
                self.f.v(v::I64X2_ADD);
                self.vst_end(rd, false);
                true
            }
            IntInsn::ShiftImm { scalar: false, q, u, esize, shift, opcode, rn, rd } => {
                self.shift_imm(q, u, esize as u32, shift as u32, opcode, rn, rd)
            }
            IntInsn::Perm { q, size, opcode, rm, rn, rd } => {
                let eb = 1usize << size;
                let n = (if q { 16 } else { 8 }) / eb;
                let part = (opcode >> 2) as usize;
                let l = match opcode & 3 {
                    // UZP: even (or odd) elements of concat(b:a)
                    1 => lanes(n, eb, |e| {
                        let k = 2 * e + part;
                        if k < n { k } else { 16 / eb + k - n }
                    }),
                    // TRN
                    2 => lanes(n, eb, |e| {
                        let p = e / 2;
                        if e % 2 == 0 { 2 * p + part } else { 16 / eb + 2 * p + part }
                    }),
                    // ZIP
                    _ => lanes(n, eb, |e| {
                        let p = e / 2 + part * n / 2;
                        if e % 2 == 0 { p } else { 16 / eb + p }
                    }),
                };
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.shuffle(l);
                self.vst_end(rd, q);
                true
            }
            IntInsn::Ext { q, imm4, rm, rn, rd } => {
                let pos = imm4 as usize;
                let l: [u8; 16] = if q {
                    core::array::from_fn(|j| (pos + j) as u8)
                } else {
                    // concat(hi.lo : lo.lo) >> pos, 8 bytes.
                    core::array::from_fn(|j| {
                        let k = (pos + j) % 16;
                        if k < 8 { k as u8 } else { (16 + k - 8) as u8 }
                    })
                };
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.shuffle(l);
                self.vst_end(rd, q);
                true
            }
            IntInsn::Tbl { q, len, tbx, rm, rn, rd } => {
                // swizzle gives 0 for indices >= 16: table k uses
                // index - 16k (out-of-table indices give 0 in all of them).
                let regs = len as u32 + 1;
                self.vst_begin();
                for k in 0..regs {
                    self.vld(((rn as u32 + k) % 32) as u8);
                    self.vld(rm);
                    if k > 0 {
                        let b = (16 * k) as u64 * 0x0101_0101_0101_0101;
                        self.f.v128_const(b, b).v(v::I8X16_SUB);
                    }
                    self.f.v(v::SWIZZLE);
                    if k > 0 {
                        self.f.v(v::OR);
                    }
                }
                if tbx {
                    // Out-of-table indices: the byte of Vd.
                    self.vld(rd);
                    self.vld(rm);
                    let b = (16 * regs) as u64 * 0x0101_0101_0101_0101;
                    self.f.v128_const(b, b).v(v::I8X16_LT_U);
                    self.f.v(v::BITSELECT);
                }
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn three_same(&mut self, q: bool, u: bool, size: u8, opcode: u8, rm: u8, rn: u8, rd: u8) -> bool {
        let s = size as usize;
        if opcode == 0b00011 {
            // Logical operations on the whole register.
            self.vst_begin();
            match (u, size) {
                (false, 0) => {
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::AND);
                }
                (false, 1) => {
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::ANDNOT);
                }
                (false, 2) => {
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::OR);
                }
                (false, _) => {
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::NOT).v(v::OR);
                }
                (true, 0) => {
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::XOR);
                }
                (true, 1) => {
                    // BSL: (d & a) | (!d & b)
                    self.vld(rn);
                    self.vld(rm);
                    self.vld(rd);
                    self.f.v(v::BITSELECT);
                }
                (true, 2) => {
                    // BIT: (d & !b) | (a & b)
                    self.vld(rn);
                    self.vld(rd);
                    self.vld(rm);
                    self.f.v(v::BITSELECT);
                }
                (true, _) => {
                    // BIF: (d & b) | (a & !b)
                    self.vld(rd);
                    self.vld(rn);
                    self.vld(rm);
                    self.f.v(v::BITSELECT);
                }
            }
            self.vst_end(rd, q);
            return true;
        }
        let bin = |t: &mut Tx, ops: BySize| -> bool {
            match ops[s] {
                Some(o) => {
                    t.vbin(o, rn, rm, rd, q);
                    true
                }
                None => false,
            }
        };
        match (u, opcode) {
            (false, 0b10000) => bin(self, ADD),
            (true, 0b10000) => bin(self, SUB),
            (true, 0b10001) => bin(self, EQ),
            (false, 0b10001) => {
                // CMTST: (a & b) != 0
                let Some(eq) = EQ[s] else { return false };
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.v(v::AND).v128_const(0, 0).v(eq).v(v::NOT);
                self.vst_end(rd, q);
                true
            }
            (false, 0b00110) => bin(self, GT_S),
            (true, 0b00110) => bin(self, GT_U),
            (false, 0b00111) => bin(self, GE_S),
            (true, 0b00111) => bin(self, GE_U),
            (false, 0b01100) => bin(self, MAX_S),
            (true, 0b01100) => bin(self, MAX_U),
            (false, 0b01101) => bin(self, MIN_S),
            (true, 0b01101) => bin(self, MIN_U),
            (false, 0b10011) => bin(self, MUL),
            (_, 0b01110 | 0b01111) => {
                // [SU]ABD, [SU]ABA: max - min (+ Vd)
                let (mx, mn) = if u { (MAX_U, MIN_U) } else { (MAX_S, MIN_S) };
                let (Some(mx), Some(mn), Some(sub), Some(add)) = (mx[s], mn[s], SUB[s], ADD[s]) else {
                    return false;
                };
                self.vst_begin();
                if opcode == 0b01111 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.vld(rm);
                self.f.v(mx);
                self.vld(rn);
                self.vld(rm);
                self.f.v(mn).v(sub);
                if opcode == 0b01111 {
                    self.f.v(add);
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b10010) => {
                // MLA/MLS: Vd ± Vn * Vm
                let (Some(mul), Some(acc)) = (MUL[s], if u { SUB[s] } else { ADD[s] }) else { return false };
                self.vst_begin();
                self.vld(rd);
                self.vld(rn);
                self.vld(rm);
                self.f.v(mul).v(acc);
                self.vst_end(rd, q);
                true
            }
            (_, 0b00001 | 0b00101) if size <= 1 => {
                // [SU]Q{ADD,SUB} at 8 and 16 bits: WASM's saturating ops; QC if a
                // lane differs from the modular sum (there is saturation
                // exactly when they differ).
                let (sat_op, wrap) = match (u, opcode, size) {
                    (false, 0b00001, 0) => (v::I8X16_ADD_SAT_S, v::I8X16_ADD),
                    (true, 0b00001, 0) => (v::I8X16_ADD_SAT_U, v::I8X16_ADD),
                    (false, _, 0) => (v::I8X16_SUB_SAT_S, v::I8X16_SUB),
                    (true, _, 0) => (v::I8X16_SUB_SAT_U, v::I8X16_SUB),
                    (false, 0b00001, _) => (v::I16X8_ADD_SAT_S, v::I16X8_ADD),
                    (true, 0b00001, _) => (v::I16X8_ADD_SAT_U, v::I16X8_ADD),
                    (false, _, _) => (v::I16X8_SUB_SAT_S, v::I16X8_SUB),
                    (true, _, _) => (v::I16X8_SUB_SAT_U, v::I16X8_SUB),
                };
                self.vld(rn);
                self.vld(rm);
                self.f.v(sat_op).local_tee(L_V0);
                self.vld(rn);
                self.vld(rm);
                self.f.v(wrap);
                self.qc_if_differ(q);
                self.vst_begin();
                self.f.local_get(L_V0);
                self.vst_end(rd, q);
                true
            }
            (true, 0b00010) if size <= 1 => {
                // URHADD: (a + b + 1) >> 1 without overflowing
                bin(self, [Some(v::I8X16_AVGR_U), Some(v::I16X8_AVGR_U), None, None])
            }
            (_, 0b10111 | 0b10100 | 0b10101) => {
                // ADDP, [SU]MAXP, [SU]MINP: pairwise over concat(a, b)
                let ops = match (u, opcode) {
                    (false, 0b10111) => ADD,
                    (true, 0b10111) => return false,
                    (false, 0b10100) => MAX_S,
                    (true, 0b10100) => MAX_U,
                    (false, _) => MIN_S,
                    (true, _) => MIN_U,
                };
                let Some(add) = ops[s] else { return false };
                let eb = 1usize << size;
                let n = (if q { 16 } else { 8 }) / eb;
                let half = n / 2;
                // pair e: elements 2e, 2e+1 of a (e < n/2) or of b
                let src = |e: usize, o: usize| {
                    if e < half { 2 * e + o } else { 16 / eb + 2 * (e - half) + o }
                };
                self.vst_begin();
                self.vld(rn);
                self.vld(rm);
                self.f.shuffle(lanes(n, eb, |e| src(e, 0)));
                self.vld(rn);
                self.vld(rm);
                self.f.shuffle(lanes(n, eb, |e| src(e, 1)));
                self.f.v(add);
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }

    fn two_misc(&mut self, q: bool, u: bool, size: u8, opcode: u8, rn: u8, rd: u8) -> bool {
        let s = size as usize;
        let un = |t: &mut Tx, o: Option<u32>| -> bool {
            let Some(o) = o else { return false };
            t.vst_begin();
            t.vld(rn);
            t.f.v(o);
            t.vst_end(rd, q);
            true
        };
        // Comparison with zero: op(Vn, 0) (op on (x, 0)) or op(0, Vn).
        let cmp0 = |t: &mut Tx, o: Option<u32>, zero_first: bool| -> bool {
            let Some(o) = o else { return false };
            t.vst_begin();
            if zero_first {
                t.f.v128_const(0, 0);
                t.vld(rn);
            } else {
                t.vld(rn);
                t.f.v128_const(0, 0);
            }
            t.f.v(o);
            t.vst_end(rd, q);
            true
        };
        match (u, opcode) {
            (false, 0b00101) if size == 0 => un(self, Some(v::I8X16_POPCNT)),
            (true, 0b00101) if size == 0 => un(self, Some(v::NOT)),
            (false, 0b01011) => un(self, ABS[s]),
            (true, 0b01011) => un(self, NEG[s]),
            (false, 0b01000) => cmp0(self, GT_S[s], false), // CMGT #0
            (true, 0b01000) => cmp0(self, GE_S[s], false),  // CMGE #0
            (false, 0b01001) => cmp0(self, EQ[s], false),   // CMEQ #0
            (true, 0b01001) => cmp0(self, GE_S[s], true),   // CMLE #0: 0 >= x
            (false, 0b01010) => cmp0(self, GT_S[s], true),  // CMLT #0: 0 > x
            (_, 0b00000 | 0b00001) => {
                // REV64 (u=0, 00000), REV32 (u=1, 00000), REV16 (u=0, 00001)
                let container = match (u, opcode) {
                    (false, 0b00000) => 8,
                    (true, 0b00000) => 4,
                    _ => 2,
                };
                let eb = 1usize << size;
                let l: [u8; 16] = core::array::from_fn(|j| {
                    let c = j / container * container;
                    let within = j % container;
                    let e = within / eb;
                    let per = container / eb;
                    (c + (per - 1 - e) * eb + within % eb) as u8
                });
                self.vst_begin();
                self.vld(rn);
                self.vld(rn);
                self.f.shuffle(l);
                self.vst_end(rd, q);
                true
            }
            (_, 0b00010 | 0b00110) if size <= 1 => {
                // [SU]ADDLP, [SU]ADALP: widening pairwise sums
                let o = match (u, size) {
                    (false, 0) => v::I16X8_EXTADD_PAIRWISE_I8X16_S,
                    (true, 0) => v::I16X8_EXTADD_PAIRWISE_I8X16_U,
                    (false, _) => v::I32X4_EXTADD_PAIRWISE_I16X8_S,
                    (true, _) => v::I32X4_EXTADD_PAIRWISE_I16X8_U,
                };
                self.vst_begin();
                if opcode == 0b00110 {
                    self.vld(rd);
                }
                self.vld(rn);
                self.f.v(o);
                if opcode == 0b00110 {
                    self.f.v(if size == 0 { v::I16X8_ADD } else { v::I32X4_ADD });
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b10100) | (true, 0b10010) if size <= 1 => {
                // SQXTN(2) (u=0, 10100), SQXTUN(2) (u=1, 10010): from 2*esize
                // with saturation (WASM's narrow, which reads the input as
                // signed); UQXTN (u=1, 10100) no. QC if the result, widened
                // back (sign-extended for SQXTN, zero-extended for SQXTUN),
                // differs from the input: a saturated result can equal the
                // truncation (0x017f -> 0x7f), so that comparison is not enough.
                let (narrow, widen) = match (u, opcode, size) {
                    (false, 0b10100, 0) => (v::I8X16_NARROW_I16X8_S, v::I16X8_EXTEND_LOW_I8X16_S),
                    (true, 0b10010, 0) => (v::I8X16_NARROW_I16X8_U, v::I16X8_EXTEND_LOW_I8X16_U),
                    (false, 0b10100, _) => (v::I16X8_NARROW_I32X4_S, v::I32X4_EXTEND_LOW_I16X8_S),
                    (true, 0b10010, _) => (v::I16X8_NARROW_I32X4_U, v::I32X4_EXTEND_LOW_I16X8_U),
                    _ => return false,
                };
                // Result (low 8 bytes) in L_V0.
                self.vld(rn);
                self.vld(rn);
                self.f.v(narrow).local_tee(L_V0);
                self.f.v(widen);
                self.vld(rn);
                self.qc_if_differ(true);
                self.vst_begin();
                if q {
                    // XTN2: low half of Vd, then the result.
                    self.vld(rd);
                    self.f.local_get(L_V0);
                    self.f
                        .shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
                    self.vst_end(rd, true);
                } else {
                    self.f.local_get(L_V0);
                    self.vst_end(rd, false);
                }
                true
            }
            (false, 0b10010) => {
                // XTN/XTN2: the low half of each element from 2*esize.
                let eb = 1usize << size;
                let n = 8 / eb;
                let l = lanes(n, eb, |e| 2 * e);
                self.vst_begin();
                if q {
                    // XTN2: low half of Vd, then the results in the high half.
                    self.vld(rd);
                    self.vld(rn);
                    self.vld(rn);
                    self.f.shuffle(l);
                    self.f
                        .shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
                    self.vst_end(rd, true);
                } else {
                    self.vld(rn);
                    self.vld(rn);
                    self.f.shuffle(l);
                    self.vst_end(rd, false);
                }
                true
            }
            _ => false,
        }
    }

    fn across(&mut self, q: bool, u: bool, size: u8, opcode: u8, rn: u8, rd: u8) -> bool {
        let s = size as usize;
        let eb = 1usize << size;
        let n = (if q { 16 } else { 8 }) / eb;
        // Reduction operation and result size.
        let (o, res_bytes, widen) = match (u, opcode) {
            (_, 0b11011) => (ADD[s], eb, None),
            (false, 0b01010) => (MAX_S[s], eb, None),
            (true, 0b01010) => (MAX_U[s], eb, None),
            (false, 0b11010) => (MIN_S[s], eb, None),
            (true, 0b11010) => (MIN_U[s], eb, None),
            (_, 0b00011) if size <= 1 => {
                // [SU]ADDLV: first the widening pairwise sums, then the sums.
                let w = match (u, size) {
                    (false, 0) => v::I16X8_EXTADD_PAIRWISE_I8X16_S,
                    (true, 0) => v::I16X8_EXTADD_PAIRWISE_I8X16_U,
                    (false, _) => v::I32X4_EXTADD_PAIRWISE_I16X8_S,
                    (true, _) => v::I32X4_EXTADD_PAIRWISE_I16X8_U,
                };
                (ADD[s + 1], 2 * eb, Some(w))
            }
            _ => return false,
        };
        let Some(o) = o else { return false };
        self.vst_begin();
        self.vld(rn);
        if !q {
            if opcode == 0b11011 || opcode == 0b00011 {
                // Sums: high half zeroed.
                self.f.v128_const(u64::MAX, 0).v(v::AND);
            } else {
                // Maxima and minima: the low half repeated.
                self.f.local_tee(L_V0);
                self.f.local_get(L_V0);
                self.f.shuffle(core::array::from_fn(|j| (j % 8) as u8));
            }
        }
        let mut width = 16usize;
        let mut ebw = eb;
        if let Some(w) = widen {
            self.f.v(w);
            ebw = 2 * eb;
        }
        // Fold in half until one element remains.
        while width > ebw {
            width /= 2;
            self.f.local_tee(L_V0);
            self.f.local_get(L_V0);
            self.f.local_get(L_V0);
            let hw = width;
            self.f.shuffle(core::array::from_fn(|j| ((j % hw) + hw) as u8));
            self.f.v(o);
        }
        let _ = n;
        let m = if res_bytes == 8 { u64::MAX } else { (1u64 << (8 * res_bytes)) - 1 };
        self.f.v128_const(m, 0).v(v::AND);
        self.vst_end(rd, true);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn shift_imm(&mut self, q: bool, u: bool, esize: u32, shift: u32, opcode: u8, rn: u8, rd: u8) -> bool {
        let s = esize.trailing_zeros() as usize - 3;
        match (u, opcode) {
            (false, 0b01010) => {
                // SHL #shift (shift < esize)
                let Some(o) = SHL[s] else { return false };
                self.vst_begin();
                self.vld(rn);
                self.f.i32_const(shift as i32).v(o);
                self.vst_end(rd, q);
                true
            }
            (_, 0b00000 | 0b00010) => {
                // [SU]SHR, [SU]SRA #shift (1..=esize)
                let (Some(sh), Some(add)) = (if u { SHR_U[s] } else { SHR_S[s] }, ADD[s]) else {
                    return false;
                };
                self.vst_begin();
                if opcode == 0b00010 {
                    self.vld(rd);
                }
                if u && shift == esize {
                    self.f.v128_const(0, 0);
                } else {
                    self.vld(rn);
                    // SSHR #esize: like #(esize - 1), the sign everywhere.
                    self.f.i32_const(shift.min(esize - 1) as i32).v(sh);
                }
                if opcode == 0b00010 {
                    self.f.v(add);
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b10100) => {
                // [SU]SHLL(2) #shift, UXTL/SXTL: extension of the low
                // (or high) half and shift left.
                let ext = match (s, u, q) {
                    (0, false, false) => v::I16X8_EXTEND_LOW_I8X16_S,
                    (0, false, true) => v::I16X8_EXTEND_HIGH_I8X16_S,
                    (0, true, false) => v::I16X8_EXTEND_LOW_I8X16_U,
                    (0, true, true) => v::I16X8_EXTEND_HIGH_I8X16_U,
                    (1, false, false) => v::I32X4_EXTEND_LOW_I16X8_S,
                    (1, false, true) => v::I32X4_EXTEND_HIGH_I16X8_S,
                    (1, true, false) => v::I32X4_EXTEND_LOW_I16X8_U,
                    (1, true, true) => v::I32X4_EXTEND_HIGH_I16X8_U,
                    (2, false, false) => v::I64X2_EXTEND_LOW_I32X4_S,
                    (2, false, true) => v::I64X2_EXTEND_HIGH_I32X4_S,
                    (2, true, false) => v::I64X2_EXTEND_LOW_I32X4_U,
                    (2, true, true) => v::I64X2_EXTEND_HIGH_I32X4_U,
                    _ => return false,
                };
                let Some(shl) = SHL[s + 1] else { return false };
                self.vst_begin();
                self.vld(rn);
                self.f.v(ext);
                if shift != 0 {
                    self.f.i32_const(shift as i32).v(shl);
                }
                self.vst_end(rd, true);
                true
            }
            (false, 0b10000) => {
                // SHRN(2) #shift: esize is the destination; elements from 2*esize.
                let Some(sh) = SHR_U[s + 1] else { return false };
                let eb = 1usize << s;
                let l = lanes(8 / eb, eb, |e| 2 * e);
                self.vst_begin();
                if q {
                    self.vld(rd);
                }
                self.vld(rn);
                self.f.i32_const(shift as i32).v(sh);
                self.f.local_tee(L_V0);
                self.f.local_get(L_V0);
                self.f.shuffle(l);
                if q {
                    self.f
                        .shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
                }
                self.vst_end(rd, q);
                true
            }
            _ => false,
        }
    }

    /// LD1/ST1 of 1-4 integer registers, LD1R, LD1/ST1 of one lane (one
    /// structure: `selem` = 1), like `simd::ldst::exec`. The loads read
    /// everything before writing the registers (a fault leaves nothing
    /// changed: the interpreter redoes the instruction and gets its state); the
    /// stores write in order (redone by the interpreter, they rewrite the
    /// same bytes). 8- or 16-byte accesses instead of per element: same
    /// bytes, and where a wide access would fail and per-element ones would not
    /// (alignment) the interpreter decides. The writeback after the last
    /// access.
    pub(super) fn vec_struct(&mut self, m: VecMemInsn) {
        use vetro_cpu::simd::Post;
        let (rn, post) = match m {
            VecMemInsn::Multi { rn, post, .. } | VecMemInsn::Single { rn, post, .. } => (rn, post),
            other => unreachable!("not a structure: {other:?}"),
        };
        self.sp_check(rn);
        let was_ok = self.sp_ok;
        let (base, addr, newb) = (t64(4), t64(8), t64(6));
        self.get_xsp(rn);
        self.f.local_set(base);
        match post {
            Post::None => {}
            Post::Imm(n) => {
                self.f.local_get(base).i64_const(n as i64).op(op::I64_ADD).local_set(newb);
            }
            Post::Reg(rm) => {
                self.f.local_get(base);
                self.get_x(rm);
                self.f.op(op::I64_ADD).local_set(newb);
            }
        }
        let mut store = false;
        match m {
            VecMemInsn::Multi { load, q, selem, esize, rt, .. } if selem > 1 => {
                store = !load;
                self.vec_interleaved(load, q, selem as u32, esize as u32 / 8, rt, base, addr);
            }
            VecMemInsn::Multi { load, q, rpt, rt, .. } => {
                let bpr = if q { 16 } else { 8 };
                store = !load;
                if !load {
                    self.f.i32_const(0).local_set(t32(0));
                }
                for r in 0..rpt as u32 {
                    let reg = ((rt as u32 + r) % 32) as u8;
                    self.f.local_get(base);
                    if r > 0 {
                        self.f.i64_const((r * bpr) as i64).op(op::I64_ADD);
                    }
                    self.f.local_set(addr);
                    let tmp = L_V0 + 2 + r;
                    match (load, q) {
                        (true, true) => {
                            self.ld_q(addr);
                            self.f.local_set(t64(3)).local_set(t64(5));
                            self.f.v128_const(0, 0).local_get(t64(5)).lane(v::I64X2_REPLACE_LANE, 0);
                            self.f.local_get(t64(3)).lane(v::I64X2_REPLACE_LANE, 1).local_set(tmp);
                        }
                        (true, false) => {
                            self.ld(addr, 8);
                            self.f.local_set(t64(5));
                            self.f.v128_const(0, 0).local_get(t64(5)).lane(v::I64X2_REPLACE_LANE, 0);
                            self.f.local_set(tmp);
                        }
                        (false, true) => {
                            self.get_v(reg, false);
                            self.f.local_set(t64(7));
                            self.get_v(reg, true);
                            self.f.local_set(t64(3));
                            self.st_q(addr, t64(7), t64(3), Some(1));
                            self.f.local_get(t32(0)).local_get(t32(1)).op(op::I32_OR).local_set(t32(0));
                        }
                        (false, false) => {
                            self.get_v(reg, false);
                            self.f.local_set(t64(7));
                            self.st(addr, 8, t64(7), Some(1));
                            self.f.local_get(t32(0)).local_get(t32(1)).op(op::I32_OR).local_set(t32(0));
                        }
                    }
                }
                if load {
                    // All loads succeeded: now the registers.
                    for r in 0..rpt as u32 {
                        let reg = (rt as u32 + r) % 32;
                        self.vst_begin();
                        self.f.local_get(L_V0 + 2 + r).v128_store(off::V + 16 * reg);
                    }
                }
            }
            VecMemInsn::Single { load: true, q, selem, scale, index, replicate, rt, .. } if selem > 1 => {
                // LD2..LD4 of one lane, LD2R..LD4R (ADR 0045): element `s` at
                // base + s × bytes into V(rt + s). All the loads first (in
                // v128 temporaries), then the registers.
                let bytes = 1u32 << scale;
                for s in 0..selem as u32 {
                    self.f.local_get(base);
                    if s > 0 {
                        self.f.i64_const((s * bytes) as i64).op(op::I64_ADD);
                    }
                    self.f.local_set(addr);
                    self.ld(addr, bytes);
                    self.f.v(v::I64X2_SPLAT).local_set(L_V0 + 2 + s);
                }
                for s in 0..selem as u32 {
                    let reg = ((rt as u32 + s) % 32) as u8;
                    self.f.local_get(L_V0 + 2 + s).lane(v::I64X2_EXTRACT_LANE, 0).local_set(t64(5));
                    if replicate {
                        self.vst_begin();
                        self.f.local_get(t64(5));
                        if scale < 3 {
                            self.f.op(op::I32_WRAP_I64);
                        }
                        self.f.v(SPLAT[scale as usize].expect("all sizes"));
                        self.vst_end(reg, q);
                    } else {
                        self.v_insert(reg, index, 8 * bytes, t64(5));
                    }
                }
            }
            VecMemInsn::Single { load, q, scale, index, replicate, rt, .. } => {
                let bytes = 1u32 << scale;
                if load {
                    self.ld(base, bytes);
                    self.f.local_set(t64(5));
                    if replicate {
                        self.vst_begin();
                        self.f.local_get(t64(5));
                        if scale < 3 {
                            self.f.op(op::I32_WRAP_I64);
                        }
                        self.f.v(SPLAT[scale as usize].expect("all sizes"));
                        self.vst_end(rt, q);
                    } else {
                        self.v_insert(rt, index, 8 * bytes, t64(5));
                    }
                } else {
                    store = true;
                    self.v_elem(rt, index, 8 * bytes);
                    self.f.local_set(t64(7));
                    if post == Post::None {
                        // Single store: STOP right after.
                        self.st(base, bytes, t64(7), None);
                        store = false;
                    } else {
                        self.st(base, bytes, t64(7), Some(0));
                    }
                }
            }
            _ => unreachable!(),
        }
        if post != Post::None {
            self.f.local_get(newb);
            self.set_xsp(rn);
            if let Post::Imm(n) = post {
                self.sp_writeback(rn, was_ok, n as i64);
            }
        }
        if store {
            self.stop_after(&[0]);
        }
    }

    /// LD2..LD4/ST2..ST4 (multiple structures, `selem` registers from `rt`,
    /// elements of `eb` bytes): the memory in 16-byte (or 8-byte) blocks, and the
    /// registers are permutations of it (with Q = 0 half a register): byte
    /// `j` of element `e` of register `s` is at offset
    /// `(e × selem + s) × eb + j`. Load: all the accesses, then the registers;
    /// store: the blocks in order, in `t32(0)` if one asks for STOP.
    #[allow(clippy::too_many_arguments)]
    fn vec_interleaved(&mut self, load: bool, q: bool, selem: u32, eb: u32, rt: u8, base: u32, addr: u32) {
        let bpr = if q { 16 } else { 8 };
        let total = selem * bpr;
        let chunks = total.div_ceil(16);
        let chunk_l = |c: u32| L_V0 + 2 + c;
        let reg = |s: u32| ((rt as u32 + s) % 32) as u8;
        // Source of byte `j` (0..16) of a result made of 16-byte
        // pieces: `src(j)` = (piece, byte in the piece), or None (byte to zero).
        // Two levels of shuffle: pairs of pieces, then the two pairs.
        let gather =
            |t: &mut Tx, piece: &dyn Fn(u32) -> u32, n: u32, src: &dyn Fn(usize) -> Option<(u32, u32)>| {
                let pairs = n.div_ceil(2);
                for p in 0..pairs {
                    let (a, b) = (2 * p, (2 * p + 1).min(n - 1));
                    t.f.local_get(piece(a)).local_get(piece(b));
                    t.f.shuffle(core::array::from_fn(|j| match src(j) {
                        Some((c, k)) if c / 2 == p => ((c % 2) * 16 + k) as u8,
                        _ => 0,
                    }));
                }
                if pairs == 2 {
                    t.f.shuffle(core::array::from_fn(|j| match src(j) {
                        Some((c, _)) if c / 2 == 1 => (16 + j) as u8,
                        _ => j as u8,
                    }));
                }
            };
        if load {
            // 16-byte blocks (the last one of 8 with Q = 0 and odd selem).
            for c in 0..chunks {
                self.f.local_get(base);
                if c > 0 {
                    self.f.i64_const((16 * c) as i64).op(op::I64_ADD);
                }
                self.f.local_set(addr);
                if 16 * (c + 1) <= total {
                    self.ld_q(addr);
                    self.f.local_set(t64(3)).local_set(t64(5));
                } else {
                    self.ld(addr, 8);
                    self.f.local_set(t64(5)).i64_const(0).local_set(t64(3));
                }
                self.f.v128_const(0, 0).local_get(t64(5)).lane(v::I64X2_REPLACE_LANE, 0);
                self.f.local_get(t64(3)).lane(v::I64X2_REPLACE_LANE, 1).local_set(chunk_l(c));
            }
            // Register s: byte j (j < bpr) comes from offset
            // ((j / eb) × selem + s) × eb + j % eb.
            for s in 0..selem {
                let src = |j: usize| -> Option<(u32, u32)> {
                    let j = j as u32;
                    (j < bpr).then(|| {
                        let off = ((j / eb) * selem + s) * eb + j % eb;
                        (off / 16, off % 16)
                    })
                };
                gather(self, &chunk_l, chunks, &src);
                self.f.local_set(L_V0 + 1);
                // The register after all loads (in L_V0 + 2 + chunks + s there is
                // no room: it is written at the end, from the pieces still intact).
                self.vst_begin();
                self.f.local_get(L_V0 + 1);
                self.vst_end(reg(s), q);
            }
        } else {
            self.f.i32_const(0).local_set(t32(0));
            // The registers in the temporaries, then the blocks.
            for s in 0..selem {
                self.vld(reg(s));
                self.f.local_set(L_V0 + 2 + s);
            }
            for c in 0..chunks {
                // Byte j of block c: offset o = 16c + j, element m = o / eb
                // of register m % selem, byte (m / selem) × eb + o % eb.
                let src = |j: usize| -> Option<(u32, u32)> {
                    let o = 16 * c + j as u32;
                    (o < total).then(|| {
                        let m = o / eb;
                        (m % selem, (m / selem) * eb + o % eb)
                    })
                };
                gather(self, &|s| L_V0 + 2 + s, selem, &src);
                self.f.local_tee(L_V0).lane(v::I64X2_EXTRACT_LANE, 0).local_set(t64(7));
                self.f.local_get(L_V0).lane(v::I64X2_EXTRACT_LANE, 1).local_set(t64(3));
                self.f.local_get(base);
                if c > 0 {
                    self.f.i64_const((16 * c) as i64).op(op::I64_ADD);
                }
                self.f.local_set(addr);
                if 16 * (c + 1) <= total {
                    self.st_q(addr, t64(7), t64(3), Some(1));
                } else {
                    self.st(addr, 8, t64(7), Some(1));
                }
                self.f.local_get(t32(0)).local_get(t32(1)).op(op::I32_OR).local_set(t32(0));
            }
        }
    }

    /// DUP from element and from general register with v128 (the two most
    /// frequent cases of `CopyOp`, the others stay in `vec_int`).
    #[allow(dead_code)]
    fn splat_op(esize: u32) -> u32 {
        SPLAT[esize.trailing_zeros() as usize - 3].expect("all sizes")
    }
}
