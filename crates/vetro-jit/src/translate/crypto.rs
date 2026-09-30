//! Cryptographic extension in the regions (ADR 0040): AESE/AESD/AESMC/AESIMC,
//! PMULL/PMULL2 of 64-bit lanes, SHA1 and SHA256.
//!
//! Before, these went through `env.simd` (the interpreter on a scratch CPU,
//! 1 KiB of registers copied in and out per instruction, S-box and GF(2^8)
//! products computed bit by bit). Android encrypts userdata (fscrypt:
//! AES-XTS with the kernel's `aes-ce`), verifies APKs and dm-verity blocks
//! with SHA-256 and speaks TLS (AES-GCM, PMULL): measured in `env.simd`.
//!
//! Each operation is a runtime function (`rt.cr<k>`, imported on first use
//! like `rt.fp<k>`) on v128 values, bit for bit the interpreter's
//! (`vetro_cpu::simd::crypto`, `int::pmul_wide`), without flags (these
//! instructions touch no FPSR bit):
//!
//! - S-box with 16 `i8x16.swizzle` of 16-byte slices of the table (an index
//!   out of 0..15 gives 0, so slice `h` answers only the bytes 16h..16h+15);
//!   ShiftRows with a shuffle; MixColumns with xtime and rotations of the
//!   columns; InvMixColumns as MixColumns after a pre-multiplication by
//!   (4x^2 + 5) (the usual decomposition);
//! - PMULL of 64 bits as three carry-less products of 32 bits (Karatsuba),
//!   each with 16 integer multiplications of operands with holes (bits 4
//!   apart: at most 8 terms per position, so no carry reaches the next one);
//! - SHA rounds in i32 locals, renamed at translation instead of moved.
//!
//! SHA1SU0 and SHA1H are inline.

use super::*;
use crate::wasm::v;
use vetro_cpu::simd::{CryptoInsn, CryptoOp};

/// A runtime function `rt.cr<k>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CrRt {
    /// (d, n) -> SubBytes(ShiftRows(d ^ n)).
    Aese,
    /// (d, n) -> InvSubBytes(InvShiftRows(d ^ n)).
    Aesd,
    /// (n) -> MixColumns(n).
    Aesmc,
    /// (n) -> InvMixColumns(n).
    Aesimc,
    /// (a: i64, b: i64) -> (low, high) of the carry-less product.
    Pmull64,
    /// (x, y, w) -> x (SHA256H: x = Vd, y = Vn) or y (SHA256H2: x = Vn, y = Vd).
    Sha256h { part1: bool },
    /// (d, n) -> SHA256SU0.
    Sha256su0,
    /// (d, n, m) -> SHA256SU1.
    Sha256su1,
    /// (x, y: i32, w) -> SHA1C/SHA1P/SHA1M (f = choose, parity, majority).
    Sha1 { f: u8 },
    /// (d, n) -> SHA1SU1.
    Sha1su1,
    /// Internal: (x) -> S-box of every byte (or of the inverse S-box).
    Sbox { inv: bool },
    /// Internal: (x: i64, y: i64) -> carry-less product of two 32-bit values.
    Bmul32,
}

/// All the `rt.cr<k>` functions, in index order (from [`f_cr0`]).
pub(super) const OPS: [CrRt; 13] = [
    CrRt::Aese,
    CrRt::Aesd,
    CrRt::Aesmc,
    CrRt::Aesimc,
    CrRt::Pmull64,
    CrRt::Sha256h { part1: true },
    CrRt::Sha256h { part1: false },
    CrRt::Sha256su0,
    CrRt::Sha256su1,
    CrRt::Sha1 { f: 0 },
    CrRt::Sha1 { f: 1 },
    CrRt::Sha1 { f: 2 },
    CrRt::Sha1su1,
];

/// The internal ones after [`OPS`] (not imported by the regions).
pub(super) const INTERNAL: [CrRt; 3] = [CrRt::Sbox { inv: false }, CrRt::Sbox { inv: true }, CrRt::Bmul32];

pub(super) fn all() -> impl Iterator<Item = CrRt> {
    OPS.into_iter().chain(INTERNAL)
}

pub(super) fn count() -> u32 {
    (OPS.len() + INTERNAL.len()) as u32
}

/// Index in the runtime of function `op`.
pub(super) fn rt_id(op_: CrRt) -> u32 {
    f_cr0() + all().position(|o| o == op_).expect("known rt.cr function") as u32
}

/// Name and signature of function `k`.
pub(super) fn rt_sig(k: usize) -> (String, Vec<ValType>, Vec<ValType>) {
    use ValType::*;
    let name = format!("cr{k}");
    let op_ = all().nth(k).expect("rt.cr index");
    let (p, r) = match op_ {
        CrRt::Aese | CrRt::Aesd | CrRt::Sha256su0 | CrRt::Sha1su1 => (vec![V128, V128], vec![V128]),
        CrRt::Aesmc | CrRt::Aesimc | CrRt::Sbox { .. } => (vec![V128], vec![V128]),
        CrRt::Pmull64 => (vec![I64, I64], vec![I64, I64]),
        CrRt::Sha256h { .. } | CrRt::Sha256su1 => (vec![V128, V128, V128], vec![V128]),
        CrRt::Sha1 { .. } => (vec![V128, I32, V128], vec![V128]),
        CrRt::Bmul32 => (vec![I64, I64], vec![I64]),
    };
    (name, p, r)
}

// --- AES tables and permutations ---

const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// The S-box and its inverse (FIPS-197), computed like the interpreter's.
const fn sboxes() -> ([u8; 256], [u8; 256]) {
    let mut s = [0u8; 256];
    let mut inv = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        let mut r: u8 = 1;
        let mut base = x as u8;
        let mut e = 254u32;
        while e > 0 {
            if e & 1 != 0 {
                r = gf_mul(r, base);
            }
            base = gf_mul(base, base);
            e >>= 1;
        }
        let b = if x == 0 { 0 } else { r };
        let v = b ^ b.rotate_left(1) ^ b.rotate_left(2) ^ b.rotate_left(3) ^ b.rotate_left(4) ^ 0x63;
        s[x] = v;
        inv[v as usize] = x as u8;
        x += 1;
    }
    (s, inv)
}

const SBOX: ([u8; 256], [u8; 256]) = sboxes();

/// Byte `i` of the state is row `i % 4` of column `i / 4`.
fn shift_rows_lanes(inverse: bool) -> [u8; 16] {
    let mut l = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            let src = if inverse { (c + 4 - r) % 4 } else { (c + r) % 4 };
            l[r + 4 * c] = (r + 4 * src) as u8;
        }
    }
    l
}

/// Row `r` of every column gets row `(r + k) % 4` of the same column.
fn rot_lanes(k: usize) -> [u8; 16] {
    core::array::from_fn(|i| (4 * (i / 4) + (i % 4 + k) % 4) as u8)
}

/// 32-bit words `w` (0..8 over the concatenation of two v128) as shuffle lanes.
fn words(w: [usize; 4]) -> [u8; 16] {
    core::array::from_fn(|i| (4 * w[i / 4] + i % 4) as u8)
}

fn splat8(b: u8) -> u64 {
    b as u64 * 0x0101_0101_0101_0101
}

/// xtime of every byte of the v128 on the stack (multiplication by x in
/// GF(2^8)); `t` is a v128 scratch local.
fn xtime(f: &mut Func, t: u32) {
    f.local_tee(t).i32_const(1).v(v::I8X16_SHL);
    f.local_get(t).i32_const(7).v(v::I8X16_SHR_S);
    f.v128_const(splat8(0x1b), splat8(0x1b)).v(v::AND).v(v::XOR);
}

/// `rotr(v, k)` of the i32x4 on the stack; `t` a v128 scratch local.
fn rotr4(f: &mut Func, t: u32, k: i32) {
    f.local_tee(t).i32_const(k).v(v::I32X4_SHR_U);
    f.local_get(t).i32_const(32 - k).v(v::I32X4_SHL).v(v::OR);
}

/// The body of runtime function `op_`; `rt` gives the runtime index of another.
pub(super) fn build(op_: CrRt, rt: impl Fn(CrRt) -> u32) -> Func {
    use ValType::*;
    match op_ {
        CrRt::Sbox { inv } => {
            // x 0; t 1
            let table = if inv { &SBOX.1 } else { &SBOX.0 };
            let mut f = Func { locals: vec![(1, V128)], ..Func::default() };
            let slice = |h: usize| {
                let lo = u64::from_le_bytes(table[16 * h..16 * h + 8].try_into().unwrap());
                let hi = u64::from_le_bytes(table[16 * h + 8..16 * h + 16].try_into().unwrap());
                (lo, hi)
            };
            let (lo, hi) = slice(0);
            f.v128_const(lo, hi).local_get(0).local_tee(1).v(v::SWIZZLE);
            for h in 1..16 {
                let (lo, hi) = slice(h);
                f.v128_const(lo, hi);
                f.local_get(1).v128_const(splat8(16), splat8(16)).v(v::I8X16_SUB).local_tee(1);
                f.v(v::SWIZZLE).v(v::OR);
            }
            f
        }
        CrRt::Aese | CrRt::Aesd => {
            // d 0, n 1; s 2. SubBytes and ShiftRows commute (bytewise).
            let inv = op_ == CrRt::Aesd;
            let mut f = Func { locals: vec![(1, V128)], ..Func::default() };
            f.local_get(0).local_get(1).v(v::XOR).call(rt(CrRt::Sbox { inv }));
            f.local_tee(2).local_get(2).shuffle(shift_rows_lanes(inv));
            f
        }
        CrRt::Aesmc => {
            // a 0; r1 1, t 2: o_r = xt(a_r ^ a_r+1) ^ a_r+1 ^ a_r+2 ^ a_r+3.
            let mut f = Func { locals: vec![(2, V128)], ..Func::default() };
            f.local_get(0).local_get(0).shuffle(rot_lanes(1)).local_tee(1);
            f.local_get(0).v(v::XOR);
            xtime(&mut f, 2);
            f.local_get(1).v(v::XOR);
            f.local_get(0).local_get(0).shuffle(rot_lanes(2)).v(v::XOR);
            f.local_get(0).local_get(0).shuffle(rot_lanes(3)).v(v::XOR);
            f
        }
        CrRt::Aesimc => {
            // a 0; t 1: MixColumns(a ^ xt(xt(a ^ rot2(a)))).
            let mut f = Func { locals: vec![(1, V128)], ..Func::default() };
            f.local_get(0);
            f.local_get(0).local_get(0).local_get(0).shuffle(rot_lanes(2)).v(v::XOR);
            xtime(&mut f, 1);
            xtime(&mut f, 1);
            f.v(v::XOR).call(rt(CrRt::Aesmc));
            f
        }
        CrRt::Bmul32 => {
            // x 0, y 1 (< 2^32); x0..x3 2..5, y0..y3 6..9
            let mut f = Func { locals: vec![(8, I64)], ..Func::default() };
            let m = [0x1111_1111i64, 0x2222_2222, 0x4444_4444, 0x8888_8888];
            for i in 0..4u32 {
                f.local_get(0).i64_const(m[i as usize]).op(op::I64_AND).local_set(2 + i);
                f.local_get(1).i64_const(m[i as usize]).op(op::I64_AND).local_set(6 + i);
            }
            let wide = [
                0x1111_1111_1111_1111u64 as i64,
                0x2222_2222_2222_2222u64 as i64,
                0x4444_4444_4444_4444u64 as i64,
                0x8888_8888_8888_8888u64 as i64,
            ];
            for c in 0..4u32 {
                // z_c = XOR of x_i * y_j with (i + j) % 4 == c
                for i in 0..4u32 {
                    let j = (c + 4 - i) % 4;
                    f.local_get(2 + i).local_get(6 + j).op(op::I64_MUL);
                    if i > 0 {
                        f.op(op::I64_XOR);
                    }
                }
                f.i64_const(wide[c as usize]).op(op::I64_AND);
                if c > 0 {
                    f.op(op::I64_OR);
                }
            }
            f
        }
        CrRt::Pmull64 => {
            // a 0, b 1; lo 2, hi 3, mid 4
            let mut f = Func { locals: vec![(3, I64)], ..Func::default() };
            let low = |f: &mut Func, x: u32| {
                f.local_get(x).i64_const(0xffff_ffff).op(op::I64_AND);
            };
            let high = |f: &mut Func, x: u32| {
                f.local_get(x).i64_const(32).op(op::I64_SHR_U);
            };
            let bm = rt(CrRt::Bmul32);
            low(&mut f, 0);
            low(&mut f, 1);
            f.call(bm).local_set(2);
            high(&mut f, 0);
            high(&mut f, 1);
            f.call(bm).local_set(3);
            low(&mut f, 0);
            high(&mut f, 0);
            f.op(op::I64_XOR);
            low(&mut f, 1);
            high(&mut f, 1);
            f.op(op::I64_XOR).call(bm);
            f.local_get(2).op(op::I64_XOR).local_get(3).op(op::I64_XOR).local_set(4);
            f.local_get(2).local_get(4).i64_const(32).op(op::I64_SHL).op(op::I64_XOR);
            f.local_get(3).local_get(4).i64_const(32).op(op::I64_SHR_U).op(op::I64_XOR);
            f
        }
        CrRt::Sha256h { part1 } => sha256h(part1),
        CrRt::Sha256su0 => {
            // d 0, n 1; t 2, s 3: sigma0([d1, d2, d3, n0]) + d
            let mut f = Func { locals: vec![(2, V128)], ..Func::default() };
            f.local_get(0).local_get(1).shuffle(words([1, 2, 3, 4])).local_set(2);
            f.local_get(2);
            rotr4(&mut f, 3, 7);
            f.local_get(2);
            rotr4(&mut f, 3, 18);
            f.v(v::XOR);
            f.local_get(2).i32_const(3).v(v::I32X4_SHR_U).v(v::XOR);
            f.local_get(0).v(v::I32X4_ADD);
            f
        }
        CrRt::Sha256su1 => {
            // d 0, n 1, m 2; u 3, t 4, s 5, lo 6
            // u = d + [n1, n2, n3, m0]; lo = u + s1([m2, m3, m2, m3]) (lanes 0, 1
            // right); hi = u + s1([r0, r1, r0, r1]) (lanes 2, 3 right).
            let mut f = Func { locals: vec![(4, V128)], ..Func::default() };
            let s1 = |f: &mut Func| {
                f.local_tee(4);
                rotr4(f, 5, 17);
                f.local_get(4);
                rotr4(f, 5, 19);
                f.v(v::XOR);
                f.local_get(4).i32_const(10).v(v::I32X4_SHR_U).v(v::XOR);
            };
            f.local_get(1)
                .local_get(2)
                .shuffle(words([1, 2, 3, 4]))
                .local_get(0)
                .v(v::I32X4_ADD)
                .local_set(3);
            f.local_get(2).local_get(2).shuffle(words([2, 3, 2, 3]));
            s1(&mut f);
            f.local_get(3).v(v::I32X4_ADD).local_set(6);
            f.local_get(6).local_get(6).shuffle(words([0, 1, 0, 1]));
            s1(&mut f);
            f.local_get(3).v(v::I32X4_ADD);
            f.local_set(4);
            f.local_get(6).local_get(4).shuffle(words([0, 1, 6, 7]));
            f
        }
        CrRt::Sha1 { f: kind } => sha1(kind),
        CrRt::Sha1su1 => {
            // d 0, n 1; t 2, s 3: t = d ^ [n1, n2, n3, 0]; rol(t, 1), and
            // lane 3 also ^ rol(t0, 2).
            let mut f = Func { locals: vec![(2, V128)], ..Func::default() };
            f.local_get(0).local_get(1).v128_const(0, 0).shuffle(words([1, 2, 3, 4])).v(v::XOR).local_set(2);
            f.local_get(2);
            rotr4(&mut f, 3, 31);
            f.v128_const(0, 0).local_get(2);
            rotr4(&mut f, 3, 30);
            f.shuffle(words([0, 1, 2, 4])).v(v::XOR);
            f
        }
    }
}

/// SHA256H (`part1`) / SHA256H2: four rounds on x (params 0) and y (1) with
/// the words of w (2), like `sha256_hash` of the interpreter.
fn sha256h(part1: bool) -> Func {
    use ValType::*;
    // i32 locals: x 3..7, y 7..11, w 11..15, t 15
    let mut f = Func { locals: vec![(13, I32)], ..Func::default() };
    let (lx, ly, lw, t) = (3u32, 7u32, 11u32, 15u32);
    for (p, base) in [(0u32, lx), (1, ly), (2, lw)] {
        for e in 0..4u32 {
            f.local_get(p).lane(v::I32X4_EXTRACT_LANE, e as u8).local_set(base + e);
        }
    }
    let mut x = [lx, lx + 1, lx + 2, lx + 3];
    let mut y = [ly, ly + 1, ly + 2, ly + 3];
    let big_sigma = |f: &mut Func, l: u32, k: [i32; 3]| {
        f.local_get(l).i32_const(k[0]).op(op::I32_ROTR);
        f.local_get(l).i32_const(k[1]).op(op::I32_ROTR).op(op::I32_XOR);
        f.local_get(l).i32_const(k[2]).op(op::I32_ROTR).op(op::I32_XOR);
    };
    for e in 0..4u32 {
        // t = y3 + S1(y0) + ch(y0, y1, y2) + w_e
        f.local_get(y[3]);
        big_sigma(&mut f, y[0], [6, 11, 25]);
        f.op(op::I32_ADD);
        f.local_get(y[1]).local_get(y[2]).op(op::I32_XOR).local_get(y[0]).op(op::I32_AND);
        f.local_get(y[2]).op(op::I32_XOR).op(op::I32_ADD);
        f.local_get(lw + e).op(op::I32_ADD).local_set(t);
        // x3 = t + x3
        f.local_get(t).local_get(x[3]).op(op::I32_ADD).local_set(x[3]);
        // y3 = t + S0(x0) + maj(x0, x1, x2)
        f.local_get(t);
        big_sigma(&mut f, x[0], [2, 13, 22]);
        f.op(op::I32_ADD);
        f.local_get(x[0]).local_get(x[1]).op(op::I32_AND);
        f.local_get(x[0]).local_get(x[1]).op(op::I32_OR).local_get(x[2]).op(op::I32_AND);
        f.op(op::I32_OR).op(op::I32_ADD).local_set(y[3]);
        // <Y, X> = ROL(Y : X, 32)
        let (nx, ny) = ([y[3], x[0], x[1], x[2]], [x[3], y[0], y[1], y[2]]);
        x = nx;
        y = ny;
    }
    let out = if part1 { x } else { y };
    f.local_get(out[0]).v(v::I32X4_SPLAT);
    for (e, &l) in out.iter().enumerate().skip(1) {
        f.local_get(l).lane(v::I32X4_REPLACE_LANE, e as u8);
    }
    f
}

/// SHA1C (`kind` 0, choose), SHA1P (1, parity), SHA1M (2, majority): four
/// rounds on x (param 0) and y (1, i32) with the words of w (2).
fn sha1(kind: u8) -> Func {
    use ValType::*;
    // i32 locals: x 3..7, w 7..11
    let mut f = Func { locals: vec![(8, I32)], ..Func::default() };
    let (lx, lw) = (3u32, 7u32);
    for (p, base) in [(0u32, lx), (2, lw)] {
        for e in 0..4u32 {
            f.local_get(p).lane(v::I32X4_EXTRACT_LANE, e as u8).local_set(base + e);
        }
    }
    let mut x = [lx, lx + 1, lx + 2, lx + 3];
    let mut y = 1u32;
    for e in 0..4u32 {
        // y = y + rol(x0, 5) + f(x1, x2, x3) + w_e
        f.local_get(y).local_get(x[0]).i32_const(5).op(op::I32_ROTL).op(op::I32_ADD);
        let (a, b, c) = (x[1], x[2], x[3]);
        match kind {
            0 => {
                f.local_get(b).local_get(c).op(op::I32_XOR).local_get(a).op(op::I32_AND);
                f.local_get(c).op(op::I32_XOR);
            }
            1 => {
                f.local_get(a).local_get(b).op(op::I32_XOR).local_get(c).op(op::I32_XOR);
            }
            _ => {
                f.local_get(a).local_get(b).op(op::I32_AND);
                f.local_get(a).local_get(b).op(op::I32_OR).local_get(c).op(op::I32_AND);
                f.op(op::I32_OR);
            }
        }
        f.op(op::I32_ADD).local_get(lw + e).op(op::I32_ADD).local_set(y);
        // x1 = rol(x1, 30)
        f.local_get(x[1]).i32_const(30).op(op::I32_ROTL).local_set(x[1]);
        // <Y, X> = ROL(Y : X, 32): x = [y, x0, x1, x2], y = x3
        let nx = [y, x[0], x[1], x[2]];
        y = x[3];
        x = nx;
    }
    f.local_get(x[0]).v(v::I32X4_SPLAT);
    for (e, &l) in x.iter().enumerate().skip(1) {
        f.local_get(l).lane(v::I32X4_REPLACE_LANE, e as u8);
    }
    f
}

// --- in the regions ---------------------------------------------------

impl Tx {
    fn rt_cr(&mut self, op_: CrRt) -> u32 {
        self.rt_opt(rt_id(op_))
    }

    /// Cryptographic instructions (all of them are translated here).
    pub(super) fn crypto(&mut self, i: CryptoInsn) {
        let CryptoInsn { op: cop, rm, rn, rd } = i;
        let call = |t: &mut Tx, op_: CrRt, args: &[u8]| {
            let f = t.rt_cr(op_);
            t.vst_begin();
            for &r in args {
                t.vld(r);
            }
            t.f.call(f);
            t.vst_end(rd, true);
        };
        match cop {
            CryptoOp::Aese => call(self, CrRt::Aese, &[rd, rn]),
            CryptoOp::Aesd => call(self, CrRt::Aesd, &[rd, rn]),
            CryptoOp::Aesmc => call(self, CrRt::Aesmc, &[rn]),
            CryptoOp::Aesimc => call(self, CrRt::Aesimc, &[rn]),
            CryptoOp::Sha256h => call(self, CrRt::Sha256h { part1: true }, &[rd, rn, rm]),
            CryptoOp::Sha256h2 => call(self, CrRt::Sha256h { part1: false }, &[rn, rd, rm]),
            CryptoOp::Sha256su0 => call(self, CrRt::Sha256su0, &[rd, rn]),
            CryptoOp::Sha256su1 => call(self, CrRt::Sha256su1, &[rd, rn, rm]),
            CryptoOp::Sha1su1 => call(self, CrRt::Sha1su1, &[rd, rn]),
            CryptoOp::Sha1c | CryptoOp::Sha1p | CryptoOp::Sha1m => {
                let kind = match cop {
                    CryptoOp::Sha1c => 0,
                    CryptoOp::Sha1p => 1,
                    _ => 2,
                };
                let f = self.rt_cr(CrRt::Sha1 { f: kind });
                self.vst_begin();
                self.vld(rd);
                self.vld(rn);
                self.f.lane(v::I32X4_EXTRACT_LANE, 0);
                self.vld(rm);
                self.f.call(f);
                self.vst_end(rd, true);
            }
            CryptoOp::Sha1su0 => {
                // ([n.D[0]] : d.D[1]) ^ d ^ m
                self.vst_begin();
                self.vld(rd);
                self.vld(rn);
                self.f.shuffle(words([2, 3, 4, 5]));
                self.vld(rd);
                self.f.v(v::XOR);
                self.vld(rm);
                self.f.v(v::XOR);
                self.vst_end(rd, true);
            }
            CryptoOp::Sha1h => {
                // Sd = rol(Sn, 30), the rest zero.
                self.vst_begin();
                self.vld(rn);
                self.f.lane(v::I32X4_EXTRACT_LANE, 0).i32_const(30).op(op::I32_ROTL);
                self.f.op(op::I64_EXTEND_I32_U).v(v::I64X2_SPLAT);
                self.vst_end(rd, false);
            }
        }
    }

    /// PMULL/PMULL2 of 64-bit lanes (lane `q` of Vn and Vm): 128-bit result.
    pub(super) fn pmull64(&mut self, q: bool, rm: u8, rn: u8, rd: u8) {
        let f = self.rt_cr(CrRt::Pmull64);
        self.simd = true;
        self.get_v(rn, q);
        self.get_v(rm, q);
        self.f.call(f).local_set(t64(1)).local_set(t64(0));
        self.set_v(rd, false, t64(0));
        self.set_v(rd, true, t64(1));
    }
}
