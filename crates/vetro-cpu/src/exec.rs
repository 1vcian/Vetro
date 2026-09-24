//! Interprete di riferimento: esegue una `Insn` decodificata.
//!
//! Scritto per essere leggibile contro il pseudocodice dell'Arm ARM, non per
//! essere veloce (la velocità è compito del JIT, M4).

use crate::bits::{ones, ror};
use crate::decode::*;
use crate::mem::{Access, MemFault, Memory};
use crate::state::{Cpu, Monitor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    /// `pc` punta già all'istruzione successiva.
    Svc(u16),
    Breakpoint(u16),
    Undefined(u32),
    Unimplemented {
        raw: u32,
        what: &'static str,
    },
    DataAbort {
        addr: u64,
        write: bool,
    },
    InstructionAbort {
        addr: u64,
    },
    Alignment {
        addr: u64,
    },
    PcAlignment {
        addr: u64,
    },
}

impl From<MemFault> for Exception {
    fn from(f: MemFault) -> Self {
        match f.access {
            Access::Fetch => Exception::InstructionAbort { addr: f.addr },
            a => Exception::DataAbort { addr: f.addr, write: a == Access::Write },
        }
    }
}

/// Dimensione zero-estesa ai 64 bit.
#[inline]
fn trunc(v: u64, sf: bool) -> u64 {
    if sf { v } else { v as u32 as u64 }
}

#[inline]
fn datasize(sf: bool) -> u32 {
    if sf { 64 } else { 32 }
}

/// `AddWithCarry(x, y, carry)`: risultato troncato e flag NZCV.
fn add_with_carry(x: u64, y: u64, carry: bool, sf: bool) -> (u64, (bool, bool, bool, bool)) {
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u64);
        let v = ((x ^ r) & (y ^ r)) >> 63 != 0;
        (r, (r >> 63 != 0, r == 0, c1 || c2, v))
    } else {
        let (x, y) = (x as u32, y as u32);
        let wide = x as u64 + y as u64 + carry as u64;
        let r = wide as u32;
        let v = ((x ^ r) & (y ^ r)) >> 31 != 0;
        (r as u64, (r >> 31 != 0, r == 0, wide >> 32 != 0, v))
    }
}

fn shift_reg(v: u64, shift: Shift, amount: u32, sf: bool) -> u64 {
    let n = datasize(sf);
    let v = trunc(v, sf);
    let r = match shift {
        Shift::Lsl => v.checked_shl(amount).unwrap_or(0),
        Shift::Lsr => v.checked_shr(amount).unwrap_or(0),
        Shift::Asr => {
            let s = crate::bits::sext(v, n);
            (s >> amount.min(63)) as u64
        }
        Shift::Ror => ror(v, amount, n),
    };
    trunc(r, sf)
}

/// `ExtendReg(reg, type, shift)` su 64 bit, poi troncato dal chiamante.
fn extend_reg(v: u64, extend: u8, shift: u8) -> u64 {
    let e = match extend {
        0 => v as u8 as u64,
        1 => v as u16 as u64,
        2 => v as u32 as u64,
        3 | 7 => v,
        4 => v as u8 as i8 as i64 as u64,
        5 => v as u16 as i16 as i64 as u64,
        _ => v as u32 as i32 as i64 as u64,
    };
    e << shift
}

fn crc32(acc: u32, data: u64, bytes: u8, c: bool) -> u32 {
    let poly = if c { 0x82F6_3B78 } else { 0xEDB8_8320 };
    let mut crc = acc;
    for i in 0..bytes {
        crc ^= (data >> (8 * i)) as u8 as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    crc
}

fn read_uint<M: Memory>(mem: &mut M, addr: u64, bytes: usize) -> Result<u128, Exception> {
    let mut b = [0u8; 16];
    mem.read(addr, &mut b[..bytes])?;
    Ok(u128::from_le_bytes(b))
}

fn write_uint<M: Memory>(mem: &mut M, addr: u64, bytes: usize, v: u128) -> Result<(), Exception> {
    mem.write(addr, &v.to_le_bytes()[..bytes])?;
    Ok(())
}

fn check_aligned(addr: u64, bytes: u64) -> Result<(), Exception> {
    if !addr.is_multiple_of(bytes) { Err(Exception::Alignment { addr }) } else { Ok(()) }
}

/// Valore caricato di `1 << size` byte, esteso come chiede `op`.
fn extend_load(raw: u64, size: u8, op: MemOp) -> u64 {
    let bits = 8u32 << size;
    match op {
        MemOp::Load { signed: true, dst64 } => {
            let s = crate::bits::sext(raw, bits) as u64;
            if dst64 { s } else { s as u32 as u64 }
        }
        _ => raw,
    }
}

impl Cpu {
    /// Esegue un'istruzione. Su eccezione lo stato resta invariato (tranne
    /// `Svc`, vedi docs/specs/cpu.md).
    pub fn step<M: Memory>(&mut self, mem: &mut M) -> Result<(), Exception> {
        let pc = self.pc;
        if pc & 3 != 0 {
            return Err(Exception::PcAlignment { addr: pc });
        }
        let raw = mem.fetch(pc)?;
        let insn = decode(raw);
        match self.execute(insn, raw, mem)? {
            Some(target) => self.pc = target,
            None => self.pc = pc.wrapping_add(4),
        }
        if let Insn::Svc { imm } = insn {
            return Err(Exception::Svc(imm));
        }
        Ok(())
    }

    /// Restituisce `Some(target)` se l'istruzione salta.
    fn execute<M: Memory>(&mut self, insn: Insn, raw: u32, mem: &mut M) -> Result<Option<u64>, Exception> {
        let pc = self.pc;
        match insn {
            Insn::AddSubImm { sf, sub, setflags, imm, rn, rd } => {
                let x = self.xsp(rn);
                let (y, c) = if sub { (!imm, true) } else { (imm, false) };
                let (r, f) = add_with_carry(x, y, c, sf);
                if setflags {
                    self.set_flags(f.0, f.1, f.2, f.3);
                    self.set_x(rd, r);
                } else {
                    self.set_xsp(rd, r);
                }
            }
            Insn::LogicalImm { sf, op, imm, rn, rd } => {
                let x = self.xr(rn);
                let r = trunc(
                    match op {
                        LogicOp::And | LogicOp::Ands => x & imm,
                        LogicOp::Orr => x | imm,
                        LogicOp::Eor => x ^ imm,
                    },
                    sf,
                );
                if op == LogicOp::Ands {
                    self.set_logic_flags(r, sf);
                    self.set_x(rd, r);
                } else {
                    self.set_xsp(rd, r);
                }
            }
            Insn::MoveWide { sf, op, shift, imm16, rd } => {
                let imm = (imm16 as u64) << shift;
                let r = match op {
                    MovOp::Movz => imm,
                    MovOp::Movn => !imm,
                    MovOp::Movk => (self.xr(rd) & !(0xffffu64 << shift)) | imm,
                };
                self.set_x(rd, trunc(r, sf));
            }
            Insn::Adr { page, imm, rd } => {
                let base = if page { pc & !0xfff } else { pc };
                self.set_x(rd, base.wrapping_add(imm as u64));
            }
            Insn::Bitfield { sf, op, r, s, wmask, tmask, rn, rd } => {
                let n = datasize(sf);
                let dst = if op == BfOp::Bfm { self.xr(rd) } else { 0 };
                let src = self.xr(rn);
                let bot = (dst & !wmask) | (ror(src, r as u32, n) & wmask);
                let top = if op == BfOp::Sbfm { if (src >> s) & 1 != 0 { ones(n) } else { 0 } } else { dst };
                self.set_x(rd, trunc((top & !tmask) | (bot & tmask), sf));
            }
            Insn::Extract { sf, lsb, rm, rn, rd } => {
                let (hi, lo) = (self.xr(rn), self.xr(rm));
                let r = if sf {
                    if lsb == 0 { lo } else { (lo >> lsb) | (hi << (64 - lsb)) }
                } else {
                    let concat = ((hi as u32 as u64) << 32) | lo as u32 as u64;
                    (concat >> lsb) as u32 as u64
                };
                self.set_x(rd, r);
            }
            Insn::LogicalReg { sf, op, invert, shift, amount, rm, rn, rd } => {
                let mut y = shift_reg(self.xr(rm), shift, amount as u32, sf);
                if invert {
                    y = trunc(!y, sf);
                }
                let x = self.xr(rn);
                let r = match op {
                    LogicOp::And | LogicOp::Ands => x & y,
                    LogicOp::Orr => x | y,
                    LogicOp::Eor => x ^ y,
                };
                let r = trunc(r, sf);
                if op == LogicOp::Ands {
                    self.set_logic_flags(r, sf);
                }
                self.set_x(rd, r);
            }
            Insn::AddSubReg { sf, sub, setflags, shift, amount, rm, rn, rd } => {
                let y = shift_reg(self.xr(rm), shift, amount as u32, sf);
                let r = self.add_sub(self.xr(rn), y, sub, setflags, sf);
                self.set_x(rd, r);
            }
            Insn::AddSubExt { sf, sub, setflags, extend, amount, rm, rn, rd } => {
                let y = trunc(extend_reg(self.xr(rm), extend, amount), sf);
                let r = self.add_sub(self.xsp(rn), y, sub, setflags, sf);
                if setflags {
                    self.set_x(rd, r);
                } else {
                    self.set_xsp(rd, r);
                }
            }
            Insn::AddSubCarry { sf, sub, setflags, rm, rn, rd } => {
                let y = if sub { !self.xr(rm) } else { self.xr(rm) };
                let c = self.nzcv & crate::state::C != 0;
                let (r, f) = add_with_carry(self.xr(rn), y, c, sf);
                if setflags {
                    self.set_flags(f.0, f.1, f.2, f.3);
                }
                self.set_x(rd, r);
            }
            Insn::CondCmp { sf, sub, operand, cond, nzcv, rn } => {
                if self.condition_holds(cond) {
                    let y = match operand {
                        CcmpOperand::Reg(rm) => self.xr(rm),
                        CcmpOperand::Imm(i) => i as u64,
                    };
                    self.add_sub(self.xr(rn), y, sub, true, sf);
                } else {
                    self.nzcv = (nzcv as u32) << 28;
                }
            }
            Insn::CondSel { sf, op, cond, rm, rn, rd } => {
                let r = if self.condition_holds(cond) {
                    self.xr(rn)
                } else {
                    let m = self.xr(rm);
                    match op {
                        CselOp::Csel => m,
                        CselOp::Csinc => m.wrapping_add(1),
                        CselOp::Csinv => !m,
                        CselOp::Csneg => m.wrapping_neg(),
                    }
                };
                self.set_x(rd, trunc(r, sf));
            }
            Insn::Dp1 { sf, op, rn, rd } => {
                let x = self.xr(rn);
                let r = if sf {
                    match op {
                        Dp1Op::Rbit => x.reverse_bits(),
                        Dp1Op::Rev16 => {
                            ((x & 0x00ff_00ff_00ff_00ff) << 8) | ((x >> 8) & 0x00ff_00ff_00ff_00ff)
                        }
                        Dp1Op::Rev32 => {
                            let lo = (x as u32).swap_bytes() as u64;
                            let hi = ((x >> 32) as u32).swap_bytes() as u64;
                            (hi << 32) | lo
                        }
                        Dp1Op::Rev64 => x.swap_bytes(),
                        Dp1Op::Clz => x.leading_zeros() as u64,
                        Dp1Op::Cls => (((x ^ (x >> 1)) & (u64::MAX >> 1)).leading_zeros() - 1) as u64,
                    }
                } else {
                    let x = x as u32;
                    (match op {
                        Dp1Op::Rbit => x.reverse_bits(),
                        Dp1Op::Rev16 => ((x & 0x00ff_00ff) << 8) | ((x >> 8) & 0x00ff_00ff),
                        Dp1Op::Rev32 | Dp1Op::Rev64 => x.swap_bytes(),
                        Dp1Op::Clz => x.leading_zeros(),
                        Dp1Op::Cls => ((x ^ (x >> 1)) & (u32::MAX >> 1)).leading_zeros() - 1,
                    }) as u64
                };
                self.set_x(rd, r);
            }
            Insn::Dp2 { sf, op, rm, rn, rd } => {
                let (x, y) = (trunc(self.xr(rn), sf), trunc(self.xr(rm), sf));
                let n = datasize(sf);
                let r = match op {
                    Dp2Op::Udiv => x.checked_div(y).unwrap_or(0),
                    Dp2Op::Sdiv => {
                        let (a, b) = (crate::bits::sext(x, n), crate::bits::sext(y, n));
                        if b == 0 { 0 } else { a.wrapping_div(b) as u64 }
                    }
                    Dp2Op::Lslv => shift_reg(x, Shift::Lsl, (y % n as u64) as u32, sf),
                    Dp2Op::Lsrv => shift_reg(x, Shift::Lsr, (y % n as u64) as u32, sf),
                    Dp2Op::Asrv => shift_reg(x, Shift::Asr, (y % n as u64) as u32, sf),
                    Dp2Op::Rorv => shift_reg(x, Shift::Ror, (y % n as u64) as u32, sf),
                    Dp2Op::Crc32 { bytes, c } => crc32(x as u32, self.xr(rm), bytes, c) as u64,
                };
                self.set_x(rd, trunc(r, sf));
            }
            Insn::Dp3 { sf, op, ra, rm, rn, rd } => {
                let (a, m, n) = (self.xr(ra), self.xr(rm), self.xr(rn));
                let sx = |v: u64| v as u32 as i32 as i64 as u64;
                let zx = |v: u64| v as u32 as u64;
                let r = match op {
                    Dp3Op::Madd => a.wrapping_add(n.wrapping_mul(m)),
                    Dp3Op::Msub => a.wrapping_sub(n.wrapping_mul(m)),
                    Dp3Op::Smaddl => a.wrapping_add(sx(n).wrapping_mul(sx(m))),
                    Dp3Op::Smsubl => a.wrapping_sub(sx(n).wrapping_mul(sx(m))),
                    Dp3Op::Umaddl => a.wrapping_add(zx(n).wrapping_mul(zx(m))),
                    Dp3Op::Umsubl => a.wrapping_sub(zx(n).wrapping_mul(zx(m))),
                    Dp3Op::Smulh => ((n as i64 as i128 * m as i64 as i128) >> 64) as u64,
                    Dp3Op::Umulh => ((n as u128 * m as u128) >> 64) as u64,
                };
                self.set_x(rd, trunc(r, sf));
            }

            Insn::B { link, offset } => {
                if link {
                    self.x[30] = pc.wrapping_add(4);
                }
                return Ok(Some(pc.wrapping_add(offset as u64)));
            }
            Insn::BCond { cond, offset } => {
                if self.condition_holds(cond) {
                    return Ok(Some(pc.wrapping_add(offset as u64)));
                }
            }
            Insn::Cbz { sf, nonzero, rt, offset } => {
                if (trunc(self.xr(rt), sf) != 0) == nonzero {
                    return Ok(Some(pc.wrapping_add(offset as u64)));
                }
            }
            Insn::Tbz { nonzero, bit, rt, offset } => {
                if ((self.xr(rt) >> bit) & 1 != 0) == nonzero {
                    return Ok(Some(pc.wrapping_add(offset as u64)));
                }
            }
            Insn::BranchReg { op, rn } => {
                let target = self.xr(rn);
                if op == BrOp::Blr {
                    self.x[30] = pc.wrapping_add(4);
                }
                return Ok(Some(target));
            }

            Insn::Svc { .. } => {}
            Insn::Brk { imm } => return Err(Exception::Breakpoint(imm)),
            Insn::Nop | Insn::Barrier | Insn::CacheMaint => {}
            Insn::Clrex => self.monitor = None,
            Insn::DcZva { rt } => {
                let addr = self.xr(rt) & !63;
                mem.write(addr, &[0u8; 64])?;
            }
            Insn::Mrs { reg, rt } => {
                let v = match reg {
                    SysReg::Nzcv => self.nzcv as u64,
                    SysReg::TpidrEl0 => self.tpidr_el0,
                    SysReg::TpidrroEl0 => self.tpidrro_el0,
                };
                self.set_x(rt, v);
            }
            Insn::Msr { reg, rt } => {
                let v = self.xr(rt);
                match reg {
                    SysReg::Nzcv => self.nzcv = (v as u32) & 0xf000_0000,
                    SysReg::TpidrEl0 => self.tpidr_el0 = v,
                    SysReg::TpidrroEl0 => unreachable!("rifiutato dal decoder"),
                }
            }

            Insn::LdSt { size, op, addr, rt, rn } => {
                let base = self.xsp(rn);
                let (address, writeback) = match addr {
                    AddrMode::Imm { offset, index } => {
                        let moved = base.wrapping_add(offset as u64);
                        match index {
                            Index::Offset => (moved, None),
                            Index::Pre => (moved, Some(moved)),
                            Index::Post => (base, Some(moved)),
                        }
                    }
                    AddrMode::Reg { rm, extend, shift } => {
                        (base.wrapping_add(extend_reg(self.xr(rm), extend, shift)), None)
                    }
                };
                let bytes = 1usize << size;
                match op {
                    MemOp::Store => write_uint(mem, address, bytes, self.xr(rt) as u128)?,
                    MemOp::Load { .. } => {
                        let raw = read_uint(mem, address, bytes)? as u64;
                        self.set_x(rt, extend_load(raw, size, op));
                    }
                    MemOp::Prefetch => {}
                }
                if let Some(wb) = writeback {
                    self.set_xsp(rn, wb);
                }
            }
            Insn::LdLiteral { size, op, offset, rt } => {
                if op != MemOp::Prefetch {
                    let raw = read_uint(mem, pc.wrapping_add(offset as u64), 1 << size)? as u64;
                    self.set_x(rt, extend_load(raw, size, op));
                }
            }
            Insn::LdStPair { size, load, signed, index, offset, rt, rt2, rn } => {
                let base = self.xsp(rn);
                let moved = base.wrapping_add(offset as u64);
                let address = if index == Index::Post { base } else { moved };
                let bytes = 1usize << size;
                let second = address.wrapping_add(bytes as u64);
                if load {
                    let op = MemOp::Load { signed, dst64: true };
                    let a = read_uint(mem, address, bytes)? as u64;
                    let b = read_uint(mem, second, bytes)? as u64;
                    self.set_x(rt, extend_load(a, size, op));
                    self.set_x(rt2, extend_load(b, size, op));
                } else {
                    let (a, b) = (self.xr(rt), self.xr(rt2));
                    write_uint(mem, address, bytes, a as u128)?;
                    write_uint(mem, second, bytes, b as u128)?;
                }
                if index != Index::Offset {
                    self.set_xsp(rn, moved);
                }
            }
            Insn::Exclusive { size, load, pair, rs, rt, rt2, rn } => {
                let address = self.xsp(rn);
                let elem = 1u32 << size;
                let total = if pair { elem * 2 } else { elem };
                check_aligned(address, total as u64)?;
                if load {
                    let v = read_uint(mem, address, total as usize)?;
                    self.monitor = Some(Monitor { addr: address, bytes: total, value: v });
                    if pair {
                        let bits = elem * 8;
                        self.set_x(rt, (v & ones(bits) as u128) as u64);
                        self.set_x(rt2, (v >> bits) as u64);
                    } else {
                        self.set_x(rt, v as u64);
                    }
                } else {
                    let new = if pair {
                        let bits = elem * 8;
                        (self.xr(rt) as u128 & ones(bits) as u128) | ((self.xr(rt2) as u128) << bits)
                    } else {
                        self.xr(rt) as u128
                    };
                    let ok = match self.monitor.take() {
                        Some(m) if m.addr == address && m.bytes == total => {
                            read_uint(mem, address, total as usize)? == m.value
                        }
                        _ => false,
                    };
                    if ok {
                        write_uint(mem, address, total as usize, new)?;
                    }
                    self.set_x(rs, !ok as u64);
                }
            }
            Insn::LoadAcquire { size, rt, rn } => {
                let address = self.xsp(rn);
                check_aligned(address, 1 << size)?;
                let v = read_uint(mem, address, 1 << size)? as u64;
                self.set_x(rt, v);
            }
            Insn::StoreRelease { size, rt, rn } => {
                let address = self.xsp(rn);
                check_aligned(address, 1 << size)?;
                write_uint(mem, address, 1 << size, self.xr(rt) as u128)?;
            }

            Insn::Undefined => return Err(Exception::Undefined(raw)),
            Insn::Unimplemented(what) => return Err(Exception::Unimplemented { raw, what }),
        }
        Ok(None)
    }

    fn set_logic_flags(&mut self, r: u64, sf: bool) {
        let neg = r >> (datasize(sf) - 1) & 1 != 0;
        self.set_flags(neg, r == 0, false, false);
    }

    fn add_sub(&mut self, x: u64, y: u64, sub: bool, setflags: bool, sf: bool) -> u64 {
        let (y, c) = if sub { (!y, true) } else { (y, false) };
        let (r, f) = add_with_carry(x, y, c, sf);
        if setflags {
            self.set_flags(f.0, f.1, f.2, f.3);
        }
        r
    }
}
