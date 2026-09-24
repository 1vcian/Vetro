//! ELF64 little-endian, AArch64, statico, un solo segmento PT_LOAD RX.
//!
//! Layout: header (64 B) + program header (56 B) + codice + dati, tutto
//! mappato a [`BASE`]. L'entry point è il primo byte del codice.

pub const BASE: u64 = 0x40_0000;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;
/// Offset del codice nel file, e quindi `BASE + CODE_OFFSET` è l'entry.
pub const CODE_OFFSET: u64 = EHDR_SIZE + PHDR_SIZE;
const EM_AARCH64: u16 = 183;

/// Indirizzo virtuale a cui finirà il byte `data_offset` dei dati, dato il
/// numero di istruzioni del codice. Serve per referenziare stringhe.
pub fn data_addr(code_len_insns: usize, data_offset: usize) -> u64 {
    BASE + CODE_OFFSET + (code_len_insns * 4 + data_offset) as u64
}

/// Costruisce l'immagine ELF completa.
pub fn build(code: &[u32], data: &[u8]) -> Vec<u8> {
    let entry = BASE + CODE_OFFSET;
    let total = CODE_OFFSET + (code.len() * 4 + data.len()) as u64;
    let mut out = Vec::with_capacity(total as usize);

    // e_ident
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    out.extend_from_slice(&EM_AARCH64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&entry.to_le_bytes());
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    debug_assert_eq!(out.len() as u64, EHDR_SIZE);

    // Program header: PT_LOAD, R+X, tutto il file.
    out.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    out.extend_from_slice(&5u32.to_le_bytes()); // p_flags = R|X
    out.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    out.extend_from_slice(&BASE.to_le_bytes()); // p_vaddr
    out.extend_from_slice(&BASE.to_le_bytes()); // p_paddr
    out.extend_from_slice(&total.to_le_bytes()); // p_filesz
    out.extend_from_slice(&total.to_le_bytes()); // p_memsz
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
    debug_assert_eq!(out.len() as u64, CODE_OFFSET);

    for insn in code {
        out.extend_from_slice(&insn.to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields() {
        let img = build(&[0xD400_0001], b"x");
        assert_eq!(&img[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes([img[18], img[19]]), EM_AARCH64);
        let entry = u64::from_le_bytes(img[24..32].try_into().unwrap());
        assert_eq!(entry, BASE + CODE_OFFSET);
        assert_eq!(img.len() as u64, CODE_OFFSET + 4 + 1);
        assert_eq!(data_addr(1, 0), BASE + CODE_OFFSET + 4);
    }
}
