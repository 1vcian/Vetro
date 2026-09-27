//! Values of the Cortex-A53 identification and reset registers.
//!
//! Read from `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53`
//! (QEMU 10.0.13) with a bare-metal program. Only intended difference:
//! ID_AA64PFR0_EL1.EL0/EL1 declare AArch64 only (1) instead of AArch64 and
//! AArch32 (2), because Vetro does not implement AArch32 (ADR 0005 and 0009).

/// MIDR_EL1: Arm, variant 0, Cortex-A53 (0xd03), revision 4.
pub const MIDR_EL1: u64 = 0x410f_d034;
pub const REVIDR_EL1: u64 = 0x0000_0100;
pub const CTR_EL0: u64 = 0x8444_8004;
/// DCZID_EL0.BS: DC ZVA blocks of 2^4 words = 64 bytes.
pub const DCZID_BS: u64 = 4;
/// Separate L1 data and instruction, unified L2; LoUU = LoUIS = 1, LoC = 2.
pub const CLIDR_EL1: u64 = 0x0a20_0023;
/// CCSIDR_EL1 for CSSELR_EL1 = 0 (L1 D, 32 KiB), 1 (L1 I, 32 KiB), 2 (L2,
/// 1 MiB); the others are 0.
pub const CCSIDR_EL1: [u64; 3] = [0x700f_e01a, 0x203f_e002, 0x707f_e07a];
/// SCTLR_EL1 at reset (like QEMU for the Cortex-A53).
pub const SCTLR_EL1_RESET: u64 = 0x00c5_0838;

/// ID space register with index `CRm * 8 + op2` (op0 = 3,
/// op1 = 0, CRn = 0, CRm = 1..=7). Reserved encodings are zero.
pub fn id_reg(index: u8, gicv3: bool) -> u64 {
    let gic = u64::from(gicv3);
    match index {
        8 => 0x0000_0131,              // ID_PFR0_EL1
        9 => 0x0001_0001 | gic << 28,  // ID_PFR1_EL1
        10 => 0x0300_0006,             // ID_DFR0_EL1
        12 => 0x1010_1105,             // ID_MMFR0_EL1
        13 => 0x4000_0000,             // ID_MMFR1_EL1
        14 => 0x0126_0000,             // ID_MMFR2_EL1
        15 => 0x0210_2211,             // ID_MMFR3_EL1
        16 => 0x0210_1110,             // ID_ISAR0_EL1
        17 => 0x1311_2111,             // ID_ISAR1_EL1
        18 => 0x2123_2042,             // ID_ISAR2_EL1
        19 => 0x0111_2131,             // ID_ISAR3_EL1
        20 => 0x0001_1142,             // ID_ISAR4_EL1
        21 => 0x0001_1121,             // ID_ISAR5_EL1
        24 => 0x1011_0222,             // MVFR0_EL1
        25 => 0x1211_1111,             // MVFR1_EL1
        26 => 0x0000_0043,             // MVFR2_EL1
        32 => 0x0000_0011 | gic << 24, // ID_AA64PFR0_EL1: EL0/EL1 AArch64 only, FP and AdvSIMD
        40 => 0x1030_5106,             // ID_AA64DFR0_EL1: 6 breakpoints, 4 watchpoints, PMUv3
        48 => 0x0001_1120,             // ID_AA64ISAR0_EL1: AES+PMULL, SHA1, SHA256, CRC32
        56 => 0x0000_1122,             // ID_AA64MMFR0_EL1: PA 40 bits, ASID 16 bits, 4K and 64K
        _ => 0,
    }
}
