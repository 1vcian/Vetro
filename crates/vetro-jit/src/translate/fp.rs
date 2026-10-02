//! Floating point in the regions (ADR 0026).
//!
//! Operations without rounding (FMOV, FABS, FNEG, FCSEL) are
//! inline. The others have a **fast path** in the runtime (`rt.fp<k>`):
//! the region calls the function with the instruction word, which reads the
//! registers from `JitState`, computes with WASM and writes the result only if
//! it is certainly the interpreter's (`vetro_cpu::simd::fp`):
//!
//! - FPCR = 0 (round to nearest even, no FZ or DN): it is
//!   WASM's IEEE 754 rounding, and input denormals
//!   count for what they are (with FZ Arm flushes them and signals IDC);
//! - no NaN in input or output (the bits of WASM NaNs are not
//!   fixed, and Arm propagates them with its own rules), no infinities produced
//!   by an overflow, no tiny results where Arm signals UFC
//!   (multiplications, divisions, FMA, narrowing conversions:
//!   result normal and greater than the smallest normal, because Arm
//!   checks tininess before rounding);
//! - IXC: if it is already 1 in FPSR (cumulative flag) inexactness changes
//!   nothing; otherwise the fast path applies only if the result is
//!   exact, verified exactly (TwoSum for sums; in single
//!   precision products, quotients and roots are rechecked in double,
//!   where they are exact); where it cannot be verified IXC must be 1.
//!
//! Otherwise the function calls `env.simd` (the interpreter, [`crate::helper`]):
//! same result, slower. Single-precision FMA is computed in
//! double with "round to odd" (Boldo and Melquiond): the
//! product is exact, and the sum rounded to odd in double, then to
//! even in single, gives the correctly rounded FMA.

use super::*;
use crate::wasm::{sat, v};
use vetro_cpu::simd::FpInsn;

/// Binary floating-point operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Bin {
    Add,
    Sub,
    Mul,
    Div,
    Max,
    Min,
    MaxNm,
    MinNm,
    /// FNMUL: -(a * b).
    Nmul,
    /// Vector FADDP: pairwise sums of concat(a, b).
    Addp,
    /// Vector FABD: |a - b|.
    Abd,
}

/// Where a scalar operation reads its operands (n, m, accumulator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Src {
    /// Element 0 of Vn, Vm, Va (bits 9:5, 20:16, 14:10).
    Regs,
    /// Elements 0 and 1 of Vn (scalar pairwise: FADDP, FMAXP...).
    Pair,
    /// Element 0 of Vn, Vm[index] (scalar by element), accumulator Vd.
    Elem,
}

/// Rounding to an integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rnd {
    Nearest,
    Ceil,
    Floor,
    Trunc,
    /// To nearest, ties away from zero (FRINTA, FCVTAS).
    Away,
    /// FRINTX: to nearest even, IXC if it changes.
    NearestX,
}

/// Vector comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cmp {
    Eq,
    Ge,
    Gt,
}

/// A runtime function `rt.fp<k>` (`d`: double precision).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FpRt {
    /// Scalar FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM, FNMUL.
    Bin {
        d: bool,
        op: Bin,
    },
    /// Scalar FADDP, FMAXP, FMINP, FMAXNMP, FMINNMP (`Src::Pair`) and scalar
    /// by-element FMUL (`Src::Elem`), ADR 0045.
    SBin {
        d: bool,
        op: Bin,
        src: Src,
    },
    /// Scalar by-element FMLA/FMLS (ADR 0045).
    SIdxFma {
        d: bool,
        neg: bool,
    },
    /// FMAXV, FMINV, FMAXNMV, FMINNMV (.4s, ADR 0045).
    Across {
        max: bool,
    },
    /// FMADD, FMSUB, FNMADD, FNMSUB in single precision.
    Fma {
        neg_a: bool,
        neg_n: bool,
    },
    Sqrt {
        d: bool,
    },
    /// FCMP/FCMPE (also with zero): returns NZCV.
    Cmp {
        d: bool,
    },
    /// FCVT from double to single and vice versa.
    CvtDS,
    CvtSD,
    Frint {
        d: bool,
        r: Rnd,
    },
    /// SCVTF/UCVTF from a general register (`sf`: 64 bits).
    FromInt {
        d: bool,
        sf: bool,
        u: bool,
    },
    /// FCVT[NPMZ][SU] to a general register.
    ToInt {
        d: bool,
        sf: bool,
        u: bool,
        r: Rnd,
    },
    /// Vector FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM (Q
    /// from the word).
    VBin {
        d: bool,
        op: Bin,
    },
    /// Vector FMLA/FMLS in single precision.
    VFma {
        neg: bool,
    },
    /// By-element FMUL; by-element FMLA/FMLS (single only).
    VIdxMul {
        d: bool,
    },
    VIdxFma {
        neg: bool,
    },
    /// FCMEQ, FCMGE, FCMGT with a register or with zero; `swap` compares
    /// (0, x) (FCMLE, FCMLT #0).
    VCmp {
        d: bool,
        op: Cmp,
        zero: bool,
        swap: bool,
    },
    VSqrt {
        d: bool,
    },
    /// FMADD and variants in double precision (emulated FMA, Boldo and
    /// Melquiond).
    FmaD {
        neg_a: bool,
        neg_n: bool,
    },
    /// Vector and by-element FMLA/FMLS in double precision.
    VFmaD {
        neg: bool,
        idx: bool,
    },
    /// Vector FCVT[NPMZA][SU].
    VToInt {
        d: bool,
        u: bool,
        r: Rnd,
    },
    /// Vector SCVTF/UCVTF.
    VFromInt {
        d: bool,
        u: bool,
    },
    /// FCVTL(2) and FCVTN(2) between single and double.
    VCvtl,
    VCvtn,
    /// FCVTL(2) and FCVTN(2) between half and single precision (ADR 0045).
    VCvtlH,
    VCvtnH,
    /// FCVT Sd, Hn and FCVT Hd, Sn.
    CvtSH,
    CvtHS,
    /// Vector FRINT[NPMZAXI] (ADR 0045).
    VFrint {
        d: bool,
        r: Rnd,
    },
}

/// All the `rt.fp<k>` functions, in index order (from `F_FP0`).
pub(super) fn rt_ops() -> &'static [FpRt] {
    static OPS: std::sync::OnceLock<Vec<FpRt>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        let mut v = Vec::new();
        let bins =
            [Bin::Add, Bin::Sub, Bin::Mul, Bin::Div, Bin::Max, Bin::Min, Bin::MaxNm, Bin::MinNm, Bin::Nmul];
        for d in [false, true] {
            for op in bins {
                v.push(FpRt::Bin { d, op });
            }
            v.push(FpRt::Sqrt { d });
            v.push(FpRt::Cmp { d });
            for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc, Rnd::NearestX, Rnd::Away] {
                v.push(FpRt::Frint { d, r });
            }
            for sf in [false, true] {
                for u in [false, true] {
                    v.push(FpRt::FromInt { d, sf, u });
                    for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc, Rnd::Away] {
                        v.push(FpRt::ToInt { d, sf, u, r });
                    }
                }
            }
            for op in bins[..8].iter().chain(&[Bin::Addp, Bin::Abd]) {
                v.push(FpRt::VBin { d, op: *op });
            }
            for u in [false, true] {
                v.push(FpRt::VFromInt { d, u });
                for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc, Rnd::Away] {
                    v.push(FpRt::VToInt { d, u, r });
                }
            }
            v.push(FpRt::VIdxMul { d });
            for op in [Cmp::Eq, Cmp::Ge, Cmp::Gt] {
                for (zero, swap) in [(false, false), (true, false), (true, true)] {
                    v.push(FpRt::VCmp { d, op, zero, swap });
                }
            }
            v.push(FpRt::VSqrt { d });
        }
        for neg_a in [false, true] {
            for neg_n in [false, true] {
                v.push(FpRt::Fma { neg_a, neg_n });
            }
        }
        for neg_a in [false, true] {
            for neg_n in [false, true] {
                v.push(FpRt::FmaD { neg_a, neg_n });
            }
        }
        for neg in [false, true] {
            for idx in [false, true] {
                v.push(FpRt::VFmaD { neg, idx });
            }
        }
        v.push(FpRt::VCvtl);
        v.push(FpRt::VCvtn);
        v.push(FpRt::CvtDS);
        v.push(FpRt::CvtSD);
        for neg in [false, true] {
            v.push(FpRt::VFma { neg });
            v.push(FpRt::VIdxFma { neg });
        }
        v.extend([FpRt::VCvtlH, FpRt::VCvtnH, FpRt::CvtSH, FpRt::CvtHS]);
        for d in [false, true] {
            for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc, Rnd::NearestX, Rnd::Away] {
                v.push(FpRt::VFrint { d, r });
            }
            for op in [Bin::Add, Bin::Max, Bin::Min, Bin::MaxNm, Bin::MinNm] {
                v.push(FpRt::SBin { d, op, src: Src::Pair });
            }
            v.push(FpRt::SBin { d, op: Bin::Mul, src: Src::Elem });
            for neg in [false, true] {
                v.push(FpRt::SIdxFma { d, neg });
            }
        }
        for max in [false, true] {
            v.push(FpRt::Across { max });
        }
        v
    })
}

/// Index in the runtime of function `op`.
pub(super) fn rt_id(op: FpRt) -> u32 {
    F_FP0 + rt_ops().iter().position(|o| *o == op).expect("known rt.fp function") as u32
}

/// Name and signature of function `k` of [`rt_ops`].
pub(super) fn rt_sig(k: usize) -> (String, Vec<ValType>, Vec<ValType>) {
    use ValType::*;
    let name = format!("fp{k}");
    match rt_ops()[k] {
        FpRt::Cmp { .. } => (name, vec![I32, I32], vec![I32]),
        FpRt::FromInt { .. } => (name, vec![I32, I32, I64], vec![]),
        FpRt::ToInt { .. } => (name, vec![I32, I32], vec![I64]),
        _ => (name, vec![I32, I32], vec![]),
    }
}

// --- in the regions ---------------------------------------------------

impl Tx {
    /// Index in the module of the `rt.fp<k>` function of `op` (imported after
    /// the fixed ones, in order of first use).
    fn rt_fp(&mut self, op_: FpRt) -> u32 {
        self.rt_opt(rt_id(op_))
    }

    /// FP instructions inline or with a runtime fast path; false if
    /// `env.simd` executes them.
    pub(super) fn fp_inline(&mut self, i: FpInsn) -> bool {
        let w = self.word as i32;
        let call = |t: &mut Tx, op_: FpRt| {
            t.simd = true;
            let f = t.rt_fp(op_);
            t.f.local_get(L_STATE).i32_const(w).call(f);
        };
        match i {
            FpInsn::Dp1 { ty: ty @ 0..=1, opcode: opcode @ 0..=2, rn, rd } => {
                // FMOV, FABS, FNEG: bits, without rounding (even on NaNs).
                let (mask, sign): (u64, u64) =
                    if ty == 0 { (0xffff_ffff, 0x8000_0000) } else { (u64::MAX, 1 << 63) };
                self.f.local_get(L_STATE);
                self.get_v(rn, false);
                self.f.i64_const(mask as i64).op(op::I64_AND);
                match opcode {
                    1 => {
                        self.f.i64_const(!sign as i64).op(op::I64_AND);
                    }
                    2 => {
                        self.f.i64_const(sign as i64).op(op::I64_XOR);
                    }
                    _ => {}
                }
                self.f.i64_store(Self::v_off(rd, false));
                self.set_v_const(rd, true, 0);
                true
            }
            FpInsn::Dp1 { ty, opcode: 3, .. } if ty <= 1 => {
                call(self, FpRt::Sqrt { d: ty == 1 });
                true
            }
            FpInsn::Dp1 { ty: 1, opcode: 4, .. } => {
                call(self, FpRt::CvtDS);
                true
            }
            FpInsn::Dp1 { ty: 0, opcode: 5, .. } => {
                call(self, FpRt::CvtSD);
                true
            }
            FpInsn::Dp1 { ty: 0, opcode: 7, .. } => {
                call(self, FpRt::CvtHS);
                true
            }
            FpInsn::Dp1 { ty: 3, opcode: 4, .. } => {
                call(self, FpRt::CvtSH);
                true
            }
            FpInsn::Dp1 { ty, opcode: opcode @ (8..=12 | 14 | 15), .. } if ty <= 1 => {
                let r = match opcode {
                    8 | 15 => Rnd::Nearest,
                    12 => Rnd::Away,
                    9 => Rnd::Ceil,
                    10 => Rnd::Floor,
                    11 => Rnd::Trunc,
                    _ => Rnd::NearestX,
                };
                call(self, FpRt::Frint { d: ty == 1, r });
                true
            }
            FpInsn::Dp2 { ty, opcode, .. } => {
                let op_ = match opcode {
                    0 => Bin::Mul,
                    1 => Bin::Div,
                    2 => Bin::Add,
                    3 => Bin::Sub,
                    4 => Bin::Max,
                    5 => Bin::Min,
                    6 => Bin::MaxNm,
                    7 => Bin::MinNm,
                    _ => Bin::Nmul,
                };
                call(self, FpRt::Bin { d: ty == 1, op: op_ });
                true
            }
            FpInsn::Dp3 { ty, neg_a, neg_n, .. } => {
                call(self, if ty == 0 { FpRt::Fma { neg_a, neg_n } } else { FpRt::FmaD { neg_a, neg_n } });
                true
            }
            FpInsn::Cmp { ty, .. } => {
                call(self, FpRt::Cmp { d: ty == 1 });
                self.set_nzcv();
                true
            }
            FpInsn::CondSel { ty, cond, rm, rn, rd } => {
                let mask: i64 = if ty == 0 { 0xffff_ffff } else { -1 };
                self.f.local_get(L_STATE);
                self.get_v(rn, false);
                self.f.i64_const(mask).op(op::I64_AND);
                self.get_v(rm, false);
                self.f.i64_const(mask).op(op::I64_AND);
                self.cond(cond);
                self.f.op(op::SELECT).i64_store(Self::v_off(rd, false));
                self.set_v_const(rd, true, 0);
                true
            }
            FpInsn::Imm { imm, rd, .. } => {
                self.set_v_const(rd, false, imm as i64);
                self.set_v_const(rd, true, 0);
                true
            }
            FpInsn::ToInt { ty, sf, unsigned, rounding, fbits: 0, rd, .. } => {
                use vetro_cpu::simd::fp::Rounding;
                let r = match rounding {
                    Rounding::TieEven => Rnd::Nearest,
                    Rounding::PosInf => Rnd::Ceil,
                    Rounding::NegInf => Rnd::Floor,
                    Rounding::Zero => Rnd::Trunc,
                    Rounding::TieAway => Rnd::Away,
                    _ => return false,
                };
                call(self, FpRt::ToInt { d: ty == 1, sf, u: unsigned, r });
                self.set_x(rd);
                true
            }
            FpInsn::FromInt { ty, sf, unsigned, fbits: 0, rn, .. } => {
                self.simd = true;
                let f = self.rt_fp(FpRt::FromInt { d: ty == 1, sf, u: unsigned });
                self.f.local_get(L_STATE).i32_const(w);
                self.get_x(rn);
                self.f.call(f);
                true
            }
            FpInsn::MovToGp { kind, rn, rd } => {
                let (hi, mask) = match kind {
                    vetro_cpu::simd::MovKind::W => (false, 0xffff_ffff),
                    vetro_cpu::simd::MovKind::X => (false, -1),
                    vetro_cpu::simd::MovKind::Top => (true, -1),
                };
                self.get_v(rn, hi);
                if mask != -1 {
                    self.f.i64_const(mask).op(op::I64_AND);
                }
                self.set_x(rd);
                true
            }
            FpInsn::MovFromGp { kind, rn, rd } => {
                self.get_x(rn);
                match kind {
                    vetro_cpu::simd::MovKind::W => {
                        self.f.i64_const(0xffff_ffff).op(op::I64_AND).local_set(t64(0));
                        self.set_v(rd, false, t64(0));
                        self.set_v_const(rd, true, 0);
                    }
                    vetro_cpu::simd::MovKind::X => {
                        self.f.local_set(t64(0));
                        self.set_v(rd, false, t64(0));
                        self.set_v_const(rd, true, 0);
                    }
                    vetro_cpu::simd::MovKind::Top => {
                        self.f.local_set(t64(0));
                        self.set_v(rd, true, t64(0));
                    }
                }
                true
            }
            FpInsn::VThreeSame { scalar: false, u, a, sz, opcode, .. } => {
                let d = sz;
                let op_ = match (u, a, opcode) {
                    (false, false, 0b11010) => FpRt::VBin { d, op: Bin::Add },
                    (false, true, 0b11010) => FpRt::VBin { d, op: Bin::Sub },
                    (true, false, 0b11011) => FpRt::VBin { d, op: Bin::Mul },
                    (true, false, 0b11111) => FpRt::VBin { d, op: Bin::Div },
                    (false, false, 0b11110) => FpRt::VBin { d, op: Bin::Max },
                    (false, true, 0b11110) => FpRt::VBin { d, op: Bin::Min },
                    (false, false, 0b11000) => FpRt::VBin { d, op: Bin::MaxNm },
                    (false, true, 0b11000) => FpRt::VBin { d, op: Bin::MinNm },
                    (false, neg, 0b11001) if !d => FpRt::VFma { neg },
                    (false, neg, 0b11001) => FpRt::VFmaD { neg, idx: false },
                    (false, false, 0b11100) => FpRt::VCmp { d, op: Cmp::Eq, zero: false, swap: false },
                    (true, false, 0b11100) => FpRt::VCmp { d, op: Cmp::Ge, zero: false, swap: false },
                    (true, true, 0b11100) => FpRt::VCmp { d, op: Cmp::Gt, zero: false, swap: false },
                    (true, false, 0b11010) => FpRt::VBin { d, op: Bin::Addp },
                    (true, true, 0b11010) => FpRt::VBin { d, op: Bin::Abd },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            FpInsn::VTwoMisc { scalar: false, u, a, sz, opcode, .. }
                if matches!(
                    (a, opcode),
                    (false, 0b10110 | 0b10111 | 0b11010 | 0b11011 | 0b11100 | 0b11101)
                ) || (a && matches!(opcode, 0b11010 | 0b11011)) =>
            {
                let d = sz;
                let op_ = match (u, a, opcode) {
                    (false, false, 0b10110) if d => FpRt::VCvtn,
                    (false, false, 0b10111) if d => FpRt::VCvtl,
                    (false, false, 0b10110) => FpRt::VCvtnH,
                    (false, false, 0b10111) => FpRt::VCvtlH,
                    (_, false, 0b11010) => FpRt::VToInt { d, u, r: Rnd::Nearest },
                    (_, true, 0b11010) => FpRt::VToInt { d, u, r: Rnd::Ceil },
                    (_, false, 0b11011) => FpRt::VToInt { d, u, r: Rnd::Floor },
                    (_, true, 0b11011) => FpRt::VToInt { d, u, r: Rnd::Trunc },
                    (_, false, 0b11100) => FpRt::VToInt { d, u, r: Rnd::Away },
                    (_, false, 0b11101) => FpRt::VFromInt { d, u },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            FpInsn::VTwoMisc { scalar: false, u, a, sz, opcode: opcode @ (0b11000 | 0b11001), .. } => {
                // FRINTN/P/M/Z/A/X/I (I and X with FPCR's rounding, which
                // the fast path requires to be to nearest).
                let r = match (u, a, opcode) {
                    (false, false, 0b11000) => Rnd::Nearest,
                    (false, true, 0b11000) => Rnd::Ceil,
                    (false, false, 0b11001) => Rnd::Floor,
                    (false, true, 0b11001) => Rnd::Trunc,
                    (true, false, 0b11000) => Rnd::Away,
                    (true, false, 0b11001) => Rnd::NearestX,
                    (true, true, 0b11001) => Rnd::Nearest,
                    _ => return false,
                };
                call(self, FpRt::VFrint { d: sz, r });
                true
            }
            FpInsn::VTwoMisc { scalar: false, q, u, a: true, sz, opcode, rn, rd } => {
                let d = sz;
                let op_ = match (u, opcode) {
                    (_, 0b01111) => {
                        // Vector FABS / FNEG: bits.
                        self.vst_begin();
                        self.vld(rn);
                        self.f.v(match (u, d) {
                            (false, false) => v::F32X4_ABS,
                            (false, true) => v::F64X2_ABS,
                            (true, false) => v::F32X4_NEG,
                            (true, true) => v::F64X2_NEG,
                        });
                        self.vst_end(rd, q);
                        return true;
                    }
                    (true, 0b11111) => FpRt::VSqrt { d },
                    (false, 0b01100) => FpRt::VCmp { d, op: Cmp::Gt, zero: true, swap: false },
                    (true, 0b01100) => FpRt::VCmp { d, op: Cmp::Ge, zero: true, swap: false },
                    (false, 0b01101) => FpRt::VCmp { d, op: Cmp::Eq, zero: true, swap: false },
                    (true, 0b01101) => FpRt::VCmp { d, op: Cmp::Ge, zero: true, swap: true },
                    (false, 0b01110) => FpRt::VCmp { d, op: Cmp::Gt, zero: true, swap: true },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            FpInsn::VPairScalar { a, sz, opcode, .. } => {
                let op_ = match (a, opcode) {
                    (false, 0b01101) => Bin::Add,
                    (false, 0b01100) => Bin::MaxNm,
                    (true, 0b01100) => Bin::MinNm,
                    (false, 0b01111) => Bin::Max,
                    (true, 0b01111) => Bin::Min,
                    _ => return false,
                };
                call(self, FpRt::SBin { d: sz, op: op_, src: Src::Pair });
                true
            }
            FpInsn::VAcross { a, .. } => {
                call(self, FpRt::Across { max: !a });
                true
            }
            FpInsn::VIndexed { scalar: true, u: false, sz, opcode, .. } => {
                let op_ = match opcode {
                    0b1001 => FpRt::SBin { d: sz, op: Bin::Mul, src: Src::Elem },
                    0b0001 => FpRt::SIdxFma { d: sz, neg: false },
                    0b0101 => FpRt::SIdxFma { d: sz, neg: true },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            FpInsn::VIndexed { scalar: false, u: false, sz, opcode, .. } => {
                let op_ = match opcode {
                    0b1001 => FpRt::VIdxMul { d: sz },
                    0b0001 if !sz => FpRt::VIdxFma { neg: false },
                    0b0101 if !sz => FpRt::VIdxFma { neg: true },
                    0b0001 => FpRt::VFmaD { neg: false, idx: true },
                    0b0101 => FpRt::VFmaD { neg: true, idx: true },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            _ => false,
        }
    }
}

// --- building the runtime functions -----------------------------------

const S_INF: u32 = 0x7f80_0000;
const S_MIN_NORMAL: u32 = 0x0080_0000;
const D_INF: u64 = 0x7ff0_0000_0000_0000;
const D_MIN_NORMAL: u64 = 0x0010_0000_0000_0000;
/// 2^-126 (the smallest normal single) as a double.
const S_MIN_NORMAL_AS_D: u64 = 0x3810_0000_0000_0000;
const IXC: i32 = 0x10;
/// FPCR.RMode (bits 23:22).
const FPCR_RMODE: i32 = 3 << 22;

/// Function parameters: state, word, [x].
const P_STATE: u32 = 0;
const P_WORD: u32 = 1;
const P_X: u32 = 2;

/// Generator of an `rt.fp<k>` function: `simd` is the index of
/// `env.simd` in the runtime.
struct G {
    f: Func,
    simd: u32,
    /// Next local variable (after the parameters).
    next: u32,
    locals: Vec<(u32, ValType)>,
}

impl G {
    fn new(simd: u32, params: u32) -> G {
        G { f: Func::default(), simd, next: params, locals: Vec::new() }
    }

    fn local(&mut self, t: ValType) -> u32 {
        self.locals.push((1, t));
        self.next += 1;
        self.next - 1
    }

    fn finish(mut self) -> Func {
        self.f.locals = self.locals;
        self.f
    }

    /// Address (i32) of `JitState` plus 16 × the register in the 5-bit
    /// field of the word starting at bit `shift`: with offset `off::V` it is the
    /// register.
    fn vaddr(&mut self, shift: i32) {
        let f = &mut self.f;
        f.local_get(P_WORD);
        if shift != 0 {
            f.i32_const(shift).op(op::I32_SHR_U);
        }
        f.i32_const(31).op(op::I32_AND).i32_const(4).op(op::I32_SHL).local_get(P_STATE).op(op::I32_ADD);
    }

    /// Element 0 of the register (field at `shift`) as f32/f64.
    fn load(&mut self, shift: i32, d: bool) {
        self.vaddr(shift);
        if d {
            self.f.f64_load(off::V);
        } else {
            self.f.f32_load(off::V);
        }
    }

    /// Operand `k` (0 = n, 1 = m, 2 = accumulator) of a scalar operation
    /// reading from `src`, as f32/f64.
    fn operand(&mut self, src: Src, k: u8, d: bool) {
        let size: u32 = if d { 8 } else { 4 };
        match (src, k) {
            (Src::Regs, 0) | (Src::Pair, 0) | (Src::Elem, 0) => self.load(5, d),
            (Src::Regs, 1) => self.load(16, d),
            (Src::Regs, _) => self.load(10, d),
            (Src::Pair, _) => {
                self.vaddr(5);
                if d {
                    self.f.f64_load(off::V + size);
                } else {
                    self.f.f32_load(off::V + size);
                }
            }
            (Src::Elem, 1) => {
                // Vm (M:Rm) + index × size: index H:L (single) or H (double)
                self.vaddr(16);
                let f = &mut self.f;
                f.local_get(P_WORD).i32_const(11).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
                if !d {
                    f.i32_const(1).op(op::I32_SHL);
                    f.local_get(P_WORD)
                        .i32_const(21)
                        .op(op::I32_SHR_U)
                        .i32_const(1)
                        .op(op::I32_AND)
                        .op(op::I32_OR);
                }
                f.i32_const(size.trailing_zeros() as i32).op(op::I32_SHL).op(op::I32_ADD);
                if d {
                    f.f64_load(off::V);
                } else {
                    f.f32_load(off::V);
                }
            }
            (Src::Elem, _) => self.load(0, d),
        }
    }

    /// Operand `k` of `src` is a denormal (i32 boolean).
    fn den_op(&mut self, src: Src, k: u8, d: bool) {
        let t = self.local(if d { ValType::F64 } else { ValType::F32 });
        self.operand(src, k, d);
        self.f.local_set(t);
        self.den(t, d);
    }

    /// The register (field at `shift`) as v128.
    fn vload(&mut self, shift: i32) {
        self.vaddr(shift);
        self.f.v128_load(off::V);
    }

    /// Vd = the bits (i64, zero-extended) in variable `bits`, high half
    /// zeroed (scalar result).
    fn store_scalar_bits(&mut self, bits: u32) {
        self.vaddr(0);
        self.f.local_get(bits).i64_store(off::V);
        self.vaddr(0);
        self.f.i64_const(0).i64_store(off::V + 8);
    }

    /// Vd = the v128 in variable `r`, high half zeroed with Q = 0.
    fn store_vec(&mut self, r: u32) {
        self.vaddr(0);
        self.f.local_get(r);
        self.himask();
        self.f.v(v::ANDNOT);
        self.f.v128_store(off::V);
    }

    /// Q (bit 30 of the word, i32).
    fn q(&mut self) {
        self.f.local_get(P_WORD).i32_const(30).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
    }

    /// v128 with the high half all ones if Q = 0, otherwise zero: the lanes to
    /// ignore in the checks.
    fn himask(&mut self) {
        self.q();
        self.f.op(op::I64_EXTEND_I32_U).i64_const(1).op(op::I64_SUB).lane_splat64();
        self.f.v128_const(0, u64::MAX).v(v::AND);
    }

    /// FPCR.RMode = round to nearest (i32 boolean). The other FPCR bits do
    /// not change a fast path's result: DN only changes NaN results (no fast
    /// path writes one), AHP and FZ16 only half precision, and FZ only
    /// denormal inputs and tiny results, which every function excludes with
    /// [`G::fz_guard`] when FZ is set (or by its own range checks).
    fn fpcr_ok(&mut self) {
        self.f.local_get(P_STATE).i32_load(off::FPCR).i32_const(FPCR_RMODE).op(op::I32_AND).op(op::I32_EQZ);
    }

    /// FPCR.FZ (i32 boolean).
    fn fz(&mut self) {
        self.f.local_get(P_STATE).i32_load(off::FPCR).i32_const(24).op(op::I32_SHR_U);
        self.f.i32_const(1).op(op::I32_AND);
    }

    /// `ok` &= !(FZ && `den`), where `den` pushes an i32 boolean "a denormal
    /// is read or written" (with FZ Arm flushes it to zero and signals IDC or
    /// UFC).
    fn fz_guard(&mut self, ok: u32, den: impl FnOnce(&mut G)) {
        self.fz();
        self.f.if_(BLOCK_EMPTY);
        den(self);
        self.f.op(op::I32_EQZ).local_get(ok).op(op::I32_AND).local_set(ok);
        self.f.end();
    }

    /// The float in `l` is a denormal (i32 boolean): |x| < smallest normal
    /// and x != 0 (false for NaN).
    fn den(&mut self, l: u32, d: bool) {
        self.f.local_get(l).op(fop(d, op::F32_ABS, op::F64_ABS));
        if d {
            self.f.i64_const(D_MIN_NORMAL as i64).op(op::F64_REINTERPRET_I64).op(op::F64_LT);
        } else {
            self.f.i32_const(S_MIN_NORMAL as i32).op(op::F32_REINTERPRET_I32).op(op::F32_LT);
        }
        self.f.local_get(l);
        self.fzero(d);
        self.f.op(fop(d, op::F32_NE, op::F64_NE)).op(op::I32_AND);
    }

    /// Element 0 of the register (field at `shift`) is a denormal.
    fn den_reg(&mut self, shift: i32, d: bool) {
        let t = self.local(if d { ValType::F64 } else { ValType::F32 });
        self.load(shift, d);
        self.f.local_set(t);
        self.den(t, d);
    }

    /// Some lane of the v128 in `l` that counts (Q) is a denormal (i32).
    fn vden(&mut self, l: u32, d: bool) {
        self.f.local_get(l).v(vop(d, v::F32X4_ABS, v::F64X2_ABS));
        splat_const(self, d, if d { D_MIN_NORMAL } else { S_MIN_NORMAL as u64 });
        self.f.v(vop(d, v::F32X4_LT, v::F64X2_LT));
        self.f.local_get(l).v128_const(0, 0).v(vop(d, v::F32X4_NE, v::F64X2_NE)).v(v::AND);
        self.himask();
        self.f.v(v::ANDNOT).v(v::ANY_TRUE);
    }

    /// IXC already 1 in FPSR (i32 boolean, 0 or 1: combines with AND).
    fn ixc(&mut self) {
        self.f.local_get(P_STATE).i32_load(off::FPSR).i32_const(IXC.trailing_zeros() as i32);
        self.f.op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
    }

    /// FPSR.IXC |= the i32 boolean in `inexact` (the fast path knows
    /// exactly whether its result is inexact).
    fn raise_ixc(&mut self, inexact: u32) {
        self.f.local_get(inexact).if_(BLOCK_EMPTY);
        self.f.local_get(P_STATE).local_get(P_STATE).i32_load(off::FPSR).i32_const(IXC).op(op::I32_OR);
        self.f.i32_store(off::FPSR);
        self.f.end();
    }

    /// FPSR.IXC |= !`exact` (an i32 boolean the fast path computed exactly).
    fn raise_unless(&mut self, exact: u32) {
        self.f.local_get(exact).op(op::I32_EQZ).if_(BLOCK_EMPTY);
        self.f.local_get(P_STATE).local_get(P_STATE).i32_load(off::FPSR).i32_const(IXC).op(op::I32_OR);
        self.f.i32_store(off::FPSR);
        self.f.end();
    }

    /// `env.simd` (the interpreter) with `x` and NZCV = 0; the result (i64)
    /// stays on the stack.
    fn fallback(&mut self, x: bool) {
        self.f.local_get(P_STATE).local_get(P_WORD);
        if x {
            self.f.local_get(P_X);
        } else {
            self.f.i64_const(0);
        }
        self.f.i32_const(0).call(self.simd);
    }

    /// Bits (i64) of the float in `l` (f32 zero-extended, or f64).
    fn bits64(&mut self, l: u32, d: bool) {
        self.f.local_get(l);
        if d {
            self.f.op(op::I64_REINTERPRET_F64);
        } else {
            self.f.op(op::I32_REINTERPRET_F32).op(op::I64_EXTEND_I32_U);
        }
    }

    /// Bits (i64) on top of the stack: finite (neither NaN nor infinity).
    fn finite_bits(&mut self, d: bool) {
        let (m, inf) = if d { (i64::MAX, D_INF as i64) } else { (0x7fff_ffff, S_INF as i64) };
        self.f.i64_const(m).op(op::I64_AND).i64_const(inf).op(op::I64_LT_U);
    }

    /// Bits (i64) on top of the stack: normal and greater than the smallest
    /// normal (no UFC or OFC).
    fn safe_bits(&mut self, d: bool) {
        let (m, lo, inf) = if d {
            (i64::MAX, D_MIN_NORMAL as i64 + 1, D_INF as i64)
        } else {
            (0x7fff_ffff, S_MIN_NORMAL as i64 + 1, S_INF as i64)
        };
        self.f.i64_const(m).op(op::I64_AND).i64_const(lo).op(op::I64_SUB);
        self.f.i64_const(inf - lo).op(op::I64_LT_U);
    }

    /// Not NaN (i32) of the float in `l`.
    fn not_nan(&mut self, l: u32, d: bool) {
        self.f.local_get(l).local_get(l).op(fop(d, op::F32_EQ, op::F64_EQ));
    }

    /// Float constant 0 of the type.
    fn fzero(&mut self, d: bool) {
        if d {
            self.f.i64_const(0).op(op::F64_REINTERPRET_I64);
        } else {
            self.f.i32_const(0).op(op::F32_REINTERPRET_I32);
        }
    }

    /// Closes a function without a result: if `ok` writes (with `store`) and
    /// returns, otherwise `env.simd`.
    fn commit(&mut self, ok: u32, store: impl FnOnce(&mut G)) {
        self.f.local_get(ok).if_(BLOCK_EMPTY);
        store(self);
        self.f.op(op::RETURN).end();
    }
}

trait SplatExt {
    fn lane_splat64(&mut self) -> &mut Self;
}

impl SplatExt for Func {
    /// `i64x2.splat`.
    fn lane_splat64(&mut self) -> &mut Self {
        self.v(v::I64X2_SPLAT)
    }
}

fn fop(d: bool, s: u8, dd: u8) -> u8 {
    if d { dd } else { s }
}

fn vop(d: bool, s: u32, dd: u32) -> u32 {
    if d { dd } else { s }
}

/// Builds function `k` of [`rt_ops`].
pub(super) fn build(k: usize, simd: u32) -> Func {
    match rt_ops()[k] {
        FpRt::Bin { d, op } => bin(simd, d, op, Src::Regs),
        FpRt::SBin { d, op, src } => bin(simd, d, op, src),
        FpRt::SIdxFma { d: false, neg } => fma_s(simd, false, neg, Src::Elem),
        FpRt::SIdxFma { d: true, neg } => fma_d(simd, false, neg, Src::Elem),
        FpRt::Across { max } => across(simd, max),
        FpRt::Fma { neg_a, neg_n } => fma_s(simd, neg_a, neg_n, Src::Regs),
        FpRt::Sqrt { d } => sqrt(simd, d),
        FpRt::Cmp { d } => cmp(simd, d),
        FpRt::CvtDS => cvt_ds(simd),
        FpRt::CvtSD => cvt_sd(simd),
        FpRt::Frint { d, r } => frint(simd, d, r),
        FpRt::FromInt { d, sf, u } => from_int(simd, d, sf, u),
        FpRt::ToInt { d, sf, u, r } => to_int(simd, d, sf, u, r),
        FpRt::VBin { d, op } => vbin(simd, d, op),
        FpRt::VFma { neg } => vfma(simd, neg, false),
        FpRt::VIdxMul { d } => vidx_mul(simd, d),
        FpRt::VIdxFma { neg } => vfma(simd, neg, true),
        FpRt::VCmp { d, op, zero, swap } => vcmp(simd, d, op, zero, swap),
        FpRt::VSqrt { d } => vsqrt(simd, d),
        FpRt::FmaD { neg_a, neg_n } => fma_d(simd, neg_a, neg_n, Src::Regs),
        FpRt::VFmaD { neg, idx } => vfma_d(simd, neg, idx),
        FpRt::VToInt { d, u, r } => vto_int(simd, d, u, r),
        FpRt::VFromInt { d, u } => vfrom_int(simd, d, u),
        FpRt::VCvtl => vcvtl(simd),
        FpRt::VCvtn => vcvtn(simd),
        FpRt::VCvtlH => cvt_h2s(simd, true),
        FpRt::VCvtnH => cvt_s2h(simd, true),
        FpRt::CvtSH => cvt_h2s(simd, false),
        FpRt::CvtHS => cvt_s2h(simd, false),
        FpRt::VFrint { d, r } => vfrint(simd, d, r),
    }
}

/// TwoSum: in `err` the exact error (a + b) - s, with s = a + b already
/// rounded (Knuth). All f32 or f64 (`d`).
fn two_sum(g: &mut G, d: bool, a: u32, b: u32, s: u32, err: u32) {
    let (add, sub) = (fop(d, op::F32_ADD, op::F64_ADD), fop(d, op::F32_SUB, op::F64_SUB));
    let t = g.local(if d { ValType::F64 } else { ValType::F32 });
    let f = &mut g.f;
    // t = s - a; err = (a - (s - t)) + (b - t)
    f.local_get(s).local_get(a).op(sub).local_set(t);
    f.local_get(a).local_get(s).local_get(t).op(sub).op(sub);
    f.local_get(b).local_get(t).op(sub).op(add).local_set(err);
}

/// Scalar FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM, FNMUL.
fn bin(simd: u32, d: bool, op_: Bin, src: Src) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, b, r) = (g.local(ft), g.local(ft), g.local(ft));
    let (bits, ok) = (g.local(ValType::I64), g.local(ValType::I32));
    // Exact result (i32): where the fast path knows it, it raises IXC itself.
    let ex = g.local(ValType::I32);
    g.f.i32_const(1).local_set(ex);
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.operand(src, 0, d);
    g.f.local_set(a);
    g.operand(src, 1, d);
    if op_ == Bin::Sub {
        // a - b = a + (-b), also for zeros.
        g.f.op(fop(d, op::F32_NEG, op::F64_NEG));
    }
    g.f.local_set(b);
    g.f.local_get(a).local_get(b);
    g.f.op(match op_ {
        Bin::Add | Bin::Sub | Bin::Addp | Bin::Abd => fop(d, op::F32_ADD, op::F64_ADD),
        Bin::Mul | Bin::Nmul => fop(d, op::F32_MUL, op::F64_MUL),
        Bin::Div => fop(d, op::F32_DIV, op::F64_DIV),
        Bin::Max | Bin::MaxNm => fop(d, op::F32_MAX, op::F64_MAX),
        Bin::Min | Bin::MinNm => fop(d, op::F32_MIN, op::F64_MIN),
    });
    g.f.local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    match op_ {
        // FADDP and FABD exist only as vector instructions.
        Bin::Addp | Bin::Abd => unreachable!("{op_:?} scalar"),
        Bin::Add | Bin::Sub => {
            // Finite; exact if TwoSum's error is zero (otherwise IXC). A sum
            // never gives an inexact tiny result.
            let err = g.local(ft);
            two_sum(&mut g, d, a, b, r, err);
            g.f.local_get(bits);
            g.finite_bits(d);
            g.f.local_set(ok);
            g.f.local_get(err);
            g.fzero(d);
            g.f.op(fop(d, op::F32_EQ, op::F64_EQ)).local_set(ex);
        }
        Bin::Max | Bin::Min | Bin::MaxNm | Bin::MinNm => {
            // No NaN: no flags, and zeros of opposite sign like
            // Arm (max +0, min -0).
            g.not_nan(a, d);
            g.not_nan(b, d);
            g.f.op(op::I32_AND).local_set(ok);
        }
        Bin::Mul | Bin::Nmul | Bin::Div => {
            // Safely normal, or (products) exact zero with a zero factor.
            g.f.local_get(bits);
            g.safe_bits(d);
            if op_ != Bin::Div {
                g.f.local_get(bits).i64_const(if d { i64::MAX } else { 0x7fff_ffff }).op(op::I64_AND);
                g.f.op(op::I64_EQZ);
                g.f.local_get(a);
                g.fzero(d);
                g.f.op(fop(d, op::F32_EQ, op::F64_EQ));
                g.f.local_get(b);
                g.fzero(d);
                g.f.op(fop(d, op::F32_EQ, op::F64_EQ)).op(op::I32_OR).op(op::I32_AND).op(op::I32_OR);
            }
            // Single precision: exact or not, checked in double (then IXC
            // is raised here); double: IXC already 1.
            if d {
                g.ixc();
            } else {
                let f = &mut g.f;
                if op_ == Bin::Div {
                    // f64(r) * f64(b) == f64(a): 24 + 24 bits, exact.
                    f.local_get(r)
                        .op(op::F64_PROMOTE_F32)
                        .local_get(b)
                        .op(op::F64_PROMOTE_F32)
                        .op(op::F64_MUL);
                    f.local_get(a).op(op::F64_PROMOTE_F32).op(op::F64_EQ);
                } else {
                    // f64(r) == f64(a) * f64(b)
                    f.local_get(r).op(op::F64_PROMOTE_F32);
                    f.local_get(a)
                        .op(op::F64_PROMOTE_F32)
                        .local_get(b)
                        .op(op::F64_PROMOTE_F32)
                        .op(op::F64_MUL);
                    f.op(op::F64_EQ);
                }
                f.local_set(ex).i32_const(1);
            }
            g.f.op(op::I32_AND).local_set(ok);
        }
    }
    g.fz_guard(ok, |g| {
        g.den(a, d);
        g.den(b, d);
        g.f.op(op::I32_OR);
        if matches!(op_, Bin::Add | Bin::Sub) {
            g.den(r, d);
            g.f.op(op::I32_OR);
        }
    });
    g.commit(ok, |g| {
        if op_ == Bin::Nmul {
            // The sign is changed after rounding.
            g.f.local_get(bits)
                .i64_const(if d { i64::MIN } else { 0x8000_0000 })
                .op(op::I64_XOR)
                .local_set(bits);
        }
        g.raise_unless(ex);
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// "Round to odd" of `s` (f64, variable) given the exact error
/// `err`: if inexact and even, the odd neighbour on the side of the error.
/// Leaves the bits (i64) on the stack.
fn round_odd(g: &mut G, s: u32, err: u32) {
    let sb = g.local(ValType::I64);
    let f = &mut g.f;
    f.local_get(s).op(op::I64_REINTERPRET_F64).local_tee(sb);
    // delta = (sb ^ bits(err)) >= 0 ? 1 : -1
    f.i64_const(1).i64_const(-1);
    f.local_get(sb).local_get(err).op(op::I64_REINTERPRET_F64).op(op::I64_XOR).i64_const(0).op(op::I64_GE_S);
    f.op(op::SELECT);
    f.i64_const(0);
    // inexact and even
    f.local_get(err).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_NE);
    f.local_get(sb).i64_const(1).op(op::I64_AND).op(op::I64_EQZ).op(op::I32_AND);
    f.op(op::SELECT).op(op::I64_ADD);
}

/// FMADD/FMSUB/FNMADD/FNMSUB in single precision: fused (±a) + (±n) × m.
fn fma_s(simd: u32, neg_a: bool, neg_n: bool, src: Src) -> Func {
    let mut g = G::new(simd, 2);
    let (p, c, s, err) =
        (g.local(ValType::F64), g.local(ValType::F64), g.local(ValType::F64), g.local(ValType::F64));
    let (r, bits, ok) = (g.local(ValType::F32), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    // p = f64(±n) * f64(m), exact
    g.operand(src, 0, false);
    if neg_n {
        g.f.op(op::F32_NEG);
    }
    g.f.op(op::F64_PROMOTE_F32);
    g.operand(src, 1, false);
    g.f.op(op::F64_PROMOTE_F32).op(op::F64_MUL).local_set(p);
    g.operand(src, 2, false);
    if neg_a {
        g.f.op(op::F32_NEG);
    }
    g.f.op(op::F64_PROMOTE_F32).local_set(c);
    g.f.local_get(p).local_get(c).op(op::F64_ADD).local_set(s);
    two_sum(&mut g, true, p, c, s, err);
    round_odd(&mut g, s, err);
    g.f.op(op::F64_REINTERPRET_I64).op(op::F32_DEMOTE_F64).local_set(r);
    g.bits64(r, false);
    g.f.local_tee(bits);
    g.safe_bits(false);
    g.f.local_set(ok);
    // exact: err == 0 and f64(r) == s (otherwise IXC, raised here)
    let ex = g.local(ValType::I32);
    g.f.local_get(err).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_EQ);
    g.f.local_get(r).op(op::F64_PROMOTE_F32).local_get(s).op(op::F64_EQ).op(op::I32_AND).local_set(ex);
    g.fz_guard(ok, |g| {
        g.den_op(src, 0, false);
        g.den_op(src, 1, false);
        g.f.op(op::I32_OR);
        g.den_op(src, 2, false);
        g.f.op(op::I32_OR);
    });
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Exponent (11-bit field) of the f64 bits `x` in the interval [1023 + lo,
/// 1023 + hi) (i32 boolean): no overflow or tininess in the exact steps
/// of the emulated FMA.
fn exp_in(g: &mut G, x: u32, lo: i64, hi: i64) {
    let f = &mut g.f;
    f.local_get(x)
        .op(op::I64_REINTERPRET_F64)
        .i64_const(52)
        .op(op::I64_SHR_U)
        .i64_const(0x7ff)
        .op(op::I64_AND);
    f.i64_const(1023 + lo).op(op::I64_SUB).i64_const(hi - lo).op(op::I64_LT_U);
}

/// FMA emulated in double (Boldo and Melquiond, "Emulation of FMA and
/// correctly rounded sums: proved algorithms using rounding to odd", 2008):
/// (uh, ul) = a × b exact (Dekker, Veltkamp splitting); (th, tl) =
/// c + uh exact (TwoSum); v = tl + ul rounded to odd; z = th + v to
/// even. Valid without overflows or tininess, guaranteed by `exp_in` on a, b
/// (|x| in [2^-400, 2^400)) and c (zero or in [2^-800, 2^800)). Leaves z
/// (f64) in variable `z`.
fn emulated_fma(g: &mut G, a: u32, b: u32, c: u32, z: u32) {
    use ValType::F64;
    let (g_, ah, al, bh, bl) = (g.local(F64), g.local(F64), g.local(F64), g.local(F64), g.local(F64));
    let (uh, ul, th, tl, s2, e2) =
        (g.local(F64), g.local(F64), g.local(F64), g.local(F64), g.local(F64), g.local(F64));
    let split = (134_217_729.0f64).to_bits() as i64; // 2^27 + 1
    let fc = |f: &mut Func, bits: i64| {
        f.i64_const(bits).op(op::F64_REINTERPRET_I64);
    };
    for (x, hi, lo) in [(a, ah, al), (b, bh, bl)] {
        let f = &mut g.f;
        fc(f, split);
        f.local_get(x).op(op::F64_MUL).local_set(g_);
        f.local_get(g_).local_get(g_).local_get(x).op(op::F64_SUB).op(op::F64_SUB).local_set(hi);
        f.local_get(x).local_get(hi).op(op::F64_SUB).local_set(lo);
    }
    let f = &mut g.f;
    f.local_get(a).local_get(b).op(op::F64_MUL).local_set(uh);
    // ul = ((ah*bh - uh) + ah*bl + al*bh) + al*bl
    f.local_get(ah).local_get(bh).op(op::F64_MUL).local_get(uh).op(op::F64_SUB);
    f.local_get(ah).local_get(bl).op(op::F64_MUL).op(op::F64_ADD);
    f.local_get(al).local_get(bh).op(op::F64_MUL).op(op::F64_ADD);
    f.local_get(al).local_get(bl).op(op::F64_MUL).op(op::F64_ADD).local_set(ul);
    f.local_get(c).local_get(uh).op(op::F64_ADD).local_set(th);
    two_sum(g, true, c, uh, th, tl);
    g.f.local_get(tl).local_get(ul).op(op::F64_ADD).local_set(s2);
    two_sum(g, true, tl, ul, s2, e2);
    round_odd(g, s2, e2);
    g.f.op(op::F64_REINTERPRET_I64).local_get(th).op(op::F64_ADD).local_set(z);
}

/// FMADD/FMSUB/FNMADD/FNMSUB in double precision.
fn fma_d(simd: u32, neg_a: bool, neg_n: bool, src: Src) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, b, c, z, bits, ok) =
        (g.local(F64), g.local(F64), g.local(F64), g.local(F64), g.local(I64), g.local(I32));
    g.fpcr_ok();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.operand(src, 0, true);
    if neg_n {
        g.f.op(op::F64_NEG);
    }
    g.f.local_set(a);
    g.operand(src, 1, true);
    g.f.local_set(b);
    g.operand(src, 2, true);
    if neg_a {
        g.f.op(op::F64_NEG);
    }
    g.f.local_set(c);
    exp_in(&mut g, a, -400, 400);
    exp_in(&mut g, b, -400, 400);
    g.f.op(op::I32_AND);
    exp_in(&mut g, c, -800, 800);
    g.f.local_get(c).op(op::I64_REINTERPRET_F64).i64_const(i64::MAX).op(op::I64_AND).op(op::I64_EQZ);
    g.f.op(op::I32_OR).op(op::I32_AND).if_(BLOCK_EMPTY);
    emulated_fma(&mut g, a, b, c, z);
    g.bits64(z, true);
    g.f.local_tee(bits);
    g.safe_bits(true);
    g.f.local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FMLA/FMLS .2d (and by element): the emulated FMA lane by lane.
fn vfma_d(simd: u32, neg: bool, idx: bool) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (n, m, acc, r) = (g.local(V128), g.local(V128), g.local(V128), g.local(V128));
    let (a, b, c, z, ok) = (g.local(F64), g.local(F64), g.local(F64), g.local(F64), g.local(I32));
    g.fpcr_ok();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.vload(5);
    if neg {
        g.f.v(v::F64X2_NEG);
    }
    g.f.local_set(n);
    if idx {
        elem_splat(&mut g, true);
    } else {
        g.vload(16);
    }
    g.f.local_set(m);
    g.vload(0);
    g.f.local_set(acc);
    g.f.i32_const(1).local_set(ok);
    g.f.local_get(acc).local_set(r);
    for lane in 0..2u8 {
        for (src, dst) in [(n, a), (m, b), (acc, c)] {
            g.f.local_get(src).lane(v::F64X2_EXTRACT_LANE, lane).local_set(dst);
        }
        exp_in(&mut g, a, -400, 400);
        exp_in(&mut g, b, -400, 400);
        g.f.op(op::I32_AND);
        exp_in(&mut g, c, -800, 800);
        g.f.local_get(c).op(op::I64_REINTERPRET_F64).i64_const(i64::MAX).op(op::I64_AND).op(op::I64_EQZ);
        g.f.op(op::I32_OR).op(op::I32_AND).local_get(ok).op(op::I32_AND).local_set(ok);
        emulated_fma(&mut g, a, b, c, z);
        g.bits64(z, true);
        g.safe_bits(true);
        g.f.local_get(ok).op(op::I32_AND).local_set(ok);
        g.f.local_get(r).local_get(z).lane(v::F64X2_REPLACE_LANE, lane).local_set(r);
    }
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Scalar FSQRT.
fn sqrt(simd: u32, d: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) = (g.local(ft), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_tee(a).op(fop(d, op::F32_SQRT, op::F64_SQRT)).local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    // a >= 0 (excludes NaN and negatives; ±0 and +inf give themselves, exact)
    g.f.local_get(a);
    g.fzero(d);
    g.f.op(fop(d, op::F32_GE, op::F64_GE));
    let ex = g.local(ValType::I32);
    g.f.i32_const(1).local_set(ex);
    if d {
        g.ixc();
    } else {
        // f64(r)² == f64(a), exact in double (otherwise IXC, raised here)
        let f = &mut g.f;
        f.local_get(r).op(op::F64_PROMOTE_F32).local_get(r).op(op::F64_PROMOTE_F32).op(op::F64_MUL);
        f.local_get(a).op(op::F64_PROMOTE_F32).op(op::F64_EQ).local_set(ex).i32_const(1);
    }
    g.f.op(op::I32_AND).local_set(ok);
    g.fz_guard(ok, |g| g.den(a, d));
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCMP/FCMPE (bit 3: with zero): NZCV without NaN.
fn cmp(simd: u32, d: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, b) = (g.local(ft), g.local(ft));
    let (lt, eq) = (fop(d, op::F32_LT, op::F64_LT), fop(d, op::F32_EQ, op::F64_EQ));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_set(a);
    // With bit 3 (FCMP with zero) b = 0, otherwise Vm.
    g.fzero(d);
    g.load(16, d);
    g.f.local_get(P_WORD).i32_const(8).op(op::I32_AND).op(op::SELECT).local_set(b);
    let ok = g.local(ValType::I32);
    g.not_nan(a, d);
    g.not_nan(b, d);
    g.f.op(op::I32_AND).local_set(ok);
    g.fz_guard(ok, |g| {
        g.den(a, d);
        g.den(b, d);
        g.f.op(op::I32_OR);
    });
    g.f.local_get(ok).if_(BLOCK_EMPTY);
    // a < b: N; a == b: Z C; a > b: C
    let f = &mut g.f;
    f.i32_const(0x8000_0000u32 as i32);
    f.i32_const(0x6000_0000).i32_const(0x2000_0000);
    f.local_get(a).local_get(b).op(eq).op(op::SELECT);
    f.local_get(a).local_get(b).op(lt).op(op::SELECT);
    f.op(op::RETURN).end();
    g.f.end();
    g.fallback(false);
    g.f.op(op::I32_WRAP_I64);
    g.finish()
}

/// FCVT Sd, Dn.
fn cvt_ds(simd: u32) -> Func {
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) =
        (g.local(ValType::F64), g.local(ValType::F32), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, true);
    g.f.local_tee(a).op(op::F32_DEMOTE_F64).local_set(r);
    g.bits64(r, false);
    g.f.local_tee(bits);
    g.safe_bits(false);
    // or zero from zero
    g.f.local_get(a).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_EQ).op(op::I32_OR);
    g.f.local_set(ok);
    // exact: the promotion gives the input back (otherwise IXC, raised here)
    let ex = g.local(ValType::I32);
    g.f.local_get(r).op(op::F64_PROMOTE_F32).local_get(a).op(op::F64_EQ).local_set(ex);
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVT Dd, Sn: exact for every value that is not NaN.
fn cvt_sd(simd: u32) -> Func {
    let mut g = G::new(simd, 2);
    let (a, r, bits) = (g.local(ValType::F32), g.local(ValType::F64), g.local(ValType::I64));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, false);
    g.f.local_tee(a).op(op::F64_PROMOTE_F32).local_set(r);
    g.bits64(r, true);
    g.f.local_set(bits);
    let ok = g.local(ValType::I32);
    g.not_nan(a, false);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| g.den(a, false));
    g.f.local_get(ok).if_(BLOCK_EMPTY);
    g.store_scalar_bits(bits);
    g.f.op(op::RETURN).end();
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Rounding `r` of f32/f64 (on the stack).
fn round_op(g: &mut G, d: bool, r: Rnd) {
    if r == Rnd::Away {
        // t = trunc(x); |x - t| (exact) >= 0.5 ? t + copysign(1, x) : t
        // (the sum is exact: t integer; t keeps the sign of zero).
        let ft = if d { ValType::F64 } else { ValType::F32 };
        let (x, t) = (g.local(ft), g.local(ft));
        let konst = |f: &mut Func, v: f64| {
            if d {
                f.i64_const(v.to_bits() as i64).op(op::F64_REINTERPRET_I64);
            } else {
                f.i32_const((v as f32).to_bits() as i32).op(op::F32_REINTERPRET_I32);
            }
        };
        let f = &mut g.f;
        f.local_tee(x).op(fop(d, op::F32_TRUNC, op::F64_TRUNC)).local_tee(t);
        konst(f, 1.0);
        f.local_get(x).op(fop(d, op::F32_COPYSIGN, op::F64_COPYSIGN)).op(fop(d, op::F32_ADD, op::F64_ADD));
        f.local_get(t);
        f.local_get(x).local_get(t).op(fop(d, op::F32_SUB, op::F64_SUB)).op(fop(d, op::F32_ABS, op::F64_ABS));
        konst(f, 0.5);
        f.op(fop(d, op::F32_GE, op::F64_GE)).op(op::SELECT);
        return;
    }
    g.f.op(match r {
        Rnd::Nearest | Rnd::NearestX => fop(d, op::F32_NEAREST, op::F64_NEAREST),
        Rnd::Ceil => fop(d, op::F32_CEIL, op::F64_CEIL),
        Rnd::Floor => fop(d, op::F32_FLOOR, op::F64_FLOOR),
        Rnd::Trunc | Rnd::Away => fop(d, op::F32_TRUNC, op::F64_TRUNC),
    });
}

/// Scalar FRINT[NPMZIX] (without NaN: no flags, except IXC of FRINTX).
fn frint(simd: u32, d: bool, rnd: Rnd) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) = (g.local(ft), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_tee(a);
    round_op(&mut g, d, rnd);
    g.f.local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    g.not_nan(a, d);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| g.den(a, d));
    g.commit(ok, |g| {
        if rnd == Rnd::NearestX {
            // IXC when the value changed
            let ex = g.local(ValType::I32);
            g.f.local_get(r).local_get(a).op(fop(d, op::F32_EQ, op::F64_EQ)).local_set(ex);
            g.raise_unless(ex);
        }
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// SCVTF/UCVTF from a general register.
fn from_int(simd: u32, d: bool, sf: bool, u: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 3);
    let (x, r, bits, ok) = (g.local(ValType::I64), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    // x extended to 64 bits as the instruction reads it.
    g.f.local_get(P_X);
    if !sf {
        if u {
            g.f.i64_const(0xffff_ffff).op(op::I64_AND);
        } else {
            g.f.op(op::I64_EXTEND32_S);
        }
    }
    g.f.local_tee(x);
    g.f.op(match (d, u) {
        (true, false) => op::F64_CONVERT_I64_S,
        (true, true) => op::F64_CONVERT_I64_U,
        (false, false) => op::F32_CONVERT_I64_S,
        (false, true) => op::F32_CONVERT_I64_U,
    });
    g.f.local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    // Exact iff the significant bits of |x| (64 - clz - ctz) fit in the
    // mantissa (24 or 53 bits); otherwise IXC, raised here. Never tiny or
    // overflowing (FZ and DN do not matter).
    let (m, ex) = (g.local(ValType::I64), g.local(ValType::I32));
    g.f.i32_const(1).local_set(ok);
    if u {
        g.f.local_get(x);
    } else {
        // |x| (i64::MIN gives 2^63 as an unsigned value)
        g.f.i64_const(0).local_get(x).op(op::I64_SUB).local_get(x);
        g.f.local_get(x).i64_const(0).op(op::I64_LT_S).op(op::SELECT);
    }
    g.f.local_set(m);
    g.f.i64_const(64)
        .local_get(m)
        .op(op::I64_CLZ)
        .op(op::I64_SUB)
        .local_get(m)
        .op(op::I64_CTZ)
        .op(op::I64_SUB);
    g.f.i64_const(if d { 53 } else { 24 }).op(op::I64_LE_S).local_set(ex);
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(true);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVT[NPMZ][SU] to a general register: rounding in double
/// (exact even for a single-precision value), range check on the
/// rounded value (like `FPToFixed`), then the saturating conversion
/// (exact here).
fn to_int(simd: u32, d: bool, sf: bool, u: bool, rnd: Rnd) -> Func {
    let mut g = G::new(simd, 2);
    let (a, t) = (g.local(ValType::F64), g.local(ValType::F64));
    let ok = g.local(ValType::I32);
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    if !d {
        g.f.op(op::F64_PROMOTE_F32);
    }
    g.f.local_tee(a);
    round_op(&mut g, true, rnd);
    g.f.local_set(t);
    // range: [lo, hi)
    let (lo, hi): (f64, f64) = match (sf, u) {
        (true, false) => (-9_223_372_036_854_775_808.0, 9_223_372_036_854_775_808.0),
        (false, false) => (-2_147_483_648.0, 2_147_483_648.0),
        (true, true) => (0.0, 18_446_744_073_709_551_616.0),
        (false, true) => (0.0, 4_294_967_296.0),
    };
    let f = &mut g.f;
    f.local_get(t).i64_const(lo.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_GE);
    f.local_get(t).i64_const(hi.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_LT).op(op::I32_AND);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| g.den_reg(5, d));
    g.f.local_get(ok).if_(BLOCK_EMPTY);
    // IXC when the rounding changed the value
    let ex = g.local(ValType::I32);
    g.f.local_get(t).local_get(a).op(op::F64_EQ).local_set(ex);
    g.raise_unless(ex);
    g.f.local_get(t);
    match (sf, u) {
        (true, false) => {
            g.f.sat(sat::I64_TRUNC_SAT_F64_S);
        }
        (true, true) => {
            g.f.sat(sat::I64_TRUNC_SAT_F64_U);
        }
        (false, false) => {
            g.f.sat(sat::I32_TRUNC_SAT_F64_S).op(op::I64_EXTEND_I32_U);
        }
        (false, true) => {
            g.f.sat(sat::I32_TRUNC_SAT_F64_U).op(op::I64_EXTEND_I32_U);
        }
    }
    g.f.op(op::RETURN).end();
    g.f.end();
    g.fallback(false);
    g.finish()
}

// --- vector ------------------------------------------------------------

/// v128 constant with `bits` in every lane (32 or 64 bits).
fn splat_const(g: &mut G, d: bool, bits: u64) {
    let w = if d { bits } else { (bits & 0xffff_ffff) * 0x1_0000_0001 };
    g.f.v128_const(w, w);
}

/// Mask (v128) of the finite lanes of `r`.
fn vfinite(g: &mut G, d: bool, r: u32) {
    g.f.local_get(r).v(vop(d, v::F32X4_ABS, v::F64X2_ABS));
    splat_const(g, d, if d { D_INF } else { S_INF as u64 });
    g.f.v(vop(d, v::F32X4_LT, v::F64X2_LT));
}

/// Mask of the lanes that are normal and greater than the smallest normal.
fn vsafe(g: &mut G, d: bool, r: u32) {
    g.f.local_get(r).v(vop(d, v::F32X4_ABS, v::F64X2_ABS));
    splat_const(g, d, if d { D_MIN_NORMAL } else { S_MIN_NORMAL as u64 });
    g.f.v(vop(d, v::F32X4_GT, v::F64X2_GT));
    vfinite(g, d, r);
    g.f.v(v::AND);
}

/// Mask of the non-NaN lanes of `x`.
fn vnot_nan(g: &mut G, d: bool, x: u32) {
    g.f.local_get(x).local_get(x).v(vop(d, v::F32X4_EQ, v::F64X2_EQ));
}

/// All lanes of the mask on top of the stack set, counting as
/// true those to ignore with Q = 0 (i32).
fn all_true(g: &mut G, d: bool) {
    g.himask();
    g.f.v(v::OR).v(vop(d, v::I32X4_ALL_TRUE, v::I64X2_ALL_TRUE));
}

fn vbin(simd: u32, d: bool, op_: Bin) -> Func {
    let mut g = G::new(simd, 2);
    let (a, b, r, ok) =
        (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    let ex = g.local(ValType::I32);
    g.f.i32_const(1).local_set(ex);
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_set(a);
    g.vload(16);
    if matches!(op_, Bin::Sub | Bin::Abd) {
        g.f.v(vop(d, v::F32X4_NEG, v::F64X2_NEG));
    }
    g.f.local_set(b);
    if op_ == Bin::Addp {
        // a' = even elements, b' = odd ones of concat(a, b) (with Q = 0 in
        // single: [a0, b0] and [a1, b1]); then a sum.
        let (ev, od) = (g.local(ValType::V128), g.local(ValType::V128));
        let sh = |f: &mut Func, l: [u8; 16], dst: u32| {
            f.local_get(a).local_get(b).shuffle(l).local_set(dst);
        };
        let pick = |idx: [usize; 4]| -> [u8; 16] { core::array::from_fn(|j| (idx[j / 4] * 4 + j % 4) as u8) };
        if d {
            sh(&mut g.f, pick([0, 1, 4, 5]), ev);
            sh(&mut g.f, pick([2, 3, 6, 7]), od);
        } else {
            let (e4, o4) = (g.local(ValType::V128), g.local(ValType::V128));
            sh(&mut g.f, pick([0, 2, 4, 6]), e4);
            sh(&mut g.f, pick([1, 3, 5, 7]), o4);
            sh(&mut g.f, pick([0, 4, 0, 4]), ev);
            sh(&mut g.f, pick([1, 5, 1, 5]), od);
            // Q = 1: e4/o4; Q = 0: ev/od.
            for (q1, dst) in [(e4, ev), (o4, od)] {
                g.f.local_get(q1).local_get(dst);
                g.q();
                g.f.op(op::SELECT).local_set(dst);
            }
        }
        g.f.local_get(ev).local_set(a).local_get(od).local_set(b);
    }
    g.f.local_get(a).local_get(b);
    g.f.v(match op_ {
        Bin::Add | Bin::Sub | Bin::Addp | Bin::Abd => vop(d, v::F32X4_ADD, v::F64X2_ADD),
        Bin::Mul | Bin::Nmul => vop(d, v::F32X4_MUL, v::F64X2_MUL),
        Bin::Div => vop(d, v::F32X4_DIV, v::F64X2_DIV),
        Bin::Max | Bin::MaxNm => vop(d, v::F32X4_MAX, v::F64X2_MAX),
        Bin::Min | Bin::MinNm => vop(d, v::F32X4_MIN, v::F64X2_MIN),
    });
    g.f.local_set(r);
    match op_ {
        Bin::Add | Bin::Sub | Bin::Addp | Bin::Abd => {
            // Finite, and exact (TwoSum per lane) or with IXC at 1.
            let (t, err) = (g.local(ValType::V128), g.local(ValType::V128));
            let (add, sub) = (vop(d, v::F32X4_ADD, v::F64X2_ADD), vop(d, v::F32X4_SUB, v::F64X2_SUB));
            let f = &mut g.f;
            f.local_get(r).local_get(a).v(sub).local_set(t);
            f.local_get(a).local_get(r).local_get(t).v(sub).v(sub);
            f.local_get(b).local_get(t).v(sub).v(add).local_set(err);
            vfinite(&mut g, d, r);
            all_true(&mut g, d);
            g.f.local_set(ok);
            // exact: every error zero (otherwise IXC, raised here)
            g.f.local_get(err).v128_const(0, 0).v(vop(d, v::F32X4_EQ, v::F64X2_EQ));
            all_true(&mut g, d);
            g.f.local_set(ex);
        }
        Bin::Max | Bin::Min | Bin::MaxNm | Bin::MinNm => {
            vnot_nan(&mut g, d, a);
            vnot_nan(&mut g, d, b);
            g.f.v(v::AND);
            all_true(&mut g, d);
            g.f.local_set(ok);
        }
        Bin::Mul | Bin::Nmul | Bin::Div => {
            // Safely normal and IXC at 1.
            vsafe(&mut g, d, r);
            all_true(&mut g, d);
            g.ixc();
            g.f.op(op::I32_AND).local_set(ok);
        }
    }
    g.fz_guard(ok, |g| {
        g.vden(a, d);
        g.vden(b, d);
        g.f.op(op::I32_OR);
        if matches!(op_, Bin::Add | Bin::Sub | Bin::Addp | Bin::Abd) {
            g.vden(r, d);
            g.f.op(op::I32_OR);
        }
    });
    if op_ == Bin::Abd {
        // The absolute value after rounding (without flags).
        g.f.local_get(r).v(vop(d, v::F32X4_ABS, v::F64X2_ABS)).local_set(r);
    }
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_vec(r);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vector FCVT[NPMZA][SU] (without fixed point): rounding,
/// range on the rounded value, saturating conversion (exact here).
fn vto_int(simd: u32, d: bool, u: bool, rnd: Rnd) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, t, r, ok) = (g.local(V128), g.local(V128), g.local(V128), g.local(I32));
    // Exact (every lane unchanged by the rounding): otherwise IXC, raised here.
    let ex = g.local(I32);
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_set(a);
    if d {
        // Lane by lane, in scalar.
        g.f.i32_const(1).local_set(ex);
        let (x, tt) = (g.local(F64), g.local(F64));
        g.f.i32_const(1).local_set(ok).v128_const(0, 0).local_set(r);
        let (lo, hi): (f64, f64) = if u {
            (0.0, 18_446_744_073_709_551_616.0)
        } else {
            (-9_223_372_036_854_775_808.0, 9_223_372_036_854_775_808.0)
        };
        for lane in 0..2u8 {
            g.f.local_get(a).lane(v::F64X2_EXTRACT_LANE, lane).local_tee(x);
            round_op(&mut g, true, rnd);
            g.f.local_set(tt);
            let f = &mut g.f;
            f.local_get(tt).i64_const(lo.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_GE);
            f.local_get(tt).i64_const(hi.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_LT);
            f.op(op::I32_AND);
            g.f.local_get(ok).op(op::I32_AND).local_set(ok);
            g.f.local_get(tt).local_get(x).op(op::F64_EQ).local_get(ex).op(op::I32_AND).local_set(ex);
            g.f.local_get(r).local_get(tt);
            g.f.sat(if u { sat::I64_TRUNC_SAT_F64_U } else { sat::I64_TRUNC_SAT_F64_S });
            g.f.lane(v::I64X2_REPLACE_LANE, lane).local_set(r);
        }
    } else {
        g.f.local_get(a);
        vround(&mut g, false, rnd);
        g.f.local_set(t);
        let (lo, hi): (f32, f32) =
            if u { (0.0, 4_294_967_296.0) } else { (-2_147_483_648.0, 2_147_483_648.0) };
        g.f.local_get(t);
        splat_const(&mut g, false, lo.to_bits() as u64);
        g.f.v(v::F32X4_GE).local_get(t);
        splat_const(&mut g, false, hi.to_bits() as u64);
        g.f.v(v::F32X4_LT).v(v::AND);
        all_true(&mut g, false);
        g.f.local_set(ok);
        g.f.local_get(t).local_get(a).v(v::F32X4_EQ);
        all_true(&mut g, false);
        g.f.local_set(ex);
        g.f.local_get(t)
            .v(if u { v::I32X4_TRUNC_SAT_F32X4_U } else { v::I32X4_TRUNC_SAT_F32X4_S })
            .local_set(r);
    }
    g.fz_guard(ok, |g| g.vden(a, d));
    g.commit(ok, |g| {
        g.raise_unless(ex);
        g.store_vec(r);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vector rounding to integer (v128 on the stack).
fn vround(g: &mut G, d: bool, r: Rnd) {
    if r == Rnd::Away {
        // Like `round_op`: t + (|x - t| >= 0.5 ? copysign(1, x) : 0), with
        // the choice per lane (t keeps the sign of zero).
        let (x, t) = (g.local(ValType::V128), g.local(ValType::V128));
        g.f.local_tee(x).v(vop(d, v::F32X4_TRUNC, v::F64X2_TRUNC)).local_set(t);
        let one = if d { 1.0f64.to_bits() } else { 1.0f32.to_bits() as u64 };
        let half = if d { 0.5f64.to_bits() } else { 0.5f32.to_bits() as u64 };
        let sign = if d { 1u64 << 63 } else { 0x8000_0000 };
        // t + copysign(1, x): the sign of x, the rest from 1.
        g.f.local_get(t);
        g.f.local_get(x);
        splat_const(g, d, one);
        splat_const(g, d, sign);
        g.f.v(v::BITSELECT).v(vop(d, v::F32X4_ADD, v::F64X2_ADD));
        g.f.local_get(t);
        g.f.local_get(x).local_get(t).v(vop(d, v::F32X4_SUB, v::F64X2_SUB)).v(vop(
            d,
            v::F32X4_ABS,
            v::F64X2_ABS,
        ));
        splat_const(g, d, half);
        g.f.v(vop(d, v::F32X4_GE, v::F64X2_GE)).v(v::BITSELECT);
        return;
    }
    g.f.v(match r {
        Rnd::Nearest | Rnd::NearestX => vop(d, v::F32X4_NEAREST, v::F64X2_NEAREST),
        Rnd::Ceil => vop(d, v::F32X4_CEIL, v::F64X2_CEIL),
        Rnd::Floor => vop(d, v::F32X4_FLOOR, v::F64X2_FLOOR),
        Rnd::Trunc | Rnd::Away => vop(d, v::F32X4_TRUNC, v::F64X2_TRUNC),
    });
}

/// Vector SCVTF/UCVTF (without fixed point): exact if every lane fits
/// in the mantissa, or with IXC at 1.
fn vfrom_int(simd: u32, d: bool, u: bool) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, r, ok) = (g.local(V128), g.local(V128), g.local(I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_set(a);
    if d {
        let x = g.local(I64);
        g.f.v128_const(0, 0).local_set(r);
        g.ixc();
        g.f.local_set(ok);
        for lane in 0..2u8 {
            g.f.local_get(a).lane(v::I64X2_EXTRACT_LANE, lane).local_set(x);
            g.f.local_get(r).local_get(x);
            g.f.op(if u { op::F64_CONVERT_I64_U } else { op::F64_CONVERT_I64_S });
            g.f.lane(v::F64X2_REPLACE_LANE, lane).local_set(r);
        }
        // all lanes exact: |x| <= 2^53
        let mut first = true;
        for lane in 0..2u8 {
            g.f.local_get(a).lane(v::I64X2_EXTRACT_LANE, lane);
            if !u {
                g.f.i64_const(1 << 53).op(op::I64_ADD).i64_const(1 << 54);
            } else {
                g.f.i64_const(1 << 53);
            }
            g.f.op(op::I64_LE_U);
            if !first {
                g.f.op(op::I32_AND);
            }
            first = false;
        }
        g.f.local_get(ok).op(op::I32_OR).local_set(ok);
    } else {
        g.f.local_get(a).v(if u { v::F32X4_CONVERT_I32X4_U } else { v::F32X4_CONVERT_I32X4_S }).local_set(r);
        g.ixc();
        g.f.local_get(a);
        if u {
            g.f.v128_const(0x0100_0001_0100_0001, 0x0100_0001_0100_0001).v(v::I32X4_LT_U);
        } else {
            g.f.v128_const(0x0100_0000_0100_0000, 0x0100_0000_0100_0000).v(v::I32X4_ADD);
            g.f.v128_const(0x0200_0001_0200_0001, 0x0200_0001_0200_0001).v(v::I32X4_LT_U);
        }
        all_true(&mut g, false);
        g.f.op(op::I32_OR).local_set(ok);
    }
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVTL/FCVTL2 (from single to double): exact without NaN.
fn vcvtl(simd: u32) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (r, ok) = (g.local(V128), g.local(I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    // High half with Q = 1 (FCVTL2).
    g.vload(5);
    g.vload(5);
    g.f.shuffle(core::array::from_fn(|j| (8 + j % 8) as u8));
    g.vload(5);
    g.q();
    g.f.op(op::SELECT).v(v::F64X2_PROMOTE_LOW_F32X4).local_tee(r);
    g.f.local_get(r).v(v::F64X2_EQ).v(v::I64X2_ALL_TRUE).local_set(ok);
    g.fz_guard(ok, |g| {
        // A single-precision denormal: |r| < 2^-126 and r != 0.
        g.f.local_get(r).v(v::F64X2_ABS);
        splat_const(g, true, S_MIN_NORMAL_AS_D);
        g.f.v(v::F64X2_LT).local_get(r).v128_const(0, 0).v(v::F64X2_NE).v(v::AND).v(v::ANY_TRUE);
    });
    g.commit(ok, |g| {
        g.vaddr(0);
        g.f.local_get(r).v128_store(off::V);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVTN/FCVTN2 (from double to single, into the low or high half of Vd).
fn vcvtn(simd: u32) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, r, ok) = (g.local(V128), g.local(V128), g.local(I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(a).v(v::F32X4_DEMOTE_F64X2_ZERO).local_set(r);
    // No NaN, and: all exact (equal promotion), or safely normal (lanes 0
    // and 1) with IXC raised here.
    let ex = g.local(I32);
    g.f.local_get(a).local_get(a).v(v::F64X2_EQ).v(v::I64X2_ALL_TRUE);
    g.f.local_get(r).v(v::F64X2_PROMOTE_LOW_F32X4).local_get(a).v(v::F64X2_EQ).v(v::I64X2_ALL_TRUE);
    g.f.local_tee(ex);
    vsafe(&mut g, false, r);
    g.f.v128_const(0, u64::MAX).v(v::OR).v(v::I32X4_ALL_TRUE);
    g.f.op(op::I32_OR).op(op::I32_AND).local_set(ok);
    g.fz_guard(ok, |g| g.vden(r, false));
    g.commit(ok, |g| {
        g.raise_unless(ex);
        // Q = 1: [Vd low, r low]; Q = 0: [r low, 0].
        g.vaddr(0);
        g.vload(0);
        g.f.local_get(r).shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
        g.f.local_get(r).v128_const(u64::MAX, 0).v(v::AND);
        g.q();
        g.f.op(op::SELECT).v128_store(off::V);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Low half (lanes 0 and 1) of a single-precision FMA: (d + n × m) in double
/// rounded to odd, then to single (lanes 0 and 1 of the result, the
/// others zero).
fn fma_half(g: &mut G, n: u32, m: u32, acc: u32) {
    let (p, c, s, t, err) = (
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::V128),
    );
    let f = &mut g.f;
    f.local_get(n).v(v::F64X2_PROMOTE_LOW_F32X4);
    f.local_get(m).v(v::F64X2_PROMOTE_LOW_F32X4).v(v::F64X2_MUL).local_set(p);
    f.local_get(acc).v(v::F64X2_PROMOTE_LOW_F32X4).local_set(c);
    f.local_get(p).local_get(c).v(v::F64X2_ADD).local_set(s);
    // TwoSum
    f.local_get(s).local_get(p).v(v::F64X2_SUB).local_set(t);
    f.local_get(p).local_get(s).local_get(t).v(v::F64X2_SUB).v(v::F64X2_SUB);
    f.local_get(c).local_get(t).v(v::F64X2_SUB).v(v::F64X2_ADD).local_set(err);
    // to odd: s + (inexact & even ? (same sign ? 1 : -1) : 0)
    f.local_get(s);
    f.v128_const(1, 1).v128_const(u64::MAX, u64::MAX);
    f.local_get(s).local_get(err).v(v::XOR).v128_const(0, 0).v(v::I64X2_GE_S);
    f.v(v::BITSELECT);
    f.local_get(err).v128_const(0, 0).v(v::F64X2_NE);
    f.local_get(s).v128_const(1, 1).v(v::AND).v128_const(0, 0).v(v::I64X2_EQ).v(v::AND);
    f.v(v::AND).v(v::I64X2_ADD);
    f.v(v::F32X4_DEMOTE_F64X2_ZERO);
}

/// Vector FMLA/FMLS (or by element, `idx`) in single precision.
fn vfma(simd: u32, neg: bool, idx: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (n, m, acc, r, ok) = (
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::V128),
        g.local(ValType::I32),
    );
    let (n2, m2, acc2) = (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::V128));
    g.fpcr_ok();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.vload(5);
    if neg {
        g.f.v(v::F32X4_NEG);
    }
    g.f.local_set(n);
    if idx {
        elem_splat(&mut g, false);
    } else {
        g.vload(16);
    }
    g.f.local_set(m);
    g.vload(0);
    g.f.local_set(acc);
    // high half in lanes 0 and 1
    let hi: [u8; 16] = core::array::from_fn(|j| (8 + j % 8) as u8);
    for (src, dst) in [(n, n2), (m, m2), (acc, acc2)] {
        g.f.local_get(src).local_get(src).shuffle(hi).local_set(dst);
    }
    fma_half(&mut g, n, m, acc);
    fma_half(&mut g, n2, m2, acc2);
    g.f.shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
    g.f.local_set(r);
    vsafe(&mut g, false, r);
    all_true(&mut g, false);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| {
        g.vden(n, false);
        g.vden(m, false);
        g.f.op(op::I32_OR);
        g.vden(acc, false);
        g.f.op(op::I32_OR);
    });
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vm[index] repeated in all lanes (by-element FMUL/FMLA): Rm in
/// bits 20:16, index H:L (single) or H (double).
fn elem_splat(g: &mut G, d: bool) {
    g.vload(16);
    // lane bytes: index × size + [0..size)
    let f = &mut g.f;
    if d {
        f.v128_const(0x0706_0504_0302_0100, 0x0706_0504_0302_0100);
        f.local_get(P_WORD).i32_const(11).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
        f.i32_const(3).op(op::I32_SHL);
    } else {
        f.v128_const(0x0302_0100_0302_0100, 0x0302_0100_0302_0100);
        f.local_get(P_WORD).i32_const(10).op(op::I32_SHR_U).i32_const(2).op(op::I32_AND);
        f.local_get(P_WORD).i32_const(21).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND).op(op::I32_OR);
        f.i32_const(2).op(op::I32_SHL);
    }
    f.v(v::I8X16_SPLAT).v(v::I8X16_ADD).v(v::SWIZZLE);
}

/// By-element FMUL.
fn vidx_mul(simd: u32, d: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (r, ok) = (g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_ok();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    let (a, b) = (g.local(ValType::V128), g.local(ValType::V128));
    g.vload(5);
    g.f.local_tee(a);
    elem_splat(&mut g, d);
    g.f.local_tee(b);
    g.f.v(vop(d, v::F32X4_MUL, v::F64X2_MUL)).local_set(r);
    vsafe(&mut g, d, r);
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| {
        g.vden(a, d);
        g.vden(b, d);
        g.f.op(op::I32_OR);
    });
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCMEQ/FCMGE/FCMGT (and with zero) without NaN: WASM masks.
fn vcmp(simd: u32, d: bool, op_: Cmp, zero: bool, swap: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (a, b, r, ok) =
        (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_set(a);
    if zero {
        g.f.v128_const(0, 0);
    } else {
        g.vload(16);
    }
    g.f.local_set(b);
    if swap {
        g.f.local_get(b).local_get(a);
    } else {
        g.f.local_get(a).local_get(b);
    }
    g.f.v(match op_ {
        Cmp::Eq => vop(d, v::F32X4_EQ, v::F64X2_EQ),
        Cmp::Ge => vop(d, v::F32X4_GE, v::F64X2_GE),
        Cmp::Gt => vop(d, v::F32X4_GT, v::F64X2_GT),
    });
    g.f.local_set(r);
    vnot_nan(&mut g, d, a);
    vnot_nan(&mut g, d, b);
    g.f.v(v::AND);
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| {
        g.vden(a, d);
        g.vden(b, d);
        g.f.op(op::I32_OR);
    });
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vector FSQRT: lanes >= 0, IXC at 1.
fn vsqrt(simd: u32, d: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (a, r, ok) = (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_ok();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(a).v(vop(d, v::F32X4_SQRT, v::F64X2_SQRT)).local_set(r);
    g.f.local_get(a).v128_const(0, 0).v(vop(d, v::F32X4_GE, v::F64X2_GE));
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| g.vden(a, d));
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

// --- half precision (ADR 0045) ----------------------------------------

/// FPCR.AHP (bit 26) clear and FPCR.RMode round to nearest (i32 boolean):
/// the IEEE half format of the fast paths.
fn ieee_half(g: &mut G) {
    g.f.local_get(P_STATE)
        .i32_load(off::FPCR)
        .i32_const(1 << 26 | FPCR_RMODE)
        .op(op::I32_AND)
        .op(op::I32_EQZ);
}

/// v128 with `x` in every 32-bit lane.
fn splat32(g: &mut G, x: u32) {
    let w = x as u64 * 0x1_0000_0001;
    g.f.v128_const(w, w);
}

/// FCVTL(2) .4s from .4h/.8h (`vector`) or FCVT Sd, Hn: exact for every
/// half that is not a NaN (the half format has no flush to zero on ARMv8.0,
/// so denormals are values like the others; FZ and DN do not matter), no
/// flags. The half bits, zero-extended to 32 bits: f = bits(abs << 13) ×
/// 2^112 (exact: an exponent shift, a denormal half becomes the normal
/// single it is), infinities set apart, then the sign.
fn cvt_h2s(simd: u32, vector: bool) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (h, abs, r, ok) = (g.local(V128), g.local(V128), g.local(V128), g.local(I32));
    ieee_half(&mut g);
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    if vector {
        // Q = 1 (FCVTL2): the high four halves.
        g.f.local_tee(h).v(v::I32X4_EXTEND_HIGH_I16X8_U);
        g.f.local_get(h).v(v::I32X4_EXTEND_LOW_I16X8_U);
        g.q();
        g.f.op(op::SELECT);
    } else {
        g.f.v(v::I32X4_EXTEND_LOW_I16X8_U);
    }
    g.f.local_tee(h);
    splat32(&mut g, 0x7fff);
    g.f.v(v::AND).local_set(abs);
    // f, or the infinity
    splat32(&mut g, S_INF);
    g.f.local_get(abs).i32_const(13).v(v::I32X4_SHL);
    splat32(&mut g, 0x7780_0000); // 2^112
    g.f.v(v::F32X4_MUL);
    g.f.local_get(abs);
    splat32(&mut g, 0x7c00);
    g.f.v(v::I32X4_EQ).v(v::BITSELECT);
    // | sign << 16
    g.f.local_get(h);
    splat32(&mut g, 0x8000);
    g.f.v(v::AND).i32_const(16).v(v::I32X4_SHL).v(v::OR).local_set(r);
    // no NaN (lanes that count: all four, or lane 0)
    g.f.local_get(abs);
    splat32(&mut g, 0x7c00);
    g.f.v(v::I32X4_GT_U);
    if !vector {
        g.f.v128_const(0xffff_ffff, 0).v(v::AND);
    }
    g.f.v(v::ANY_TRUE).op(op::I32_EQZ).local_set(ok);
    g.commit(ok, |g| {
        if vector {
            g.vaddr(0);
            g.f.local_get(r).v128_store(off::V);
        } else {
            let b = g.local(I64);
            g.f.local_get(r).lane(v::I32X4_EXTRACT_LANE, 0).op(op::I64_EXTEND_I32_U).local_set(b);
            g.store_scalar_bits(b);
        }
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVTN(2) .4h/.8h from .4s (`vector`) or FCVT Hd, Sn, rounding to nearest
/// even with integer arithmetic on the single bits. Every lane must be
/// ±0, ±infinity (exact) or of magnitude in [2^-14, 65520): a normal half
/// after rounding, never tiny (so no UFC, and FZ cannot flush a denormal
/// input: there is none) and no overflow. Inexact exactly when the 13 low
/// bits of the mantissa are not zero: then FPSR.IXC is set here.
fn cvt_s2h(simd: u32, vector: bool) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (x, abs, h, m, ok) = (g.local(V128), g.local(V128), g.local(V128), g.local(V128), g.local(I32));
    let (zero, inf, inexact) = (g.local(V128), g.local(V128), g.local(I32));
    ieee_half(&mut g);
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(x);
    splat32(&mut g, 0x7fff_ffff);
    g.f.v(v::AND).local_set(abs);
    // lanes that count: all four, or lane 0
    if vector {
        g.f.v128_const(u64::MAX, u64::MAX);
    } else {
        g.f.v128_const(0xffff_ffff, 0);
    }
    g.f.local_set(m);
    g.f.local_get(abs).v128_const(0, 0).v(v::I32X4_EQ).local_set(zero);
    g.f.local_get(abs);
    splat32(&mut g, S_INF);
    g.f.v(v::I32X4_EQ).local_set(inf);
    // in range: abs - 0x38800000 < 0x477ff000 - 0x38800000 (unsigned)
    g.f.local_get(abs);
    splat32(&mut g, 0x3880_0000);
    g.f.v(v::I32X4_SUB);
    splat32(&mut g, 0x477f_f000 - 0x3880_0000);
    g.f.v(v::I32X4_LT_U).local_tee(h);
    g.f.local_get(zero).v(v::OR).local_get(inf).v(v::OR).local_get(m).v(v::NOT).v(v::OR);
    g.f.v(v::I32X4_ALL_TRUE).local_set(ok);
    // inexact: a lane in range (the others are exact) with low bits
    g.f.local_get(abs);
    splat32(&mut g, 0x1fff);
    g.f.v(v::AND).v128_const(0, 0).v(v::I32X4_EQ).v(v::NOT);
    g.f.local_get(h).v(v::AND).local_get(m).v(v::AND).v(v::ANY_TRUE).local_set(inexact);
    // rounded = (abs - (112 << 23) + 0xfff + ((abs >> 13) & 1)) >> 13
    g.f.local_get(abs);
    splat32(&mut g, (112 << 23) - 0xfff);
    g.f.v(v::I32X4_SUB);
    g.f.local_get(abs).i32_const(13).v(v::I32X4_SHR_U);
    splat32(&mut g, 1);
    g.f.v(v::AND).v(v::I32X4_ADD).i32_const(13).v(v::I32X4_SHR_U);
    // zeros give 0, infinities 0x7c00
    let t = g.local(V128);
    g.f.local_get(zero).v(v::ANDNOT).local_set(t);
    splat32(&mut g, 0x7c00);
    g.f.local_get(t).local_get(inf).v(v::BITSELECT);
    // | sign
    g.f.local_get(x).i32_const(16).v(v::I32X4_SHR_U);
    splat32(&mut g, 0x8000);
    g.f.v(v::AND).v(v::OR).local_set(h);
    g.commit(ok, |g| {
        g.raise_ixc(inexact);
        // the four halves in the low 64 bits
        g.f.local_get(h).local_get(h).v(v::I16X8_NARROW_I32X4_U).local_set(h);
        g.vaddr(0);
        if vector {
            // Q = 0: [halves, 0]; Q = 1 (FCVTN2): [Vd low, halves].
            g.vload(0);
            g.f.local_get(h)
                .shuffle(core::array::from_fn(|j| if j < 8 { j as u8 } else { (16 + j - 8) as u8 }));
            g.f.local_get(h).v128_const(u64::MAX, 0).v(v::AND);
            g.q();
            g.f.op(op::SELECT).v128_store(off::V);
        } else {
            g.f.local_get(h).v128_const(0xffff, 0).v(v::AND).v128_store(off::V);
        }
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vector FRINT[NPMZAXI]: every lane that counts not a NaN (infinities and
/// zeros round to themselves, with their sign; no flags), FZ guard on the
/// inputs; FRINTX raises IXC exactly when a lane changed.
fn vfrint(simd: u32, d: bool, rnd: Rnd) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, r, ok, inexact) = (g.local(V128), g.local(V128), g.local(I32), g.local(I32));
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(a);
    vround(&mut g, d, rnd);
    g.f.local_set(r);
    vnot_nan(&mut g, d, a);
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.fz_guard(ok, |g| g.vden(a, d));
    g.commit(ok, |g| {
        if rnd == Rnd::NearestX {
            g.f.local_get(r).local_get(a).v(vop(d, v::F32X4_NE, v::F64X2_NE));
            g.himask();
            g.f.v(v::ANDNOT).v(v::ANY_TRUE).local_set(inexact);
            g.raise_ixc(inexact);
        }
        g.store_vec(r);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FMAXV/FMINV/FMAXNMV/FMINNMV .4s: without NaNs the four forms agree,
/// and WASM's max/min order zeros like Arm (-0 < +0); the tree of the
/// interpreter, (e0 op e1) op (e2 op e3), gives the same value. No flags.
fn across(simd: u32, max: bool) -> Func {
    use ValType::*;
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) = (g.local(V128), g.local(V128), g.local(I64), g.local(I32));
    let mm = if max { v::F32X4_MAX } else { v::F32X4_MIN };
    g.fpcr_ok();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(a).local_get(a).local_get(a);
    g.f.shuffle([4, 5, 6, 7, 0, 1, 2, 3, 12, 13, 14, 15, 8, 9, 10, 11]).v(mm).local_tee(r);
    g.f.local_get(r).local_get(r).shuffle([8, 9, 10, 11, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3]).v(mm);
    g.f.lane(v::I32X4_EXTRACT_LANE, 0).op(op::I64_EXTEND_I32_U).local_set(bits);
    vnot_nan(&mut g, false, a);
    g.f.v(v::I32X4_ALL_TRUE).local_set(ok);
    g.fz_guard(ok, |g| g.vden(a, false));
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}
