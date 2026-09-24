//! Programmi casuali di istruzioni intere, da confrontare con QEMU (ADR 0006).
//!
//! Ogni istruzione nasce da bit casuali dentro la maschera della sua classe
//! (come RISU), poi si correggono solo i campi che servono a mantenere il
//! programma confrontabile:
//! - x27/x28 non vengono mai scritti (x28 è la base della memoria, x27
//!   l'indice piccolo per gli indirizzamenti a registro);
//! - i load/store usano x28 come base e offset dentro il blocco di memoria;
//!   dopo un writeback il valore di x28 si copia in un registro visibile e
//!   x28 si ripristina;
//! - i salti vanno solo in avanti, e sempre all'inizio di un'unità;
//! - i casi CONSTRAINED UNPREDICTABLE sono esclusi.
//!
//! Le istruzioni che il nostro decoder considera UNDEFINED si scartano dal
//! corpo; ma in una parte dei programmi il corpo termina con una di esse, e
//! QEMU deve dare SIGILL.

use crate::harness::{BASE_PTR, BASE_REG, INDEX_REG, MEM_SIZE, Program};
use crate::rng::Rng;
use vetro_cpu::{Insn, decode};

pub struct Case {
    pub seed: u64,
    pub program: Program,
    /// Il corpo termina con un'istruzione che per noi è UNDEFINED.
    pub undefined_tail: Option<u32>,
}

/// Classe di istruzioni: bit fissi (`value` sotto `mask`) più correzioni.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    AddSubImm,
    LogicalImm,
    MoveWide,
    Bitfield,
    Extract,
    Adr,
    LogicalReg,
    AddSubReg,
    AddSubExt,
    AddSubCarry,
    CondCmp,
    CondSel,
    Dp2,
    Dp1,
    Dp3,
    LdStUnsigned,
    LdStImm9,
    LdStReg,
    LdStPair,
    LdLiteral,
    Exclusive,
    AcqRel,
    Branch,
}

const WEIGHTS: &[(Class, u64)] = &[
    (Class::AddSubImm, 6),
    (Class::LogicalImm, 5),
    (Class::MoveWide, 4),
    (Class::Bitfield, 6),
    (Class::Extract, 3),
    (Class::Adr, 2),
    (Class::LogicalReg, 6),
    (Class::AddSubReg, 6),
    (Class::AddSubExt, 5),
    (Class::AddSubCarry, 4),
    (Class::CondCmp, 5),
    (Class::CondSel, 5),
    (Class::Dp2, 5),
    (Class::Dp1, 4),
    (Class::Dp3, 5),
    (Class::LdStUnsigned, 6),
    (Class::LdStImm9, 6),
    (Class::LdStReg, 5),
    (Class::LdStPair, 5),
    (Class::LdLiteral, 2),
    (Class::Exclusive, 3),
    (Class::AcqRel, 2),
    (Class::Branch, 6),
];

/// Classi da cui si pesca l'istruzione UNDEFINED finale: solo bit casuali
/// nella maschera, senza correzioni.
const DP_CLASSES: &[(u32, u32)] = &[
    (0x1F80_0000, 0x1100_0000), // add/sub imm
    (0x1F80_0000, 0x1200_0000), // logical imm
    (0x1F80_0000, 0x1280_0000), // move wide
    (0x1F80_0000, 0x1300_0000), // bitfield
    (0x1F80_0000, 0x1380_0000), // extract
    (0x1F00_0000, 0x0A00_0000), // logical reg
    (0x1F20_0000, 0x0B00_0000), // add/sub shifted
    (0x1F20_0000, 0x0B20_0000), // add/sub extended
    (0x1FE0_0000, 0x1A00_0000), // adc/sbc
    (0x1FE0_0000, 0x1A40_0000), // ccmp/ccmn
    (0x1FE0_0000, 0x1A80_0000), // csel
    (0x5FE0_0000, 0x1AC0_0000), // dp 2-source
    (0x5FE0_0000, 0x5AC0_0000), // dp 1-source
    (0x1F00_0000, 0x1B00_0000), // dp 3-source
    (0x3F20_0000, 0x3800_0000), // ld/st imm9
    (0x3F20_0000, 0x3820_0000), // ld/st registro
    (0x3F00_0000, 0x3900_0000), // ld/st unsigned
    (0x3E00_0000, 0x2800_0000), // ld/st coppia
    (0x3F00_0000, 0x0800_0000), // esclusive
];

/// Unità del corpo: i salti arrivano solo all'inizio di un'unità.
enum Unit {
    Plain(Vec<u32>),
    /// Salto con offset da calcolare: `ahead` unità in avanti.
    Branch {
        raw: u32,
        kind: BranchKind,
        ahead: usize,
    },
    /// Load da letterale: offset verso un'istruzione del corpo.
    Literal {
        raw: u32,
        target: u64,
    },
}

#[derive(Clone, Copy)]
enum BranchKind {
    Imm26,
    Imm19,
    Imm14,
}

#[inline]
fn set(w: u32, hi: u32, lo: u32, v: u32) -> u32 {
    let m = (u32::MAX >> (31 - (hi - lo))) << lo;
    (w & !m) | ((v << lo) & m)
}

#[inline]
fn get(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & (u32::MAX >> (31 - (hi - lo)))
}

/// Registro di destinazione ammesso: tutti tranne x27 e x28.
fn dest_reg(rng: &mut Rng) -> u32 {
    loop {
        let r = rng.below(32) as u32;
        if r != BASE_REG && r != INDEX_REG {
            return r;
        }
    }
}

fn fix_rd(w: u32, rng: &mut Rng) -> u32 {
    let rd = get(w, 4, 0);
    if rd == BASE_REG || rd == INDEX_REG { set(w, 4, 0, dest_reg(rng)) } else { w }
}

fn accepted(w: u32) -> bool {
    !matches!(decode(w), Insn::Undefined | Insn::Unimplemented(_))
}

struct Gen<'a> {
    rng: &'a mut Rng,
}

impl Gen<'_> {
    fn raw(&mut self, mask: u32, value: u32) -> u32 {
        value | (self.rng.next_u32() & !mask)
    }

    /// Istruzione di elaborazione dati della classe `c`, già accettata dal
    /// decoder.
    fn dp(&mut self, c: Class) -> u32 {
        loop {
            let w = self.dp_candidate(c);
            if accepted(w) {
                return w;
            }
        }
    }

    fn dp_candidate(&mut self, c: Class) -> u32 {
        let rng = &mut *self.rng;
        let r = |rng: &mut Rng, mask: u32, value: u32| value | (rng.next_u32() & !mask);
        let w = match c {
            Class::AddSubImm => r(rng, 0x1F80_0000, 0x1100_0000),
            Class::LogicalImm => r(rng, 0x1F80_0000, 0x1200_0000),
            Class::MoveWide => r(rng, 0x1F80_0000, 0x1280_0000),
            Class::Bitfield => {
                let mut w = r(rng, 0x1F80_0000, 0x1300_0000);
                let sf = get(w, 31, 31);
                w = set(w, 22, 22, sf);
                if sf == 0 {
                    w = set(w, 21, 21, 0);
                    w = set(w, 15, 15, 0);
                }
                w
            }
            Class::Extract => {
                let mut w = r(rng, 0x1F80_0000, 0x1380_0000);
                let sf = get(w, 31, 31);
                w = set(w, 30, 29, 0);
                w = set(w, 22, 22, sf);
                w = set(w, 21, 21, 0);
                if sf == 0 {
                    w = set(w, 15, 15, 0);
                }
                w
            }
            Class::Adr => r(rng, 0x1F00_0000, 0x1000_0000),
            Class::LogicalReg => {
                let mut w = r(rng, 0x1F00_0000, 0x0A00_0000);
                if get(w, 31, 31) == 0 {
                    w = set(w, 15, 15, 0);
                }
                w
            }
            Class::AddSubReg => {
                let mut w = r(rng, 0x1F20_0000, 0x0B00_0000);
                if get(w, 23, 22) == 3 {
                    w = set(w, 23, 22, rng.below(3) as u32);
                }
                if get(w, 31, 31) == 0 {
                    w = set(w, 15, 15, 0);
                }
                w
            }
            Class::AddSubExt => {
                let mut w = r(rng, 0x1F20_0000, 0x0B20_0000);
                w = set(w, 23, 22, 0);
                set(w, 12, 10, rng.below(5) as u32)
            }
            Class::AddSubCarry => r(rng, 0x1FE0_FC00, 0x1A00_0000),
            Class::CondCmp => {
                let mut w = r(rng, 0x1FE0_0000, 0x1A40_0000);
                w = set(w, 29, 29, 1);
                w = set(w, 10, 10, 0);
                set(w, 4, 4, 0)
            }
            Class::CondSel => {
                let w = r(rng, 0x1FE0_0000, 0x1A80_0000);
                set(set(w, 29, 29, 0), 11, 11, 0)
            }
            Class::Dp2 => {
                const OPS: [u32; 14] = [
                    0b000010, 0b000011, 0b001000, 0b001001, 0b001010, 0b001011, 0b010000, 0b010001, 0b010010,
                    0b010011, 0b010100, 0b010101, 0b010110, 0b010111,
                ];
                let mut w = r(rng, 0x5FE0_0000, 0x1AC0_0000);
                let op = *rng.pick(&OPS);
                w = set(w, 15, 10, op);
                w = set(w, 29, 29, 0);
                if op & 0b010000 != 0 {
                    w = set(w, 31, 31, (op & 3 == 3) as u32);
                }
                w
            }
            Class::Dp1 => {
                let mut w = r(rng, 0x5FE0_0000, 0x5AC0_0000);
                w = set(w, 29, 29, 0);
                w = set(w, 20, 16, 0);
                set(w, 15, 10, rng.below(6) as u32)
            }
            Class::Dp3 => {
                let mut w = r(rng, 0x1F00_0000, 0x1B00_0000);
                w = set(w, 30, 29, 0);
                let op31 = *rng.pick(&[0b000, 0b000, 0b001, 0b010, 0b101, 0b110]);
                w = set(w, 23, 21, op31);
                if op31 != 0 {
                    w = set(w, 31, 31, 1);
                }
                if op31 == 0b010 || op31 == 0b110 {
                    w = set(w, 15, 15, 0);
                    w = set(w, 14, 10, 31); // Ra: should-be-one
                }
                w
            }
            _ => unreachable!("non è una classe di elaborazione dati: {c:?}"),
        };
        fix_rd(w, self.rng)
    }

    /// Registro di trasferimento per un load/store: tutti tranne x27/x28.
    fn rt(&mut self) -> u32 {
        dest_reg(self.rng)
    }

    /// Copia di x28 in un registro visibile, poi ripristino di x28.
    fn capture_and_restore(&mut self, out: &mut Vec<u32>) {
        let visible = dest_reg(self.rng) % 31;
        out.push(crate::a64::mov_reg(visible, BASE_REG));
        out.push(crate::a64::movz(BASE_REG, BASE_PTR as u16, 0));
        out.push(crate::a64::movk(BASE_REG, (BASE_PTR >> 16) as u16, 1));
    }

    fn mem_unit(&mut self, c: Class) -> Vec<u32> {
        let mut out = Vec::new();
        loop {
            let (w, writeback) = match c {
                Class::LdStUnsigned => {
                    let mut w = self.raw(0x3F00_0000, 0x3900_0000);
                    let size = get(w, 31, 30);
                    w = set(w, 21, 10, self.rng.below(0x800 >> size) as u32);
                    (w, false)
                }
                Class::LdStImm9 => {
                    let w = self.raw(0x3F20_0000, 0x3800_0000);
                    let idx = get(w, 11, 10);
                    (w, idx == 0b01 || idx == 0b11)
                }
                Class::LdStReg => {
                    let mut w = self.raw(0x3F20_0C00, 0x3820_0800);
                    w = set(w, 20, 16, INDEX_REG);
                    w = set(w, 15, 13, *self.rng.pick(&[0b010, 0b011, 0b110, 0b111]));
                    (w, false)
                }
                Class::LdStPair => {
                    let mut w = self.raw(0x3E00_0000, 0x2800_0000);
                    let rt2 = self.rt();
                    w = set(w, 14, 10, rt2);
                    let load = get(w, 22, 22) == 1;
                    if load && get(w, 4, 0) == rt2 {
                        continue; // Rt == Rt2 in un load: UNPREDICTABLE
                    }
                    let idx = get(w, 24, 23);
                    (w, idx == 0b01 || idx == 0b11)
                }
                _ => unreachable!(),
            };
            let mut w = set(w, 9, 5, BASE_REG);
            let rt = get(w, 4, 0);
            if rt == BASE_REG || rt == INDEX_REG {
                w = set(w, 4, 0, self.rt());
            }
            if c == Class::LdStPair && get(w, 22, 22) == 1 && get(w, 4, 0) == get(w, 14, 10) {
                continue;
            }
            if !accepted(w) {
                continue;
            }
            out.push(w);
            if writeback {
                self.capture_and_restore(&mut out);
            }
            return out;
        }
    }

    /// LDXR…STXR con la stessa dimensione, e qualche istruzione in mezzo.
    fn exclusive_unit(&mut self) -> Vec<u32> {
        let pair = self.rng.chance(1, 4);
        let size = if pair { 2 + self.rng.below(2) as u32 } else { self.rng.below(4) as u32 };
        let base = |size: u32, load: bool, o1: bool, acq: bool| -> u32 {
            (size << 30)
                | (0b001000 << 24)
                | ((load as u32) << 22)
                | ((o1 as u32) << 21)
                | ((acq as u32) << 15)
        };
        let (rt, rt2) = loop {
            let (a, b) = (self.rt(), self.rt());
            if !pair || a != b {
                break (a, b);
            }
        };
        let mut out = Vec::new();
        let acq = self.rng.chance(1, 2);
        let ld = base(size, true, pair, acq) | (31 << 16) | ((if pair { rt2 } else { 31 }) << 10);
        out.push(ld | (BASE_REG << 5) | rt);
        for _ in 0..self.rng.below(3) {
            if self.rng.chance(1, 4) {
                out.extend(self.mem_unit(Class::LdStUnsigned));
            } else {
                let c = *self.rng.pick(&[Class::AddSubImm, Class::LogicalReg, Class::MoveWide]);
                out.push(self.dp(c));
            }
        }
        // STXR: Rs diverso da Rt, Rt2 e dalla base.
        let (st, st2) = (self.rt(), self.rt());
        let rs = loop {
            let r = self.rt();
            if r != st && (!pair || r != st2) && r != 31 {
                break r;
            }
        };
        let rel = self.rng.chance(1, 2);
        let st_w = base(size, false, pair, rel) | (rs << 16) | ((if pair { st2 } else { 31 }) << 10);
        out.push(st_w | (BASE_REG << 5) | st);
        out
    }

    fn acq_rel(&mut self) -> u32 {
        let size = self.rng.below(4) as u32;
        let load = self.rng.chance(1, 2);
        (size << 30)
            | (0b001000 << 24)
            | (1 << 23)
            | ((load as u32) << 22)
            | (31 << 16)
            | (1 << 15)
            | (31 << 10)
            | (BASE_REG << 5)
            | self.rt()
    }

    fn branch(&mut self) -> Unit {
        let ahead = 1 + self.rng.below(5) as usize;
        let rng = &mut *self.rng;
        let (raw, kind) = match rng.below(5) {
            0 => ((0b0101_0100 << 24) | rng.below(16) as u32, BranchKind::Imm19), // B.cond
            1 => {
                let w = (0b011010 << 25) | (rng.below(2) as u32) << 31 | (rng.below(2) as u32) << 24;
                (w | rng.below(32) as u32, BranchKind::Imm19) // CBZ/CBNZ
            }
            2 => {
                let w = (0b011011 << 25) | (rng.below(2) as u32) << 31 | (rng.below(2) as u32) << 24;
                (w | (rng.below(32) as u32) << 19 | rng.below(32) as u32, BranchKind::Imm14) // TBZ/TBNZ
            }
            3 => (0b000101 << 26, BranchKind::Imm26), // B
            _ => (0b100101 << 26, BranchKind::Imm26), // BL
        };
        Unit::Branch { raw, kind, ahead }
    }

    fn literal(&mut self) -> Unit {
        let raw = (self.rng.below(4) as u32) << 30 | (0b011000 << 24) | self.rt();
        Unit::Literal { raw, target: self.rng.next_u64() }
    }

    fn unit(&mut self) -> Unit {
        let total: u64 = WEIGHTS.iter().map(|(_, w)| w).sum();
        let mut pick = self.rng.below(total);
        let class = WEIGHTS
            .iter()
            .find(|(_, w)| {
                if pick < *w {
                    true
                } else {
                    pick -= w;
                    false
                }
            })
            .map(|(c, _)| *c)
            .unwrap();
        match class {
            Class::LdStUnsigned | Class::LdStImm9 | Class::LdStReg | Class::LdStPair => {
                Unit::Plain(self.mem_unit(class))
            }
            Class::Exclusive => Unit::Plain(self.exclusive_unit()),
            Class::AcqRel => Unit::Plain(vec![self.acq_rel()]),
            Class::Branch => self.branch(),
            Class::LdLiteral => self.literal(),
            c => Unit::Plain(vec![self.dp(c)]),
        }
    }

    fn undefined(&mut self) -> u32 {
        loop {
            let (mask, value) = *self.rng.pick(DP_CLASSES);
            let w = self.raw(mask, value);
            if decode(w) == Insn::Undefined {
                return w;
            }
        }
    }
}

/// Dispone le unità: calcola gli offset di salti e letterali.
fn layout(units: Vec<Unit>) -> Vec<u32> {
    let mut starts = Vec::with_capacity(units.len() + 1);
    let mut n = 0usize;
    for u in &units {
        starts.push(n);
        n += match u {
            Unit::Plain(v) => v.len(),
            _ => 1,
        };
    }
    starts.push(n);
    let len = n;
    let mut body = Vec::with_capacity(len);
    for (i, u) in units.into_iter().enumerate() {
        let pos = body.len();
        match u {
            Unit::Plain(v) => body.extend(v),
            Unit::Branch { raw, kind, ahead } => {
                let target = starts[(i + ahead).min(starts.len() - 1)];
                let off = (target - pos) as u32;
                body.push(match kind {
                    BranchKind::Imm26 => set(raw, 25, 0, off),
                    BranchKind::Imm19 => set(raw, 23, 5, off),
                    BranchKind::Imm14 => set(raw, 18, 5, off),
                });
            }
            Unit::Literal { raw, target } => {
                // Qualsiasi istruzione del corpo (per un LDR X servono 8 byte:
                // dopo il corpo c'è comunque l'epilogo).
                let t = (target % len as u64) as i64;
                let off = (t - pos as i64) as u32;
                body.push(set(raw, 23, 5, off));
            }
        }
    }
    body
}

/// Genera il caso `seed` con circa `body_len` istruzioni nel corpo.
pub fn generate(seed: u64, body_len: usize) -> Case {
    let mut rng = Rng::new(seed);
    let mut units = Vec::new();
    let mut n = 0;
    {
        let mut g = Gen { rng: &mut rng };
        while n < body_len {
            let u = g.unit();
            n += match &u {
                Unit::Plain(v) => v.len(),
                _ => 1,
            };
            units.push(u);
        }
    }
    let mut body = layout(units);
    let undefined_tail = if rng.chance(1, 8) {
        let w = Gen { rng: &mut rng }.undefined();
        body.push(w);
        Some(w)
    } else {
        None
    };

    let mut program = Program::new(body);
    for (i, x) in program.x.iter_mut().enumerate() {
        *x = match i as u32 {
            BASE_REG => BASE_PTR,
            INDEX_REG => rng.below(256),
            _ => rng.interesting_u64(),
        };
    }
    program.nzcv = (rng.below(16) as u32) << 28;
    program.mem = (0..MEM_SIZE).map(|_| rng.next_u32() as u8).collect();
    Case { seed, program, undefined_tail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_well_formed() {
        let a = generate(7, 40);
        let b = generate(7, 40);
        assert_eq!(a.program.body, b.program.body);
        for seed in 0..200 {
            let c = generate(seed, 40);
            let body = &c.program.body;
            let n = body.len() - c.undefined_tail.is_some() as usize;
            for &w in &body[..n] {
                assert!(accepted(w), "seed {seed}: {w:#010x} rifiutata dal decoder");
            }
        }
    }
}
