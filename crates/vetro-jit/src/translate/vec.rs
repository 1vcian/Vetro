//! SIMD intero in linea con le istruzioni WASM a 128 bit (ADR 0026).
//!
//! Solo operazioni esatte per costruzione (niente flag: le saturanti, che
//! scrivono FPSR.QC, restano a `env.simd`), con la semantica di
//! `vetro_cpu::simd::int`: con Q = 0 la metà alta del risultato è zero.
//! Quello che non ha una forma qui torna `false` e lo esegue l'interprete
//! dalla regione ([`Tx::simd_helper`]).

use super::*;
use crate::wasm::v;

/// Opcode WASM per dimensione dell'elemento (8, 16, 32, 64 bit); `None` se
/// l'operazione non esiste a quella dimensione.
type BySize = [Option<u32>; 4];

const ADD: BySize = [Some(v::I8X16_ADD), Some(v::I16X8_ADD), Some(v::I32X4_ADD), Some(v::I64X2_ADD)];
const SUB: BySize = [Some(v::I8X16_SUB), Some(v::I16X8_SUB), Some(v::I32X4_SUB), Some(v::I64X2_SUB)];
const EQ: BySize = [Some(v::I8X16_EQ), Some(v::I16X8_EQ), Some(v::I32X4_EQ), Some(v::I64X2_EQ)];
const GT_S: BySize = [Some(v::I8X16_GT_S), Some(v::I16X8_GT_S), Some(v::I32X4_GT_S), Some(v::I64X2_GT_S)];
const GT_U: BySize = [Some(v::I8X16_GT_U), Some(v::I16X8_GT_U), Some(v::I32X4_GT_U), None];
const GE_S: BySize = [Some(v::I8X16_GE_S), Some(v::I16X8_GE_S), Some(v::I32X4_GE_S), Some(v::I64X2_GE_S)];
const GE_U: BySize = [Some(v::I8X16_GE_U), Some(v::I16X8_GE_U), Some(v::I32X4_GE_U), None];
const MAX_S: BySize = [Some(v::I8X16_MAX_S), Some(v::I16X8_MAX_S), Some(v::I32X4_MAX_S), None];
const MAX_U: BySize = [Some(v::I8X16_MAX_U), Some(v::I16X8_MAX_U), Some(v::I32X4_MAX_U), None];
const MIN_S: BySize = [Some(v::I8X16_MIN_S), Some(v::I16X8_MIN_S), Some(v::I32X4_MIN_S), None];
const MIN_U: BySize = [Some(v::I8X16_MIN_U), Some(v::I16X8_MIN_U), Some(v::I32X4_MIN_U), None];
const MUL: BySize = [None, Some(v::I16X8_MUL), Some(v::I32X4_MUL), None];
const ABS: BySize = [Some(v::I8X16_ABS), Some(v::I16X8_ABS), Some(v::I32X4_ABS), Some(v::I64X2_ABS)];
const NEG: BySize = [Some(v::I8X16_NEG), Some(v::I16X8_NEG), Some(v::I32X4_NEG), Some(v::I64X2_NEG)];
const SHL: BySize = [Some(v::I8X16_SHL), Some(v::I16X8_SHL), Some(v::I32X4_SHL), Some(v::I64X2_SHL)];
const SHR_S: BySize =
    [Some(v::I8X16_SHR_S), Some(v::I16X8_SHR_S), Some(v::I32X4_SHR_S), Some(v::I64X2_SHR_S)];
const SHR_U: BySize =
    [Some(v::I8X16_SHR_U), Some(v::I16X8_SHR_U), Some(v::I32X4_SHR_U), Some(v::I64X2_SHR_U)];
const SPLAT: BySize =
    [Some(v::I8X16_SPLAT), Some(v::I16X8_SPLAT), Some(v::I32X4_SPLAT), Some(v::I64X2_SPLAT)];

/// Indici di `i8x16.shuffle` per un risultato di `n` elementi di `eb` byte
/// in cui l'elemento `e` viene dall'elemento `src(e)` della concatenazione
/// (primo operando: elementi 0..16/eb, secondo: i successivi). Gli elementi
/// oltre `n` ripetono il primo (il chiamante li azzera se servono).
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
    /// Vr (v128) sullo stack.
    pub(super) fn vld(&mut self, r: u8) {
        self.simd = true;
        self.f.local_get(L_STATE).v128_load(off::V + 16 * r as u32);
    }

    /// Prima di calcolare un valore da scrivere in un registro V: l'indirizzo.
    pub(super) fn vst_begin(&mut self) {
        self.simd = true;
        self.f.local_get(L_STATE);
    }

    /// Scrive il v128 in cima allo stack in Vd (metà alta a zero se `!q`).
    pub(super) fn vst_end(&mut self, rd: u8, q: bool) {
        if !q {
            self.f.v128_const(u64::MAX, 0).v(v::AND);
        }
        self.f.v128_store(off::V + 16 * rd as u32);
    }

    /// Vd = op(Vn, Vm) con un'istruzione WASM binaria.
    fn vbin(&mut self, op_: u32, rn: u8, rm: u8, rd: u8, q: bool) {
        self.vst_begin();
        self.vld(rn);
        self.vld(rm);
        self.f.v(op_);
        self.vst_end(rd, q);
    }

    /// Istruzioni SIMD intere in linea; falso se non ce n'è una forma (le
    /// esegue `env.simd`).
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
                    // UZP: elementi pari (o dispari) di concat(b:a)
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
                    // concat(hi.lo : lo.lo) >> pos, 8 byte.
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
                // swizzle dà 0 per gli indici >= 16: la tabella k usa
                // indice - 16k (gli indici fuori tabella danno 0 in tutte).
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
                    // Indici fuori tabella: il byte di Vd.
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
            // Logiche su tutto il registro.
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
            (true, 0b00010) if size <= 1 => {
                // URHADD: (a + b + 1) >> 1 senza traboccare
                bin(self, [Some(v::I8X16_AVGR_U), Some(v::I16X8_AVGR_U), None, None])
            }
            (false, 0b10111) => {
                // ADDP: somme a coppie di concat(a, b)
                let Some(add) = ADD[s] else { return false };
                let eb = 1usize << size;
                let n = (if q { 16 } else { 8 }) / eb;
                let half = n / 2;
                // coppia e: elementi 2e, 2e+1 di a (e < n/2) o di b
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
        // Confronto con zero: op(Vn, 0) (op su (x, 0)) o op(0, Vn).
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
                // [SU]ADDLP, [SU]ADALP: somme a coppie estese
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
            (false, 0b10010) => {
                // XTN/XTN2: la metà bassa di ogni elemento da 2*esize.
                let eb = 1usize << size;
                let n = 8 / eb;
                let l = lanes(n, eb, |e| 2 * e);
                self.vst_begin();
                if q {
                    // XTN2: metà bassa di Vd, poi i risultati nella metà alta.
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
        // Operazione di riduzione e dimensione del risultato.
        let (o, res_bytes, widen) = match (u, opcode) {
            (_, 0b11011) => (ADD[s], eb, None),
            (false, 0b01010) => (MAX_S[s], eb, None),
            (true, 0b01010) => (MAX_U[s], eb, None),
            (false, 0b11010) => (MIN_S[s], eb, None),
            (true, 0b11010) => (MIN_U[s], eb, None),
            (_, 0b00011) if size <= 1 => {
                // [SU]ADDLV: prima le somme a coppie estese, poi le somme.
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
                // Somme: metà alta a zero.
                self.f.v128_const(u64::MAX, 0).v(v::AND);
            } else {
                // Massimi e minimi: la metà bassa ripetuta.
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
        // Piega a metà finché resta un elemento.
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
                    // SSHR #esize: come #(esize - 1), il segno ovunque.
                    self.f.i32_const(shift.min(esize - 1) as i32).v(sh);
                }
                if opcode == 0b00010 {
                    self.f.v(add);
                }
                self.vst_end(rd, q);
                true
            }
            (_, 0b10100) => {
                // [SU]SHLL(2) #shift, UXTL/SXTL: estensione della metà
                // bassa (o alta) e shift a sinistra.
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
                // SHRN(2) #shift: esize è la destinazione; elementi da 2*esize.
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

    /// DUP da elemento e da registro generale con v128 (i due casi più
    /// frequenti di `CopyOp`, gli altri restano in `vec_int`).
    #[allow(dead_code)]
    fn splat_op(esize: u32) -> u32 {
        SPLAT[esize.trailing_zeros() as usize - 3].expect("tutte le dimensioni")
    }
}
