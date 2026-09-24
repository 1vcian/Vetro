//! Load/store dei registri SIMD/FP (V=1): singoli, coppie, letterali e
//! strutture (LD1–LD4, ST1–ST4, LDnR).

use super::SimdInsn;
use super::vreg::{clip, elem, set_elem};
use crate::bits::{bit, field, sext};
use crate::decode::{AddrMode, Index, Insn};
use crate::exec::{Exception, extend_reg};
use crate::mem::Memory;
use crate::state::Cpu;

/// Incremento dopo l'accesso per le strutture: immediato implicito o Xm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Post {
    None,
    Imm(u64),
    Reg(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecMemInsn {
    /// `scale` = log2 dei byte (0..=4, 4 = Q).
    Reg {
        scale: u8,
        load: bool,
        addr: AddrMode,
        rt: u8,
        rn: u8,
    },
    Literal {
        scale: u8,
        offset: i64,
        rt: u8,
    },
    Pair {
        scale: u8,
        load: bool,
        index: Index,
        offset: i64,
        rt: u8,
        rt2: u8,
        rn: u8,
    },
    /// LD1–LD4/ST1–ST4 (strutture multiple).
    Multi {
        load: bool,
        q: bool,
        rpt: u8,
        selem: u8,
        esize: u8,
        rt: u8,
        rn: u8,
        post: Post,
    },
    /// Struttura singola: una corsia (`index`) oppure replica (LDnR).
    Single {
        load: bool,
        q: bool,
        selem: u8,
        scale: u8,
        index: u8,
        replicate: bool,
        rt: u8,
        rn: u8,
        post: Post,
    },
}

fn ok(i: VecMemInsn) -> Insn {
    Insn::Simd(SimdInsn::Mem(i))
}

pub fn decode(w: u32) -> Insn {
    let rt = field(w, 4, 0) as u8;
    let rn = field(w, 9, 5) as u8;
    // Strutture: 31=0, 29:27 = 001, 26 = 1 (V), 25 = 0... (bits 29:24 = 0011xx).
    if field(w, 29, 24) == 0b001100 || field(w, 29, 24) == 0b001101 {
        return structures(w, rt, rn);
    }
    match field(w, 29, 27) {
        0b011 => {
            if bit(w, 24) {
                return Insn::Undefined;
            }
            let scale = match field(w, 31, 30) {
                0 => 2,
                1 => 3,
                2 => 4,
                _ => return Insn::Undefined,
            };
            ok(VecMemInsn::Literal { scale, offset: sext(field(w, 23, 5) as u64, 19) << 2, rt })
        }
        0b101 => {
            let scale = match field(w, 31, 30) {
                0 => 2,
                1 => 3,
                2 => 4,
                _ => return Insn::Undefined,
            };
            let index = match field(w, 24, 23) {
                0b00 | 0b10 => Index::Offset,
                0b01 => Index::Post,
                _ => Index::Pre,
            };
            let offset = sext(field(w, 21, 15) as u64, 7) << scale;
            ok(VecMemInsn::Pair {
                scale,
                load: bit(w, 22),
                index,
                offset,
                rt,
                rt2: field(w, 14, 10) as u8,
                rn,
            })
        }
        0b111 => {
            let scale = ((field(w, 23, 23) << 2) | field(w, 31, 30)) as u8;
            if scale > 4 {
                return Insn::Undefined;
            }
            let load = bit(w, 22);
            if bit(w, 24) {
                let offset = (field(w, 21, 10) as i64) << scale;
                return ok(VecMemInsn::Reg {
                    scale,
                    load,
                    addr: AddrMode::Imm { offset, index: Index::Offset },
                    rt,
                    rn,
                });
            }
            if bit(w, 21) {
                let extend = field(w, 15, 13) as u8;
                if field(w, 11, 10) != 0b10 || extend & 0b010 == 0 {
                    return Insn::Undefined;
                }
                let shift = if bit(w, 12) { scale } else { 0 };
                return ok(VecMemInsn::Reg {
                    scale,
                    load,
                    addr: AddrMode::Reg { rm: field(w, 20, 16) as u8, extend, shift },
                    rt,
                    rn,
                });
            }
            let index = match field(w, 11, 10) {
                0b00 => Index::Offset,
                0b01 => Index::Post,
                0b11 => Index::Pre,
                _ => return Insn::Undefined, // niente LDTR/STTR per i registri FP
            };
            let offset = sext(field(w, 20, 12) as u64, 9);
            ok(VecMemInsn::Reg { scale, load, addr: AddrMode::Imm { offset, index }, rt, rn })
        }
        _ => Insn::Undefined,
    }
}

fn structures(w: u32, rt: u8, rn: u8) -> Insn {
    if bit(w, 31) {
        return Insn::Undefined;
    }
    let q = bit(w, 30);
    let load = bit(w, 22);
    let postidx = bit(w, 23);
    let rm = field(w, 20, 16) as u8;
    if !bit(w, 24) {
        // Strutture multiple: 0 Q 0011000 L 000000 opcode size Rn Rt
        if bit(w, 21) || !postidx && rm != 0 {
            return Insn::Undefined;
        }
        let (rpt, selem) = match field(w, 15, 12) {
            0b0000 => (1, 4),
            0b0010 => (4, 1),
            0b0100 => (1, 3),
            0b0110 => (3, 1),
            0b0111 => (1, 1),
            0b1000 => (1, 2),
            0b1010 => (2, 1),
            _ => return Insn::Undefined,
        };
        let size = field(w, 11, 10) as u8;
        if size == 3 && !q && selem > 1 {
            return Insn::Undefined;
        }
        let bytes = (if q { 16 } else { 8 }) * rpt as u64 * selem as u64;
        let post = if !postidx {
            Post::None
        } else if rm == 31 {
            Post::Imm(bytes)
        } else {
            Post::Reg(rm)
        };
        return ok(VecMemInsn::Multi { load, q, rpt, selem, esize: 8 << size, rt, rn, post });
    }
    // Struttura singola: 0 Q 0011010 L R 00000 opcode S size Rn Rt
    if !postidx && rm != 0 {
        return Insn::Undefined;
    }
    let r = field(w, 21, 21);
    let opcode = field(w, 15, 13);
    let s = field(w, 12, 12);
    let size = field(w, 11, 10);
    let selem = (((opcode & 1) << 1) | r) as u8 + 1;
    let qb = q as u32;
    let (scale, index, replicate) = match opcode >> 1 {
        0 => (0, (qb << 3) | (s << 2) | size, false),
        1 => {
            if size & 1 != 0 {
                return Insn::Undefined;
            }
            (1, (qb << 2) | (s << 1) | (size >> 1), false)
        }
        2 => match (size, s) {
            (0, _) => (2, (qb << 1) | s, false),
            (1, 0) => (3, qb, false),
            _ => return Insn::Undefined,
        },
        _ => {
            if !load || s != 0 {
                return Insn::Undefined;
            }
            (size, 0, true)
        }
    };
    let bytes = (selem as u64) << scale;
    let post = if !postidx {
        Post::None
    } else if rm == 31 {
        Post::Imm(bytes)
    } else {
        Post::Reg(rm)
    };
    ok(VecMemInsn::Single { load, q, selem, scale: scale as u8, index: index as u8, replicate, rt, rn, post })
}

fn read<M: Memory>(mem: &mut M, addr: u64, bytes: usize) -> Result<u128, Exception> {
    let mut b = [0u8; 16];
    mem.read(addr, &mut b[..bytes])?;
    Ok(u128::from_le_bytes(b))
}

fn write<M: Memory>(mem: &mut M, addr: u64, bytes: usize, v: u128) -> Result<(), Exception> {
    mem.write(addr, &v.to_le_bytes()[..bytes])?;
    Ok(())
}

pub(crate) fn exec<M: Memory>(cpu: &mut Cpu, i: VecMemInsn, mem: &mut M) -> Result<(), Exception> {
    match i {
        VecMemInsn::Reg { scale, load, addr, rt, rn } => {
            let base = cpu.xsp(rn);
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
                    (base.wrapping_add(extend_reg(cpu.xr(rm), extend, shift)), None)
                }
            };
            let bytes = 1usize << scale;
            if load {
                cpu.v[rt as usize] = read(mem, address, bytes)?;
            } else {
                write(mem, address, bytes, cpu.v[rt as usize])?;
            }
            if let Some(wb) = writeback {
                cpu.set_xsp(rn, wb);
            }
        }
        VecMemInsn::Literal { scale, offset, rt } => {
            cpu.v[rt as usize] = read(mem, cpu.pc.wrapping_add(offset as u64), 1 << scale)?;
        }
        VecMemInsn::Pair { scale, load, index, offset, rt, rt2, rn } => {
            let base = cpu.xsp(rn);
            let moved = base.wrapping_add(offset as u64);
            let address = if index == Index::Post { base } else { moved };
            let bytes = 1usize << scale;
            let second = address.wrapping_add(bytes as u64);
            if load {
                let a = read(mem, address, bytes)?;
                let b = read(mem, second, bytes)?;
                cpu.v[rt as usize] = a;
                cpu.v[rt2 as usize] = b;
            } else {
                write(mem, address, bytes, cpu.v[rt as usize])?;
                write(mem, second, bytes, cpu.v[rt2 as usize])?;
            }
            if index != Index::Offset {
                cpu.set_xsp(rn, moved);
            }
        }
        VecMemInsn::Multi { load, q, rpt, selem, esize, rt, rn, post } => {
            let datasize = if q { 128 } else { 64 };
            let elements = datasize / esize as usize;
            let ebytes = esize as usize / 8;
            let base = cpu.xsp(rn);
            let mut offs = 0u64;
            for r in 0..rpt as usize {
                for e in 0..elements {
                    let mut tt = (rt as usize + r) % 32;
                    for _ in 0..selem {
                        let a = base.wrapping_add(offs);
                        if load {
                            let x = read(mem, a, ebytes)? as u64;
                            let v = clip(cpu.v[tt], datasize as u32);
                            cpu.v[tt] = set_elem(v, e, esize as u32, x);
                        } else {
                            write(mem, a, ebytes, elem(cpu.v[tt], e, esize as u32) as u128)?;
                        }
                        offs += ebytes as u64;
                        tt = (tt + 1) % 32;
                    }
                }
            }
            post_index(cpu, rn, base, post);
        }
        VecMemInsn::Single { load, q, selem, scale, index, replicate, rt, rn, post } => {
            let datasize = if q { 128 } else { 64 };
            let esize = 8u32 << scale;
            let ebytes = 1usize << scale;
            let base = cpu.xsp(rn);
            let mut offs = 0u64;
            let mut t = rt as usize;
            for _ in 0..selem {
                let a = base.wrapping_add(offs);
                if replicate {
                    let x = read(mem, a, ebytes)? as u64;
                    let mut v = 0u128;
                    for e in 0..(datasize / esize) as usize {
                        v = set_elem(v, e, esize, x);
                    }
                    cpu.v[t] = v;
                } else if load {
                    let x = read(mem, a, ebytes)? as u64;
                    // Una corsia: il registro resta intero (Q conta solo per l'indice).
                    cpu.v[t] = set_elem(cpu.v[t], index as usize, esize, x);
                } else {
                    write(mem, a, ebytes, elem(cpu.v[t], index as usize, esize) as u128)?;
                }
                offs += ebytes as u64;
                t = (t + 1) % 32;
            }
            post_index(cpu, rn, base, post);
        }
    }
    Ok(())
}

fn post_index(cpu: &mut Cpu, rn: u8, base: u64, post: Post) {
    match post {
        Post::None => {}
        Post::Imm(n) => cpu.set_xsp(rn, base.wrapping_add(n)),
        Post::Reg(rm) => {
            let off = cpu.xr(rm);
            cpu.set_xsp(rn, base.wrapping_add(off));
        }
    }
}
