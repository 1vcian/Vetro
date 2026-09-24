//! Registri di sistema che governano la traduzione stage 1 del regime EL1&0.

/// Dimensione del granulo di traduzione, l'unico implementato: 4 KiB.
pub const PAGE_SIZE: u64 = 4096;

/// Bit di SCTLR_EL1 letti dalla MMU.
pub mod sctlr {
    /// Abilita la traduzione stage 1.
    pub const M: u64 = 1 << 0;
    /// Cache istruzioni: decide gli attributi dei fetch a MMU spenta.
    pub const I: u64 = 1 << 12;
    /// Write implies eXecute-Never.
    pub const WXN: u64 = 1 << 19;
    /// Endianness degli accessi a EL1 e dei descrittori.
    pub const EE: u64 = 1 << 25;
}

/// Campi di TCR_EL1 letti dalla MMU.
pub mod tcr {
    /// T0SZ, bit 5:0.
    pub const T0SZ_SHIFT: u32 = 0;
    /// Nessun walk da TTBR0 su un miss del TLB.
    pub const EPD0: u64 = 1 << 7;
    /// TG0, bit 15:14 (00 = 4 KiB, 01 = 64 KiB, 10 = 16 KiB).
    pub const TG0_SHIFT: u32 = 14;
    /// T1SZ, bit 21:16.
    pub const T1SZ_SHIFT: u32 = 16;
    /// L'ASID corrente viene da TTBR1 invece che da TTBR0.
    pub const A1: u64 = 1 << 22;
    /// Nessun walk da TTBR1 su un miss del TLB.
    pub const EPD1: u64 = 1 << 23;
    /// TG1, bit 31:30 (01 = 16 KiB, 10 = 4 KiB, 11 = 64 KiB).
    pub const TG1_SHIFT: u32 = 30;
    /// IPS, bit 34:32: dimensione dell'indirizzo fisico in uscita.
    pub const IPS_SHIFT: u32 = 32;
    /// ASID a 16 bit (altrimenti 8).
    pub const AS: u64 = 1 << 36;
    /// Top Byte Ignore per gli indirizzi con bit 55 = 0.
    pub const TBI0: u64 = 1 << 37;
    /// Top Byte Ignore per gli indirizzi con bit 55 = 1.
    pub const TBI1: u64 = 1 << 38;
}

/// I registri che determinano la traduzione. Li scrive il sistema (MSR);
/// la MMU li legge a ogni accesso.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MmuRegs {
    pub sctlr: u64,
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub mair: u64,
}

/// Granulo effettivo di una delle due metà dello spazio virtuale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Granule {
    K4,
    K64,
}

impl MmuRegs {
    /// SCTLR_EL1.M.
    pub fn enabled(&self) -> bool {
        self.sctlr & sctlr::M != 0
    }

    /// SCTLR_EL1.WXN.
    pub fn wxn(&self) -> bool {
        self.sctlr & sctlr::WXN != 0
    }

    /// ASID corrente: da TTBR1 se TCR.A1, altrimenti da TTBR0; ridotto a
    /// 8 bit se TCR.AS = 0.
    pub fn asid(&self) -> u16 {
        let ttbr = if self.tcr & tcr::A1 != 0 { self.ttbr1 } else { self.ttbr0 };
        let asid = (ttbr >> 48) as u16;
        if self.tcr & tcr::AS != 0 { asid } else { asid & 0xff }
    }

    /// Bit più alto dell'indirizzo che partecipa alla traduzione
    /// (`AddrTop` dello pseudocodice): 55 con Top Byte Ignore, 63 senza.
    /// Il TBI si sceglie col bit 55.
    pub fn addr_top(&self, va: u64) -> u32 {
        let tbi = if va >> 55 & 1 != 0 { tcr::TBI1 } else { tcr::TBI0 };
        if self.tcr & tbi != 0 { 55 } else { 63 }
    }

    /// Bit di indirizzo virtuale tradotti (64 - TxSZ). I valori di TxSZ fuori
    /// da 16..=39 sono CONSTRAINED UNPREDICTABLE: come QEMU li riportiamo al
    /// limite più vicino.
    pub(crate) fn input_size(&self, hi: bool) -> u32 {
        let shift = if hi { tcr::T1SZ_SHIFT } else { tcr::T0SZ_SHIFT };
        let txsz = (self.tcr >> shift & 0x3f) as u32;
        64 - txsz.clamp(16, 39)
    }

    /// Granulo scelto da TG0/TG1. La Cortex-A53 ha 4 KiB e 64 KiB: 16 KiB e i
    /// valori riservati valgono come una scelta IMPLEMENTATION DEFINED fra
    /// quelli implementati, e (come QEMU) scegliamo il più piccolo.
    pub(crate) fn granule(&self, hi: bool) -> Granule {
        let k64 =
            if hi { self.tcr >> tcr::TG1_SHIFT & 3 == 0b11 } else { self.tcr >> tcr::TG0_SHIFT & 3 == 0b01 };
        if k64 { Granule::K64 } else { Granule::K4 }
    }

    /// EPD0/EPD1: walk disabilitato per quella metà.
    pub(crate) fn walk_disabled(&self, hi: bool) -> bool {
        self.tcr & if hi { tcr::EPD1 } else { tcr::EPD0 } != 0
    }

    pub(crate) fn ttbr(&self, hi: bool) -> u64 {
        if hi { self.ttbr1 } else { self.ttbr0 }
    }

    /// Bit dell'indirizzo fisico in uscita: TCR.IPS limitato a `pa_bits`
    /// (PARange della CPU). I valori riservati valgono 48 prima del limite.
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

    /// Byte di MAIR_EL1 selezionato da AttrIndx.
    pub fn mair_attr(&self, attr_index: u8) -> u8 {
        (self.mair >> (8 * u32::from(attr_index & 7))) as u8
    }
}
