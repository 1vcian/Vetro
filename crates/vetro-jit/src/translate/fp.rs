//! Virgola mobile nelle regioni (ADR 0026).
//!
//! Le operazioni senza arrotondamento (FMOV, FABS, FNEG, FCSEL) sono in
//! linea. Le altre hanno un **percorso veloce** nel runtime (`rt.fp<k>`):
//! la regione chiama la funzione con la parola dell'istruzione, che legge i
//! registri da `JitState`, calcola col WASM e scrive il risultato solo se
//! è sicuramente quello dell'interprete (`vetro_cpu::simd::fp`):
//!
//! - FPCR = 0 (arrotondamento al pari più vicino, niente FZ né DN): è
//!   l'arrotondamento dell'IEEE 754 del WASM, e i denormali in ingresso
//!   valgono per quello che sono (con FZ l'Arm li azzera e segnala IDC);
//! - niente NaN in ingresso o in uscita (i bit dei NaN del WASM non sono
//!   fissati, e l'Arm li propaga con regole sue), niente infiniti prodotti
//!   da un trabocco, niente risultati minuscoli dove l'Arm segnala UFC
//!   (moltiplicazioni, divisioni, FMA, conversioni che restringono:
//!   risultato normale e maggiore del più piccolo normale, perché l'Arm
//!   guarda la minuscolità prima dell'arrotondamento);
//! - IXC: se è già a 1 in FPSR (flag cumulativo) l'inesattezza non cambia
//!   nulla; altrimenti il percorso veloce vale solo se il risultato è
//!   esatto, verificato in modo esatto (TwoSum per le somme; in singola
//!   precisione prodotti, quozienti e radici si ricontrollano in doppia,
//!   dove sono esatti); dove non si sa verificarlo serve IXC a 1.
//!
//! Altrimenti la funzione chiama `env.simd` (l'interprete, [`crate::helper`]):
//! stesso risultato, più lento. La FMA in singola precisione si calcola in
//! doppia con l'arrotondamento "a dispari" (Boldo e Melquiond): il
//! prodotto è esatto, e la somma arrotondata a dispari in doppia, poi al
//! pari in singola, dà l'arrotondamento corretto della FMA.

use super::*;
use crate::wasm::{sat, v};
use vetro_cpu::simd::FpInsn;

/// Operazioni binarie in virgola mobile.
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
}

/// Arrotondamento verso un intero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rnd {
    Nearest,
    Ceil,
    Floor,
    Trunc,
    /// FRINTX: al pari più vicino, IXC se cambia.
    NearestX,
}

/// Confronti vettoriali.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cmp {
    Eq,
    Ge,
    Gt,
}

/// Una funzione `rt.fp<k>` del runtime (`d`: doppia precisione).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FpRt {
    /// FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM, FNMUL scalari.
    Bin {
        d: bool,
        op: Bin,
    },
    /// FMADD, FMSUB, FNMADD, FNMSUB in singola precisione.
    Fma {
        neg_a: bool,
        neg_n: bool,
    },
    Sqrt {
        d: bool,
    },
    /// FCMP/FCMPE (anche con zero): restituisce NZCV.
    Cmp {
        d: bool,
    },
    /// FCVT da doppia a singola e viceversa.
    CvtDS,
    CvtSD,
    Frint {
        d: bool,
        r: Rnd,
    },
    /// SCVTF/UCVTF da registro generale (`sf`: 64 bit).
    FromInt {
        d: bool,
        sf: bool,
        u: bool,
    },
    /// FCVT[NPMZ][SU] verso un registro generale.
    ToInt {
        d: bool,
        sf: bool,
        u: bool,
        r: Rnd,
    },
    /// FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM vettoriali (Q
    /// dalla parola).
    VBin {
        d: bool,
        op: Bin,
    },
    /// FMLA/FMLS vettoriali in singola precisione.
    VFma {
        neg: bool,
    },
    /// FMUL per elemento; FMLA/FMLS per elemento (solo singola).
    VIdxMul {
        d: bool,
    },
    VIdxFma {
        neg: bool,
    },
    /// FCMEQ, FCMGE, FCMGT con registro o con zero; `swap` confronta
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
}

/// Tutte le funzioni `rt.fp<k>`, nell'ordine degli indici (da `F_FP0`).
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
            for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc, Rnd::NearestX] {
                v.push(FpRt::Frint { d, r });
            }
            for sf in [false, true] {
                for u in [false, true] {
                    v.push(FpRt::FromInt { d, sf, u });
                    for r in [Rnd::Nearest, Rnd::Ceil, Rnd::Floor, Rnd::Trunc] {
                        v.push(FpRt::ToInt { d, sf, u, r });
                    }
                }
            }
            for op in &bins[..8] {
                v.push(FpRt::VBin { d, op: *op });
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
        v.push(FpRt::CvtDS);
        v.push(FpRt::CvtSD);
        for neg in [false, true] {
            v.push(FpRt::VFma { neg });
            v.push(FpRt::VIdxFma { neg });
        }
        v
    })
}

/// Indice nel runtime della funzione `op`.
pub(super) fn rt_id(op: FpRt) -> u32 {
    F_FP0 + rt_ops().iter().position(|o| *o == op).expect("funzione rt.fp conosciuta") as u32
}

/// Nome e firma della funzione `k` di [`rt_ops`].
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

// --- nelle regioni ----------------------------------------------------

impl Tx {
    /// Istruzioni FP in linea o con un percorso veloce del runtime; falso se
    /// le esegue `env.simd`.
    pub(super) fn fp_inline(&mut self, i: FpInsn) -> bool {
        let w = self.word as i32;
        let call = |t: &mut Tx, op_: FpRt| {
            t.simd = true;
            t.f.local_get(L_STATE).i32_const(w).call(rt_id(op_));
        };
        match i {
            FpInsn::Dp1 { ty: ty @ 0..=1, opcode: opcode @ 0..=2, rn, rd } => {
                // FMOV, FABS, FNEG: bit, senza arrotondamento (anche sui NaN).
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
            FpInsn::Dp1 { ty, opcode: opcode @ (8..=11 | 14 | 15), .. } if ty <= 1 => {
                let r = match opcode {
                    8 | 15 => Rnd::Nearest,
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
            FpInsn::Dp3 { ty: 0, neg_a, neg_n, .. } => {
                call(self, FpRt::Fma { neg_a, neg_n });
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
                    _ => return false,
                };
                call(self, FpRt::ToInt { d: ty == 1, sf, u: unsigned, r });
                self.set_x(rd);
                true
            }
            FpInsn::FromInt { ty, sf, unsigned, fbits: 0, rn, .. } => {
                self.simd = true;
                self.f.local_get(L_STATE).i32_const(w);
                self.get_x(rn);
                self.f.call(rt_id(FpRt::FromInt { d: ty == 1, sf, u: unsigned }));
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
                    (false, false, 0b11100) => FpRt::VCmp { d, op: Cmp::Eq, zero: false, swap: false },
                    (true, false, 0b11100) => FpRt::VCmp { d, op: Cmp::Ge, zero: false, swap: false },
                    (true, true, 0b11100) => FpRt::VCmp { d, op: Cmp::Gt, zero: false, swap: false },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            FpInsn::VTwoMisc { scalar: false, q, u, a: true, sz, opcode, rn, rd } => {
                let d = sz;
                let op_ = match (u, opcode) {
                    (_, 0b01111) => {
                        // FABS / FNEG vettoriali: bit.
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
            FpInsn::VIndexed { scalar: false, u: false, sz, opcode, .. } => {
                let op_ = match opcode {
                    0b1001 => FpRt::VIdxMul { d: sz },
                    0b0001 if !sz => FpRt::VIdxFma { neg: false },
                    0b0101 if !sz => FpRt::VIdxFma { neg: true },
                    _ => return false,
                };
                call(self, op_);
                true
            }
            _ => false,
        }
    }
}

// --- costruzione delle funzioni del runtime -----------------------------

const S_INF: u32 = 0x7f80_0000;
const S_MIN_NORMAL: u32 = 0x0080_0000;
const D_INF: u64 = 0x7ff0_0000_0000_0000;
const D_MIN_NORMAL: u64 = 0x0010_0000_0000_0000;
const IXC: i32 = 0x10;

/// Parametri delle funzioni: stato, parola, [x].
const P_STATE: u32 = 0;
const P_WORD: u32 = 1;
const P_X: u32 = 2;

/// Generatore di una funzione `rt.fp<k>`: `simd` è l'indice di
/// `env.simd` nel runtime.
struct G {
    f: Func,
    simd: u32,
    /// Prossima variabile locale (dopo i parametri).
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

    /// Indirizzo (i32) di `JitState` più 16 × il registro nel campo di 5
    /// bit della parola che inizia al bit `shift`: con offset `off::V` è il
    /// registro.
    fn vaddr(&mut self, shift: i32) {
        let f = &mut self.f;
        f.local_get(P_WORD);
        if shift != 0 {
            f.i32_const(shift).op(op::I32_SHR_U);
        }
        f.i32_const(31).op(op::I32_AND).i32_const(4).op(op::I32_SHL).local_get(P_STATE).op(op::I32_ADD);
    }

    /// Elemento 0 del registro (campo a `shift`) come f32/f64.
    fn load(&mut self, shift: i32, d: bool) {
        self.vaddr(shift);
        if d {
            self.f.f64_load(off::V);
        } else {
            self.f.f32_load(off::V);
        }
    }

    /// Il registro (campo a `shift`) come v128.
    fn vload(&mut self, shift: i32) {
        self.vaddr(shift);
        self.f.v128_load(off::V);
    }

    /// Vd = i bit (i64, estesi a zero) nella variabile `bits`, metà alta a
    /// zero (risultato scalare).
    fn store_scalar_bits(&mut self, bits: u32) {
        self.vaddr(0);
        self.f.local_get(bits).i64_store(off::V);
        self.vaddr(0);
        self.f.i64_const(0).i64_store(off::V + 8);
    }

    /// Vd = il v128 nella variabile `r`, metà alta a zero con Q = 0.
    fn store_vec(&mut self, r: u32) {
        self.vaddr(0);
        self.f.local_get(r);
        self.himask();
        self.f.v(v::ANDNOT);
        self.f.v128_store(off::V);
    }

    /// Q (bit 30 della parola, i32).
    fn q(&mut self) {
        self.f.local_get(P_WORD).i32_const(30).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
    }

    /// v128 con la metà alta a uno se Q = 0, altrimenti zero: le corsie da
    /// ignorare nei controlli.
    fn himask(&mut self) {
        self.q();
        self.f.op(op::I64_EXTEND_I32_U).i64_const(1).op(op::I64_SUB).lane_splat64();
        self.f.v128_const(0, u64::MAX).v(v::AND);
    }

    /// FPCR == 0 (i32 booleano).
    fn fpcr_zero(&mut self) {
        self.f.local_get(P_STATE).i32_load(off::FPCR).op(op::I32_EQZ);
    }

    /// IXC già a 1 in FPSR (i32, non zero se sì).
    fn ixc(&mut self) {
        self.f.local_get(P_STATE).i32_load(off::FPSR).i32_const(IXC).op(op::I32_AND);
    }

    /// `env.simd` (l'interprete) con `x` e NZCV = 0; il risultato (i64)
    /// resta sullo stack.
    fn fallback(&mut self, x: bool) {
        self.f.local_get(P_STATE).local_get(P_WORD);
        if x {
            self.f.local_get(P_X);
        } else {
            self.f.i64_const(0);
        }
        self.f.i32_const(0).call(self.simd);
    }

    /// Bit (i64) del float in `l` (f32 esteso a zero, o f64).
    fn bits64(&mut self, l: u32, d: bool) {
        self.f.local_get(l);
        if d {
            self.f.op(op::I64_REINTERPRET_F64);
        } else {
            self.f.op(op::I32_REINTERPRET_F32).op(op::I64_EXTEND_I32_U);
        }
    }

    /// Bit (i64) in cima allo stack: finito (né NaN né infinito).
    fn finite_bits(&mut self, d: bool) {
        let (m, inf) = if d { (i64::MAX, D_INF as i64) } else { (0x7fff_ffff, S_INF as i64) };
        self.f.i64_const(m).op(op::I64_AND).i64_const(inf).op(op::I64_LT_U);
    }

    /// Bit (i64) in cima allo stack: normale e maggiore del più piccolo
    /// normale (niente UFC né OFC).
    fn safe_bits(&mut self, d: bool) {
        let (m, lo, inf) = if d {
            (i64::MAX, D_MIN_NORMAL as i64 + 1, D_INF as i64)
        } else {
            (0x7fff_ffff, S_MIN_NORMAL as i64 + 1, S_INF as i64)
        };
        self.f.i64_const(m).op(op::I64_AND).i64_const(lo).op(op::I64_SUB);
        self.f.i64_const(inf - lo).op(op::I64_LT_U);
    }

    /// Non NaN (i32) del float in `l`.
    fn not_nan(&mut self, l: u32, d: bool) {
        self.f.local_get(l).local_get(l).op(fop(d, op::F32_EQ, op::F64_EQ));
    }

    /// Costante float 0 del tipo.
    fn fzero(&mut self, d: bool) {
        if d {
            self.f.i64_const(0).op(op::F64_REINTERPRET_I64);
        } else {
            self.f.i32_const(0).op(op::F32_REINTERPRET_I32);
        }
    }

    /// Chiude una funzione senza risultato: se `ok` scrive (con `store`) e
    /// torna, altrimenti `env.simd`.
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

/// Costruisce la funzione `k` di [`rt_ops`].
pub(super) fn build(k: usize, simd: u32) -> Func {
    match rt_ops()[k] {
        FpRt::Bin { d, op } => bin(simd, d, op),
        FpRt::Fma { neg_a, neg_n } => fma_s(simd, neg_a, neg_n),
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
    }
}

/// TwoSum: in `err` l'errore esatto (a + b) - s, con s = a + b già
/// arrotondato (Knuth). Tutti f32 o f64 (`d`).
fn two_sum(g: &mut G, d: bool, a: u32, b: u32, s: u32, err: u32) {
    let (add, sub) = (fop(d, op::F32_ADD, op::F64_ADD), fop(d, op::F32_SUB, op::F64_SUB));
    let t = g.local(if d { ValType::F64 } else { ValType::F32 });
    let f = &mut g.f;
    // t = s - a; err = (a - (s - t)) + (b - t)
    f.local_get(s).local_get(a).op(sub).local_set(t);
    f.local_get(a).local_get(s).local_get(t).op(sub).op(sub);
    f.local_get(b).local_get(t).op(sub).op(add).local_set(err);
}

/// FADD, FSUB, FMUL, FDIV, FMAX, FMIN, FMAXNM, FMINNM, FNMUL scalari.
fn bin(simd: u32, d: bool, op_: Bin) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, b, r) = (g.local(ft), g.local(ft), g.local(ft));
    let (bits, ok) = (g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_set(a);
    g.load(16, d);
    if op_ == Bin::Sub {
        // a - b = a + (-b), anche per gli zeri.
        g.f.op(fop(d, op::F32_NEG, op::F64_NEG));
    }
    g.f.local_set(b);
    g.f.local_get(a).local_get(b);
    g.f.op(match op_ {
        Bin::Add | Bin::Sub => fop(d, op::F32_ADD, op::F64_ADD),
        Bin::Mul | Bin::Nmul => fop(d, op::F32_MUL, op::F64_MUL),
        Bin::Div => fop(d, op::F32_DIV, op::F64_DIV),
        Bin::Max | Bin::MaxNm => fop(d, op::F32_MAX, op::F64_MAX),
        Bin::Min | Bin::MinNm => fop(d, op::F32_MIN, op::F64_MIN),
    });
    g.f.local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    match op_ {
        Bin::Add | Bin::Sub => {
            // Finito, ed esatto (TwoSum) o con IXC già a 1. Una somma non
            // dà mai un risultato minuscolo inesatto.
            let err = g.local(ft);
            two_sum(&mut g, d, a, b, r, err);
            g.f.local_get(bits);
            g.finite_bits(d);
            g.ixc();
            g.f.local_get(err);
            g.fzero(d);
            g.f.op(fop(d, op::F32_EQ, op::F64_EQ)).op(op::I32_OR).op(op::I32_AND).local_set(ok);
        }
        Bin::Max | Bin::Min | Bin::MaxNm | Bin::MinNm => {
            // Niente NaN: nessun flag, e gli zeri di segno opposto come
            // l'Arm (max +0, min -0).
            g.not_nan(a, d);
            g.not_nan(b, d);
            g.f.op(op::I32_AND).local_set(ok);
        }
        Bin::Mul | Bin::Nmul | Bin::Div => {
            // Normale sicuro, o (prodotti) zero esatto con un fattore zero.
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
            // Esatto, o IXC già a 1.
            g.ixc();
            if !d {
                let f = &mut g.f;
                if op_ == Bin::Div {
                    // f64(r) * f64(b) == f64(a): 24 + 24 bit, esatto.
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
                f.op(op::I32_OR);
            }
            g.f.op(op::I32_AND).local_set(ok);
        }
    }
    g.commit(ok, |g| {
        if op_ == Bin::Nmul {
            // Il segno si cambia dopo l'arrotondamento.
            g.f.local_get(bits)
                .i64_const(if d { i64::MIN } else { 0x8000_0000 })
                .op(op::I64_XOR)
                .local_set(bits);
        }
        g.store_scalar_bits(bits);
    });
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Arrotondamento "a dispari" di `s` (f64, variabile) dato l'errore esatto
/// `err`: se inesatto e pari, il vicino dispari dalla parte dell'errore.
/// Lascia i bit (i64) sullo stack.
fn round_odd(g: &mut G, s: u32, err: u32) {
    let sb = g.local(ValType::I64);
    let f = &mut g.f;
    f.local_get(s).op(op::I64_REINTERPRET_F64).local_tee(sb);
    // delta = (sb ^ bits(err)) >= 0 ? 1 : -1
    f.i64_const(1).i64_const(-1);
    f.local_get(sb).local_get(err).op(op::I64_REINTERPRET_F64).op(op::I64_XOR).i64_const(0).op(op::I64_GE_S);
    f.op(op::SELECT);
    f.i64_const(0);
    // inesatto e pari
    f.local_get(err).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_NE);
    f.local_get(sb).i64_const(1).op(op::I64_AND).op(op::I64_EQZ).op(op::I32_AND);
    f.op(op::SELECT).op(op::I64_ADD);
}

/// FMADD/FMSUB/FNMADD/FNMSUB in singola precisione: (±a) + (±n) × m fusa.
fn fma_s(simd: u32, neg_a: bool, neg_n: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (p, c, s, err) =
        (g.local(ValType::F64), g.local(ValType::F64), g.local(ValType::F64), g.local(ValType::F64));
    let (r, bits, ok) = (g.local(ValType::F32), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    // p = f64(±n) * f64(m), esatto
    g.load(5, false);
    if neg_n {
        g.f.op(op::F32_NEG);
    }
    g.f.op(op::F64_PROMOTE_F32);
    g.load(16, false);
    g.f.op(op::F64_PROMOTE_F32).op(op::F64_MUL).local_set(p);
    g.load(10, false);
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
    // esatto: err == 0 e f64(r) == s
    g.ixc();
    g.f.local_get(err).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_EQ);
    g.f.local_get(r).op(op::F64_PROMOTE_F32).local_get(s).op(op::F64_EQ).op(op::I32_AND);
    g.f.op(op::I32_OR).op(op::I32_AND).local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FSQRT scalare.
fn sqrt(simd: u32, d: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) = (g.local(ft), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_tee(a).op(fop(d, op::F32_SQRT, op::F64_SQRT)).local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    // a >= 0 (esclude NaN e negativi; ±0 e +inf danno se stessi, esatti)
    g.f.local_get(a);
    g.fzero(d);
    g.f.op(fop(d, op::F32_GE, op::F64_GE));
    g.ixc();
    if !d {
        // f64(r)² == f64(a), esatto in doppia
        let f = &mut g.f;
        f.local_get(r).op(op::F64_PROMOTE_F32).local_get(r).op(op::F64_PROMOTE_F32).op(op::F64_MUL);
        f.local_get(a).op(op::F64_PROMOTE_F32).op(op::F64_EQ).op(op::I32_OR);
    }
    g.f.op(op::I32_AND).local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCMP/FCMPE (bit 3: con zero): NZCV senza NaN.
fn cmp(simd: u32, d: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, b) = (g.local(ft), g.local(ft));
    let (lt, eq) = (fop(d, op::F32_LT, op::F64_LT), fop(d, op::F32_EQ, op::F64_EQ));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_set(a);
    // Con il bit 3 (FCMP con zero) b = 0, altrimenti Vm.
    g.fzero(d);
    g.load(16, d);
    g.f.local_get(P_WORD).i32_const(8).op(op::I32_AND).op(op::SELECT).local_set(b);
    g.not_nan(a, d);
    g.not_nan(b, d);
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
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
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, true);
    g.f.local_tee(a).op(op::F32_DEMOTE_F64).local_set(r);
    g.bits64(r, false);
    g.f.local_tee(bits);
    g.safe_bits(false);
    // o zero da zero
    g.f.local_get(a).i64_const(0).op(op::F64_REINTERPRET_I64).op(op::F64_EQ).op(op::I32_OR);
    g.ixc();
    g.f.local_get(r).op(op::F64_PROMOTE_F32).local_get(a).op(op::F64_EQ).op(op::I32_OR);
    g.f.op(op::I32_AND).local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVT Dd, Sn: esatta per ogni valore che non è NaN.
fn cvt_sd(simd: u32) -> Func {
    let mut g = G::new(simd, 2);
    let (a, r, bits) = (g.local(ValType::F32), g.local(ValType::F64), g.local(ValType::I64));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, false);
    g.f.local_tee(a).op(op::F64_PROMOTE_F32).local_set(r);
    g.bits64(r, true);
    g.f.local_set(bits);
    g.not_nan(a, false);
    g.f.if_(BLOCK_EMPTY);
    g.store_scalar_bits(bits);
    g.f.op(op::RETURN).end();
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Arrotondamento `r` di f32/f64 (sullo stack).
fn round_op(g: &mut G, d: bool, r: Rnd) {
    g.f.op(match r {
        Rnd::Nearest | Rnd::NearestX => fop(d, op::F32_NEAREST, op::F64_NEAREST),
        Rnd::Ceil => fop(d, op::F32_CEIL, op::F64_CEIL),
        Rnd::Floor => fop(d, op::F32_FLOOR, op::F64_FLOOR),
        Rnd::Trunc => fop(d, op::F32_TRUNC, op::F64_TRUNC),
    });
}

/// FRINT[NPMZIX] scalari (senza NaN: nessun flag, tranne IXC di FRINTX).
fn frint(simd: u32, d: bool, rnd: Rnd) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 2);
    let (a, r, bits, ok) = (g.local(ft), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    g.f.local_tee(a);
    round_op(&mut g, d, rnd);
    g.f.local_set(r);
    g.bits64(r, d);
    g.f.local_set(bits);
    g.not_nan(a, d);
    if rnd == Rnd::NearestX {
        g.ixc();
        g.f.local_get(r).local_get(a).op(fop(d, op::F32_EQ, op::F64_EQ)).op(op::I32_OR).op(op::I32_AND);
    }
    g.f.local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// SCVTF/UCVTF da registro generale.
fn from_int(simd: u32, d: bool, sf: bool, u: bool) -> Func {
    let ft = if d { ValType::F64 } else { ValType::F32 };
    let mut g = G::new(simd, 3);
    let (x, r, bits, ok) = (g.local(ValType::I64), g.local(ft), g.local(ValType::I64), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    // x esteso a 64 bit come lo legge l'istruzione.
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
    // Esatto se |x| sta nella mantissa, o IXC già a 1.
    let mant: i64 = if d { 1 << 53 } else { 1 << 24 };
    g.ixc();
    if u {
        g.f.local_get(x).i64_const(mant).op(op::I64_LE_U);
    } else {
        g.f.local_get(x).i64_const(mant).op(op::I64_ADD).i64_const(2 * mant).op(op::I64_LE_U);
    }
    g.f.op(op::I32_OR).local_set(ok);
    g.commit(ok, |g| g.store_scalar_bits(bits));
    g.f.end();
    g.fallback(true);
    g.f.op(op::DROP);
    g.finish()
}

/// FCVT[NPMZ][SU] verso un registro generale: arrotondamento in doppia
/// (esatto anche per un valore in singola), controllo dell'intervallo sul
/// valore arrotondato (come `FPToFixed`), poi la conversione saturante
/// (qui esatta).
fn to_int(simd: u32, d: bool, sf: bool, u: bool, rnd: Rnd) -> Func {
    let mut g = G::new(simd, 2);
    let (a, t) = (g.local(ValType::F64), g.local(ValType::F64));
    let ok = g.local(ValType::I32);
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.load(5, d);
    if !d {
        g.f.op(op::F64_PROMOTE_F32);
    }
    g.f.local_tee(a);
    round_op(&mut g, true, rnd);
    g.f.local_set(t);
    // intervallo: [lo, hi)
    let (lo, hi): (f64, f64) = match (sf, u) {
        (true, false) => (-9_223_372_036_854_775_808.0, 9_223_372_036_854_775_808.0),
        (false, false) => (-2_147_483_648.0, 2_147_483_648.0),
        (true, true) => (0.0, 18_446_744_073_709_551_616.0),
        (false, true) => (0.0, 4_294_967_296.0),
    };
    let f = &mut g.f;
    f.local_get(t).i64_const(lo.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_GE);
    f.local_get(t).i64_const(hi.to_bits() as i64).op(op::F64_REINTERPRET_I64).op(op::F64_LT).op(op::I32_AND);
    g.ixc();
    g.f.local_get(t).local_get(a).op(op::F64_EQ).op(op::I32_OR).op(op::I32_AND).local_set(ok);
    g.f.local_get(ok).if_(BLOCK_EMPTY);
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

// --- vettoriali --------------------------------------------------------

/// Costante v128 con `bits` in ogni corsia (32 o 64 bit).
fn splat_const(g: &mut G, d: bool, bits: u64) {
    let w = if d { bits } else { (bits & 0xffff_ffff) * 0x1_0000_0001 };
    g.f.v128_const(w, w);
}

/// Maschera (v128) delle corsie finite di `r`.
fn vfinite(g: &mut G, d: bool, r: u32) {
    g.f.local_get(r).v(vop(d, v::F32X4_ABS, v::F64X2_ABS));
    splat_const(g, d, if d { D_INF } else { S_INF as u64 });
    g.f.v(vop(d, v::F32X4_LT, v::F64X2_LT));
}

/// Maschera delle corsie normali e maggiori del più piccolo normale.
fn vsafe(g: &mut G, d: bool, r: u32) {
    g.f.local_get(r).v(vop(d, v::F32X4_ABS, v::F64X2_ABS));
    splat_const(g, d, if d { D_MIN_NORMAL } else { S_MIN_NORMAL as u64 });
    g.f.v(vop(d, v::F32X4_GT, v::F64X2_GT));
    vfinite(g, d, r);
    g.f.v(v::AND);
}

/// Maschera delle corsie non NaN di `x`.
fn vnot_nan(g: &mut G, d: bool, x: u32) {
    g.f.local_get(x).local_get(x).v(vop(d, v::F32X4_EQ, v::F64X2_EQ));
}

/// Tutte le corsie della maschera in cima allo stack a uno, contando come
/// vere quelle da ignorare con Q = 0 (i32).
fn all_true(g: &mut G, d: bool) {
    g.himask();
    g.f.v(v::OR).v(vop(d, v::I32X4_ALL_TRUE, v::I64X2_ALL_TRUE));
}

fn vbin(simd: u32, d: bool, op_: Bin) -> Func {
    let mut g = G::new(simd, 2);
    let (a, b, r, ok) =
        (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_zero();
    g.f.if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_set(a);
    g.vload(16);
    if op_ == Bin::Sub {
        g.f.v(vop(d, v::F32X4_NEG, v::F64X2_NEG));
    }
    g.f.local_set(b);
    g.f.local_get(a).local_get(b);
    g.f.v(match op_ {
        Bin::Add | Bin::Sub => vop(d, v::F32X4_ADD, v::F64X2_ADD),
        Bin::Mul | Bin::Nmul => vop(d, v::F32X4_MUL, v::F64X2_MUL),
        Bin::Div => vop(d, v::F32X4_DIV, v::F64X2_DIV),
        Bin::Max | Bin::MaxNm => vop(d, v::F32X4_MAX, v::F64X2_MAX),
        Bin::Min | Bin::MinNm => vop(d, v::F32X4_MIN, v::F64X2_MIN),
    });
    g.f.local_set(r);
    match op_ {
        Bin::Add | Bin::Sub => {
            // Finite, ed esatte (TwoSum per corsia) o con IXC a 1.
            let (t, err) = (g.local(ValType::V128), g.local(ValType::V128));
            let (add, sub) = (vop(d, v::F32X4_ADD, v::F64X2_ADD), vop(d, v::F32X4_SUB, v::F64X2_SUB));
            let f = &mut g.f;
            f.local_get(r).local_get(a).v(sub).local_set(t);
            f.local_get(a).local_get(r).local_get(t).v(sub).v(sub);
            f.local_get(b).local_get(t).v(sub).v(add).local_set(err);
            vfinite(&mut g, d, r);
            all_true(&mut g, d);
            g.ixc();
            g.f.local_get(err).v128_const(0, 0).v(vop(d, v::F32X4_EQ, v::F64X2_EQ));
            all_true(&mut g, d);
            g.f.op(op::I32_OR).op(op::I32_AND).local_set(ok);
        }
        Bin::Max | Bin::Min | Bin::MaxNm | Bin::MinNm => {
            vnot_nan(&mut g, d, a);
            vnot_nan(&mut g, d, b);
            g.f.v(v::AND);
            all_true(&mut g, d);
            g.f.local_set(ok);
        }
        Bin::Mul | Bin::Nmul | Bin::Div => {
            // Normali sicure e IXC a 1.
            vsafe(&mut g, d, r);
            all_true(&mut g, d);
            g.ixc();
            g.f.op(op::I32_AND).local_set(ok);
        }
    }
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Metà bassa (corsie 0 e 1) di una FMA in singola: (d + n × m) in doppia
/// arrotondata a dispari, poi in singola (corsie 0 e 1 del risultato, le
/// altre zero).
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
    // a dispari: s + (inesatto & pari ? (stesso segno ? 1 : -1) : 0)
    f.local_get(s);
    f.v128_const(1, 1).v128_const(u64::MAX, u64::MAX);
    f.local_get(s).local_get(err).v(v::XOR).v128_const(0, 0).v(v::I64X2_GE_S);
    f.v(v::BITSELECT);
    f.local_get(err).v128_const(0, 0).v(v::F64X2_NE);
    f.local_get(s).v128_const(1, 1).v(v::AND).v128_const(0, 0).v(v::I64X2_EQ).v(v::AND);
    f.v(v::AND).v(v::I64X2_ADD);
    f.v(v::F32X4_DEMOTE_F64X2_ZERO);
}

/// FMLA/FMLS vettoriali (o per elemento, `idx`) in singola precisione.
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
    g.fpcr_zero();
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
    // metà alta nelle corsie 0 e 1
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
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// Vm[indice] ripetuto in tutte le corsie (FMUL/FMLA per elemento): Rm nei
/// bit 20:16, indice H:L (singola) o H (doppia).
fn elem_splat(g: &mut G, d: bool) {
    g.vload(16);
    // byte della corsia: indice × dimensione + [0..dimensione)
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

/// FMUL per elemento.
fn vidx_mul(simd: u32, d: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (r, ok) = (g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_zero();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.vload(5);
    elem_splat(&mut g, d);
    g.f.v(vop(d, v::F32X4_MUL, v::F64X2_MUL)).local_set(r);
    vsafe(&mut g, d, r);
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FCMEQ/FCMGE/FCMGT (e con zero) senza NaN: maschere del WASM.
fn vcmp(simd: u32, d: bool, op_: Cmp, zero: bool, swap: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (a, b, r, ok) =
        (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_zero();
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
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}

/// FSQRT vettoriale: corsie >= 0, IXC a 1.
fn vsqrt(simd: u32, d: bool) -> Func {
    let mut g = G::new(simd, 2);
    let (a, r, ok) = (g.local(ValType::V128), g.local(ValType::V128), g.local(ValType::I32));
    g.fpcr_zero();
    g.ixc();
    g.f.op(op::I32_AND).if_(BLOCK_EMPTY);
    g.vload(5);
    g.f.local_tee(a).v(vop(d, v::F32X4_SQRT, v::F64X2_SQRT)).local_set(r);
    g.f.local_get(a).v128_const(0, 0).v(vop(d, v::F32X4_GE, v::F64X2_GE));
    all_true(&mut g, d);
    g.f.local_set(ok);
    g.commit(ok, |g| g.store_vec(r));
    g.f.end();
    g.fallback(false);
    g.f.op(op::DROP);
    g.finish()
}
