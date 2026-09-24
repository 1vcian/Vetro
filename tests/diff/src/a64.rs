//! Codifiche AArch64 minime (Arm ARM, sezione C6). Servono solo a generare
//! programmi di test: la CPU vera sta in `vetro-cpu`.

/// Numeri di syscall Linux arm64 (asm-generic).
pub mod sys {
    pub const WRITE: u16 = 64;
    pub const EXIT: u16 = 93;
}

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

    // Valori di riferimento assemblati con GNU as.
    #[test]
    fn known_encodings() {
        assert_eq!(movz(0, 42, 0), 0xD280_0540); // mov x0, #42
        assert_eq!(movz(8, 93, 0), 0xD280_0BA8); // mov x8, #93
        assert_eq!(movk(1, 0x40, 1), 0xF2A0_0801); // movk x1, #0x40, lsl #16
        assert_eq!(svc(0), 0xD400_0001);
    }
}
