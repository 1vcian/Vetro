//! Istruzioni in virgola mobile: classi FP scalari e operazioni FP dentro
//! le classi AdvSIMD. L'aritmetica è in `fp` (software, bit-exact).

use super::SimdInsn;
use super::fp::{self, Ctx, D, Fmt, H, Rounding, S};
use super::vreg::{elem, set_elem};
use crate::bits::{bit, field};
use crate::decode::Insn;
use crate::state::Cpu;

/// Tipo FP: 0 = singola, 1 = doppia, 3 = mezza (solo per FCVT su v8.0).
fn fmt_of(ty: u8) -> Fmt {
    match ty {
        0 => S,
        1 => D,
        _ => H,
    }
}

fn rmode_rounding(rmode: u8) -> Rounding {
    match rmode {
        0 => Rounding::TieEven,
        1 => Rounding::PosInf,
        2 => Rounding::NegInf,
        _ => Rounding::Zero,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovKind {
    /// FMOV Wd, Sn / FMOV Sd, Wn
    W,
    /// FMOV Xd, Dn / FMOV Dd, Xn
    X,
    /// FMOV Xd, Vn.D[1] / FMOV Vd.D[1], Xn
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpInsn {
    Dp1 {
        ty: u8,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    Dp2 {
        ty: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Dp3 {
        ty: u8,
        neg_a: bool,
        neg_n: bool,
        ra: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Cmp {
        ty: u8,
        zero: bool,
        signal: bool,
        rm: u8,
        rn: u8,
    },
    CondCmp {
        ty: u8,
        signal: bool,
        cond: u8,
        nzcv: u8,
        rm: u8,
        rn: u8,
    },
    CondSel {
        ty: u8,
        cond: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Imm {
        ty: u8,
        imm: u64,
        rd: u8,
    },
    /// FCVT[NPMZA][SU] e FCVTZ[SU] (virgola fissa).
    ToInt {
        ty: u8,
        sf: bool,
        unsigned: bool,
        rounding: Rounding,
        fbits: u8,
        rn: u8,
        rd: u8,
    },
    /// SCVTF/UCVTF (intero o virgola fissa).
    FromInt {
        ty: u8,
        sf: bool,
        unsigned: bool,
        fbits: u8,
        rn: u8,
        rd: u8,
    },
    MovToGp {
        kind: MovKind,
        rn: u8,
        rd: u8,
    },
    MovFromGp {
        kind: MovKind,
        rn: u8,
        rd: u8,
    },

    VThreeSame {
        scalar: bool,
        q: bool,
        u: bool,
        a: bool,
        sz: bool,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    VTwoMisc {
        scalar: bool,
        q: bool,
        u: bool,
        a: bool,
        sz: bool,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    VAcross {
        a: bool,
        max_num: bool,
        rn: u8,
        rd: u8,
    },
    VPairScalar {
        a: bool,
        sz: bool,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    VFixed {
        scalar: bool,
        q: bool,
        u: bool,
        sz: bool,
        fbits: u8,
        to_int: bool,
        rn: u8,
        rd: u8,
    },
    VIndexed {
        scalar: bool,
        q: bool,
        u: bool,
        sz: bool,
        index: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
}

fn ok(i: FpInsn) -> Insn {
    Insn::Simd(SimdInsn::Fp(i))
}

#[inline]
fn m(w: u32, mask: u32, value: u32) -> bool {
    w & mask == value
}

/// Classi FP scalari (bit 30 = 0, 28:24 = 1111x).
pub fn decode(w: u32) -> Insn {
    let rd = field(w, 4, 0) as u8;
    let rn = field(w, 9, 5) as u8;
    let rm = field(w, 20, 16) as u8;
    let ty = field(w, 23, 22) as u8;
    let sf = bit(w, 31);
    if bit(w, 29) {
        return Insn::Undefined; // S = 1
    }
    // FP <-> virgola fissa
    if m(w, 0x5F20_0000, 0x1E00_0000) {
        let scale = field(w, 15, 10) as u8;
        if ty > 1 || !sf && scale < 32 {
            return Insn::Undefined;
        }
        let fbits = 64 - scale;
        return match (field(w, 20, 19), field(w, 18, 16)) {
            (0, 2) => ok(FpInsn::FromInt { ty, sf, unsigned: false, fbits, rn, rd }),
            (0, 3) => ok(FpInsn::FromInt { ty, sf, unsigned: true, fbits, rn, rd }),
            (3, 0) => ok(FpInsn::ToInt { ty, sf, unsigned: false, rounding: Rounding::Zero, fbits, rn, rd }),
            (3, 1) => ok(FpInsn::ToInt { ty, sf, unsigned: true, rounding: Rounding::Zero, fbits, rn, rd }),
            _ => Insn::Undefined,
        };
    }
    if bit(w, 31) && !m(w, 0x5F20_FC00, 0x1E20_0000) {
        return Insn::Undefined; // M = 1 fuori dalle conversioni con interi
    }
    // FP <-> intero
    if m(w, 0x5F20_FC00, 0x1E20_0000) {
        let rmode = field(w, 20, 19) as u8;
        let opcode = field(w, 18, 16);
        return match opcode {
            0 | 1 if ty <= 1 => ok(FpInsn::ToInt {
                ty,
                sf,
                unsigned: opcode == 1,
                rounding: rmode_rounding(rmode),
                fbits: 0,
                rn,
                rd,
            }),
            2 | 3 if ty <= 1 && rmode == 0 => {
                ok(FpInsn::FromInt { ty, sf, unsigned: opcode == 3, fbits: 0, rn, rd })
            }
            4 | 5 if ty <= 1 && rmode == 0 => ok(FpInsn::ToInt {
                ty,
                sf,
                unsigned: opcode == 5,
                rounding: Rounding::TieAway,
                fbits: 0,
                rn,
                rd,
            }),
            6 | 7 => {
                let kind = match (sf, ty, rmode) {
                    (false, 0, 0) => MovKind::W,
                    (true, 1, 0) => MovKind::X,
                    (true, 2, 1) => MovKind::Top,
                    _ => return Insn::Undefined, // FMOV mezza precisione, FJCVTZS: non su v8.0
                };
                if opcode == 6 {
                    ok(FpInsn::MovToGp { kind, rn, rd })
                } else {
                    ok(FpInsn::MovFromGp { kind, rn, rd })
                }
            }
            _ => Insn::Undefined,
        };
    }
    if m(w, 0x5F00_0000, 0x1F00_0000) {
        if ty > 1 {
            return Insn::Undefined;
        }
        let (o1, o0) = (bit(w, 21), bit(w, 15));
        return ok(FpInsn::Dp3 { ty, neg_a: o1, neg_n: o1 != o0, ra: field(w, 14, 10) as u8, rm, rn, rd });
    }
    if !bit(w, 21) {
        return Insn::Undefined;
    }
    match field(w, 11, 10) {
        0b01 => {
            if ty > 1 {
                return Insn::Undefined;
            }
            ok(FpInsn::CondCmp {
                ty,
                signal: bit(w, 4),
                cond: field(w, 15, 12) as u8,
                nzcv: field(w, 3, 0) as u8,
                rm,
                rn,
            })
        }
        0b10 => {
            let opcode = field(w, 15, 12) as u8;
            if ty > 1 || opcode > 8 {
                return Insn::Undefined;
            }
            ok(FpInsn::Dp2 { ty, opcode, rm, rn, rd })
        }
        0b11 => {
            if ty > 1 {
                return Insn::Undefined;
            }
            ok(FpInsn::CondSel { ty, cond: field(w, 15, 12) as u8, rm, rn, rd })
        }
        _ => {
            if field(w, 12, 10) == 0b100 {
                if ty > 1 || field(w, 9, 5) != 0 {
                    return Insn::Undefined;
                }
                let imm = fp::expand_imm(fmt_of(ty), field(w, 20, 13) as u64);
                return ok(FpInsn::Imm { ty, imm, rd });
            }
            if field(w, 13, 10) == 0b1000 {
                if ty > 1 || field(w, 15, 14) != 0 || field(w, 2, 0) != 0 {
                    return Insn::Undefined;
                }
                let zero = bit(w, 3);
                return ok(FpInsn::Cmp { ty, zero, signal: bit(w, 4), rm, rn });
            }
            if field(w, 14, 10) == 0b10000 {
                let opcode = field(w, 20, 15) as u8;
                let valid = match opcode {
                    0..=3 | 8..=12 | 14 | 15 => ty <= 1,
                    4..=7 => {
                        let to = opcode & 3;
                        to != 2 && to != ty && ty != 2
                    }
                    _ => false,
                };
                if !valid {
                    return Insn::Undefined;
                }
                return ok(FpInsn::Dp1 { ty, opcode, rn, rd });
            }
            Insn::Undefined
        }
    }
}

/// Operazioni FP dentro le classi AdvSIMD (chiamato da `int::decode` con
/// la parola intera, già riconosciuta come appartenente a una classe).
pub fn decode_vec(w: u32) -> Insn {
    let rd = field(w, 4, 0) as u8;
    let rn = field(w, 9, 5) as u8;
    let rm = field(w, 20, 16) as u8;
    let q = bit(w, 30);
    let u = bit(w, 29);
    let scalar = field(w, 31, 30) == 0b01 && bit(w, 28);
    let a = bit(w, 23);
    let sz = bit(w, 22);
    let vec_ok = scalar || !(sz && !q);

    // Shift per immediato: SCVTF/UCVTF e FCVTZS/FCVTZU in virgola fissa.
    if m(w, 0x9F80_0400, 0x0F00_0400) || m(w, 0xDF80_0400, 0x5F00_0400) {
        let immh = field(w, 22, 19);
        let immhb = field(w, 22, 16);
        let (sz, esize) = if immh & 0b1000 != 0 {
            (true, 64)
        } else if immh & 0b0100 != 0 {
            (false, 32)
        } else {
            return Insn::Undefined; // mezza precisione (FP16)
        };
        if !scalar && sz && !q {
            return Insn::Undefined;
        }
        let fbits = (2 * esize - immhb) as u8;
        let to_int = field(w, 15, 11) == 0b11111;
        return ok(FpInsn::VFixed { scalar, q, u, sz, fbits, to_int, rn, rd });
    }
    // Elemento indicizzato: FMLA, FMLS, FMUL, FMULX.
    if m(w, 0x9F00_0400, 0x0F00_0000) || m(w, 0xDF00_0400, 0x5F00_0000) {
        let opcode = field(w, 15, 12) as u8;
        if !a || u && opcode != 0b1001 {
            return Insn::Undefined;
        }
        let (h, l, mb) = (field(w, 11, 11), field(w, 21, 21), field(w, 20, 20));
        let rm = ((mb << 4) | field(w, 19, 16)) as u8;
        let index = if sz {
            if l != 0 || !scalar && !q {
                return Insn::Undefined;
            }
            h
        } else {
            (h << 1) | l
        };
        return ok(FpInsn::VIndexed { scalar, q, u, sz, index: index as u8, opcode, rm, rn, rd });
    }
    // Riduzione tra corsie o a coppie scalare (classe 11000 ... 10).
    if field(w, 21, 17) == 0b11000 && field(w, 11, 10) == 0b10 {
        let opcode = field(w, 16, 12) as u8;
        if !u {
            return Insn::Undefined;
        }
        if scalar {
            let valid = matches!((a, opcode), (_, 0b01100) | (false, 0b01101) | (_, 0b01111));
            if !valid {
                return Insn::Undefined;
            }
            return ok(FpInsn::VPairScalar { a, sz, opcode, rn, rd });
        }
        if sz || !q || !matches!(opcode, 0b01100 | 0b01111) {
            return Insn::Undefined;
        }
        return ok(FpInsn::VAcross { a, max_num: opcode == 0b01100, rn, rd });
    }
    // Three same (opcode 11xxx).
    if bit(w, 21) && bit(w, 10) {
        let opcode = field(w, 15, 11) as u8;
        let valid = if scalar {
            matches!(
                (u, a, opcode),
                (false, false, 0b11011 | 0b11100 | 0b11111)
                    | (false, true, 0b11111)
                    | (true, false, 0b11100 | 0b11101)
                    | (true, true, 0b11010 | 0b11100 | 0b11101)
            )
        } else {
            matches!(
                (u, a, opcode),
                (false, false, 0b11000..=0b11100 | 0b11110 | 0b11111)
                    | (false, true, 0b11000 | 0b11001 | 0b11010 | 0b11110 | 0b11111)
                    | (true, false, 0b11000 | 0b11010 | 0b11011 | 0b11100 | 0b11101 | 0b11110 | 0b11111)
                    | (true, true, 0b11000 | 0b11010 | 0b11100 | 0b11101 | 0b11110)
            )
        };
        if !valid || !vec_ok {
            return Insn::Undefined;
        }
        return ok(FpInsn::VThreeSame { scalar, q, u, a, sz, opcode, rm, rn, rd });
    }
    // Two-reg misc.
    if field(w, 21, 17) == 0b10000 && field(w, 11, 10) == 0b10 {
        let opcode = field(w, 16, 12) as u8;
        let valid = match (u, a, opcode) {
            (false, false, 0b10110 | 0b10111) => !scalar, // FCVTN, FCVTL
            (true, false, 0b10110) => sz,                 // FCVTXN
            (false, _, 0b11000 | 0b11001) | (true, false, 0b11000) | (true, _, 0b11001) => !scalar && vec_ok, // FRINT*
            (_, _, 0b11010 | 0b11011) | (_, false, 0b11100) => vec_ok, // FCVT*
            (_, true, 0b11100) => !scalar && !sz,                      // URECPE, URSQRTE
            (_, _, 0b11101) => vec_ok,                                 // SCVTF/UCVTF, FRECPE, FRSQRTE
            (false, true, 0b11111) => scalar,                          // FRECPX
            (true, true, 0b11111) => !scalar && vec_ok,                // FSQRT
            (_, true, 0b01100 | 0b01101) | (false, true, 0b01110) => vec_ok, // FCM* #0
            (_, true, 0b01111) => !scalar && vec_ok,                   // FABS, FNEG
            _ => false,
        };
        if !valid {
            return Insn::Undefined;
        }
        return ok(FpInsn::VTwoMisc { scalar, q, u, a, sz, opcode, rn, rd });
    }
    Insn::Undefined
}

// ------------------------------------------------------------------
// Esecuzione
// ------------------------------------------------------------------

fn get(cpu: &Cpu, r: u8, f: Fmt) -> u64 {
    elem(cpu.v[r as usize], 0, f.n)
}

fn put(cpu: &mut Cpu, r: u8, x: u64) {
    cpu.v[r as usize] = x as u128;
}

/// Operazione binaria a due operandi dell'AdvSIMD (per elemento).
#[allow(clippy::too_many_arguments)]
fn binop(f: Fmt, u: bool, a: bool, opcode: u8, x: u64, y: u64, acc: u64, ctx: &mut Ctx) -> u64 {
    let ones = crate::bits::ones(f.n);
    let b = |v: bool| if v { ones } else { 0 };
    match (u, a, opcode) {
        (false, false, 0b11000) => fp::max_min(f, x, y, true, true, ctx),
        (false, true, 0b11000) => fp::max_min(f, x, y, false, true, ctx),
        (false, false, 0b11001) => fp::mul_add(f, acc, x, y, ctx),
        (false, true, 0b11001) => fp::mul_add(f, acc, f.neg(x), y, ctx),
        (false, false, 0b11010) => fp::add(f, x, y, ctx),
        (false, true, 0b11010) => fp::sub(f, x, y, ctx),
        (false, false, 0b11011) => fp::mul_x(f, x, y, true, ctx),
        (false, false, 0b11100) => b(fp::compare_eq(f, x, y, ctx)),
        (false, false, 0b11110) => fp::max_min(f, x, y, true, false, ctx),
        (false, true, 0b11110) => fp::max_min(f, x, y, false, false, ctx),
        (false, false, 0b11111) => fp::step_fused(f, x, y, false, ctx),
        (false, true, 0b11111) => fp::step_fused(f, x, y, true, ctx),
        (true, false, 0b11000) => fp::max_min(f, x, y, true, true, ctx),
        (true, true, 0b11000) => fp::max_min(f, x, y, false, true, ctx),
        (true, false, 0b11010) => fp::add(f, x, y, ctx),
        (true, true, 0b11010) => f.abs(fp::sub(f, x, y, ctx)),
        (true, false, 0b11011) => fp::mul(f, x, y, ctx),
        (true, false, 0b11100) => b(fp::compare_ge(f, x, y, false, ctx)),
        (true, true, 0b11100) => b(fp::compare_ge(f, x, y, true, ctx)),
        (true, false, 0b11101) => b(fp::compare_ge(f, f.abs(x), f.abs(y), false, ctx)),
        (true, true, 0b11101) => b(fp::compare_ge(f, f.abs(x), f.abs(y), true, ctx)),
        (true, false, 0b11110) => fp::max_min(f, x, y, true, false, ctx),
        (true, true, 0b11110) => fp::max_min(f, x, y, false, false, ctx),
        (true, false, 0b11111) => fp::div(f, x, y, ctx),
        _ => unreachable!("FP three-same {u} {a} {opcode:05b}"),
    }
}

/// FMAXNMP, FMINNMP, FADDP, FMAXP, FMINP (FABD ha lo stesso opcode di FADDP).
fn is_pairwise(u: bool, a: bool, opcode: u8) -> bool {
    u && (matches!(opcode, 0b11000 | 0b11110) || opcode == 0b11010 && !a)
}

pub(crate) fn exec(cpu: &mut Cpu, i: FpInsn) {
    let mut ctx = Ctx::new(cpu.fpcr);
    exec_inner(cpu, i, &mut ctx);
    cpu.fpsr |= ctx.flags;
}

fn exec_inner(cpu: &mut Cpu, i: FpInsn, ctx: &mut Ctx) {
    let rm_default = ctx.rounding();
    match i {
        FpInsn::Dp1 { ty, opcode, rn, rd } => {
            let f = fmt_of(ty);
            let x = get(cpu, rn, f);
            let r = match opcode {
                0 => x,
                1 => f.abs(x),
                2 => f.neg(x),
                3 => fp::sqrt(f, x, ctx),
                4..=7 => fp::convert(f, fmt_of(opcode & 3), x, rm_default, ctx),
                8 => fp::round_int(f, x, Rounding::TieEven, false, ctx),
                9 => fp::round_int(f, x, Rounding::PosInf, false, ctx),
                10 => fp::round_int(f, x, Rounding::NegInf, false, ctx),
                11 => fp::round_int(f, x, Rounding::Zero, false, ctx),
                12 => fp::round_int(f, x, Rounding::TieAway, false, ctx),
                14 => fp::round_int(f, x, rm_default, true, ctx),
                _ => fp::round_int(f, x, rm_default, false, ctx),
            };
            put(cpu, rd, r);
        }
        FpInsn::Dp2 { ty, opcode, rm, rn, rd } => {
            let f = fmt_of(ty);
            let (x, y) = (get(cpu, rn, f), get(cpu, rm, f));
            let r = match opcode {
                0 => fp::mul(f, x, y, ctx),
                1 => fp::div(f, x, y, ctx),
                2 => fp::add(f, x, y, ctx),
                3 => fp::sub(f, x, y, ctx),
                4 => fp::max_min(f, x, y, true, false, ctx),
                5 => fp::max_min(f, x, y, false, false, ctx),
                6 => fp::max_min(f, x, y, true, true, ctx),
                7 => fp::max_min(f, x, y, false, true, ctx),
                _ => f.neg(fp::mul(f, x, y, ctx)),
            };
            put(cpu, rd, r);
        }
        FpInsn::Dp3 { ty, neg_a, neg_n, ra, rm, rn, rd } => {
            let f = fmt_of(ty);
            let mut a = get(cpu, ra, f);
            let mut n = get(cpu, rn, f);
            if neg_a {
                a = f.neg(a);
            }
            if neg_n {
                n = f.neg(n);
            }
            let r = fp::mul_add(f, a, n, get(cpu, rm, f), ctx);
            put(cpu, rd, r);
        }
        FpInsn::Cmp { ty, zero, signal, rm, rn } => {
            let f = fmt_of(ty);
            let y = if zero { 0 } else { get(cpu, rm, f) };
            cpu.nzcv = fp::compare(f, get(cpu, rn, f), y, signal, ctx) << 28;
        }
        FpInsn::CondCmp { ty, signal, cond, nzcv, rm, rn } => {
            let f = fmt_of(ty);
            cpu.nzcv = if cpu.condition_holds(cond) {
                fp::compare(f, get(cpu, rn, f), get(cpu, rm, f), signal, ctx) << 28
            } else {
                (nzcv as u32) << 28
            };
        }
        FpInsn::CondSel { ty, cond, rm, rn, rd } => {
            let f = fmt_of(ty);
            let r = if cpu.condition_holds(cond) { get(cpu, rn, f) } else { get(cpu, rm, f) };
            put(cpu, rd, r);
        }
        FpInsn::Imm { imm, rd, .. } => put(cpu, rd, imm),
        FpInsn::ToInt { ty, sf, unsigned, rounding, fbits, rn, rd } => {
            let f = fmt_of(ty);
            let bits = if sf { 64 } else { 32 };
            let r = fp::to_fixed(f, get(cpu, rn, f), fbits as u32, unsigned, bits, rounding, ctx);
            cpu.set_x(rd, r);
        }
        FpInsn::FromInt { ty, sf, unsigned, fbits, rn, rd } => {
            let f = fmt_of(ty);
            let bits = if sf { 64 } else { 32 };
            let r = fp::from_fixed(f, cpu.xr(rn), fbits as u32, unsigned, bits, rm_default, ctx);
            put(cpu, rd, r);
        }
        FpInsn::MovToGp { kind, rn, rd } => {
            let v = cpu.v[rn as usize];
            let x = match kind {
                MovKind::W => v as u32 as u64,
                MovKind::X => v as u64,
                MovKind::Top => (v >> 64) as u64,
            };
            cpu.set_x(rd, x);
        }
        FpInsn::MovFromGp { kind, rn, rd } => {
            let x = cpu.xr(rn);
            cpu.v[rd as usize] = match kind {
                MovKind::W => x as u32 as u128,
                MovKind::X => x as u128,
                MovKind::Top => (cpu.v[rd as usize] & u64::MAX as u128) | ((x as u128) << 64),
            };
        }
        FpInsn::VThreeSame { scalar, q, u, a, sz, opcode, rm, rn, rd } => {
            let f = if sz { D } else { S };
            let datasize = if scalar {
                f.n
            } else if q {
                128
            } else {
                64
            };
            let n = (datasize / f.n) as usize;
            let (va, vb, vd) = (cpu.v[rn as usize], cpu.v[rm as usize], cpu.v[rd as usize]);
            let mut r = 0u128;
            for e in 0..n {
                let (x, y) = if !scalar && is_pairwise(u, a, opcode) {
                    let src = if 2 * e < n { va } else { vb };
                    let k = (2 * e) % n;
                    (elem(src, k, f.n), elem(src, k + 1, f.n))
                } else {
                    (elem(va, e, f.n), elem(vb, e, f.n))
                };
                let res = binop(f, u, a, opcode, x, y, elem(vd, e, f.n), ctx);
                r = set_elem(r, e, f.n, res);
            }
            cpu.v[rd as usize] = r;
        }
        FpInsn::VPairScalar { a, sz, opcode, rn, rd } => {
            let f = if sz { D } else { S };
            let v = cpu.v[rn as usize];
            let (x, y) = (elem(v, 0, f.n), elem(v, 1, f.n));
            let r = match (a, opcode) {
                (false, 0b01100) => fp::max_min(f, x, y, true, true, ctx),
                (true, 0b01100) => fp::max_min(f, x, y, false, true, ctx),
                (false, 0b01101) => fp::add(f, x, y, ctx),
                (false, _) => fp::max_min(f, x, y, true, false, ctx),
                (true, _) => fp::max_min(f, x, y, false, false, ctx),
            };
            put(cpu, rd, r);
        }
        FpInsn::VAcross { a, max_num, rn, rd } => {
            // Riduzione ad albero: (e0 op e1) op (e2 op e3), come Reduce().
            let v = cpu.v[rn as usize];
            let op = |x, y, ctx: &mut Ctx| fp::max_min(S, x, y, !a, max_num, ctx);
            let lo = op(elem(v, 0, 32), elem(v, 1, 32), ctx);
            let hi = op(elem(v, 2, 32), elem(v, 3, 32), ctx);
            let r = op(lo, hi, ctx);
            put(cpu, rd, r);
        }
        FpInsn::VFixed { scalar, q, u, sz, fbits, to_int, rn, rd } => {
            let f = if sz { D } else { S };
            let datasize = if scalar {
                f.n
            } else if q {
                128
            } else {
                64
            };
            let v = cpu.v[rn as usize];
            let mut r = 0u128;
            for e in 0..(datasize / f.n) as usize {
                let x = elem(v, e, f.n);
                let res = if to_int {
                    fp::to_fixed(f, x, fbits as u32, u, f.n, Rounding::Zero, ctx)
                } else {
                    fp::from_fixed(f, x, fbits as u32, u, f.n, rm_default, ctx)
                };
                r = set_elem(r, e, f.n, res);
            }
            cpu.v[rd as usize] = r;
        }
        FpInsn::VIndexed { scalar, q, u, sz, index, opcode, rm, rn, rd } => {
            let f = if sz { D } else { S };
            let datasize = if scalar {
                f.n
            } else if q {
                128
            } else {
                64
            };
            let y = elem(cpu.v[rm as usize], index as usize, f.n);
            let (va, vd) = (cpu.v[rn as usize], cpu.v[rd as usize]);
            let mut r = 0u128;
            for e in 0..(datasize / f.n) as usize {
                let x = elem(va, e, f.n);
                let acc = elem(vd, e, f.n);
                let res = match (u, opcode) {
                    (false, 0b0001) => fp::mul_add(f, acc, x, y, ctx),
                    (false, 0b0101) => fp::mul_add(f, acc, f.neg(x), y, ctx),
                    (false, _) => fp::mul(f, x, y, ctx),
                    (true, _) => fp::mul_x(f, x, y, true, ctx),
                };
                r = set_elem(r, e, f.n, res);
            }
            cpu.v[rd as usize] = r;
        }
        FpInsn::VTwoMisc { scalar, q, u, a, sz, opcode, rn, rd } => {
            two_misc(cpu, scalar, q, u, a, sz, opcode, rn, rd, ctx)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn two_misc(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    a: bool,
    sz: bool,
    opcode: u8,
    rn: u8,
    rd: u8,
    ctx: &mut Ctx,
) {
    let v = cpu.v[rn as usize];
    let d = cpu.v[rd as usize];
    let rm_default = ctx.rounding();
    // Conversioni che cambiano dimensione.
    match (u, opcode) {
        (false, 0b10110) | (true, 0b10110) => {
            // FCVTN / FCVTXN: da 2*esize a esize, metà bassa o alta.
            let (from, to) = if sz { (D, S) } else { (S, H) };
            let n = if scalar { 1 } else { (64 / to.n) as usize };
            let rounding = if u { Rounding::Odd } else { rm_default };
            let mut r = 0u128;
            for e in 0..n {
                r = set_elem(r, e, to.n, fp::convert(from, to, elem(v, e, from.n), rounding, ctx));
            }
            cpu.v[rd as usize] = if !scalar && q { (d & u64::MAX as u128) | (r << 64) } else { r };
            return;
        }
        (false, 0b10111) => {
            // FCVTL: da esize a 2*esize, dalla metà bassa o alta.
            let (from, to) = if sz { (S, D) } else { (H, S) };
            let n = (64 / from.n) as usize;
            let part = q as usize;
            let mut r = 0u128;
            for e in 0..n {
                r = set_elem(
                    r,
                    e,
                    to.n,
                    fp::convert(from, to, elem(v, part * n + e, from.n), rm_default, ctx),
                );
            }
            cpu.v[rd as usize] = r;
            return;
        }
        _ => {}
    }
    let f = if sz { D } else { S };
    let datasize = if scalar {
        f.n
    } else if q {
        128
    } else {
        64
    };
    let ones = crate::bits::ones(f.n);
    let mut r = 0u128;
    for e in 0..(datasize / f.n) as usize {
        let x = elem(v, e, f.n);
        let res = match (u, a, opcode) {
            (false, false, 0b11000) => fp::round_int(f, x, Rounding::TieEven, false, ctx),
            (false, true, 0b11000) => fp::round_int(f, x, Rounding::PosInf, false, ctx),
            (false, false, 0b11001) => fp::round_int(f, x, Rounding::NegInf, false, ctx),
            (false, true, 0b11001) => fp::round_int(f, x, Rounding::Zero, false, ctx),
            (true, false, 0b11000) => fp::round_int(f, x, Rounding::TieAway, false, ctx),
            (true, false, 0b11001) => fp::round_int(f, x, rm_default, true, ctx),
            (true, true, 0b11001) => fp::round_int(f, x, rm_default, false, ctx),
            (_, false, 0b11010) => fp::to_fixed(f, x, 0, u, f.n, Rounding::TieEven, ctx),
            (_, true, 0b11010) => fp::to_fixed(f, x, 0, u, f.n, Rounding::PosInf, ctx),
            (_, false, 0b11011) => fp::to_fixed(f, x, 0, u, f.n, Rounding::NegInf, ctx),
            (_, true, 0b11011) => fp::to_fixed(f, x, 0, u, f.n, Rounding::Zero, ctx),
            (_, false, 0b11100) => fp::to_fixed(f, x, 0, u, f.n, Rounding::TieAway, ctx),
            (false, true, 0b11100) => fp::unsigned_recip_estimate(x as u32) as u64,
            (true, true, 0b11100) => fp::unsigned_rsqrt_estimate(x as u32) as u64,
            (_, false, 0b11101) => fp::from_fixed(f, x, 0, u, f.n, rm_default, ctx),
            (false, true, 0b11101) => fp::recip_estimate(f, x, ctx),
            (true, true, 0b11101) => fp::rsqrt_estimate(f, x, ctx),
            (false, true, 0b11111) => fp::recpx(f, x, ctx),
            (true, true, 0b11111) => fp::sqrt(f, x, ctx),
            (false, true, 0b01100) => {
                if fp::compare_ge(f, x, 0, true, ctx) {
                    ones
                } else {
                    0
                }
            }
            (true, true, 0b01100) => {
                if fp::compare_ge(f, x, 0, false, ctx) {
                    ones
                } else {
                    0
                }
            }
            (false, true, 0b01101) => {
                if fp::compare_eq(f, x, 0, ctx) {
                    ones
                } else {
                    0
                }
            }
            (true, true, 0b01101) => {
                if fp::compare_ge(f, 0, x, false, ctx) {
                    ones
                } else {
                    0
                }
            }
            (false, true, 0b01110) => {
                if fp::compare_ge(f, 0, x, true, ctx) {
                    ones
                } else {
                    0
                }
            }
            (false, true, 0b01111) => f.abs(x),
            (true, true, 0b01111) => f.neg(x),
            _ => unreachable!("FP two-misc {u} {a} {opcode:05b}"),
        };
        r = set_elem(r, e, f.n, res);
    }
    cpu.v[rd as usize] = r;
}
