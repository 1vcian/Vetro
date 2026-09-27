//! System registers that govern stage 1 translation of the EL1&0 regime.

/// Size of the translation granule, the only one implemented: 4 KiB.
pub const PAGE_SIZE: u64 = 4096;

/// SCTLR_EL1 bits read by the MMU.
pub mod sctlr {
    /// Enables stage 1 translation.
    pub const M: u64 = 1 << 0;
    /// Instruction cache: decides the attributes of fetches with the MMU off.
    pub const I: u64 = 1 << 12;
    /// Write implies eXecute-Never.
    pub const WXN: u64 = 1 << 19;
    /// Endianness of EL1 accesses and of descriptors.
    pub const EE: u64 = 1 << 25;
}

/// TCR_EL1 fields read by the MMU.
pub mod tcr {
    /// T0SZ, bits 5:0.
    pub const T0SZ_SHIFT: u32 = 0;
    /// No walk from TTBR0 on a TLB miss.
    pub const EPD0: u64 = 1 << 7;
    /// TG0, bits 15:14 (00 = 4 KiB, 01 = 64 KiB, 10 = 16 KiB).
    pub const TG0_SHIFT: u32 = 14;
    /// T1SZ, bits 21:16.
    pub const T1SZ_SHIFT: u32 = 16;
    /// The current ASID comes from TTBR1 instead of TTBR0.
    pub const A1: u64 = 1 << 22;
    /// No walk from TTBR1 on a TLB miss.
    pub const EPD1: u64 = 1 << 23;
    /// TG1, bits 31:30 (01 = 16 KiB, 10 = 4 KiB, 11 = 64 KiB).
    pub const TG1_SHIFT: u32 = 30;
    /// IPS, bits 34:32: size of the output physical address.
    pub const IPS_SHIFT: u32 = 32;
    /// 16-bit ASID (otherwise 8).
    pub const AS: u64 = 1 << 36;
    /// Top Byte Ignore for addresses with bit 55 = 0.
    pub const TBI0: u64 = 1 << 37;
    /// Top Byte Ignore for addresses with bit 55 = 1.
    pub const TBI1: u64 = 1 << 38;
}

/// The registers that determine translation. The system writes them (MSR);
/// the MMU reads them on every access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MmuRegs {
    pub sctlr: u64,
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub mair: u64,
}

/// Effective granule of one of the two halves of the virtual space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Granule {
    K4,
    K64,
}

impl MmuRegs {
    /// Branchless equality (checked on every access).
    #[inline]
    pub(crate) fn same(&self, o: &MmuRegs) -> bool {
        (self.sctlr ^ o.sctlr)
            | (self.tcr ^ o.tcr)
            | (self.ttbr0 ^ o.ttbr0)
            | (self.ttbr1 ^ o.ttbr1)
            | (self.mair ^ o.mair)
            == 0
    }

    /// SCTLR_EL1.M.
    pub fn enabled(&self) -> bool {
        self.sctlr & sctlr::M != 0
    }

    /// SCTLR_EL1.WXN.
    pub fn wxn(&self) -> bool {
        self.sctlr & sctlr::WXN != 0
    }

    /// Current ASID: from TTBR1 if TCR.A1, otherwise from TTBR0; reduced to
    /// 8 bits if TCR.AS = 0.
    pub fn asid(&self) -> u16 {
        let ttbr = if self.tcr & tcr::A1 != 0 { self.ttbr1 } else { self.ttbr0 };
        let asid = (ttbr >> 48) as u16;
        if self.tcr & tcr::AS != 0 { asid } else { asid & 0xff }
    }

    /// Highest address bit that takes part in translation
    /// (`AddrTop` in the pseudocode): 55 with Top Byte Ignore, 63 without.
    /// TBI is selected by bit 55.
    pub fn addr_top(&self, va: u64) -> u32 {
        let tbi = if va >> 55 & 1 != 0 { tcr::TBI1 } else { tcr::TBI0 };
        if self.tcr & tbi != 0 { 55 } else { 63 }
    }

    /// Translated virtual address bits (64 - TxSZ). TxSZ values outside
    /// 16..=39 are CONSTRAINED UNPREDICTABLE: like QEMU we clamp them to the
    /// nearest limit.
    pub(crate) fn input_size(&self, hi: bool) -> u32 {
        let shift = if hi { tcr::T1SZ_SHIFT } else { tcr::T0SZ_SHIFT };
        let txsz = (self.tcr >> shift & 0x3f) as u32;
        64 - txsz.clamp(16, 39)
    }

    /// Granule selected by TG0/TG1. The Cortex-A53 has 4 KiB and 64 KiB: 16 KiB and the
    /// reserved values count as an IMPLEMENTATION DEFINED choice among
    /// the implemented ones, and (like QEMU) we choose the smallest.
    pub(crate) fn granule(&self, hi: bool) -> Granule {
        let k64 =
            if hi { self.tcr >> tcr::TG1_SHIFT & 3 == 0b11 } else { self.tcr >> tcr::TG0_SHIFT & 3 == 0b01 };
        if k64 { Granule::K64 } else { Granule::K4 }
    }

    /// EPD0/EPD1: walk disabled for that half.
    pub(crate) fn walk_disabled(&self, hi: bool) -> bool {
        self.tcr & if hi { tcr::EPD1 } else { tcr::EPD0 } != 0
    }

    pub(crate) fn ttbr(&self, hi: bool) -> u64 {
        if hi { self.ttbr1 } else { self.ttbr0 }
    }

    /// Output physical address bits: TCR.IPS limited to `pa_bits`
    /// (the CPU's PARange). Reserved values count as 48 before the limit.
    pub(crate) fn output_size(&self, pa_bits: u32) -> u32 {
        let ips = match self.tcr >> tcr::IPS_SHIFT & 7 {
            0 => 32,
            1 => 36,
            2 => 40,
            3 => 42,
            4 => 44,
            _ => 48,
        };
        ips.min(pa_bits)
    }

    /// MAIR_EL1 byte selected by AttrIndx.
    pub fn mair_attr(&self, attr_index: u8) -> u8 {
        (self.mair >> (8 * u32::from(attr_index & 7))) as u8
    }
}
