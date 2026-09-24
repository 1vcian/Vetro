//! Codifiche AArch64 minime (Arm ARM, sezione C6), quanto basta per
//! prologhi ed epiloghi dei programmi di test. La CPU vera sta in `vetro-cpu`.

/// Numeri di syscall Linux arm64 (asm-generic).
pub mod sys {
    pub const WRITE: u16 = 64;
    pub const EXIT: u16 = 93;
}

/// Registro 31 nei campi che lo interpretano come SP.
pub const SP: u32 = 31;

/// `MOVZ Xd, #imm16, LSL #(hw*16)`
pub const fn movz(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xD280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// `MOVK Xd, #imm16, LSL #(hw*16)`
pub const fn movk(rd: u32, imm16: u16, hw: u32) -> u32 {
    0xF280_0000 | (hw << 21) | ((imm16 as u32) << 5) | rd
}

/// `SVC #imm16`
pub const fn svc(imm16: u16) -> u32 {
    0xD400_0001 | ((imm16 as u32) << 5)
}

/// `ADD Xd|SP, Xn|SP, #imm12`
pub const fn add_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `SUB Xd|SP, Xn|SP, #imm12`
pub const fn sub_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xD100_0000 | (imm12 << 10) | (rn << 5) | rd
}

/// `STR Xt, [Xn|SP, #offset]` (offset multiplo di 8, < 32 KiB)
pub const fn str_x(rt: u32, rn: u32, offset: u32) -> u32 {
    0xF900_0000 | ((offset / 8) << 10) | (rn << 5) | rt
}

/// `ORR Xd, XZR, Xm` (alias `MOV Xd, Xm`)
pub const fn mov_reg(rd: u32, rm: u32) -> u32 {
    0xAA00_03E0 | (rm << 16) | rd
}

/// `MRS Xt, NZCV`
pub const fn mrs_nzcv(rt: u32) -> u32 {
    0xD53B_4200 | rt
}

/// `MSR NZCV, Xt`
pub const fn msr_nzcv(rt: u32) -> u32 {
    0xD51B_4200 | rt
}

/// `LDP Qt, Qt2, [Xn, #offset]` (offset multiplo di 16, < 1024)
pub const fn ldp_q(rt: u32, rt2: u32, rn: u32, offset: u32) -> u32 {
    0xAD40_0000 | ((offset / 16) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `STR Qt, [Xn, #offset]` (offset multiplo di 16)
pub const fn str_q(rt: u32, rn: u32, offset: u32) -> u32 {
    0x3D80_0000 | ((offset / 16) << 10) | (rn << 5) | rt
}

pub const fn msr_fpcr(rt: u32) -> u32 {
    0xD51B_4400 | rt
}
pub const fn msr_fpsr(rt: u32) -> u32 {
    0xD51B_4420 | rt
}
pub const fn mrs_fpcr(rt: u32) -> u32 {
    0xD53B_4400 | rt
}
pub const fn mrs_fpsr(rt: u32) -> u32 {
    0xD53B_4420 | rt
}

/// `NOP`
pub const NOP: u32 = 0xD503_201F;

/// Carica un valore a 64 bit in `Xd` con MOVZ + MOVK (sempre 4 istruzioni).
pub fn mov64(rd: u32, value: u64) -> [u32; 4] {
    [
        movz(rd, value as u16, 0),
        movk(rd, (value >> 16) as u16, 1),
        movk(rd, (value >> 32) as u16, 2),
        movk(rd, (value >> 48) as u16, 3),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Calcolati a mano dalla spec; i test dell'oracolo li eseguono sotto QEMU.
    #[test]
    fn known_encodings() {
        assert_eq!(movz(0, 42, 0), 0xD280_0540); // mov x0, #42
        assert_eq!(movz(8, 93, 0), 0xD280_0BA8); // mov x8, #93
        assert_eq!(movk(1, 0x40, 1), 0xF2A0_0801); // movk x1, #0x40, lsl #16
        assert_eq!(svc(0), 0xD400_0001);
        assert_eq!(add_imm(SP, 0, 0), 0x9100_001F); // mov sp, x0
        assert_eq!(str_x(1, 28, 0x808), 0xF904_0781); // str x1, [x28, #2056]
        assert_eq!(mov_reg(3, 28), 0xAA1C_03E3); // mov x3, x28
        // Da tools/a64asm.sh:
        assert_eq!(ldp_q(2, 3, 0, 32), 0xad41_0c02); // ldp q2, q3, [x0, #32]
        assert_eq!(str_q(5, 28, 0xa00), 0x3d82_8385); // str q5, [x28, #0xa00]
        assert_eq!(msr_fpcr(0), 0xd51b_4400);
        assert_eq!(msr_fpsr(0), 0xd51b_4420);
        assert_eq!(mrs_fpcr(0), 0xd53b_4400);
        assert_eq!(mrs_fpsr(0), 0xd53b_4420);
    }
}
