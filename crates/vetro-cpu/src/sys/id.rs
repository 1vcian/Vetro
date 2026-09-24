//! Valori dei registri di identificazione e di reset della Cortex-A53.
//!
//! Letti da `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53`
//! (QEMU 10.0.13) con un programma bare-metal. Unica differenza voluta:
//! ID_AA64PFR0_EL1.EL0/EL1 dichiarano solo AArch64 (1) invece di AArch64 e
//! AArch32 (2), perché Vetro non implementa AArch32 (ADR 0005 e 0009).

/// MIDR_EL1: Arm, variante 0, Cortex-A53 (0xd03), revisione 4.
pub const MIDR_EL1: u64 = 0x410f_d034;
pub const REVIDR_EL1: u64 = 0x0000_0100;
pub const CTR_EL0: u64 = 0x8444_8004;
/// DCZID_EL0.BS: blocchi di DC ZVA da 2^4 parole = 64 byte.
pub const DCZID_BS: u64 = 4;
/// L1 dati e istruzioni separate, L2 unificata; LoUU = LoUIS = 1, LoC = 2.
pub const CLIDR_EL1: u64 = 0x0a20_0023;
/// CCSIDR_EL1 per CSSELR_EL1 = 0 (L1 D, 32 KiB), 1 (L1 I, 32 KiB), 2 (L2,
/// 1 MiB); gli altri valgono 0.
pub const CCSIDR_EL1: [u64; 3] = [0x700f_e01a, 0x203f_e002, 0x707f_e07a];
/// SCTLR_EL1 al reset (come QEMU per la Cortex-A53).
pub const SCTLR_EL1_RESET: u64 = 0x00c5_0838;

/// Registro dello spazio degli ID con indice `CRm * 8 + op2` (op0 = 3,
/// op1 = 0, CRn = 0, CRm = 1..=7). Le codifiche riservate valgono zero.
pub fn id_reg(index: u8, gicv3: bool) -> u64 {
    let gic = u64::from(gicv3);
    match index {
        8 => 0x0000_0131,               // ID_PFR0_EL1
        9 => 0x0001_0001 | gic << 28,   // ID_PFR1_EL1
        10 => 0x0300_0006,              // ID_DFR0_EL1
        12 => 0x1010_1105,              // ID_MMFR0_EL1
        13 => 0x4000_0000,              // ID_MMFR1_EL1
        14 => 0x0126_0000,              // ID_MMFR2_EL1
        15 => 0x0210_2211,              // ID_MMFR3_EL1
        16 => 0x0210_1110,              // ID_ISAR0_EL1
        17 => 0x1311_2111,              // ID_ISAR1_EL1
        18 => 0x2123_2042,              // ID_ISAR2_EL1
        19 => 0x0111_2131,              // ID_ISAR3_EL1
        20 => 0x0001_1142,              // ID_ISAR4_EL1
        21 => 0x0001_1121,              // ID_ISAR5_EL1
        24 => 0x1011_0222,              // MVFR0_EL1
        25 => 0x1211_1111,              // MVFR1_EL1
        26 => 0x0000_0043,              // MVFR2_EL1
        32 => 0x0000_0011 | gic << 24,  // ID_AA64PFR0_EL1: EL0/EL1 solo AArch64, FP e AdvSIMD
        40 => 0x1030_5106,              // ID_AA64DFR0_EL1: 6 breakpoint, 4 watchpoint, PMUv3
        48 => 0x0001_1120,              // ID_AA64ISAR0_EL1: AES+PMULL, SHA1, SHA256, CRC32
        56 => 0x0000_1122,              // ID_AA64MMFR0_EL1: PA 40 bit, ASID 16 bit, 4K e 64K
        _ => 0,
    }
}
