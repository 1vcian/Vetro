//! LZ77 semplice per blocchi piccoli (una pagina da 4 KiB), senza
//! dipendenze.
//!
//! Il blocco compresso è una sequenza di gettoni, ciascuno un intero
//! LEB128 `t`:
//! - `t` pari: `t >> 1` byte letterali (almeno 1), che seguono;
//! - `t` dispari: una copia di `(t >> 1) + MIN_MATCH` byte da `d` byte
//!   indietro nell'uscita, con `d` (LEB128, almeno 1) subito dopo. La
//!   copia può sovrapporsi a sé stessa: con `d = 1` è una ripetizione (RLE).
//!
//! Il compressore è goloso, con una tabella di hash di 4 byte: sceglie
//! sempre la stessa codifica per gli stessi byte (niente dipende da
//! indirizzi o da blocchi precedenti), quindi due salvataggi dello stesso
//! stato danno gli stessi byte.

use crate::{Error, Result};

/// Copia più corta che conviene codificare.
pub const MIN_MATCH: usize = 4;
const HASH_BITS: u32 = 12;

/// Tabella di hash riutilizzabile fra un blocco e l'altro (non cambia il
/// risultato: le voci di un blocco precedente non valgono per il
/// successivo).
pub struct Table {
    slots: Vec<u32>,
    /// Le voci valide del blocco corrente valgono almeno `base`.
    base: u32,
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    pub fn new() -> Self {
        Table { slots: vec![0; 1 << HASH_BITS], base: 1 }
    }

    /// Inizio di un blocco lungo `len`: le voci precedenti non valgono più.
    fn start(&mut self, len: usize) -> u32 {
        let next = u64::from(self.base) + len as u64 + 1;
        if next >= u64::from(u32::MAX) {
            self.slots.fill(0);
            self.base = 1;
        }
        let b = self.base;
        self.base += len as u32 + 1;
        b
    }
}

fn hash(v: u32) -> usize {
    (v.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(src: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *src.get(*pos).ok_or(Error::Truncated)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::invalid("intero LEB128 troppo lungo"))
}

fn literals(out: &mut Vec<u8>, lit: &[u8]) {
    if !lit.is_empty() {
        put_varint(out, (lit.len() as u64) << 1);
        out.extend_from_slice(lit);
    }
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("4 byte"))
}

/// Comprime `src` accodando a `out`.
pub fn compress(src: &[u8], out: &mut Vec<u8>, t: &mut Table) {
    let base = t.start(src.len());
    let n = src.len();
    let mut lit_start = 0usize;
    let mut i = 0usize;
    while i + MIN_MATCH <= n {
        let h = hash(word(src, i));
        let cand = t.slots[h];
        t.slots[h] = base + i as u32;
        if cand >= base {
            let c = (cand - base) as usize;
            if c < i && word(src, c) == word(src, i) {
                let mut len = MIN_MATCH;
                while i + len < n && src[c + len] == src[i + len] {
                    len += 1;
                }
                literals(out, &src[lit_start..i]);
                put_varint(out, ((len - MIN_MATCH) as u64) << 1 | 1);
                put_varint(out, (i - c) as u64);
                i += len;
                lit_start = i;
                continue;
            }
        }
        i += 1;
    }
    literals(out, &src[lit_start..]);
}

/// Decomprime `src` in `dst`, che deve risultare riempito esattamente.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<()> {
    let bad = |what: &str| Error::invalid(format!("blocco compresso: {what}"));
    let (mut p, mut o) = (0usize, 0usize);
    while p < src.len() {
        let t = get_varint(src, &mut p)?;
        if t & 1 == 0 {
            let len = (t >> 1) as usize;
            if len == 0 || len > dst.len() - o || len > src.len() - p {
                return Err(bad("letterali oltre il blocco"));
            }
            dst[o..o + len].copy_from_slice(&src[p..p + len]);
            p += len;
            o += len;
        } else {
            let len = (t >> 1).saturating_add(MIN_MATCH as u64);
            let d = get_varint(src, &mut p)?;
            if d == 0 || d > o as u64 || len > (dst.len() - o) as u64 {
                return Err(bad("copia fuori dal blocco"));
            }
            let (len, d) = (len as usize, d as usize);
            if d >= len {
                dst.copy_within(o - d..o - d + len, o);
            } else {
                for k in 0..len {
                    dst[o + k] = dst[o + k - d];
                }
            }
            o += len;
        }
    }
    if o != dst.len() {
        return Err(bad("più corto del previsto"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &[u8]) -> usize {
        let mut t = Table::new();
        let mut c = Vec::new();
        compress(src, &mut c, &mut t);
        let mut d = vec![0u8; src.len()];
        decompress(&c, &mut d).unwrap();
        assert_eq!(d, src);
        // Deterministico e indipendente dalla storia della tabella.
        let mut c2 = Vec::new();
        compress(src, &mut c2, &mut t);
        assert_eq!(c, c2);
        c.len()
    }

    #[test]
    fn andata_e_ritorno() {
        assert_eq!(roundtrip(&[]), 0);
        roundtrip(b"abc");
        assert!(roundtrip(&[7u8; 4096]) < 16, "una ripetizione è un gettone");
        let text: Vec<u8> = b"la macchina e' deterministica. ".iter().cycle().take(4096).copied().collect();
        assert!(roundtrip(&text) < 100);
        let noise: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        roundtrip(&noise);
        let mut mixed = noise.clone();
        mixed[1000..3000].fill(0);
        assert!(roundtrip(&mixed) < 2500);
    }

    #[test]
    fn rifiuta_blocchi_rovinati() {
        let mut d = [0u8; 8];
        assert!(decompress(&[0x08, 1, 2, 3], &mut d).is_err(), "letterali oltre l'ingresso");
        assert!(decompress(&[0x03, 0x01], &mut d).is_err(), "copia prima dell'inizio");
        assert!(decompress(&[0x02, 1], &mut d).is_err(), "troppo corto");
        assert!(decompress(&[0x80], &mut d).is_err(), "intero troncato");
    }
}
