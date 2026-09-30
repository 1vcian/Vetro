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

/// Per-class counters. Instruction words are counted as they come (cheap:
/// the profile runs on every interpreter step and `env.simd` call) and
/// grouped into classes only for the report.
#[derive(Clone, Debug, Default)]
pub struct Profile {
    words: HashMap<(u32, Option<SysTarget>), u64>,
    total: u64,
}

impl Profile {
    /// Counts instruction `w`; `sys` as for [`kind_in`]: the class also says
    /// whether the JIT would know how to translate it (then it is cold code or a
    /// step after an exit, not a missing instruction).
    pub fn note(&mut self, w: u32, sys: Option<SysTarget>) {
        *self.words.entry((w, sys)).or_default() += 1;
        self.total += 1;
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
