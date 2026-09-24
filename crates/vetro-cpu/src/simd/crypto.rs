//! Estensioni crittografiche della Cortex-A53: AES, SHA1, SHA256 (Arm ARM,
//! shared/functions/crypto). PMULL sta con le operazioni intere.

use super::SimdInsn;
use super::vreg::{elem, set_elem};
use crate::bits::field;
use crate::decode::Insn;
use crate::state::Cpu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoOp {
    Aese,
    Aesd,
    Aesmc,
    Aesimc,
    Sha1c,
    Sha1p,
    Sha1m,
    Sha1su0,
    Sha256h,
    Sha256h2,
    Sha256su1,
    Sha1h,
    Sha1su1,
    Sha256su0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CryptoInsn {
    pub op: CryptoOp,
    pub rm: u8,
    pub rn: u8,
    pub rd: u8,
}

/// Classi crittografiche: AES (0x4E28_0800), SHA a tre registri
/// (0x5E00_0000), SHA a due registri (0x5E28_0800).
pub fn decode(w: u32) -> Insn {
    if field(w, 23, 22) != 0 {
        return Insn::Undefined;
    }
    let (rd, rn, rm) = (field(w, 4, 0) as u8, field(w, 9, 5) as u8, field(w, 20, 16) as u8);
    let op = if w & 0xFF3E_0C00 == 0x4E28_0800 {
        match field(w, 16, 12) {
            0b00100 => CryptoOp::Aese,
            0b00101 => CryptoOp::Aesd,
            0b00110 => CryptoOp::Aesmc,
            0b00111 => CryptoOp::Aesimc,
            _ => return Insn::Undefined,
        }
    } else if w & 0xFF3E_0C00 == 0x5E28_0800 {
        match field(w, 16, 12) {
            0b00000 => CryptoOp::Sha1h,
            0b00001 => CryptoOp::Sha1su1,
            0b00010 => CryptoOp::Sha256su0,
            _ => return Insn::Undefined,
        }
    } else {
        match field(w, 14, 12) {
            0b000 => CryptoOp::Sha1c,
            0b001 => CryptoOp::Sha1p,
            0b010 => CryptoOp::Sha1m,
            0b011 => CryptoOp::Sha1su0,
            0b100 => CryptoOp::Sha256h,
            0b101 => CryptoOp::Sha256h2,
            0b110 => CryptoOp::Sha256su1,
            _ => return Insn::Undefined,
        }
    };
    Insn::Simd(SimdInsn::Crypto(CryptoInsn { op, rm, rn, rd }))
}

// --- AES ---

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
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

const fn sbox_table() -> ([u8; 256], [u8; 256]) {
    let mut s = [0u8; 256];
    let mut inv = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        // inverso moltiplicativo: x^254 in GF(2^8)
        let mut r: u8 = 1;
        let mut base = x as u8;
        let mut e = 254u32;
        while e > 0 {
            if e & 1 != 0 {
                r = gf_mul_const(r, base);
            }
            base = gf_mul_const(base, base);
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

const fn gf_mul_const(mut a: u8, mut b: u8) -> u8 {
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

const SBOX: ([u8; 256], [u8; 256]) = sbox_table();

fn bytes(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

fn from_bytes(b: [u8; 16]) -> u128 {
    u128::from_le_bytes(b)
}

/// ShiftRows (o la sua inversa): il byte i è la riga i%4 della colonna i/4.
fn shift_rows(s: [u8; 16], inverse: bool) -> [u8; 16] {
    let mut o = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            let src = if inverse { (c + 4 - r) % 4 } else { (c + r) % 4 };
            o[r + 4 * c] = s[r + 4 * src];
        }
    }
    o
}

fn mix_columns(s: [u8; 16], inverse: bool) -> [u8; 16] {
    let m: [u8; 4] = if inverse { [14, 11, 13, 9] } else { [2, 3, 1, 1] };
    let mut o = [0u8; 16];
    for c in 0..4 {
        let col = &s[4 * c..4 * c + 4];
        for r in 0..4 {
            let mut acc = 0u8;
            for k in 0..4 {
                acc ^= gf_mul(col[k], m[(k + 4 - r) % 4]);
            }
            o[4 * c + r] = acc;
        }
    }
    o
}

// --- SHA ---

fn choose(x: u32, y: u32, z: u32) -> u32 {
    ((y ^ z) & x) ^ z
}
fn majority(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | ((x | y) & z)
}
fn parity(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}
fn w32(v: u128, e: usize) -> u32 {
    elem(v, e, 32) as u32
}

fn sha1_hash(mut x: u128, mut y: u32, w: u128, f: fn(u32, u32, u32) -> u32) -> u128 {
    for e in 0..4 {
        let t = f(w32(x, 1), w32(x, 2), w32(x, 3));
        y = y.wrapping_add(w32(x, 0).rotate_left(5)).wrapping_add(t).wrapping_add(w32(w, e));
        x = set_elem(x, 1, 32, w32(x, 1).rotate_left(30) as u64);
        // <Y, X> = ROL(Y : X, 32)
        let new_y = w32(x, 3);
        x = (x << 32) | y as u128;
        y = new_y;
    }
    x
}

fn sha256_hash(mut x: u128, mut y: u128, w: u128, part1: bool) -> u128 {
    let sigma0 = |v: u32| v.rotate_right(2) ^ v.rotate_right(13) ^ v.rotate_right(22);
    let sigma1 = |v: u32| v.rotate_right(6) ^ v.rotate_right(11) ^ v.rotate_right(25);
    for e in 0..4 {
        let chs = choose(w32(y, 0), w32(y, 1), w32(y, 2));
        let maj = majority(w32(x, 0), w32(x, 1), w32(x, 2));
        let t = w32(y, 3).wrapping_add(sigma1(w32(y, 0))).wrapping_add(chs).wrapping_add(w32(w, e));
        x = set_elem(x, 3, 32, t.wrapping_add(w32(x, 3)) as u64);
        y = set_elem(y, 3, 32, t.wrapping_add(sigma0(w32(x, 0))).wrapping_add(maj) as u64);
        // <Y, X> = ROL(Y : X, 32)
        let (nx, ny) = ((x << 32) | w32(y, 3) as u128, (y << 32) | w32(x, 3) as u128);
        x = nx;
        y = ny;
    }
    if part1 { x } else { y }
}

pub(crate) fn exec(cpu: &mut Cpu, i: CryptoInsn) {
    let (d, n, m) = (cpu.v[i.rd as usize], cpu.v[i.rn as usize], cpu.v[i.rm as usize]);
    let r = match i.op {
        CryptoOp::Aese | CryptoOp::Aesd => {
            let inv = i.op == CryptoOp::Aesd;
            let s = shift_rows(bytes(d ^ n), inv);
            let table = if inv { &SBOX.1 } else { &SBOX.0 };
            from_bytes(s.map(|b| table[b as usize]))
        }
        CryptoOp::Aesmc => from_bytes(mix_columns(bytes(n), false)),
        CryptoOp::Aesimc => from_bytes(mix_columns(bytes(n), true)),
        CryptoOp::Sha1c => sha1_hash(d, w32(n, 0), m, choose),
        CryptoOp::Sha1p => sha1_hash(d, w32(n, 0), m, parity),
        CryptoOp::Sha1m => sha1_hash(d, w32(n, 0), m, majority),
        CryptoOp::Sha1su0 => {
            let r = ((n as u64 as u128) << 64) | (d >> 64);
            r ^ d ^ m
        }
        CryptoOp::Sha1h => w32(n, 0).rotate_left(30) as u128,
        CryptoOp::Sha1su1 => {
            let t = d ^ (n >> 32);
            let mut r = 0u128;
            for e in 0..4 {
                r = set_elem(r, e, 32, w32(t, e).rotate_left(1) as u64);
            }
            set_elem(r, 3, 32, (w32(t, 3).rotate_left(1) ^ w32(t, 0).rotate_left(2)) as u64)
        }
        CryptoOp::Sha256h => sha256_hash(d, n, m, true),
        CryptoOp::Sha256h2 => sha256_hash(n, d, m, false),
        CryptoOp::Sha256su0 => {
            let t = ((w32(n, 0) as u128) << 96) | (d >> 32);
            let mut r = 0u128;
            for e in 0..4 {
                let x = w32(t, e);
                let x = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
                r = set_elem(r, e, 32, x.wrapping_add(w32(d, e)) as u64);
            }
            r
        }
        CryptoOp::Sha256su1 => {
            let t0 = ((w32(m, 0) as u128) << 96) | (n >> 32);
            let s1 = |x: u32| x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
            let mut r = 0u128;
            let t1 = m >> 64;
            for e in 0..2 {
                let x = s1(w32(t1, e)).wrapping_add(w32(d, e)).wrapping_add(w32(t0, e));
                r = set_elem(r, e, 32, x as u64);
            }
            let t1 = r;
            for e in 2..4 {
                let x = s1(w32(t1, e - 2)).wrapping_add(w32(d, e)).wrapping_add(w32(t0, e));
                r = set_elem(r, e, 32, x as u64);
            }
            r
        }
    };
    cpu.v[i.rd as usize] = r;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbox_known_values() {
        assert_eq!(SBOX.0[0x00], 0x63);
        assert_eq!(SBOX.0[0x01], 0x7c);
        assert_eq!(SBOX.0[0x53], 0xed);
        assert_eq!(SBOX.1[0x63], 0x00);
    }

    #[test]
    fn mix_columns_roundtrip() {
        // Vettore FIPS-197: colonna db 13 53 45 → 8e 4d a1 bc
        let mut s = [0u8; 16];
        s[..4].copy_from_slice(&[0xdb, 0x13, 0x53, 0x45]);
        let m = mix_columns(s, false);
        assert_eq!(&m[..4], &[0x8e, 0x4d, 0xa1, 0xbc]);
        assert_eq!(mix_columns(m, true), s);
    }
}
