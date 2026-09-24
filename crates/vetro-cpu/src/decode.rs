//! Decoder AArch64 (Arm ARM, sezione C4 "A64 Instruction Set Encoding").
//!
//! Livello ARMv8.0 con le estensioni della Cortex-A53 (ADR 0005). Ogni
//! codifica produce una di tre cose: un'istruzione eseguibile, `Undefined`
//! (UNDEFINED per l'architettura a questo livello) o `Unimplemented`
//! (valida ma non ancora scritta da noi).

use crate::bits::{bit, decode_bit_masks, field, sext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Orr,
    Eor,
    Ands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovOp {
    Movn,
    Movz,
    Movk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BfOp {
    Sbfm,
    Bfm,
    Ubfm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CselOp {
    Csel,
    Csinc,
    Csinv,
    Csneg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp1Op {
    Rbit,
    Rev16,
    /// REV32 (64 bit) o REV (32 bit): inverte i byte di ogni parola.
    Rev32,
    Rev64,
    Clz,
    Cls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp2Op {
    Udiv,
    Sdiv,
    Lslv,
    Lsrv,
    Asrv,
    Rorv,
    /// CRC32{B,H,W,X}: `bytes` byte di Rm; `c` = CRC32C.
    Crc32 {
        bytes: u8,
        c: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp3Op {
    Madd,
    Msub,
    Smaddl,
    Smsubl,
    Umaddl,
    Umsubl,
    Smulh,
    Umulh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrOp {
    Br,
    Blr,
    Ret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcmpOperand {
    Reg(u8),
    Imm(u8),
}

/// Registri di sistema accessibili a EL0 che implementiamo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysReg {
    Nzcv,
    TpidrEl0,
    TpidrroEl0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemOp {
    Store,
    /// Load con estensione a zero (o di segno) fino a 64 bit, oppure di
    /// segno fino a 32 bit (`dst64 = false`, parte alta azzerata).
    Load {
        signed: bool,
        dst64: bool,
    },
    Prefetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Index {
    Offset,
    Pre,
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrMode {
    Imm {
        offset: i64,
        index: Index,
    },
    /// Base + ExtendReg(Rm, extend, shift).
    Reg {
        rm: u8,
        extend: u8,
        shift: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insn {
    AddSubImm {
        sf: bool,
        sub: bool,
        setflags: bool,
        imm: u64,
        rn: u8,
        rd: u8,
    },
    LogicalImm {
        sf: bool,
        op: LogicOp,
        imm: u64,
        rn: u8,
        rd: u8,
    },
    MoveWide {
        sf: bool,
        op: MovOp,
        shift: u8,
        imm16: u16,
        rd: u8,
    },
    Adr {
        page: bool,
        imm: i64,
        rd: u8,
    },
    Bitfield {
        sf: bool,
        op: BfOp,
        r: u8,
        s: u8,
        wmask: u64,
        tmask: u64,
        rn: u8,
        rd: u8,
    },
    Extract {
        sf: bool,
        lsb: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    LogicalReg {
        sf: bool,
        op: LogicOp,
        invert: bool,
        shift: Shift,
        amount: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    AddSubReg {
        sf: bool,
        sub: bool,
        setflags: bool,
        shift: Shift,
        amount: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    AddSubExt {
        sf: bool,
        sub: bool,
        setflags: bool,
        extend: u8,
        amount: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    AddSubCarry {
        sf: bool,
        sub: bool,
        setflags: bool,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    CondCmp {
        sf: bool,
        sub: bool,
        operand: CcmpOperand,
        cond: u8,
        nzcv: u8,
        rn: u8,
    },
    CondSel {
        sf: bool,
        op: CselOp,
        cond: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Dp1 {
        sf: bool,
        op: Dp1Op,
        rn: u8,
        rd: u8,
    },
    Dp2 {
        sf: bool,
        op: Dp2Op,
        rm: u8,
        rn: u8,
        rd: u8,
    },
    Dp3 {
        sf: bool,
        op: Dp3Op,
        ra: u8,
        rm: u8,
        rn: u8,
        rd: u8,
    },

    B {
        link: bool,
        offset: i64,
    },
    BCond {
        cond: u8,
        offset: i64,
    },
    Cbz {
        sf: bool,
        nonzero: bool,
        rt: u8,
        offset: i64,
    },
    Tbz {
        nonzero: bool,
        bit: u8,
        rt: u8,
        offset: i64,
    },
    BranchReg {
        op: BrOp,
        rn: u8,
    },

    Svc {
        imm: u16,
    },
    Brk {
        imm: u16,
    },
    Nop,
    Barrier,
    Clrex,
    DcZva {
        rt: u8,
    },
    /// DC CVAC/CVAU/CIVAC, IC IVAU: nessun effetto osservabile in M1.
    CacheMaint,
    Mrs {
        reg: SysReg,
        rt: u8,
    },
    Msr {
        reg: SysReg,
        rt: u8,
    },

    /// `size` = log2 dei byte (0..=3).
    LdSt {
        size: u8,
        op: MemOp,
        addr: AddrMode,
        rt: u8,
        rn: u8,
    },
    LdLiteral {
        size: u8,
        op: MemOp,
        offset: i64,
        rt: u8,
    },
    LdStPair {
        size: u8,
        load: bool,
        signed: bool,
        index: Index,
        offset: i64,
        rt: u8,
        rt2: u8,
        rn: u8,
    },
    /// LDXR/LDAXR/STXR/STLXR e le versioni a coppia. `size` per elemento.
    Exclusive {
        size: u8,
        load: bool,
        pair: bool,
        rs: u8,
        rt: u8,
        rt2: u8,
        rn: u8,
    },
    LoadAcquire {
        size: u8,
        rt: u8,
        rn: u8,
    },
    StoreRelease {
        size: u8,
        rt: u8,
        rn: u8,
    },

    Undefined,
    Unimplemented(&'static str),
}

use Insn::{Undefined, Unimplemented};

#[inline]
fn r(w: u32, lo: u32) -> u8 {
    field(w, lo + 4, lo) as u8
}

pub fn decode(w: u32) -> Insn {
    match field(w, 28, 25) {
        0b1000 | 0b1001 => dp_imm(w),
        0b1010 | 0b1011 => branch_sys(w),
        0b0100 | 0b0110 | 0b1100 | 0b1110 => ldst(w),
        0b0101 | 0b1101 => dp_reg(w),
        0b0111 | 0b1111 => Unimplemented("SIMD/FP"),
        // 0000 riservato (UDF, SME), 0010 SVE, 0001/0011 non allocati.
        _ => Undefined,
    }
}

fn dp_imm(w: u32) -> Insn {
    let sf = bit(w, 31);
    let rd = r(w, 0);
    let rn = r(w, 5);
    match field(w, 25, 23) {
        0b000 | 0b001 => {
            let imm = sext(((field(w, 23, 5) << 2) | field(w, 30, 29)) as u64, 21);
            let page = bit(w, 31);
            Insn::Adr { page, imm: if page { imm << 12 } else { imm }, rd }
        }
        0b010 => {
            let imm12 = field(w, 21, 10) as u64;
            Insn::AddSubImm {
                sf,
                sub: bit(w, 30),
                setflags: bit(w, 29),
                imm: if bit(w, 22) { imm12 << 12 } else { imm12 },
                rn,
                rd,
            }
        }
        0b100 => {
            let n = field(w, 22, 22);
            if !sf && n == 1 {
                return Undefined;
            }
            let datasize = if sf { 64 } else { 32 };
            let Some((imm, _)) = decode_bit_masks(n, field(w, 15, 10), field(w, 21, 16), true, datasize)
            else {
                return Undefined;
            };
            let op = [LogicOp::And, LogicOp::Orr, LogicOp::Eor, LogicOp::Ands][field(w, 30, 29) as usize];
            Insn::LogicalImm { sf, op, imm, rn, rd }
        }
        0b101 => {
            let hw = field(w, 22, 21);
            let op = match field(w, 30, 29) {
                0 => MovOp::Movn,
                2 => MovOp::Movz,
                3 => MovOp::Movk,
                _ => return Undefined,
            };
            if !sf && hw >= 2 {
                return Undefined;
            }
            Insn::MoveWide { sf, op, shift: (hw * 16) as u8, imm16: field(w, 20, 5) as u16, rd }
        }
        0b110 => {
            let op = match field(w, 30, 29) {
                0 => BfOp::Sbfm,
                1 => BfOp::Bfm,
                2 => BfOp::Ubfm,
                _ => return Undefined,
            };
            let (n, immr, imms) = (field(w, 22, 22), field(w, 21, 16), field(w, 15, 10));
            if sf && n != 1 || !sf && (n != 0 || immr >= 32 || imms >= 32) {
                return Undefined;
            }
            let datasize = if sf { 64 } else { 32 };
            let Some((wmask, tmask)) = decode_bit_masks(n, imms, immr, false, datasize) else {
                return Undefined;
            };
            Insn::Bitfield { sf, op, r: immr as u8, s: imms as u8, wmask, tmask, rn, rd }
        }
        0b111 => {
            let imms = field(w, 15, 10);
            if field(w, 30, 29) != 0 || bit(w, 21) || bit(w, 22) != sf || !sf && imms >= 32 {
                return Undefined;
            }
            Insn::Extract { sf, lsb: imms as u8, rm: r(w, 16), rn, rd }
        }
        // 011: add/sub con tag (MTE) e min/max immediati (CSSC).
        _ => Undefined,
    }
}

fn branch_sys(w: u32) -> Insn {
    let op0 = field(w, 31, 29);
    match op0 & 0b011 {
        0b000 => {
            return Insn::B { link: bit(w, 31), offset: sext(field(w, 25, 0) as u64, 26) << 2 };
        }
        0b001 => {
            let rt = r(w, 0);
            let nonzero = bit(w, 24);
            return if !bit(w, 25) {
                Insn::Cbz { sf: bit(w, 31), nonzero, rt, offset: sext(field(w, 23, 5) as u64, 19) << 2 }
            } else {
                let b = (field(w, 31, 31) << 5) | field(w, 23, 19);
                Insn::Tbz { nonzero, bit: b as u8, rt, offset: sext(field(w, 18, 5) as u64, 14) << 2 }
            };
        }
        _ => {}
    }
    match op0 {
        0b010 => {
            if bit(w, 25) || bit(w, 24) || bit(w, 4) {
                return Undefined; // o1=1 non allocato, o0=1 è BC.cond (FEAT_HBC)
            }
            Insn::BCond { cond: field(w, 3, 0) as u8, offset: sext(field(w, 23, 5) as u64, 19) << 2 }
        }
        0b110 => {
            if bit(w, 25) {
                branch_reg(w)
            } else if !bit(w, 24) {
                exception(w)
            } else if field(w, 23, 22) == 0 {
                system(w)
            } else {
                Undefined
            }
        }
        _ => Undefined,
    }
}

fn branch_reg(w: u32) -> Insn {
    // opc(24:21) op2(20:16)=11111 op3(15:10)=0 Rn op4(4:0)=0; il resto è
    // PAuth, ERET, DRPS: non disponibili (o non a EL0) su v8.0.
    if field(w, 20, 16) != 0b11111 || field(w, 15, 10) != 0 || field(w, 4, 0) != 0 {
        return Undefined;
    }
    let op = match field(w, 24, 21) {
        0 => BrOp::Br,
        1 => BrOp::Blr,
        2 => BrOp::Ret,
        _ => return Undefined,
    };
    Insn::BranchReg { op, rn: r(w, 5) }
}

fn exception(w: u32) -> Insn {
    let imm = field(w, 20, 5) as u16;
    match (field(w, 23, 21), field(w, 4, 2), field(w, 1, 0)) {
        (0b000, 0, 0b01) => Insn::Svc { imm },
        (0b001, 0, 0b00) => Insn::Brk { imm },
        // HVC/SMC sono UNDEFINED a EL0; HLT lo è con halting disabilitato;
        // DCPSx fuori dallo stato di debug.
        _ => Undefined,
    }
}

fn system(w: u32) -> Insn {
    let l = bit(w, 21);
    let op0 = field(w, 20, 19);
    let op1 = field(w, 18, 16);
    let crn = field(w, 15, 12);
    let crm = field(w, 11, 8);
    let op2 = field(w, 7, 5);
    let rt = r(w, 0);
    match op0 {
        0b00 => {
            if l || rt != 31 {
                return Undefined;
            }
            match (crn, op1) {
                // HINT: gli hint non allocati si comportano come NOP (così
                // anche PAC*SP, BTI ecc. su una CPU v8.0).
                (0b0010, 0b011) => Insn::Nop,
                (0b0011, 0b011) => match op2 {
                    0b010 => Insn::Clrex,
                    0b100..=0b110 => Insn::Barrier,
                    _ => Undefined,
                },
                // MSR (immediato) su PSTATE: nulla è accessibile a EL0 su v8.0.
                _ => Undefined,
            }
        }
        0b01 => {
            if l || op1 != 0b011 || crn != 0b0111 || op2 != 1 {
                return Undefined;
            }
            match crm {
                0b0100 => Insn::DcZva { rt },
                0b0101 | 0b1010 | 0b1011 | 0b1110 => Insn::CacheMaint,
                _ => Undefined,
            }
        }
        _ => {
            let reg = match (op0, op1, crn, crm, op2) {
                (3, 3, 4, 2, 0) => SysReg::Nzcv,
                (3, 3, 13, 0, 2) => SysReg::TpidrEl0,
                (3, 3, 13, 0, 3) => SysReg::TpidrroEl0,
                _ => return Unimplemented("MRS/MSR registro di sistema"),
            };
            if l {
                Insn::Mrs { reg, rt }
            } else if reg == SysReg::TpidrroEl0 {
                Undefined // sola lettura a EL0
            } else {
                Insn::Msr { reg, rt }
            }
        }
    }
}

fn ldst(w: u32) -> Insn {
    if bit(w, 26) {
        return Unimplemented("SIMD/FP load/store");
    }
    let rt = r(w, 0);
    let rn = r(w, 5);
    match field(w, 29, 27) {
        0b001 => {
            if field(w, 25, 24) != 0 {
                return Undefined;
            }
            exclusive(w)
        }
        0b011 => {
            if bit(w, 24) {
                return Undefined; // RCpc / MTE: non su v8.0
            }
            let offset = sext(field(w, 23, 5) as u64, 19) << 2;
            let (size, op) = match field(w, 31, 30) {
                0 => (2, MemOp::Load { signed: false, dst64: false }),
                1 => (3, MemOp::Load { signed: false, dst64: true }),
                2 => (2, MemOp::Load { signed: true, dst64: true }),
                _ => (3, MemOp::Prefetch),
            };
            Insn::LdLiteral { size, op, offset, rt }
        }
        0b101 => {
            let load = bit(w, 22);
            let index = match field(w, 24, 23) {
                0b00 | 0b10 => Index::Offset,
                0b01 => Index::Post,
                _ => Index::Pre,
            };
            let noalloc = field(w, 24, 23) == 0;
            let (size, signed) = match field(w, 31, 30) {
                0b00 => (2, false),
                0b10 => (3, false),
                0b01 if load && !noalloc => (2, true), // LDPSW
                _ => return Undefined,
            };
            let offset = sext(field(w, 21, 15) as u64, 7) << size;
            Insn::LdStPair { size, load, signed, index, offset, rt, rt2: r(w, 10), rn }
        }
        _ => {
            // 0b111: registro.
            let size = field(w, 31, 30) as u8;
            let opc = field(w, 23, 22);
            if bit(w, 24) {
                let Some(op) = mem_op(size, opc, true) else {
                    return Undefined;
                };
                let offset = (field(w, 21, 10) as i64) << size;
                return Insn::LdSt { size, op, addr: AddrMode::Imm { offset, index: Index::Offset }, rt, rn };
            }
            if bit(w, 21) {
                if field(w, 11, 10) != 0b10 {
                    return Undefined; // atomiche LSE, load con PAuth
                }
                let extend = field(w, 15, 13) as u8;
                if extend & 0b010 == 0 {
                    return Undefined;
                }
                let Some(op) = mem_op(size, opc, true) else {
                    return Undefined;
                };
                let shift = if bit(w, 12) { size } else { 0 };
                return Insn::LdSt { size, op, addr: AddrMode::Reg { rm: r(w, 16), extend, shift }, rt, rn };
            }
            let offset = sext(field(w, 20, 12) as u64, 9);
            let (index, prfm_ok) = match field(w, 11, 10) {
                0b00 => (Index::Offset, true), // LDUR/STUR/PRFUM
                0b01 => (Index::Post, false),
                0b10 => (Index::Offset, false), // LDTR/STTR: a EL0 come i normali
                _ => (Index::Pre, false),
            };
            let Some(op) = mem_op(size, opc, prfm_ok) else {
                return Undefined;
            };
            Insn::LdSt { size, op, addr: AddrMode::Imm { offset, index }, rt, rn }
        }
    }
}

/// Operazione di load/store intera da `size` e `opc`.
fn mem_op(size: u8, opc: u32, prfm_ok: bool) -> Option<MemOp> {
    Some(match (opc, size) {
        (0b00, _) => MemOp::Store,
        (0b01, _) => MemOp::Load { signed: false, dst64: size == 3 },
        (0b10, 3) => {
            if prfm_ok {
                MemOp::Prefetch
            } else {
                return None;
            }
        }
        (0b10, _) => MemOp::Load { signed: true, dst64: true },
        (0b11, 0 | 1) => MemOp::Load { signed: true, dst64: false },
        _ => return None,
    })
}

fn exclusive(w: u32) -> Insn {
    let size = field(w, 31, 30) as u8;
    let load = bit(w, 22);
    let o0 = bit(w, 15);
    let (rs, rt2, rn, rt) = (r(w, 16), r(w, 10), r(w, 5), r(w, 0));
    match (bit(w, 23), bit(w, 21)) {
        (false, false) => Insn::Exclusive { size, load, pair: false, rs, rt, rt2, rn },
        (false, true) => {
            if size < 2 {
                return Undefined; // CASP (LSE)
            }
            Insn::Exclusive { size, load, pair: true, rs, rt, rt2, rn }
        }
        (true, false) => {
            if !o0 {
                return Undefined; // LDLAR/STLLR (LORegions)
            }
            if load { Insn::LoadAcquire { size, rt, rn } } else { Insn::StoreRelease { size, rt, rn } }
        }
        (true, true) => Undefined, // CAS (LSE)
    }
}

fn dp_reg(w: u32) -> Insn {
    let sf = bit(w, 31);
    let (rm, rn, rd) = (r(w, 16), r(w, 5), r(w, 0));
    let op2 = field(w, 24, 21);
    if !bit(w, 28) {
        let imm6 = field(w, 15, 10) as u8;
        let shift_ty = field(w, 23, 22);
        if !bit(w, 24) {
            if !sf && imm6 >= 32 {
                return Undefined;
            }
            let op = [LogicOp::And, LogicOp::Orr, LogicOp::Eor, LogicOp::Ands][field(w, 30, 29) as usize];
            let shift = [Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Ror][shift_ty as usize];
            return Insn::LogicalReg { sf, op, invert: bit(w, 21), shift, amount: imm6, rm, rn, rd };
        }
        let (sub, setflags) = (bit(w, 30), bit(w, 29));
        if !bit(w, 21) {
            if shift_ty == 3 || !sf && imm6 >= 32 {
                return Undefined;
            }
            let shift = [Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Ror][shift_ty as usize];
            return Insn::AddSubReg { sf, sub, setflags, shift, amount: imm6, rm, rn, rd };
        }
        let imm3 = field(w, 12, 10) as u8;
        if shift_ty != 0 || imm3 > 4 {
            return Undefined;
        }
        return Insn::AddSubExt {
            sf,
            sub,
            setflags,
            extend: field(w, 15, 13) as u8,
            amount: imm3,
            rm,
            rn,
            rd,
        };
    }
    match op2 {
        0b0000 => {
            if field(w, 15, 10) != 0 {
                return Undefined; // RMIF, SETF (FlagM)
            }
            Insn::AddSubCarry { sf, sub: bit(w, 30), setflags: bit(w, 29), rm, rn, rd }
        }
        0b0010 => {
            if !bit(w, 29) || bit(w, 10) || bit(w, 4) {
                return Undefined;
            }
            let operand = if bit(w, 11) { CcmpOperand::Imm(rm) } else { CcmpOperand::Reg(rm) };
            Insn::CondCmp {
                sf,
                sub: bit(w, 30),
                operand,
                cond: field(w, 15, 12) as u8,
                nzcv: field(w, 3, 0) as u8,
                rn,
            }
        }
        0b0100 => {
            if bit(w, 29) || bit(w, 11) {
                return Undefined;
            }
            let op = match (bit(w, 30), bit(w, 10)) {
                (false, false) => CselOp::Csel,
                (false, true) => CselOp::Csinc,
                (true, false) => CselOp::Csinv,
                (true, true) => CselOp::Csneg,
            };
            Insn::CondSel { sf, op, cond: field(w, 15, 12) as u8, rm, rn, rd }
        }
        0b0110 => {
            if bit(w, 29) {
                return Undefined;
            }
            let opcode = field(w, 15, 10);
            if bit(w, 30) {
                if field(w, 20, 16) != 0 {
                    return Undefined; // PAuth
                }
                let op = match (opcode, sf) {
                    (0, _) => Dp1Op::Rbit,
                    (1, _) => Dp1Op::Rev16,
                    (2, _) => Dp1Op::Rev32,
                    (3, true) => Dp1Op::Rev64,
                    (4, _) => Dp1Op::Clz,
                    (5, _) => Dp1Op::Cls,
                    _ => return Undefined,
                };
                return Insn::Dp1 { sf, op, rn, rd };
            }
            let op = match opcode {
                0b000010 => Dp2Op::Udiv,
                0b000011 => Dp2Op::Sdiv,
                0b001000 => Dp2Op::Lslv,
                0b001001 => Dp2Op::Lsrv,
                0b001010 => Dp2Op::Asrv,
                0b001011 => Dp2Op::Rorv,
                0b010000..=0b010111 => {
                    let sz = opcode & 3;
                    if (sz == 3) != sf {
                        return Undefined;
                    }
                    Dp2Op::Crc32 { bytes: 1 << sz, c: opcode & 4 != 0 }
                }
                _ => return Undefined,
            };
            Insn::Dp2 { sf, op, rm, rn, rd }
        }
        0b1000..=0b1111 => {
            if field(w, 30, 29) != 0 {
                return Undefined;
            }
            let o0 = bit(w, 15);
            let op = match (field(w, 23, 21), o0) {
                (0b000, false) => Dp3Op::Madd,
                (0b000, true) => Dp3Op::Msub,
                (0b001, false) => Dp3Op::Smaddl,
                (0b001, true) => Dp3Op::Smsubl,
                (0b010, false) => Dp3Op::Smulh,
                (0b101, false) => Dp3Op::Umaddl,
                (0b101, true) => Dp3Op::Umsubl,
                (0b110, false) => Dp3Op::Umulh,
                _ => return Undefined,
            };
            if !sf && !matches!(op, Dp3Op::Madd | Dp3Op::Msub) {
                return Undefined;
            }
            Insn::Dp3 { sf, op, ra: r(w, 10), rm, rn, rd }
        }
        _ => Undefined,
    }
}
