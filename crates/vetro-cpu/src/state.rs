//! Architectural state: EL0 registers and, for system mode,
//! [`SysState`](crate::sys::SysState).

/// Local exclusive monitor: set by LDXR/LDAXR/LDXP, consumed by
/// STXR/STLXR/STXP (see docs/specs/cpu.md, "Exclusives").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub addr: u64,
    /// Total bytes of the access (for pairs, both elements).
    pub bytes: u32,
    pub value: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cpu {
    pub x: [u64; 31],
    /// Stack pointer in use: SP_EL0 in user mode; in system mode
    /// SP_EL0 or SP_EL1 depending on PSTATE (the other one is in `sys.sp_el`).
    pub sp: u64,
    pub pc: u64,
    /// N, Z, C, V flags in bits 31:28 (same format as `MRS Xt, NZCV`).
    pub nzcv: u32,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub monitor: Option<Monitor>,
    /// SIMD/FP registers V0–V31.
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
    /// PSTATE beyond NZCV and EL1 registers (system mode).
    pub sys: crate::sys::SysState,
}

/// Writable FPCR bits on Cortex-A53: AHP, DN, FZ, RMode. The trap
/// enables are RAZ/WI (no FP traps), Len/Stride are RES0 in AArch64.
pub const FPCR_MASK: u32 = 0x07C0_0000;
/// FPSR bits: QC, IDC and the cumulative flags IXC, UFC, OFC, DZC, IOC.
pub const FPSR_MASK: u32 = 0x0800_009F;

pub const N: u32 = 1 << 31;
pub const Z: u32 = 1 << 30;
pub const C: u32 = 1 << 29;
pub const V: u32 = 1 << 28;

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    /// General register; 31 is XZR.
    #[inline]
    pub fn xr(&self, r: u8) -> u64 {
        if r == 31 { 0 } else { self.x[r as usize] }
    }

    /// General register; 31 is SP.
    #[inline]
    pub fn xsp(&self, r: u8) -> u64 {
        if r == 31 { self.sp } else { self.x[r as usize] }
    }

    /// Writes a register; 31 (XZR) discards the value.
    #[inline]
    pub fn set_x(&mut self, r: u8, v: u64) {
        if r != 31 {
            self.x[r as usize] = v;
        }
    }

    /// Writes a register; 31 is SP.
    #[inline]
    pub fn set_xsp(&mut self, r: u8, v: u64) {
        if r == 31 {
            self.sp = v;
        } else {
            self.x[r as usize] = v;
        }
    }

    #[inline]
    pub fn set_flags(&mut self, n: bool, z: bool, c: bool, v: bool) {
        self.nzcv = (n as u32) << 31 | (z as u32) << 30 | (c as u32) << 29 | (v as u32) << 28;
    }

    /// `ConditionHolds(cond)`.
    pub fn condition_holds(&self, cond: u8) -> bool {
        let f = self.nzcv;
        let (n, z, c, v) = (f & N != 0, f & Z != 0, f & C != 0, f & V != 0);
        let r = match cond >> 1 {
            0 => z,
            1 => c,
            2 => n,
            3 => v,
            4 => c && !z,
            5 => n == v,
            6 => n == v && !z,
            _ => true,
        };
        if cond & 1 == 1 && cond != 0b1111 { !r } else { r }
    }
}
