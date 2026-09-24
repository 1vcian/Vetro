//! AdvSIMD intero (vettoriale e scalare), ARMv8.0.
//!
//! Le operazioni in virgola mobile che condividono queste classi di codifica
//! (FADD vettoriale, FCVTZS, FMLA indicizzato, ...) vengono inoltrate a
//! `fpinsn`.

use super::SimdInsn;
use super::fpinsn;
use super::vreg::{clip, elem, emask, set_elem, sx};
use crate::bits::{bit, field};
use crate::decode::Insn;
use crate::state::Cpu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovImmOp {
    Movi,
    Mvni,
    Orr,
    Bic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOp {
    /// DUP (elemento), anche scalare (`MOV Bd, Vn.B[i]`).
    DupElem,
    DupGen,
    InsGen,
    InsElem,
    Smov,
    Umov,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntInsn {
    ThreeSame {
        scalar: bool,
        q: bool,
        u: bool,
        size: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    ThreeDiff {
        scalar: bool,
        q: bool,
        u: bool,
        size: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    TwoMisc {
        scalar: bool,
        q: bool,
        u: bool,
        size: u8,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    Across {
        q: bool,
        u: bool,
        size: u8,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    /// ADDP scalare (Dd = somma delle due corsie a 64 bit).
    AddpScalar {
        rn: u8,
        rd: u8,
    },
    ShiftImm {
        scalar: bool,
        q: bool,
        u: bool,
        esize: u8,
        shift: u8,
        opcode: u8,
        rn: u8,
        rd: u8,
    },
    Indexed {
        scalar: bool,
        q: bool,
        u: bool,
        size: u8,
        index: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Copy {
        op: CopyOp,
        scalar: bool,
        q: bool,
        esize: u8,
        index: u8,
        index2: u8,
        rn: u8,
        rd: u8,
    },
    MovImm {
        q: bool,
        op: MovImmOp,
        imm: u64,
        rd: u8,
    },
    Perm {
        q: bool,
        size: u8,
        opcode: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Ext {
        q: bool,
        imm4: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Tbl {
        q: bool,
        len: u8,
        tbx: bool,
        rm: u8,
        rn: u8,
        rd: u8,
    },
}

fn ok(i: IntInsn) -> Insn {
    Insn::Simd(SimdInsn::Int(i))
}

#[inline]
fn m(w: u32, mask: u32, value: u32) -> bool {
    w & mask == value
}

pub fn decode(w: u32) -> Insn {
    let rd = field(w, 4, 0) as u8;
    let rn = field(w, 9, 5) as u8;
    let rm = field(w, 20, 16) as u8;
    let q = bit(w, 30);
    let u = bit(w, 29);
    let size = field(w, 23, 22) as u8;

    // Crittografia (AES, SHA1, SHA256): valida su Cortex-A53, non ancora scritta.
    if m(w, 0xFF3E_0C00, 0x4E28_0800) || m(w, 0xFF20_8C00, 0x5E00_0000) || m(w, 0xFF3E_0C00, 0x5E28_0800) {
        return super::crypto::decode(w);
    }

    let scalar = field(w, 31, 30) == 0b01 && bit(w, 28);
    if !scalar && (bit(w, 31) || bit(w, 28)) {
        return Insn::Undefined;
    }

    // Copie
    if m(w, 0x9FE0_8400, 0x0E00_0400) || m(w, 0xDFE0_8400, 0x5E00_0400) {
        return copy(w, scalar, q, rn, rd);
    }
    // Immediato modificato / shift per immediato (vettoriale)
    if !scalar && m(w, 0x9F80_0400, 0x0F00_0400) {
        if field(w, 22, 19) == 0 {
            return mod_imm(w, q, rd);
        }
        return shift_imm(w, false, q, u, rn, rd);
    }
    if scalar && m(w, 0xDF80_0400, 0x5F00_0400) {
        if field(w, 22, 19) == 0 {
            return Insn::Undefined;
        }
        return shift_imm(w, true, q, u, rn, rd);
    }
    // Elemento indicizzato
    if m(w, 0x9F00_0400, 0x0F00_0000) || m(w, 0xDF00_0400, 0x5F00_0000) {
        return indexed(w, scalar, q, u, size, rd, rn);
    }
    // TBL/TBX, permutazioni, EXT (solo vettoriali)
    if !scalar {
        if m(w, 0xBF20_8C00, 0x0E00_0000) {
            if size != 0 {
                return Insn::Undefined;
            }
            return ok(IntInsn::Tbl { q, len: field(w, 14, 13) as u8, tbx: bit(w, 12), rm, rn, rd });
        }
        if m(w, 0xBF20_8C00, 0x0E00_0800) {
            let opcode = field(w, 14, 12) as u8;
            if opcode & 3 == 0 || size == 3 && !q {
                return Insn::Undefined;
            }
            return ok(IntInsn::Perm { q, size, opcode, rm, rn, rd });
        }
        if m(w, 0xBF20_8400, 0x2E00_0000) {
            let imm4 = field(w, 14, 11) as u8;
            if size != 0 || !q && imm4 >= 8 {
                return Insn::Undefined;
            }
            return ok(IntInsn::Ext { q, imm4, rm, rn, rd });
        }
    }
    // Classi con dimensione: la parte 28:24 è 01110 (vettoriale) o 11110 (scalare).
    if field(w, 28, 24) & 0b01111 != 0b01110 {
        return Insn::Undefined;
    }
    if bit(w, 21) && bit(w, 10) {
        return three_same(w, scalar, q, u, size, rm, rn, rd);
    }
    if bit(w, 21) && field(w, 11, 10) == 0b00 {
        return three_diff(w, scalar, q, u, size, rm, rn, rd);
    }
    if field(w, 21, 17) == 0b10000 && field(w, 11, 10) == 0b10 {
        return two_misc(w, scalar, q, u, size, rn, rd);
    }
    if field(w, 21, 17) == 0b11000 && field(w, 11, 10) == 0b10 {
        let opcode = field(w, 16, 12) as u8;
        if scalar {
            return match (u, opcode) {
                (false, 0b11011) if size == 3 => ok(IntInsn::AddpScalar { rn, rd }),
                (true, 0b01100 | 0b01101 | 0b01111) => fpinsn::decode_vec(w),
                _ => Insn::Undefined,
            };
        }
        return across(w, q, u, size, opcode, rn, rd);
    }
    Insn::Undefined
}

fn copy(w: u32, scalar: bool, q: bool, rn: u8, rd: u8) -> Insn {
    let imm5 = field(w, 20, 16);
    let imm4 = field(w, 14, 11);
    let op = bit(w, 29);
    if imm5 & 0xF == 0 {
        return Insn::Undefined;
    }
    let size = imm5.trailing_zeros();
    let esize = 8u8 << size;
    let index = (imm5 >> (size + 1)) as u8;
    let copy = |op, q, index2| ok(IntInsn::Copy { op, scalar, q, esize, index, index2, rn, rd });
    if scalar {
        return if !op && imm4 == 0 { copy(CopyOp::DupElem, q, 0) } else { Insn::Undefined };
    }
    if op {
        if !q {
            return Insn::Undefined;
        }
        return copy(CopyOp::InsElem, q, (imm4 >> size) as u8);
    }
    match imm4 {
        0b0000 if !(size == 3 && !q) => copy(CopyOp::DupElem, q, 0),
        0b0001 if !(size == 3 && !q) => copy(CopyOp::DupGen, q, 0),
        0b0011 if q => copy(CopyOp::InsGen, q, 0),
        0b0101 if (q && size <= 2 || !q && size <= 1) => copy(CopyOp::Smov, q, 0),
        0b0111 if (q && size == 3 || !q && size <= 2) => copy(CopyOp::Umov, q, 0),
        _ => Insn::Undefined,
    }
}

fn mod_imm(w: u32, q: bool, rd: u8) -> Insn {
    let op = bit(w, 29);
    let cmode = field(w, 15, 12);
    if bit(w, 11) {
        return Insn::Undefined; // o2: FMOV (vettoriale, mezza precisione) richiede FP16
    }
    let imm8 = ((field(w, 18, 16) << 5) | field(w, 9, 5)) as u64;
    let rep = |v: u64, esize: u32| crate::bits::replicate(v, esize, 64);
    let imm = match cmode >> 1 {
        0 => rep(imm8, 32),
        1 => rep(imm8 << 8, 32),
        2 => rep(imm8 << 16, 32),
        3 => rep(imm8 << 24, 32),
        4 => rep(imm8, 16),
        5 => rep(imm8 << 8, 16),
        6 => {
            if cmode & 1 == 0 {
                rep((imm8 << 8) | 0xff, 32)
            } else {
                rep((imm8 << 16) | 0xffff, 32)
            }
        }
        _ => match (cmode & 1, op) {
            (0, false) => rep(imm8, 8),
            (0, true) => {
                let mut v = 0u64;
                for i in 0..8 {
                    if imm8 >> i & 1 != 0 {
                        v |= 0xff << (8 * i);
                    }
                }
                v
            }
            (1, false) => {
                let b = |n: u32| (imm8 >> n) & 1;
                let f = (b(7) << 31)
                    | ((b(6) ^ 1) << 30)
                    | (if b(6) != 0 { 0x1F << 25 } else { 0 })
                    | ((imm8 & 0x3f) << 19);
                rep(f, 32)
            }
            _ => {
                if !q {
                    return Insn::Undefined;
                }
                let b = |n: u32| (imm8 >> n) & 1;
                (b(7) << 63)
                    | ((b(6) ^ 1) << 62)
                    | (if b(6) != 0 { 0xFF << 54 } else { 0 })
                    | ((imm8 & 0x3f) << 48)
            }
        },
    };
    let opk = match (cmode, op) {
        (0b1110 | 0b1111, _) => MovImmOp::Movi,
        (c, false) if c & 0b1001 == 0b0001 || c & 0b1101 == 0b1001 => MovImmOp::Orr,
        (c, true) if c & 0b1001 == 0b0001 || c & 0b1101 == 0b1001 => MovImmOp::Bic,
        (_, false) => MovImmOp::Movi,
        (_, true) => MovImmOp::Mvni,
    };
    ok(IntInsn::MovImm { q, op: opk, imm, rd })
}

fn shift_imm(w: u32, scalar: bool, q: bool, u: bool, rn: u8, rd: u8) -> Insn {
    let immh = field(w, 22, 19);
    let immhb = field(w, 22, 16);
    let opcode = field(w, 15, 11) as u8;
    let lg = 31 - immh.leading_zeros(); // 0..3
    let esize = 8u32 << lg;
    let right = (2 * esize - immhb) as u8;
    let left = (immhb - esize) as u8;
    let narrow = matches!(opcode, 0b10000..=0b10011);
    let wide = opcode == 0b10100;
    if matches!(opcode, 0b11100 | 0b11111) {
        return fpinsn::decode_vec(w);
    }
    if narrow || wide {
        if immh & 0b1000 != 0 {
            return Insn::Undefined;
        }
        if scalar && (wide || opcode == 0b10000 && !u || opcode == 0b10001 && !u) {
            return Insn::Undefined; // SHRN/RSHRN/SSHLL non hanno forma scalare
        }
    } else if scalar {
        let only64 = matches!(opcode, 0b00000 | 0b00010 | 0b00100 | 0b00110 | 0b01000 | 0b01010);
        if only64 && immh & 0b1000 == 0 {
            return Insn::Undefined;
        }
    } else if immh & 0b1000 != 0 && !q {
        return Insn::Undefined;
    }
    let (shift, valid) = match (u, opcode) {
        (_, 0b00000 | 0b00010 | 0b00100 | 0b00110) => (right, true),
        (true, 0b01000) => (right, true),        // SRI
        (_, 0b01010) => (left, true),            // SHL / SLI
        (true, 0b01100) => (left, true),         // SQSHLU
        (_, 0b01110) => (left, true),            // SQSHL / UQSHL
        (_, 0b10000..=0b10011) => (right, true), // (R)SHRN, SQ(R)SHR(U)N, UQ(R)SHRN
        (_, 0b10100) => (left, true),            // SSHLL / USHLL
        _ => (0, false),
    };
    if !valid {
        return Insn::Undefined;
    }
    ok(IntInsn::ShiftImm { scalar, q, u, esize: esize as u8, shift, opcode, rn, rd })
}

fn indexed(w: u32, scalar: bool, q: bool, u: bool, size: u8, rd: u8, rn: u8) -> Insn {
    let opcode = field(w, 15, 12) as u8;
    if matches!((u, opcode), (_, 0b0001 | 0b0101 | 0b1001)) {
        return fpinsn::decode_vec(w);
    }
    let (h, l, mb) = (field(w, 11, 11), field(w, 21, 21), field(w, 20, 20));
    let (index, rm) = match size {
        1 => ((h << 2) | (l << 1) | mb, field(w, 19, 16)),
        2 => ((h << 1) | l, (mb << 4) | field(w, 19, 16)),
        _ => return Insn::Undefined,
    };
    let valid = match (u, opcode) {
        (true, 0b0000 | 0b0100) | (false, 0b1000) => !scalar, // MLA, MLS, MUL
        (_, 0b0010 | 0b0110 | 0b1010) => !scalar,             // [SU]MLAL, [SU]MLSL, [SU]MULL
        (false, 0b0011 | 0b0111 | 0b1011 | 0b1100 | 0b1101) => true, // SQDMLAL, SQDMLSL, SQDMULL, SQ(R)DMULH
        _ => false,
    };
    if !valid {
        return Insn::Undefined;
    }
    ok(IntInsn::Indexed { scalar, q, u, size, index: index as u8, opcode, rm: rm as u8, rn, rd })
}

#[allow(clippy::too_many_arguments)]
fn three_same(w: u32, scalar: bool, q: bool, u: bool, size: u8, rm: u8, rn: u8, rd: u8) -> Insn {
    let opcode = field(w, 15, 11) as u8;
    if opcode >= 0b11000 {
        return fpinsn::decode_vec(w);
    }
    let s3 = size == 3;
    let valid = if scalar {
        match opcode {
            0b00001 | 0b00101 | 0b01001 | 0b01011 => true, // SQADD, SQSUB, SQSHL, SQRSHL (e U)
            0b00110 | 0b00111 | 0b01000 | 0b01010 | 0b10000 | 0b10001 => s3,
            0b10110 => size == 1 || size == 2, // SQDMULH, SQRDMULH
            _ => false,
        }
    } else {
        let no64 = !s3;
        let base = !(s3 && !q);
        base && match (u, opcode) {
            (_, 0b00011) => true, // logiche: size codifica l'operazione
            (_, 0b00000 | 0b00010 | 0b00100 | 0b01100 | 0b01101 | 0b01110 | 0b01111 | 0b10100 | 0b10101) => {
                no64
            }
            (_, 0b10010) => no64,         // MLA/MLS
            (false, 0b10011) => no64,     // MUL
            (true, 0b10011) => size == 0, // PMUL
            (_, 0b10110) => size == 1 || size == 2,
            (false, 0b10111) => true, // ADDP
            (true, 0b10111) => false,
            _ => true,
        }
    };
    if !valid {
        return Insn::Undefined;
    }
    let _ = w;
    ok(IntInsn::ThreeSame { scalar, q, u, size, opcode, rm, rn, rd })
}

#[allow(clippy::too_many_arguments)]
fn three_diff(w: u32, scalar: bool, q: bool, u: bool, size: u8, rm: u8, rn: u8, rd: u8) -> Insn {
    let opcode = field(w, 15, 12) as u8;
    let valid = if scalar {
        !u && matches!(opcode, 0b1001 | 0b1011 | 0b1101) && (size == 1 || size == 2)
    } else {
        match (u, opcode) {
            (false, 0b1110) => size == 0 || size == 3, // PMULL (64 bit con crittografia)
            (true, 0b1110) | (_, 0b1111) => false,
            (true, 0b1001 | 0b1011 | 0b1101) => false,
            (false, 0b1001 | 0b1011 | 0b1101) => size == 1 || size == 2,
            _ => size != 3,
        }
    };
    if !valid {
        return Insn::Undefined;
    }
    ok(IntInsn::ThreeDiff { scalar, q, u, size, opcode, rm, rn, rd })
}

fn two_misc(w: u32, scalar: bool, q: bool, u: bool, size: u8, rn: u8, rd: u8) -> Insn {
    let opcode = field(w, 16, 12) as u8;
    if opcode >= 0b01100 && !matches!(opcode, 0b10010..=0b10100) {
        // 01100–01111 e 10110–11111: virgola mobile (o URECPE/URSQRTE).
        return fpinsn::decode_vec(w);
    }
    let s3 = size == 3;
    let valid = if scalar {
        match (u, opcode) {
            (_, 0b00011 | 0b00111) => true, // SUQADD/USQADD, SQABS/SQNEG
            (_, 0b01000 | 0b01001) | (false, 0b01010) | (_, 0b01011) => s3, // CM* #0, ABS/NEG
            (_, 0b10100) | (true, 0b10010) => !s3, // SQXTN, UQXTN, SQXTUN
            _ => false,
        }
    } else {
        let base = !(s3 && !q);
        match (u, opcode) {
            (false, 0b00000) => size <= 2, // REV64
            (true, 0b00000) => size <= 1,  // REV32
            (false, 0b00001) => size == 0, // REV16
            (true, 0b00001) => false,
            (_, 0b00010 | 0b00110) => !s3,  // [SU]ADDLP, [SU]ADALP
            (_, 0b00011 | 0b00111) => base, // SUQADD, USQADD, SQABS, SQNEG
            (_, 0b00100) => !s3,            // CLS, CLZ
            (false, 0b00101) => size == 0,  // CNT
            (true, 0b00101) => size <= 1,   // NOT, RBIT
            (_, 0b01000 | 0b01001 | 0b01011) | (false, 0b01010) => base,
            (true, 0b01010) => false,
            (false, 0b10010) | (_, 0b10100) | (true, 0b10010) | (true, 0b10011) => !s3, // XTN, SQXTN, UQXTN, SQXTUN, SHLL
            _ => false,
        }
    };
    if !valid {
        return Insn::Undefined;
    }
    ok(IntInsn::TwoMisc { scalar, q, u, size, opcode, rn, rd })
}

#[allow(clippy::too_many_arguments)]
fn across(w: u32, q: bool, u: bool, size: u8, opcode: u8, rn: u8, rd: u8) -> Insn {
    let valid = match (u, opcode) {
        (_, 0b00011 | 0b01010 | 0b11010) | (false, 0b11011) => size <= 1 || size == 2 && q,
        (_, 0b01100 | 0b01111) => return fpinsn::decode_vec(w),
        _ => false,
    };
    if !valid {
        return Insn::Undefined;
    }
    ok(IntInsn::Across { q, u, size, opcode, rn, rd })
}

// ------------------------------------------------------------------
// Esecuzione
// ------------------------------------------------------------------

fn sat_s(x: i128, esize: u32, qc: &mut bool) -> u64 {
    let max = (1i128 << (esize - 1)) - 1;
    let min = -(1i128 << (esize - 1));
    let r = if x > max {
        *qc = true;
        max
    } else if x < min {
        *qc = true;
        min
    } else {
        x
    };
    (r as u64) & emask(esize)
}

fn sat_u(x: i128, esize: u32, qc: &mut bool) -> u64 {
    let max = emask(esize) as i128;
    let r = if x > max {
        *qc = true;
        max
    } else if x < 0 {
        *qc = true;
        0
    } else {
        x
    };
    r as u64
}

/// Valore di un elemento come intero con o senza segno.
#[inline]
fn ext(x: u64, esize: u32, signed: bool) -> i128 {
    if signed { sx(x, esize) as i128 } else { x as i128 }
}

/// Shift per registro (SSHL/USHL e varianti): `shift` è il byte basso con
/// segno di Vm. Restituisce il valore esatto (prima della saturazione).
fn shl_reg(x: i128, shift: i8, round: bool) -> i128 {
    let s = shift as i32;
    if s >= 0 {
        if s >= 64 {
            if x == 0 { 0 } else { x.signum() * (1i128 << 100) } // fuori scala: satura o tronca a 0
        } else {
            x << s
        }
    } else {
        let n = -s;
        if round {
            if n >= 65 { 0 } else { (x + (1i128 << (n - 1))) >> n }
        } else if n >= 64 {
            if x < 0 { -1 } else { 0 }
        } else {
            x >> n
        }
    }
}

fn clz(x: u64, esize: u32) -> u64 {
    (x.leading_zeros() - (64 - esize)) as u64
}

fn cls(x: u64, esize: u32) -> u64 {
    let s = sx(x, esize);
    let y = (s ^ (s >> 1)) as u64 & emask(esize);
    // conta gli zeri iniziali di y su esize-1 bit
    (y.leading_zeros() - (64 - esize) - 1) as u64
}

fn pmul8(a: u8, b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        if b >> i & 1 != 0 {
            r ^= a << i;
        }
    }
    r
}

fn pmul_wide(a: u64, b: u64) -> u128 {
    let mut r = 0u128;
    for i in 0..64 {
        if b >> i & 1 != 0 {
            r ^= (a as u128) << i;
        }
    }
    r
}

pub(crate) fn exec(cpu: &mut Cpu, i: IntInsn) {
    let mut qc = false;
    match i {
        IntInsn::ThreeSame { scalar, q, u, size, opcode, rm, rn, rd } => {
            three_same_exec(cpu, scalar, q, u, size, opcode, rm, rn, rd, &mut qc)
        }
        IntInsn::ThreeDiff { scalar, q, u, size, opcode, rm, rn, rd } => {
            three_diff_exec(cpu, scalar, q, u, size, opcode, rm, rn, rd, &mut qc)
        }
        IntInsn::TwoMisc { scalar, q, u, size, opcode, rn, rd } => {
            two_misc_exec(cpu, scalar, q, u, size, opcode, rn, rd, &mut qc)
        }
        IntInsn::Across { q, u, size, opcode, rn, rd } => {
            let esize = 8u32 << size;
            let n = (if q { 128 } else { 64 }) / esize as usize;
            let v = cpu.v[rn as usize];
            let signed = !u;
            let vals = (0..n).map(|e| ext(elem(v, e, esize), esize, signed));
            let r: u128 = match opcode {
                0b00011 => {
                    let s: i128 = vals.sum();
                    ((s as u128) & emask(2 * esize) as u128) as u64 as u128
                }
                0b01010 => (vals.max().unwrap() as u64 & emask(esize)) as u128,
                0b11010 => (vals.min().unwrap() as u64 & emask(esize)) as u128,
                _ => {
                    let s: i128 = vals.sum();
                    (s as u64 & emask(esize)) as u128
                }
            };
            cpu.v[rd as usize] = r;
        }
        IntInsn::AddpScalar { rn, rd } => {
            let v = cpu.v[rn as usize];
            cpu.v[rd as usize] = elem(v, 0, 64).wrapping_add(elem(v, 1, 64)) as u128;
        }
        IntInsn::ShiftImm { scalar, q, u, esize, shift, opcode, rn, rd } => {
            shift_imm_exec(cpu, scalar, q, u, esize as u32, shift as u32, opcode, rn, rd, &mut qc)
        }
        IntInsn::Indexed { scalar, q, u, size, index, opcode, rm, rn, rd } => {
            indexed_exec(cpu, scalar, q, u, size, index, opcode, rm, rn, rd, &mut qc)
        }
        IntInsn::Copy { op, scalar, q, esize, index, index2, rn, rd } => {
            let es = esize as u32;
            let datasize = if q { 128 } else { 64 };
            match op {
                CopyOp::DupElem => {
                    let x = elem(cpu.v[rn as usize], index as usize, es);
                    if scalar {
                        cpu.v[rd as usize] = x as u128;
                    } else {
                        let mut v = 0u128;
                        for e in 0..(datasize / es) as usize {
                            v = set_elem(v, e, es, x);
                        }
                        cpu.v[rd as usize] = v;
                    }
                }
                CopyOp::DupGen => {
                    let x = cpu.xr(rn) & emask(es);
                    let mut v = 0u128;
                    for e in 0..(datasize / es) as usize {
                        v = set_elem(v, e, es, x);
                    }
                    cpu.v[rd as usize] = v;
                }
                CopyOp::InsGen => {
                    let x = cpu.xr(rn);
                    cpu.v[rd as usize] = set_elem(cpu.v[rd as usize], index as usize, es, x);
                }
                CopyOp::InsElem => {
                    let x = elem(cpu.v[rn as usize], index2 as usize, es);
                    cpu.v[rd as usize] = set_elem(cpu.v[rd as usize], index as usize, es, x);
                }
                CopyOp::Smov => {
                    let x = sx(elem(cpu.v[rn as usize], index as usize, es), es) as u64;
                    cpu.set_x(rd, if q { x } else { x as u32 as u64 });
                }
                CopyOp::Umov => {
                    let x = elem(cpu.v[rn as usize], index as usize, es);
                    cpu.set_x(rd, x);
                }
            }
        }
        IntInsn::MovImm { q, op, imm, rd } => {
            let imm128 = (imm as u128) | if q { (imm as u128) << 64 } else { 0 };
            let d = cpu.v[rd as usize];
            let r = match op {
                MovImmOp::Movi => imm128,
                MovImmOp::Mvni => !imm128,
                MovImmOp::Orr => d | imm128,
                MovImmOp::Bic => d & !imm128,
            };
            cpu.v[rd as usize] = clip(r, if q { 128 } else { 64 });
        }
        IntInsn::Perm { q, size, opcode, rm, rn, rd } => {
            let esize = 8u32 << size;
            let n = (if q { 128 } else { 64 }) / esize as usize;
            let (a, b) = (cpu.v[rn as usize], cpu.v[rm as usize]);
            let part = (opcode >> 2) as usize; // 0: *1, 1: *2
            let mut r = 0u128;
            for e in 0..n {
                let x = match opcode & 3 {
                    1 => {
                        // UZP: elementi pari (o dispari) di concat(b:a)
                        let k = 2 * e + part;
                        if k < n { elem(a, k, esize) } else { elem(b, k - n, esize) }
                    }
                    2 => {
                        // TRN
                        let p = e / 2;
                        if e % 2 == 0 { elem(a, 2 * p + part, esize) } else { elem(b, 2 * p + part, esize) }
                    }
                    _ => {
                        // ZIP
                        let base = part * n / 2;
                        let p = e / 2;
                        if e % 2 == 0 { elem(a, base + p, esize) } else { elem(b, base + p, esize) }
                    }
                };
                r = set_elem(r, e, esize, x);
            }
            cpu.v[rd as usize] = r;
        }
        IntInsn::Ext { q, imm4, rm, rn, rd } => {
            let (lo, hi) = (cpu.v[rn as usize], cpu.v[rm as usize]);
            let pos = imm4 as u32 * 8;
            let r = if q {
                if pos == 0 { lo } else { (lo >> pos) | (hi << (128 - pos)) }
            } else {
                let concat = ((hi as u64 as u128) << 64) | (lo as u64 as u128);
                (concat >> pos) as u64 as u128
            };
            cpu.v[rd as usize] = r;
        }
        IntInsn::Tbl { q, len, tbx, rm, rn, rd } => {
            let regs = len as usize + 1;
            let idx = cpu.v[rm as usize];
            let d = cpu.v[rd as usize];
            let n = if q { 16 } else { 8 };
            let mut r = 0u128;
            for e in 0..n {
                let k = elem(idx, e, 8) as usize;
                let x = if k < 16 * regs {
                    elem(cpu.v[(rn as usize + k / 16) % 32], k % 16, 8)
                } else if tbx {
                    elem(d, e, 8)
                } else {
                    0
                };
                r = set_elem(r, e, 8, x);
            }
            cpu.v[rd as usize] = r;
        }
    }
    if qc {
        cpu.fpsr |= 1 << 27;
    }
}

#[allow(clippy::too_many_arguments)]
fn three_same_exec(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    size: u8,
    opcode: u8,
    rm: u8,
    rn: u8,
    rd: u8,
    qc: &mut bool,
) {
    let (a, b, d) = (cpu.v[rn as usize], cpu.v[rm as usize], cpu.v[rd as usize]);
    let datasize = if scalar {
        8u32 << size
    } else if q {
        128
    } else {
        64
    };
    if opcode == 0b00011 {
        // Operazioni logiche bit a bit sull'intero registro.
        let r = match (u, size) {
            (false, 0) => a & b,
            (false, 1) => a & !b,
            (false, 2) => a | b,
            (false, _) => a | !b,
            (true, 0) => a ^ b,
            (true, 1) => (d & a) | (!d & b), // BSL
            (true, 2) => (d & !b) | (a & b), // BIT
            (true, _) => (d & b) | (a & !b), // BIF
        };
        cpu.v[rd as usize] = clip(r, datasize);
        return;
    }
    let esize = 8u32 << size;
    let n = (datasize / esize) as usize;
    let signed = !u;
    let mut r = 0u128;
    let pairwise = matches!(opcode, 0b10100 | 0b10101 | 0b10111);
    for e in 0..n {
        let (x, y) = if pairwise {
            let src = if 2 * e < n { a } else { b };
            let k = (2 * e) % n;
            (elem(src, k, esize), elem(src, k + 1, esize))
        } else {
            (elem(a, e, esize), elem(b, e, esize))
        };
        let (sx_, sy) = (ext(x, esize, signed), ext(y, esize, signed));
        let mk = emask(esize);
        let res: u64 = match opcode {
            0b00000 => ((sx_ + sy) >> 1) as u64 & mk,
            0b00001 => {
                if signed {
                    sat_s(sx_ + sy, esize, qc)
                } else {
                    sat_u(sx_ + sy, esize, qc)
                }
            }
            0b00010 => ((sx_ + sy + 1) >> 1) as u64 & mk,
            0b00100 => ((sx_ - sy) >> 1) as u64 & mk,
            0b00101 => {
                if signed {
                    sat_s(sx_ - sy, esize, qc)
                } else {
                    sat_u(sx_ - sy, esize, qc)
                }
            }
            0b00110 => {
                if sx_ > sy {
                    mk
                } else {
                    0
                }
            }
            0b00111 => {
                if sx_ >= sy {
                    mk
                } else {
                    0
                }
            }
            0b01000 | 0b01010 => {
                let v = shl_reg(sx_, y as u8 as i8, opcode == 0b01010);
                v as u64 & mk
            }
            0b01001 | 0b01011 => {
                let v = shl_reg(sx_, y as u8 as i8, opcode == 0b01011);
                if signed { sat_s(v, esize, qc) } else { sat_u(v, esize, qc) }
            }
            0b01100 | 0b10100 => sx_.max(sy) as u64 & mk,
            0b01101 | 0b10101 => sx_.min(sy) as u64 & mk,
            0b01110 => (sx_ - sy).unsigned_abs() as u64 & mk,
            0b01111 => (elem(d, e, esize) as i128 + (sx_ - sy).abs()) as u64 & mk,
            0b10000 => {
                if u {
                    x.wrapping_sub(y) & mk
                } else {
                    x.wrapping_add(y) & mk
                }
            }
            0b10001 => {
                if u {
                    if x == y { mk } else { 0 }
                } else if x & y != 0 {
                    mk
                } else {
                    0
                }
            }
            0b10010 => {
                let p = x.wrapping_mul(y);
                let acc = elem(d, e, esize);
                (if u { acc.wrapping_sub(p) } else { acc.wrapping_add(p) }) & mk
            }
            0b10011 => {
                if u {
                    pmul8(x as u8, y as u8) as u64
                } else {
                    x.wrapping_mul(y) & mk
                }
            }
            0b10110 => {
                // SQDMULH / SQRDMULH
                let p = 2 * sx(x, esize) as i128 * sx(y, esize) as i128;
                let p = if u { p + (1i128 << (esize - 1)) } else { p };
                sat_s(p >> esize, esize, qc)
            }
            _ => x.wrapping_add(y) & mk, // ADDP
        };
        r = set_elem(r, e, esize, res);
    }
    cpu.v[rd as usize] = r;
}

#[allow(clippy::too_many_arguments)]
fn three_diff_exec(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    size: u8,
    opcode: u8,
    rm: u8,
    rn: u8,
    rd: u8,
    qc: &mut bool,
) {
    let esize = 8u32 << size;
    let (a, b, d) = (cpu.v[rn as usize], cpu.v[rm as usize], cpu.v[rd as usize]);
    let signed = !u;
    if opcode == 0b1110 {
        // PMULL / PMULL2
        let part = q as usize;
        let r = if size == 3 {
            pmul_wide(elem(a, part, 64), elem(b, part, 64))
        } else {
            let mut r = 0u128;
            for e in 0..8 {
                let (x, y) = (elem(a, 8 * part + e, 8), elem(b, 8 * part + e, 8));
                let mut p = 0u64;
                for i in 0..8 {
                    if y >> i & 1 != 0 {
                        p ^= x << i;
                    }
                }
                r = set_elem(r, e, 16, p);
            }
            r
        };
        cpu.v[rd as usize] = r;
        return;
    }
    let n = if scalar { 1 } else { 64 / esize as usize };
    let part = if scalar { 0 } else { q as usize };
    let big = 2 * esize;
    let mut r = 0u128;
    match opcode {
        0b0100 | 0b0110 => {
            // ADDHN/RADDHN, SUBHN/RSUBHN: da 2*esize a esize, metà alta
            for e in 0..n {
                let (x, y) = (elem(a, e, big), elem(b, e, big));
                let mut s = if opcode == 0b0100 { x.wrapping_add(y) } else { x.wrapping_sub(y) } as u128;
                if u {
                    s += 1u128 << (esize - 1);
                }
                let hi = ((s >> esize) as u64) & emask(esize);
                r = set_elem(r, e, esize, hi);
            }
            let dst = if part == 1 { (d & u64::MAX as u128) | (r << 64) } else { r };
            cpu.v[rd as usize] = dst;
            return;
        }
        _ => {}
    }
    for e in 0..n {
        let wide_a = matches!(opcode, 0b0001 | 0b0011);
        let x = if wide_a {
            ext(elem(a, e, big), big, signed)
        } else {
            ext(elem(a, part * n + e, esize), esize, signed)
        };
        let y = ext(elem(b, part * n + e, esize), esize, signed);
        let acc = ext(elem(d, e, big), big, true);
        let mk = emask(big);
        let res = match opcode {
            0b0000 | 0b0001 => (x + y) as u64 & mk,
            0b0010 | 0b0011 => (x - y) as u64 & mk,
            0b0101 => (acc + (x - y).abs()) as u64 & mk,
            0b0111 => (x - y).unsigned_abs() as u64 & mk,
            0b1000 => (acc + x * y) as u64 & mk,
            0b1010 => (acc - x * y) as u64 & mk,
            0b1100 => (x * y) as u64 & mk,
            0b1001 | 0b1011 | 0b1101 => {
                let p = sat_s(2 * x * y, big, qc);
                let p = sx(p, big) as i128;
                match opcode {
                    0b1001 => sat_s(acc + p, big, qc),
                    0b1011 => sat_s(acc - p, big, qc),
                    _ => p as u64 & mk,
                }
            }
            _ => unreachable!(),
        };
        r = set_elem(r, e, big, res);
    }
    cpu.v[rd as usize] = r;
}

#[allow(clippy::too_many_arguments)]
fn two_misc_exec(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    size: u8,
    opcode: u8,
    rn: u8,
    rd: u8,
    qc: &mut bool,
) {
    // NOT, RBIT e CNT lavorano sempre su byte (size codifica l'operazione).
    let esize = if opcode == 0b00101 { 8 } else { 8u32 << size };
    let a = cpu.v[rn as usize];
    let d = cpu.v[rd as usize];
    let datasize = if scalar {
        esize
    } else if q {
        128
    } else {
        64
    };
    let signed = !u;
    let mk = emask(esize);
    // Operazioni che restringono (XTN e simili) o allargano (SHLL).
    if matches!(opcode, 0b10010 | 0b10100) || opcode == 0b10011 && u {
        let part = if scalar { 0 } else { q as usize };
        let n = if scalar { 1 } else { 64 / esize as usize };
        let big = 2 * esize;
        if opcode == 0b10011 {
            // SHLL: allarga e sposta di esize
            let mut r = 0u128;
            for e in 0..n {
                let x = elem(a, part * n + e, esize);
                r = set_elem(r, e, big, (x << esize) & emask(big));
            }
            cpu.v[rd as usize] = r;
            return;
        }
        let mut r = 0u128;
        for e in 0..n {
            let x = elem(a, e, big);
            let res = match (u, opcode) {
                (false, 0b10010) => x & mk,                              // XTN
                (true, 0b10010) => sat_u(sx(x, big) as i128, esize, qc), // SQXTUN
                (false, _) => sat_s(sx(x, big) as i128, esize, qc),      // SQXTN
                (true, _) => sat_u(x as i128, esize, qc),                // UQXTN
            };
            r = set_elem(r, e, esize, res);
        }
        cpu.v[rd as usize] = if part == 1 { (d & u64::MAX as u128) | (r << 64) } else { r };
        return;
    }
    let n = (datasize / esize) as usize;
    match opcode {
        0b00010 | 0b00110 => {
            // [SU]ADDLP / [SU]ADALP: somma coppie adiacenti su 2*esize
            let big = 2 * esize;
            let mut r = 0u128;
            for e in 0..n / 2 {
                let s =
                    ext(elem(a, 2 * e, esize), esize, signed) + ext(elem(a, 2 * e + 1, esize), esize, signed);
                let acc = if opcode == 0b00110 { elem(d, e, big) as i128 } else { 0 };
                r = set_elem(r, e, big, ((acc + s) as u64) & emask(big));
            }
            cpu.v[rd as usize] = clip(r, datasize);
            return;
        }
        0b00000 | 0b00001 => {
            // REV64 / REV32 / REV16: inverte gli elementi dentro contenitori
            let container = match (u, opcode) {
                (false, 0b00000) => 64,
                (true, 0b00000) => 32,
                _ => 16,
            };
            let per = (container / esize) as usize;
            let mut r = 0u128;
            for e in 0..n {
                let base = e / per * per;
                let src = base + (per - 1 - (e - base));
                r = set_elem(r, e, esize, elem(a, src, esize));
            }
            cpu.v[rd as usize] = r;
            return;
        }
        _ => {}
    }
    let mut r = 0u128;
    for e in 0..n {
        let x = elem(a, e, esize);
        let s = sx(x, esize) as i128;
        let res = match (u, opcode) {
            (false, 0b00011) => {
                // SUQADD: Vd (con segno) + Vn (senza segno)
                let acc = sx(elem(d, e, esize), esize) as i128;
                sat_s(acc + x as i128, esize, qc)
            }
            (true, 0b00011) => {
                // USQADD: Vd (senza segno) + Vn (con segno)
                let acc = elem(d, e, esize) as i128;
                sat_u(acc + s, esize, qc)
            }
            (false, 0b00100) => cls(x, esize),
            (true, 0b00100) => clz(x, esize),
            (false, 0b00101) => (x as u8).count_ones() as u64,
            (true, 0b00101) => {
                if size == 0 {
                    !x & mk
                } else {
                    (x as u8).reverse_bits() as u64
                }
            }
            (false, 0b00111) => sat_s(s.abs(), esize, qc),
            (true, 0b00111) => sat_s(-s, esize, qc),
            (false, 0b01000) => {
                if s > 0 {
                    mk
                } else {
                    0
                }
            }
            (true, 0b01000) => {
                if s >= 0 {
                    mk
                } else {
                    0
                }
            }
            (false, 0b01001) => {
                if s == 0 {
                    mk
                } else {
                    0
                }
            }
            (true, 0b01001) => {
                if s <= 0 {
                    mk
                } else {
                    0
                }
            }
            (false, 0b01010) => {
                if s < 0 {
                    mk
                } else {
                    0
                }
            }
            (false, 0b01011) => (s.unsigned_abs() as u64) & mk,
            (true, 0b01011) => ((-s) as u64) & mk,
            _ => unreachable!("two-misc {u} {opcode:05b}"),
        };
        r = set_elem(r, e, esize, res);
    }
    let _ = d;
    cpu.v[rd as usize] = r;
}

#[allow(clippy::too_many_arguments)]
fn shift_imm_exec(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    esize: u32,
    shift: u32,
    opcode: u8,
    rn: u8,
    rd: u8,
    qc: &mut bool,
) {
    let a = cpu.v[rn as usize];
    let d = cpu.v[rd as usize];
    let signed = !u;
    let mk = emask(esize);
    if matches!(opcode, 0b10000..=0b10011) {
        // Restringimenti: esize è la dimensione di destinazione.
        let dst = esize;
        let src = 2 * esize;
        let n = if scalar { 1 } else { 64 / dst as usize };
        let part = if scalar { 0 } else { q as usize };
        let round = opcode & 1 == 1;
        let mut r = 0u128;
        for e in 0..n {
            let x = elem(a, e, src);
            let src_signed = match (u, opcode) {
                (false, 0b10000 | 0b10001) => false, // SHRN/RSHRN: indifferente, si tronca
                (true, 0b10000 | 0b10001) => true,   // SQSHRUN/SQRSHRUN: sorgente con segno
                (false, _) => true,                  // SQSHRN
                (true, _) => false,                  // UQSHRN
            };
            let v = ext(x, src, src_signed);
            let v = if round { (v + (1i128 << (shift - 1))) >> shift } else { v >> shift };
            let res = match (u, opcode) {
                (false, 0b10000 | 0b10001) => v as u64 & emask(dst),
                (true, 0b10000 | 0b10001) => sat_u(v, dst, qc),
                (false, _) => sat_s(v, dst, qc),
                (true, _) => sat_u(v, dst, qc),
            };
            r = set_elem(r, e, dst, res);
        }
        cpu.v[rd as usize] = if part == 1 { (d & u64::MAX as u128) | (r << 64) } else { r };
        return;
    }
    if opcode == 0b10100 {
        // SSHLL/USHLL
        let n = 64 / esize as usize;
        let part = q as usize;
        let big = 2 * esize;
        let mut r = 0u128;
        for e in 0..n {
            let x = ext(elem(a, part * n + e, esize), esize, signed);
            r = set_elem(r, e, big, ((x << shift) as u64) & emask(big));
        }
        cpu.v[rd as usize] = r;
        return;
    }
    let datasize = if scalar {
        esize
    } else if q {
        128
    } else {
        64
    };
    let n = (datasize / esize) as usize;
    let mut r = 0u128;
    for e in 0..n {
        let x = elem(a, e, esize);
        let v = ext(x, esize, signed);
        let acc = elem(d, e, esize);
        let rshift =
            |round: bool| -> i128 { if round { (v + (1i128 << (shift - 1))) >> shift } else { v >> shift } };
        let res = match (u, opcode) {
            (_, 0b00000) => rshift(false) as u64 & mk,
            (_, 0b00010) => acc.wrapping_add(rshift(false) as u64) & mk,
            (_, 0b00100) => rshift(true) as u64 & mk,
            (_, 0b00110) => acc.wrapping_add(rshift(true) as u64) & mk,
            (true, 0b01000) => {
                // SRI: inserisce x >> shift, conserva i bit alti di Vd
                let ins = if shift >= esize { 0 } else { mk >> shift };
                let shifted = if shift >= 64 { 0 } else { x >> shift };
                (acc & !ins) | (shifted & ins)
            }
            (false, 0b01010) => (x << shift) & mk,
            (true, 0b01010) => {
                // SLI
                let ins = (mk << shift) & mk;
                (acc & !ins) | ((x << shift) & ins)
            }
            (true, 0b01100) => sat_u(sx(x, esize) as i128 * (1i128 << shift), esize, qc),
            (false, 0b01110) => sat_s(sx(x, esize) as i128 * (1i128 << shift), esize, qc),
            (true, 0b01110) => sat_u((x as i128) << shift, esize, qc),
            _ => unreachable!(),
        };
        r = set_elem(r, e, esize, res);
    }
    cpu.v[rd as usize] = r;
}

#[allow(clippy::too_many_arguments)]
fn indexed_exec(
    cpu: &mut Cpu,
    scalar: bool,
    q: bool,
    u: bool,
    size: u8,
    index: u8,
    opcode: u8,
    rm: u8,
    rn: u8,
    rd: u8,
    qc: &mut bool,
) {
    let esize = 8u32 << size;
    let (a, d) = (cpu.v[rn as usize], cpu.v[rd as usize]);
    let y_raw = elem(cpu.v[rm as usize], index as usize, esize);
    let signed = !u;
    let mk = emask(esize);
    let long = matches!(opcode, 0b0010 | 0b0011 | 0b0110 | 0b0111 | 0b1010 | 0b1011);
    if long {
        let n = if scalar { 1 } else { 64 / esize as usize };
        let part = if scalar { 0 } else { q as usize };
        let big = 2 * esize;
        let mut r = 0u128;
        for e in 0..n {
            let x = ext(elem(a, part * n + e, esize), esize, signed);
            let y = ext(y_raw, esize, signed);
            let acc = sx(elem(d, e, big), big) as i128;
            let res = match opcode {
                0b0010 => (acc + x * y) as u64,
                0b0110 => (acc - x * y) as u64,
                0b1010 => (x * y) as u64,
                _ => {
                    let p = sx(sat_s(2 * x * y, big, qc), big) as i128;
                    match opcode {
                        0b0011 => sat_s(acc + p, big, qc),
                        0b0111 => sat_s(acc - p, big, qc),
                        _ => p as u64,
                    }
                }
            };
            r = set_elem(r, e, big, res & emask(big));
        }
        cpu.v[rd as usize] = r;
        return;
    }
    let datasize = if scalar {
        esize
    } else if q {
        128
    } else {
        64
    };
    let n = (datasize / esize) as usize;
    let mut r = 0u128;
    for e in 0..n {
        let x = elem(a, e, esize);
        let acc = elem(d, e, esize);
        let res = match (u, opcode) {
            (true, 0b0000) => acc.wrapping_add(x.wrapping_mul(y_raw)) & mk,
            (true, 0b0100) => acc.wrapping_sub(x.wrapping_mul(y_raw)) & mk,
            (false, 0b1000) => x.wrapping_mul(y_raw) & mk,
            (false, 0b1100 | 0b1101) => {
                let p = 2 * sx(x, esize) as i128 * sx(y_raw, esize) as i128;
                let p = if opcode == 0b1101 { p + (1i128 << (esize - 1)) } else { p };
                sat_s(p >> esize, esize, qc)
            }
            _ => unreachable!(),
        };
        r = set_elem(r, e, esize, res);
    }
    cpu.v[rd as usize] = r;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cls_clz() {
        assert_eq!(clz(1, 8), 7);
        assert_eq!(cls(0xff, 8), 7);
        assert_eq!(cls(0x40, 8), 0);
    }
}
