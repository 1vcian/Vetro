//! ELF64 little-endian, AArch64, statico.
//!
//! Layout: header (64 B) + 2 program header (2 × 56 B) + codice + dati, tutto
//! in un PT_LOAD RX a [`BASE`]; poi, facoltativo, un PT_LOAD RW a
//! [`RW_BASE`] che inizia a un offset di file allineato alla pagina. Se il
//! segmento RW manca, il secondo program header è PT_NULL.

pub const BASE: u64 = 0x40_0000;
pub const RW_BASE: u64 = 0x80_0000;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;
const PHNUM: u64 = 2;
/// Offset del codice nel file: `BASE + CODE_OFFSET` è l'entry point.
pub const CODE_OFFSET: u64 = EHDR_SIZE + PHNUM * PHDR_SIZE;
const EM_AARCH64: u16 = 183;
const PAGE: u64 = 0x1000;

/// Indirizzo del byte `data_offset` dei dati, dato il numero di istruzioni.
pub fn data_addr(code_len_insns: usize, data_offset: usize) -> u64 {
    BASE + CODE_OFFSET + (code_len_insns * 4 + data_offset) as u64
}

/// Indirizzo dell'istruzione di indice `i`.
pub fn insn_addr(i: usize) -> u64 {
    BASE + CODE_OFFSET + i as u64 * 4
}

/// Segmento RW: contenuto iniziale e dimensione totale in memoria (il resto
/// è azzerato, come una bss).
pub struct Rw<'a> {
    pub init: &'a [u8],
    pub memsz: u64,
}

pub fn build(code: &[u32], data: &[u8]) -> Vec<u8> {
    build_with_rw(code, data, None)
}

pub fn build_with_rw(code: &[u32], data: &[u8], rw: Option<Rw<'_>>) -> Vec<u8> {
    let entry = BASE + CODE_OFFSET;
    let text_len = CODE_OFFSET + (code.len() * 4 + data.len()) as u64;
    let mut out = Vec::new();

    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    out.extend_from_slice(&EM_AARCH64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&entry.to_le_bytes());
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHNUM as u16).to_le_bytes());
    out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    let phdr = |out: &mut Vec<u8>, ty: u32, flags: u32, off: u64, vaddr: u64, filesz: u64, memsz: u64| {
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&vaddr.to_le_bytes());
        out.extend_from_slice(&vaddr.to_le_bytes());
        out.extend_from_slice(&filesz.to_le_bytes());
        out.extend_from_slice(&memsz.to_le_bytes());
        out.extend_from_slice(&PAGE.to_le_bytes());
    };
    phdr(&mut out, 1, 5, 0, BASE, text_len, text_len); // PT_LOAD R|X
    let rw_off = text_len.next_multiple_of(PAGE);
    match &rw {
        Some(rw) => phdr(&mut out, 1, 6, rw_off, RW_BASE, rw.init.len() as u64, rw.memsz),
        None => phdr(&mut out, 0, 0, 0, 0, 0, 0), // PT_NULL
    }
    debug_assert_eq!(out.len() as u64, CODE_OFFSET);

    for insn in code {
        out.extend_from_slice(&insn.to_le_bytes());
    }
    out.extend_from_slice(data);
    if let Some(rw) = rw {
        out.resize(rw_off as usize, 0);
        out.extend_from_slice(rw.init);
    }
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

    #[test]
    fn rw_segment_is_page_aligned() {
        let img = build_with_rw(&[0; 3], &[], Some(Rw { init: &[7; 16], memsz: 0x2000 }));
        assert_eq!(img.len(), 0x1000 + 16);
        assert_eq!(img[0x1000], 7);
    }
}
