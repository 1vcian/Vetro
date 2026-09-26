//! ELF a 64 bit little endian: simboli dai byte del file (`.symtab`,
//! `.dynsym`) o dall'immagine caricata in memoria (tabella dinamica
//! trovata da `PT_DYNAMIC`, numero di simboli da `DT_HASH` o
//! `DT_GNU_HASH`). Serve a risolvere i nomi delle funzioni dello spazio
//! utente (libc, BoringSSL, libbinder, libart, programmi statici non
//! strippati) per i punti di aggancio.

/// Un simbolo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfSym {
    pub name: String,
    /// Valore: indirizzo virtuale del file, o già spostato per la memoria.
    pub value: u64,
    pub size: u64,
    /// `STT_*` (2 = funzione, 1 = oggetto).
    pub kind: u8,
    /// `STB_*` (0 locale, 1 globale, 2 debole).
    pub bind: u8,
    /// Indice della sezione (0 = non definito).
    pub shndx: u16,
}

pub const STT_FUNC: u8 = 2;
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;

/// Intestazione ELF e programma.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfHeader {
    pub e_type: u16,
    pub machine: u16,
    pub entry: u64,
    pub phoff: u64,
    pub shoff: u64,
    pub phnum: u16,
    pub shnum: u16,
    pub shstrndx: u16,
}

/// Una voce della tabella dei programmi.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phdr {
    pub p_type: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

fn cstr(b: &[u8], o: usize) -> String {
    let s = b.get(o..).unwrap_or(&[]);
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

/// Legge l'intestazione ELF dai primi 64 byte.
pub fn header(b: &[u8]) -> Option<ElfHeader> {
    if b.get(..4)? != b"\x7fELF" || *b.get(4)? != 2 || *b.get(5)? != 1 {
        return None;
    }
    Some(ElfHeader {
        e_type: u16_at(b, 16)?,
        machine: u16_at(b, 18)?,
        entry: u64_at(b, 24)?,
        phoff: u64_at(b, 32)?,
        shoff: u64_at(b, 40)?,
        phnum: u16_at(b, 56)?,
        shnum: u16_at(b, 60)?,
        shstrndx: u16_at(b, 62)?,
    })
}

/// Una voce di 56 byte della tabella dei programmi.
pub fn phdr(b: &[u8]) -> Option<Phdr> {
    Some(Phdr {
        p_type: u32_at(b, 0)?,
        flags: u32_at(b, 4)?,
        offset: u64_at(b, 8)?,
        vaddr: u64_at(b, 16)?,
        filesz: u64_at(b, 32)?,
        memsz: u64_at(b, 40)?,
        align: u64_at(b, 48)?,
    })
}

/// Le voci di 24 byte di una tabella dei simboli con le sue stringhe.
fn parse_syms(tab: &[u8], strs: &[u8], bias: u64) -> Vec<ElfSym> {
    tab.as_chunks::<24>()
        .0
        .iter()
        .filter_map(|e| {
            let name = u32_at(e, 0)? as usize;
            let info = e[4];
            let shndx = u16_at(e, 6)?;
            let value = u64_at(e, 8)?;
            let n = cstr(strs, name);
            if n.is_empty() {
                return None;
            }
            Some(ElfSym {
                name: n,
                value: if shndx != 0 { value.wrapping_add(bias) } else { value },
                size: u64_at(e, 16)?,
                kind: info & 0xf,
                bind: info >> 4,
                shndx,
            })
        })
        .collect()
}

/// Simboli dai byte del file: `.symtab` e `.dynsym`, con i valori del
/// file (indirizzi virtuali di collegamento).
pub fn file_symbols(b: &[u8]) -> Vec<ElfSym> {
    let Some(h) = header(b) else { return Vec::new() };
    let mut out = Vec::new();
    let sh = |i: u64| -> Option<(u32, u64, u64, u32)> {
        let o = usize::try_from(h.shoff.checked_add(i.checked_mul(64)?)?).ok()?;
        Some((u32_at(b, o + 4)?, u64_at(b, o + 24)?, u64_at(b, o + 32)?, u32_at(b, o + 40)?))
    };
    let slice = |off: u64, size: u64| -> Option<&[u8]> {
        let s = usize::try_from(off).ok()?;
        b.get(s..s.checked_add(usize::try_from(size).ok()?)?)
    };
    for i in 0..u64::from(h.shnum) {
        let Some((ty, off, size, link)) = sh(i) else { continue };
        if ty != 2 && ty != 11 {
            continue;
        }
        let Some((_, soff, ssize, _)) = sh(u64::from(link)) else { continue };
        if let (Some(tab), Some(strs)) = (slice(off, size), slice(soff, ssize)) {
            out.extend(parse_syms(tab, strs, 0));
        }
    }
    out
}

/// Lettura della memoria di un processo: `read(va, buf)`.
pub trait VirtRead {
    fn read_virt(&self, va: u64, buf: &mut [u8]) -> bool;
}

impl<F: Fn(u64, &mut [u8]) -> bool> VirtRead for F {
    fn read_virt(&self, va: u64, buf: &mut [u8]) -> bool {
        self(va, buf)
    }
}

fn rd(m: &impl VirtRead, va: u64, len: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; len];
    m.read_virt(va, &mut b).then_some(b)
}

/// Spostamento di un'immagine caricata a `base` (indirizzo della prima
/// pagina, con l'intestazione): 0 per `ET_EXEC`, altrimenti `base` meno
/// l'indirizzo virtuale del primo `PT_LOAD` (arrotondato alla pagina).
pub fn load_bias(h: &ElfHeader, phdrs: &[Phdr], base: u64) -> u64 {
    if h.e_type == ET_EXEC {
        return 0;
    }
    let first = phdrs.iter().filter(|p| p.p_type == PT_LOAD).map(|p| p.vaddr & !0xfff).min().unwrap_or(0);
    base.wrapping_sub(first)
}

/// Intestazione e programma di un'immagine in memoria a `base`.
pub fn loaded_headers(m: &impl VirtRead, base: u64) -> Option<(ElfHeader, Vec<Phdr>)> {
    let h = header(&rd(m, base, 64)?)?;
    let n = usize::from(h.phnum).min(256);
    let ph = rd(m, base.checked_add(h.phoff)?, n * 56)?;
    let phdrs = ph.as_chunks::<56>().0.iter().filter_map(|c| phdr(c)).collect();
    Some((h, phdrs))
}

/// Simboli dinamici di un'immagine caricata a `base` (valori già
/// spostati: indirizzi del processo).
pub fn dynamic_symbols(m: &impl VirtRead, base: u64) -> Vec<ElfSym> {
    dynamic_symbols_inner(m, base).unwrap_or_default()
}

fn dynamic_symbols_inner(m: &impl VirtRead, base: u64) -> Option<Vec<ElfSym>> {
    let (h, phdrs) = loaded_headers(m, base)?;
    let bias = load_bias(&h, &phdrs, base);
    let dynph = phdrs.iter().find(|p| p.p_type == PT_DYNAMIC)?;
    let n = usize::try_from(dynph.memsz / 16).ok()?.min(4096);
    let dynb = rd(m, bias.wrapping_add(dynph.vaddr), n * 16)?;
    let (mut symtab, mut strtab, mut strsz, mut hash, mut gnu) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for e in dynb.as_chunks::<16>().0 {
        let tag = u64_at(e, 0)?;
        let val = u64_at(e, 8)?;
        // Il linker può aver già spostato i puntatori (glibc lo fa).
        let ptr = if bias != 0 && val >= bias { val } else { val.wrapping_add(bias) };
        match tag {
            0 => break,
            4 => hash = ptr,
            5 => strtab = ptr,
            6 => symtab = ptr,
            10 => strsz = val,
            0x6fff_fef5 => gnu = ptr,
            _ => {}
        }
    }
    if symtab == 0 || strtab == 0 {
        return None;
    }
    let count = if hash != 0 {
        u64::from(u32::from_le_bytes(rd(m, hash.wrapping_add(4), 4)?.try_into().ok()?))
    } else if gnu != 0 {
        gnu_hash_count(m, gnu)?
    } else {
        return None;
    };
    let count = count.min(1 << 20) as usize;
    let tab = rd(m, symtab, count * 24)?;
    let strs = rd(m, strtab, strsz.min(1 << 24) as usize)?;
    Some(parse_syms(&tab, &strs, bias))
}

/// Numero di simboli da una tabella `DT_GNU_HASH`: il massimo indice
/// raggiunto dalle catene.
fn gnu_hash_count(m: &impl VirtRead, at: u64) -> Option<u64> {
    let hdr = rd(m, at, 16)?;
    let nbuckets = u64::from(u32_at(&hdr, 0)?);
    let symoffset = u64::from(u32_at(&hdr, 4)?);
    let bloom = u64::from(u32_at(&hdr, 8)?);
    let buckets_at = at.wrapping_add(16).wrapping_add(bloom.wrapping_mul(8));
    let buckets = rd(m, buckets_at, (nbuckets.min(1 << 20) * 4) as usize)?;
    let last = buckets.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).max().unwrap_or(0);
    if u64::from(last) < symoffset {
        return Some(symoffset);
    }
    let chains = buckets_at.wrapping_add(nbuckets.wrapping_mul(4));
    let mut i = u64::from(last);
    for _ in 0..(1 << 20) {
        let v = u32::from_le_bytes(
            rd(m, chains.wrapping_add((i - symoffset).wrapping_mul(4)), 4)?.try_into().ok()?,
        );
        i += 1;
        if v & 1 != 0 {
            return Some(i);
        }
    }
    None
}

/// Cerca una funzione per nome (prima i simboli definiti).
pub fn find<'a>(syms: &'a [ElfSym], name: &str) -> Option<&'a ElfSym> {
    syms.iter().find(|s| s.name == name && s.shndx != 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Un ELF minimo con .symtab (e .strtab) per i test.
    pub(crate) fn tiny_elf(e_type: u16, syms: &[(&str, u64)]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24];
        for (n, v) in syms {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
            let mut e = vec![0u8; 24];
            e[0..4].copy_from_slice(&off.to_le_bytes());
            e[4] = 0x12; // globale, funzione
            e[6..8].copy_from_slice(&1u16.to_le_bytes());
            e[8..16].copy_from_slice(&v.to_le_bytes());
            e[16..24].copy_from_slice(&4u64.to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&e_type.to_le_bytes());
        b[18..20].copy_from_slice(&183u16.to_le_bytes());
        b[24..32].copy_from_slice(&0x40_0100u64.to_le_bytes());
        let sym_off = b.len() as u64;
        b.extend_from_slice(&symtab);
        let str_off = b.len() as u64;
        b.extend_from_slice(&strtab);
        while !b.len().is_multiple_of(8) {
            b.push(0);
        }
        let shoff = b.len() as u64;
        b[40..48].copy_from_slice(&shoff.to_le_bytes());
        b[60..62].copy_from_slice(&3u16.to_le_bytes());
        let sh = |ty: u32, off: u64, size: u64, link: u32| {
            let mut s = vec![0u8; 64];
            s[4..8].copy_from_slice(&ty.to_le_bytes());
            s[24..32].copy_from_slice(&off.to_le_bytes());
            s[32..40].copy_from_slice(&size.to_le_bytes());
            s[40..44].copy_from_slice(&link.to_le_bytes());
            s
        };
        b.extend_from_slice(&sh(0, 0, 0, 0));
        b.extend_from_slice(&sh(2, sym_off, symtab.len() as u64, 2));
        b.extend_from_slice(&sh(3, str_off, strtab.len() as u64, 0));
        b
    }

    #[test]
    fn simboli_dal_file() {
        let b = tiny_elf(ET_EXEC, &[("write", 0x40_1000), ("main", 0x40_2000)]);
        let h = header(&b).unwrap();
        assert_eq!((h.e_type, h.machine, h.entry), (ET_EXEC, 183, 0x40_0100));
        let s = file_symbols(&b);
        assert_eq!(find(&s, "write").unwrap().value, 0x40_1000);
        assert_eq!(find(&s, "main").unwrap().kind, STT_FUNC);
        assert!(find(&s, "nope").is_none());
        for cut in 0..b.len() {
            let _ = file_symbols(&b[..cut]);
        }
    }

    /// Un'immagine caricata con PT_DYNAMIC, DT_GNU_HASH e simboli
    /// dinamici, letta dalla "memoria" del processo.
    #[test]
    fn simboli_dinamici_dalla_memoria() {
        let base = 0x7f00_0000_0000u64;
        let mut img = vec![0u8; 0x3000];
        img[..4].copy_from_slice(b"\x7fELF");
        img[4] = 2;
        img[5] = 1;
        img[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
        img[32..40].copy_from_slice(&64u64.to_le_bytes());
        img[56..58].copy_from_slice(&2u16.to_le_bytes());
        let ph = |ty: u32, vaddr: u64, memsz: u64| {
            let mut p = vec![0u8; 56];
            p[0..4].copy_from_slice(&ty.to_le_bytes());
            p[16..24].copy_from_slice(&vaddr.to_le_bytes());
            p[40..48].copy_from_slice(&memsz.to_le_bytes());
            p
        };
        img[64..120].copy_from_slice(&ph(PT_LOAD, 0, 0x3000));
        img[120..176].copy_from_slice(&ph(PT_DYNAMIC, 0x1000, 5 * 16));
        // .dynamic: GNU_HASH 0x1100, STRTAB 0x1200, SYMTAB 0x1300, STRSZ, NULL
        let dy: [(u64, u64); 5] = [(0x6fff_fef5, 0x1100), (5, 0x1200), (6, 0x1300), (10, 32), (0, 0)];
        for (i, (t, v)) in dy.iter().enumerate() {
            img[0x1000 + 16 * i..0x1008 + 16 * i].copy_from_slice(&t.to_le_bytes());
            img[0x1008 + 16 * i..0x1010 + 16 * i].copy_from_slice(&v.to_le_bytes());
        }
        // GNU_HASH: 1 bucket, symoffset 1, 1 parola di bloom, shift 0;
        // bucket[0] = 1; catene: simbolo 1 (continua), simbolo 2 (fine).
        let gh: [u32; 4] = [1, 1, 1, 0];
        for (i, v) in gh.iter().enumerate() {
            img[0x1100 + 4 * i..0x1104 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        img[0x1118..0x111c].copy_from_slice(&1u32.to_le_bytes());
        img[0x111c..0x1120].copy_from_slice(&0u32.to_le_bytes());
        img[0x1120..0x1124].copy_from_slice(&1u32.to_le_bytes());
        img[0x1200..0x1212].copy_from_slice(b"\0SSL_write\0SSL_rd\0");
        for (i, (name, v)) in [(1u32, 0x2000u64), (11, 0x2100)].iter().enumerate() {
            let e = 0x1300 + 24 * (i + 1);
            img[e..e + 4].copy_from_slice(&name.to_le_bytes());
            img[e + 4] = 0x12;
            img[e + 6..e + 8].copy_from_slice(&9u16.to_le_bytes());
            img[e + 8..e + 16].copy_from_slice(&v.to_le_bytes());
        }
        let mem = |va: u64, buf: &mut [u8]| -> bool {
            let Some(o) = va.checked_sub(base) else { return false };
            match img.get(o as usize..o as usize + buf.len()) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            }
        };
        let syms = dynamic_symbols(&mem, base);
        assert_eq!(syms.len(), 2, "{syms:?}");
        assert_eq!(find(&syms, "SSL_write").unwrap().value, base + 0x2000);
        assert_eq!(find(&syms, "SSL_rd").unwrap().value, base + 0x2100);
    }
}
