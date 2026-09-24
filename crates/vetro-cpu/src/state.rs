//! Stato architetturale visibile a EL0.

/// Monitor esclusivo locale: impostato da LDXR/LDAXR/LDXP, consumato da
/// STXR/STLXR/STXP (vedi docs/specs/cpu.md, "Esclusive").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub addr: u64,
    /// Byte totali dell'accesso (per le coppie, entrambi gli elementi).
    pub bytes: u32,
    pub value: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cpu {
    pub x: [u64; 31],
    /// SP_EL0.
    pub sp: u64,
    pub pc: u64,
    /// Flag N, Z, C, V nei bit 31:28 (stesso formato di `MRS Xt, NZCV`).
    pub nzcv: u32,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub monitor: Option<Monitor>,
    /// Registri SIMD/FP V0–V31.
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
}

/// Bit scrivibili di FPCR su Cortex-A53: AHP, DN, FZ, RMode. Le abilitazioni
/// delle trap sono RAZ/WI (niente trap FP), Len/Stride sono RES0 in AArch64.
pub const FPCR_MASK: u32 = 0x07C0_0000;
/// Bit di FPSR: QC, IDC e i flag cumulativi IXC, UFC, OFC, DZC, IOC.
pub const FPSR_MASK: u32 = 0x0800_009F;

pub const N: u32 = 1 << 31;
pub const Z: u32 = 1 << 30;
pub const C: u32 = 1 << 29;
pub const V: u32 = 1 << 28;

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registro generale; 31 vale XZR.
    #[inline]
    pub fn xr(&self, r: u8) -> u64 {
        if r == 31 { 0 } else { self.x[r as usize] }
    }

    /// Registro generale; 31 vale SP.
    #[inline]
    pub fn xsp(&self, r: u8) -> u64 {
        if r == 31 { self.sp } else { self.x[r as usize] }
    }

    /// Scrive un registro; 31 (XZR) scarta il valore.
    #[inline]
    pub fn set_x(&mut self, r: u8, v: u64) {
        if r != 31 {
            self.x[r as usize] = v;
        }
    }

    /// Scrive un registro; 31 vale SP.
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
