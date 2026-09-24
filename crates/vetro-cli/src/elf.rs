//! Caricatore di ELF64 AArch64 statici (ET_EXEC).
//!
//! Mappa i PT_LOAD a granularità di pagina come fa Linux: la parte di pagina
//! oltre `p_filesz` contiene i byte successivi del file, tranne quando il
//! segmento ha una bss (`p_memsz > p_filesz`), che viene azzerata.

use vetro_cpu::{Perm, UserMemory};

pub const PAGE: u64 = 0x1000;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    NotElf,
    Unsupported(&'static str),
    Truncated,
    Overlap(u64),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotElf => write!(f, "non è un file ELF"),
            LoadError::Unsupported(what) => write!(f, "ELF non supportato: {what}"),
            LoadError::Truncated => write!(f, "ELF troncato"),
            LoadError::Overlap(a) => write!(f, "segmenti sovrapposti a {a:#x}"),
        }
    }
}

impl std::error::Error for LoadError {}

pub struct Loaded {
    pub entry: u64,
    pub phdr_addr: u64,
    pub phnum: u16,
    /// Fine dell'ultimo segmento caricato (inizio del brk).
    pub end: u64,
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, LoadError> {
    Ok(u16::from_le_bytes(b.get(o..o + 2).ok_or(LoadError::Truncated)?.try_into().unwrap()))
}
fn u32_at(b: &[u8], o: usize) -> Result<u32, LoadError> {
    Ok(u32::from_le_bytes(b.get(o..o + 4).ok_or(LoadError::Truncated)?.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> Result<u64, LoadError> {
    Ok(u64::from_le_bytes(b.get(o..o + 8).ok_or(LoadError::Truncated)?.try_into().unwrap()))
}

pub fn load(image: &[u8], mem: &mut UserMemory) -> Result<Loaded, LoadError> {
    if image.get(..4) != Some(b"\x7fELF") {
        return Err(LoadError::NotElf);
    }
    if image[4] != 2 || image[5] != 1 {
        return Err(LoadError::Unsupported("serve ELF64 little-endian"));
    }
    if u16_at(image, 18)? != EM_AARCH64 {
        return Err(LoadError::Unsupported("e_machine non è AArch64"));
    }
    match u16_at(image, 16)? {
        2 => {}
        3 => return Err(LoadError::Unsupported("ET_DYN (PIE/dinamico) arriva con M2")),
        _ => return Err(LoadError::Unsupported("e_type")),
    }
    let entry = u64_at(image, 24)?;
    let phoff = u64_at(image, 32)? as usize;
    let phentsize = u16_at(image, 54)? as usize;
    let phnum = u16_at(image, 56)?;
    let mut phdr_addr = 0;
    let mut load_end = 0;

    for i in 0..phnum as usize {
        let ph = phoff + i * phentsize;
        if u32_at(image, ph)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(image, ph + 4)?;
        let offset = u64_at(image, ph + 8)?;
        let vaddr = u64_at(image, ph + 16)?;
        let filesz = u64_at(image, ph + 32)?;
        let memsz = u64_at(image, ph + 40)?;
        if memsz == 0 {
            continue;
        }
        if (vaddr ^ offset) % PAGE != 0 {
            return Err(LoadError::Unsupported("p_offset e p_vaddr non congruenti"));
        }
        if offset.checked_add(filesz).is_none_or(|e| e > image.len() as u64) {
            return Err(LoadError::Truncated);
        }
        let start = vaddr & !(PAGE - 1);
        let end = (vaddr + memsz).next_multiple_of(PAGE);
        let file_start = offset - (vaddr - start);
        let seg_file_end = (vaddr - start) + filesz; // relativo a `start`
        let has_bss = memsz > filesz;
        let mut data = vec![0u8; (end - start) as usize];
        for (i, byte) in data.iter_mut().enumerate() {
            let i = i as u64;
            if (i < seg_file_end || !has_bss)
                && let Some(&b) = image.get((file_start + i) as usize)
            {
                *byte = b;
            }
        }
        let perm = Perm { read: flags & 4 != 0, write: flags & 2 != 0, exec: flags & 1 != 0 };
        mem.map(start, data, perm).map_err(|e| LoadError::Overlap(e.base))?;
        load_end = load_end.max(vaddr + memsz);
        if offset <= phoff as u64 && (phoff as u64) < offset + filesz {
            phdr_addr = vaddr + (phoff as u64 - offset);
        }
    }
    Ok(Loaded { entry, phdr_addr, phnum, end: load_end })
}
