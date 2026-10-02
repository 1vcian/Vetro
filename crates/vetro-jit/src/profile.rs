//! Per-class counters of the instructions executed by the interpreter while
//! the JIT is active (region exits and cold code): they are used to
//! choose what to translate (M4, ADR 0026). They are enabled with
//! `JitConfig::profile` / `SysJitConfig::profile` (in `vetro`, with the
//! environment variable `VETRO_JIT_PROFILE=1`), and change nothing
//! about execution.

use std::collections::HashMap;
use std::fmt::Debug;

use vetro_cpu::Insn;
use vetro_cpu::simd::{FpInsn, IntInsn, SimdInsn, VecMemInsn};

use crate::translate::{Kind, SysTarget, kind_in};

/// Name of the variant (`Foo` of `Foo { .. }`).
fn variant<T: Debug>(x: &T) -> String {
    let s = format!("{x:?}");
    s.split([' ', '{', '(']).next().unwrap_or("").to_string()
}

/// Class of an instruction: the variant and the fields that decide
/// the operation (not the registers). SIMD/FP instructions have fine
/// classes, the others only the variant (and the register for MRS/MSR).
pub fn class(insn: &Insn) -> String {
    match *insn {
        Insn::Simd(SimdInsn::Fp(f)) => match f {
            FpInsn::Dp1 { ty, opcode, .. } => format!("fp dp1 op{opcode} ty{ty}"),
            FpInsn::Dp2 { ty, opcode, .. } => format!("fp dp2 op{opcode} ty{ty}"),
            FpInsn::Dp3 { ty, neg_a, neg_n, .. } => {
                format!("fp dp3 na{} nn{} ty{ty}", neg_a as u8, neg_n as u8)
            }
            FpInsn::Cmp { ty, zero, signal, .. } => {
                format!("fp cmp z{} e{} ty{ty}", zero as u8, signal as u8)
            }
            FpInsn::CondCmp { ty, .. } => format!("fp ccmp ty{ty}"),
            FpInsn::CondSel { ty, .. } => format!("fp csel ty{ty}"),
            FpInsn::Imm { ty, .. } => format!("fp imm ty{ty}"),
            FpInsn::ToInt { ty, sf, unsigned, rounding, fbits, .. } => {
                format!(
                    "fp toint {rounding:?} u{} sf{} fb{} ty{ty}",
                    unsigned as u8,
                    sf as u8,
                    (fbits > 0) as u8
                )
            }
            FpInsn::FromInt { ty, sf, unsigned, fbits, .. } => {
                format!("fp fromint u{} sf{} fb{} ty{ty}", unsigned as u8, sf as u8, (fbits > 0) as u8)
            }
            FpInsn::MovToGp { .. } => "fp mov_to_gp".into(),
            FpInsn::MovFromGp { .. } => "fp mov_from_gp".into(),
            FpInsn::VThreeSame { scalar, u, a, sz, opcode, q, .. } => format!(
                "vfp 3same s{} q{} u{} a{} sz{} op{opcode:05b}",
                scalar as u8, q as u8, u as u8, a as u8, sz as u8
            ),
            FpInsn::VTwoMisc { scalar, u, a, sz, opcode, q, .. } => format!(
                "vfp 2misc s{} q{} u{} a{} sz{} op{opcode:05b}",
                scalar as u8, q as u8, u as u8, a as u8, sz as u8
            ),
            FpInsn::VAcross { a, max_num, .. } => format!("vfp across a{} nm{}", a as u8, max_num as u8),
            FpInsn::VPairScalar { a, sz, opcode, .. } => {
                format!("vfp pair a{} sz{} op{opcode:05b}", a as u8, sz as u8)
            }
            FpInsn::VFixed { scalar, to_int, sz, .. } => {
                format!("vfp fixed s{} toint{} sz{}", scalar as u8, to_int as u8, sz as u8)
            }
            FpInsn::VIndexed { scalar, u, sz, opcode, q, .. } => {
                format!(
                    "vfp indexed s{} q{} u{} sz{} op{opcode:04b}",
                    scalar as u8, q as u8, u as u8, sz as u8
                )
            }
        },
        Insn::Simd(SimdInsn::Int(i)) => match i {
            IntInsn::ThreeSame { scalar, q, u, size, opcode, .. } => {
                format!("vint 3same s{} q{} u{} size{size} op{opcode:05b}", scalar as u8, q as u8, u as u8)
            }
            IntInsn::ThreeDiff { u, size, opcode, .. } => {
                format!("vint 3diff u{} size{size} op{opcode:04b}", u as u8)
            }
            IntInsn::TwoMisc { scalar, u, size, opcode, .. } => {
                format!("vint 2misc s{} u{} size{size} op{opcode:05b}", scalar as u8, u as u8)
            }
            IntInsn::Across { u, size, opcode, .. } => {
                format!("vint across u{} size{size} op{opcode:05b}", u as u8)
            }
            IntInsn::AddpScalar { .. } => "vint addp_scalar".into(),
            IntInsn::ShiftImm { scalar, u, opcode, .. } => {
                format!("vint shift s{} u{} op{opcode:05b}", scalar as u8, u as u8)
            }
            IntInsn::Indexed { u, opcode, .. } => format!("vint indexed u{} op{opcode:04b}", u as u8),
            IntInsn::Copy { op, .. } => format!("vint copy {op:?}"),
            IntInsn::MovImm { op, .. } => format!("vint movimm {op:?}"),
            IntInsn::Perm { opcode, .. } => format!("vint perm op{opcode}"),
            IntInsn::Ext { .. } => "vint ext".into(),
            IntInsn::Tbl { tbx, len, .. } => format!("vint tbl x{} len{len}", tbx as u8),
        },
        Insn::Simd(SimdInsn::Mem(m)) => match m {
            VecMemInsn::Multi { load, selem, rpt, .. } => {
                format!("vmem multi l{} selem{selem} rpt{rpt}", load as u8)
            }
            VecMemInsn::Single { load, selem, replicate, .. } => {
                format!("vmem single l{} selem{selem} r{}", load as u8, replicate as u8)
            }
            other => format!("vmem {}", variant(&other)),
        },
        Insn::Simd(SimdInsn::Crypto(c)) => format!("crypto {:?}", c.op),
        Insn::Mrs { reg, .. } => format!("mrs {reg:?}"),
        Insn::Msr { reg, .. } => format!("msr {reg:?}"),
        ref other => variant(other),
    }
}

/// A multiplicative hash (the keys are instruction words and parameters):
/// the profile counts on every interpreter step and `env.simd` call.
#[derive(Clone, Copy, Debug, Default)]
struct WordHasher(u64);

impl std::hash::Hasher for WordHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0.rotate_left(32) ^ v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// Per-class counters. Instruction words are counted as they come (cheap:
/// the profile runs on every interpreter step and `env.simd` call) and
/// grouped into classes only for the report.
#[derive(Clone, Debug, Default)]
pub struct Profile {
    words: HashMap<(u32, Option<SysTarget>), u64, std::hash::BuildHasherDefault<WordHasher>>,
    /// FP instructions by word and [`fp_reason`] (why a fast path could miss).
    reasons: HashMap<(u32, u32), u64, std::hash::BuildHasherDefault<WordHasher>>,
    total: u64,
}

/// Bits of [`fp_reason`] besides FPCR\[26:19\] (bits 0..7).
pub mod why {
    /// FPSR.IXC was already 1.
    pub const IXC_SET: u32 = 1 << 8;
    /// Flags the instruction raised (shifted FPSR bits IOC, DZC, OFC, UFC,
    /// IXC at 9..13, IDC at 14).
    pub const NEW_SHIFT: u32 = 9;
    /// An input lane is a NaN, a denormal, an infinity, a zero.
    pub const IN_NAN: u32 = 1 << 16;
    pub const IN_DEN: u32 = 1 << 17;
    pub const IN_INF: u32 = 1 << 18;
    pub const IN_ZERO: u32 = 1 << 19;
}

/// Source registers, element size (16/32/64) and whether all lanes count
/// (vector Q = 1), for the input classes of [`fp_reason`].
fn fp_sources(f: FpInsn) -> Option<(Vec<u8>, u32, u32)> {
    let ty = |t: u8| match t {
        0 => 32,
        1 => 64,
        _ => 16,
    };
    let sz = |s: bool| if s { 64 } else { 32 };
    let lanes = |scalar: bool, q: bool| {
        if scalar {
            0
        } else if q {
            128
        } else {
            64
        }
    };
    Some(match f {
        FpInsn::Dp1 { ty: t, rn, .. } => (vec![rn], ty(t), 0),
        FpInsn::Dp2 { ty: t, rn, rm, .. } => (vec![rn, rm], ty(t), 0),
        FpInsn::Dp3 { ty: t, rn, rm, ra, .. } => (vec![rn, rm, ra], ty(t), 0),
        FpInsn::Cmp { ty: t, rn, rm, zero, .. } => (if zero { vec![rn] } else { vec![rn, rm] }, ty(t), 0),
        FpInsn::CondCmp { ty: t, rn, rm, .. } => (vec![rn, rm], ty(t), 0),
        FpInsn::ToInt { ty: t, rn, .. } => (vec![rn], ty(t), 0),
        FpInsn::VThreeSame { scalar, q, sz: s, rn, rm, rd, opcode, .. } => {
            // FMLA/FMLS also read Vd.
            let r = if opcode == 0b11001 { vec![rn, rm, rd] } else { vec![rn, rm] };
            (r, sz(s), lanes(scalar, q))
        }
        FpInsn::VTwoMisc { scalar, q, sz: s, rn, opcode, a, .. } => {
            // FCVTL reads halves (sz = 0) or singles.
            let e = if !a && opcode == 0b10111 { if s { 32 } else { 16 } } else { sz(s) };
            (vec![rn], e, lanes(scalar, q))
        }
        FpInsn::VAcross { rn, .. } => (vec![rn], 32, 128),
        FpInsn::VPairScalar { sz: s, rn, .. } => (vec![rn], sz(s), 128),
        FpInsn::VFixed { scalar, q, sz: s, rn, to_int: true, .. } => (vec![rn], sz(s), lanes(scalar, q)),
        FpInsn::VIndexed { scalar, q, sz: s, rn, rm, rd, opcode, .. } => {
            let r = if opcode & 0b1011 == 0b0001 { vec![rn, rm, rd] } else { vec![rn, rm] };
            (r, sz(s), lanes(scalar, q))
        }
        _ => return None,
    })
}

/// Why an FP instruction might miss a fast path, from the state before it
/// executes (`before`) and, if known, the FPSR after it: FPCR\[26:19\] in bits
/// 0..7 (FZ16, Stride, RMode, FZ, DN, AHP) and the [`why`] bits. None for
/// instructions that are not FP arithmetic.
pub fn fp_reason(word: u32, cpu: &vetro_cpu::Cpu, fpsr_after: Option<u32>) -> Option<u32> {
    let Insn::Simd(SimdInsn::Fp(f)) = vetro_cpu::decode(word) else { return None };
    let (regs, esize, width) = fp_sources(f)?;
    let mut r = (cpu.fpcr >> 19) & 0xff;
    if cpu.fpsr & 0x10 != 0 {
        r |= why::IXC_SET;
    }
    if let Some(after) = fpsr_after {
        let new = after & !cpu.fpsr;
        r |= (new & 0x1f) << why::NEW_SHIFT | ((new >> 7) & 1) << (why::NEW_SHIFT + 5);
    }
    let (exp_bits, frac_bits) = match esize {
        16 => (5, 10),
        32 => (8, 23),
        _ => (11, 52),
    };
    let n = (width.max(esize)) / esize;
    for &reg in &regs {
        let v = cpu.v[reg as usize];
        for l in 0..n {
            let x = (v >> (l * esize)) as u64 & (u64::MAX >> (64 - esize));
            let e = (x >> frac_bits) & ((1 << exp_bits) - 1);
            let m = x & ((1u64 << frac_bits) - 1);
            r |= match (e, m) {
                (0, 0) => why::IN_ZERO,
                (0, _) => why::IN_DEN,
                (e, 0) if e == (1 << exp_bits) - 1 => why::IN_INF,
                (e, _) if e == (1 << exp_bits) - 1 => why::IN_NAN,
                _ => 0,
            };
        }
    }
    Some(r)
}

/// Readable form of a [`fp_reason`].
pub fn reason_text(r: u32) -> String {
    let mut s = Vec::new();
    let fpcr = (r & 0xff) << 19;
    if fpcr == 0 {
        s.push("fpcr=0".to_string());
    } else {
        let mut c = Vec::new();
        if fpcr & 1 << 26 != 0 {
            c.push("AHP");
        }
        if fpcr & 1 << 25 != 0 {
            c.push("DN");
        }
        if fpcr & 1 << 24 != 0 {
            c.push("FZ");
        }
        c.push(["RN", "RP", "RM", "RZ"][((fpcr >> 22) & 3) as usize]);
        if fpcr & 3 << 20 != 0 {
            c.push("stride");
        }
        if fpcr & 1 << 19 != 0 {
            c.push("FZ16");
        }
        s.push(format!("fpcr={}", c.join("+")));
    }
    s.push(if r & why::IXC_SET != 0 { "ixc=1" } else { "ixc=0" }.into());
    let new = r >> why::NEW_SHIFT & 0x3f;
    if new != 0 {
        let names = ["IOC", "DZC", "OFC", "UFC", "IXC", "IDC"];
        let v: Vec<&str> = (0..6).filter(|b| new & 1 << b != 0).map(|b| names[b]).collect();
        s.push(format!("raised={}", v.join("+")));
    }
    for (b, n) in [(why::IN_NAN, "nan"), (why::IN_DEN, "den"), (why::IN_INF, "inf"), (why::IN_ZERO, "zero")] {
        if r & b != 0 {
            s.push(format!("in:{n}"));
        }
    }
    s.join(" ")
}

impl Profile {
    /// Counts instruction `w`; `sys` as for [`kind_in`]: the class also says
    /// whether the JIT would know how to translate it (then it is cold code or a
    /// step after an exit, not a missing instruction).
    pub fn note(&mut self, w: u32, sys: Option<SysTarget>) {
        *self.words.entry((w, sys)).or_default() += 1;
        self.total += 1;
    }

    /// Counts FP instruction `w` with its [`fp_reason`] `r`.
    pub fn note_fp(&mut self, w: u32, r: u32) {
        *self.reasons.entry((w, r)).or_default() += 1;
    }

    /// The `n` most frequent (class, reason) pairs of [`Profile::note_fp`],
    /// with the instruction's mnemonic-like class.
    pub fn top_reasons(&self, n: usize) -> Vec<(String, u64)> {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (&(w, r), &k) in &self.reasons {
            let c = class(&vetro_cpu::decode(w));
            *counts.entry(format!("{c} | {}", reason_text(r))).or_default() += k;
        }
        let mut v: Vec<(String, u64)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Counts per class.
    fn counts(&self) -> HashMap<String, u64> {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (&(w, sys), &k) in &self.words {
            let insn = vetro_cpu::decode(w);
            let mut c = class(&insn);
            if kind_in(&insn, sys) != Kind::Unsupported {
                c.push_str(" [translated]");
            }
            *counts.entry(c).or_default() += k;
        }
        counts
    }

    /// Instructions counted.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The `n` most frequent classes, in decreasing order.
    pub fn top(&self, n: usize) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = self.counts().into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Readable report of the `n` most frequent classes.
    pub fn report(&self, n: usize) -> String {
        let mut s = format!("interpreter instructions with the JIT active: {}\n", self.total);
        for (c, k) in self.top(n) {
            s += &format!("{k:>12} {:5.1}%  {c}\n", 100.0 * k as f64 / self.total.max(1) as f64);
        }
        if !self.reasons.is_empty() {
            s += "FP by state (FPCR, IXC before, flags raised, input classes):\n";
            for (c, k) in self.top_reasons(n) {
                s += &format!("{k:>12} {:5.1}%  {c}\n", 100.0 * k as f64 / self.total.max(1) as f64);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_without_registers() {
        // fadd d0, d1, d2 and fadd d3, d4, d5 (tools/a64asm.sh): same class.
        let a = class(&vetro_cpu::decode(0x1e622820));
        let b = class(&vetro_cpu::decode(0x1e652883));
        assert_eq!(a, b);
        assert_eq!(a, "fp dp2 op2 ty1");
        let mut p = Profile::default();
        p.note(0x1e622820, None);
        p.note(0x1e652883, None);
        p.note(0x91000421, None); // add x1, x1, #1
        assert_eq!(p.total(), 3);
        assert_eq!(p.top(1)[0].1, 2);
        assert!(p.report(5).contains("AddSubImm [translated]"), "{}", p.report(5));
    }
}
