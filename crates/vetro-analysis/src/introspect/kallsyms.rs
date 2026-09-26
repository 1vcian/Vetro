//! Simboli del kernel: `System.map` e la tabella kallsyms dentro l'`Image`.
//!
//! Il kernel porta i propri simboli compressi (`CONFIG_KALLSYMS`, con
//! `CONFIG_KALLSYMS_ALL` anche i dati): `kallsyms_num_syms`,
//! `kallsyms_names` (per simbolo un byte di lunghezza, due se oltre 127,
//! poi indici di token), `kallsyms_markers` (ogni 256 simboli l'offset nei
//! nomi), `kallsyms_token_table` (256 stringhe terminate da zero),
//! `kallsyms_token_index` (256 u16), `kallsyms_offsets` (u32 da
//! `kallsyms_relative_base`) e `kallsyms_relative_base`. Nessun simbolo ne
//! indica la posizione: si trova la tabella dei token dalle cifre `0`..`9`
//! (token di se stesse), la si verifica con l'indice, e da lì il resto
//! (`scripts/kallsyms.c`). Gli indirizzi sono quelli di collegamento
//! (senza KASLR): lo spostamento lo ricava [`super::linux`].

use std::collections::BTreeMap;

/// Un simbolo del kernel.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KSym {
    pub addr: u64,
    /// Tipo di `nm` (`T`, `t`, `D`, `d`, `B`, `R`, ...).
    pub kind: char,
    pub name: String,
}

/// Tabella dei simboli del kernel: nome → indirizzo e indirizzo → nome.
#[derive(Clone, Debug, Default)]
pub struct Symbols {
    by_name: BTreeMap<String, u64>,
    /// Ordinati per indirizzo.
    sorted: Vec<KSym>,
}

impl Symbols {
    pub fn from_syms(mut syms: Vec<KSym>) -> Symbols {
        syms.sort();
        let mut by_name = BTreeMap::new();
        for s in &syms {
            // A parità di nome (simboli locali ripetuti) vale il primo.
            by_name.entry(s.name.clone()).or_insert(s.addr);
        }
        Symbols { by_name, sorted: syms }
    }

    /// Legge un `System.map` (`indirizzo tipo nome` per riga).
    pub fn parse_system_map(text: &str) -> Symbols {
        let syms = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let addr = u64::from_str_radix(it.next()?, 16).ok()?;
                let kind = it.next()?.chars().next()?;
                let name = it.next()?.to_string();
                Some(KSym { addr, kind, name })
            })
            .collect();
        Symbols::from_syms(syms)
    }

    /// Estrae la tabella kallsyms da un `Image` arm64.
    pub fn from_image(image: &[u8]) -> Result<Symbols, KallsymsError> {
        Ok(Symbols::from_syms(extract(image)?))
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        self.by_name.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &KSym> {
        self.sorted.iter()
    }

    /// Il simbolo che contiene `addr` (l'ultimo che non lo supera) e lo
    /// scostamento.
    pub fn lookup(&self, addr: u64) -> Option<(&KSym, u64)> {
        let i = self.sorted.partition_point(|s| s.addr <= addr);
        let s = self.sorted.get(i.checked_sub(1)?)?;
        Some((s, addr - s.addr))
    }
}

/// Perché la tabella kallsyms non si è trovata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KallsymsError {
    TokenTable,
    Markers,
    Names,
    Offsets,
    Base,
}

impl core::fmt::Display for KallsymsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self {
            KallsymsError::TokenTable => "tabella dei token",
            KallsymsError::Markers => "marcatori",
            KallsymsError::Names => "nomi",
            KallsymsError::Offsets => "offset",
            KallsymsError::Base => "base relativa",
        };
        write!(f, "kallsyms: {what} non trovati")
    }
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

fn align(x: usize, a: usize) -> usize {
    x.div_ceil(a) * a
}

/// Tabella dei token: posizione, 256 offset, fine dell'indice.
struct Tokens {
    table: usize,
    offs: [u16; 256],
    index_end: usize,
}

impl Tokens {
    fn token<'a>(&self, image: &'a [u8], t: u8) -> &'a [u8] {
        let s = self.table + usize::from(self.offs[usize::from(t)]);
        let rest = &image[s..];
        &rest[..rest.iter().position(|&c| c == 0).unwrap_or(rest.len())]
    }
}

/// Le 256 stringhe da `t`, se `t + offs[i]` le delimita tutte: offsets e
/// fine.
fn strings_at(image: &[u8], t: usize) -> Option<([u16; 256], usize)> {
    let mut offs = [0u16; 256];
    let mut p = t;
    for o in offs.iter_mut() {
        *o = u16::try_from(p - t).ok()?;
        let len = image.get(p..)?.iter().take(256).position(|&c| c == 0)?;
        if image[p..p + len].iter().any(|&c| !(0x20..0x7f).contains(&c)) {
            return None;
        }
        p += len + 1;
    }
    Some((offs, p))
}

fn find_tokens(image: &[u8]) -> Option<Tokens> {
    const DIGITS: &[u8] = b"0\x001\x002\x003\x004\x005\x006\x007\x008\x009\x00";
    let mut from = 0;
    while let Some(p) = image.get(from..)?.windows(DIGITS.len()).position(|w| w == DIGITS) {
        let d = from + p;
        from = d + 1;
        // Il token di '0' (0x30) è il 48°: la tabella inizia al più 48
        // token lunghi prima.
        for t in (d.saturating_sub(48 * 64)..=d).rev() {
            let Some((offs, end)) = strings_at(image, t) else { continue };
            if t + usize::from(offs[0x30]) != d {
                continue;
            }
            // L'indice segue la tabella, allineato.
            for idx in [end, align(end, 2), align(end, 4), align(end, 8)] {
                if (0..256).all(|i| u16_at(image, idx + 2 * i) == Some(offs[i])) {
                    return Some(Tokens { table: t, offs, index_end: idx + 512 });
                }
            }
        }
    }
    None
}

/// Legge la lunghezza di un nome a `p`: (lunghezza, byte dell'intestazione).
fn name_len(image: &[u8], p: usize) -> Option<(usize, usize)> {
    let b = *image.get(p)?;
    if b & 0x80 != 0 {
        let b2 = *image.get(p + 1)?;
        Some((usize::from(b & 0x7f) | usize::from(b2) << 7, 2))
    } else {
        Some((usize::from(b), 1))
    }
}

/// Candidati per i marcatori: posizione e valori, cercando all'indietro
/// dalla tabella dei token (u32 crescenti dal primo, che vale 0), con o
/// senza 4 byte di allineamento prima della tabella.
fn find_markers(image: &[u8], table: usize) -> Vec<(usize, Vec<u32>)> {
    let mut out = Vec::new();
    for pad in [0usize, 4] {
        let Some(mut k) = table.checked_sub(pad + 4) else { continue };
        let mut vals = Vec::new();
        loop {
            let Some(v) = u32_at(image, k) else { break };
            if let Some(&last) = vals.last()
                && v >= last
            {
                break;
            }
            vals.push(v);
            if v == 0 {
                vals.reverse();
                out.push((k, vals));
                break;
            }
            let Some(nk) = k.checked_sub(4) else { break };
            k = nk;
        }
    }
    out
}

/// Cammina `n` nomi da `s`; restituisce la fine e gli offset ogni 256.
fn walk_names(image: &[u8], s: usize, n: usize, markers: &[u32]) -> Option<usize> {
    let mut p = s;
    for i in 0..n {
        if i % 256 == 0 && markers.get(i / 256).map(|&m| m as usize) != Some(p - s) {
            return None;
        }
        let (len, h) = name_len(image, p)?;
        if len == 0 {
            return None;
        }
        p += h + len;
    }
    Some(p)
}

/// `kallsyms_num_syms` e i nomi prima dei marcatori a `mpos`: posizione
/// di num_syms, inizio dei nomi, numero di simboli.
fn find_names(image: &[u8], mpos: usize, markers: &[u32]) -> Option<(usize, usize, usize)> {
    let nm = markers.len();
    let last = *markers.last()? as usize;
    // num_syms è un u32 subito prima dei nomi (allineati a 8).
    let hi = mpos.checked_sub(last)?;
    let lo = hi.saturating_sub(256 * 260 + 16);
    for p in (lo..hi).rev() {
        if p % 4 != 0 {
            continue;
        }
        let Some(n) = u32_at(image, p) else { continue };
        let n = n as usize;
        if n == 0 || n > 256 * nm || n <= 256 * (nm - 1) {
            continue;
        }
        for s in [p + 4, align(p + 4, 8)] {
            if let Some(end) = walk_names(image, s, n, markers)
                && end <= mpos
                && mpos - end < 8
            {
                return Some((p, s, n));
            }
        }
    }
    None
}

/// La tabella kallsyms di un `Image`: simboli con gli indirizzi di
/// collegamento, in ordine di indirizzo.
pub fn extract(image: &[u8]) -> Result<Vec<KSym>, KallsymsError> {
    let tok = find_tokens(image).ok_or(KallsymsError::TokenTable)?;
    let cands = find_markers(image, tok.table);
    if cands.is_empty() {
        return Err(KallsymsError::Markers);
    }
    let found = cands.iter().find_map(|(mpos, markers)| find_names(image, *mpos, markers));
    let (num_pos, names, n) = found.ok_or(KallsymsError::Names)?;
    // Offset: dopo l'indice dei token (kernel recenti) o prima di
    // relative_base e num_syms (kernel più vecchi).
    let sorted_at = |o: usize| -> bool {
        let mut prev = 0u32;
        (0..n).all(|i| match u32_at(image, o + 4 * i) {
            Some(v) if v >= prev => {
                prev = v;
                true
            }
            _ => false,
        })
    };
    let new = align(tok.index_end, 8);
    let (offs, base_pos) = if sorted_at(new) {
        (new, align(new + 4 * n, 8))
    } else {
        let b = num_pos.checked_sub(8).ok_or(KallsymsError::Offsets)?;
        let o = (b.checked_sub(4 * n).ok_or(KallsymsError::Offsets)?) & !7;
        let o = [o, o + 4].into_iter().find(|&o| sorted_at(o)).ok_or(KallsymsError::Offsets)?;
        (o, b)
    };
    let base = relative_base(image, base_pos).ok_or(KallsymsError::Base)?;
    let mut out = Vec::with_capacity(n);
    let mut p = names;
    let mut buf = Vec::new();
    for i in 0..n {
        let (len, h) = name_len(image, p).ok_or(KallsymsError::Names)?;
        buf.clear();
        for &t in image.get(p + h..p + h + len).ok_or(KallsymsError::Names)? {
            buf.extend_from_slice(tok.token(image, t));
        }
        p += h + len;
        let off = u32_at(image, offs + 4 * i).ok_or(KallsymsError::Offsets)?;
        let (&kind, name) = buf.split_first().ok_or(KallsymsError::Names)?;
        out.push(KSym {
            addr: base.wrapping_add(u64::from(off)),
            kind: char::from(kind),
            name: String::from_utf8_lossy(name).into_owned(),
        });
    }
    Ok(out)
}

/// `kallsyms_relative_base` a `pos`. Con `--no-apply-dynamic-relocs` (il
/// kernel arm64 con le rilocazioni RELA) il valore nel file è zero, e sta
/// nell'addendo della rilocazione R_AARCH64_RELATIVE di quel posto: la si
/// cerca fra le voci `.rela.dyn` (r_offset = base del kernel + pos).
fn relative_base(image: &[u8], pos: usize) -> Option<u64> {
    let v = u64_at(image, pos)?;
    if v != 0 {
        return Some(v);
    }
    // Le voci R_AARCH64_RELATIVE (r_offset, 1027, addendo).
    const R_AARCH64_RELATIVE: u64 = 1027;
    let len = image.len() as u64;
    let mut relas = Vec::new();
    let mut q = 8;
    while q + 16 <= image.len() {
        if u64_at(image, q) == Some(R_AARCH64_RELATIVE)
            && let (Some(r), Some(a)) = (u64_at(image, q - 8), u64_at(image, q + 8))
            && r >> 48 == 0xffff
        {
            relas.push((r, a));
        }
        q += 8;
    }
    // La base di collegamento dell'`Image` giusta è quella per cui i posti
    // da rilocare contengono zero nel file (il valore sta nell'addendo):
    // fra le voci che puntano a `pos`, quella con la base più coerente.
    let zero_at = |base: u64, r: u64| -> bool {
        r.checked_sub(base).and_then(|o| usize::try_from(o).ok()).and_then(|o| u64_at(image, o)) == Some(0)
    };
    let sample: Vec<(u64, u64)> = relas.iter().step_by((relas.len() / 512).max(1)).copied().collect();
    relas
        .iter()
        .filter_map(|&(r, a)| {
            let base = r.wrapping_sub(pos as u64);
            (base % 4096 == 0 && a >= base && a < base.wrapping_add(len)).then_some((base, a))
        })
        .map(|(base, a)| (sample.iter().filter(|&&(r, _)| zero_at(base, r)).count(), a))
        .filter(|&(score, _)| score * 10 >= sample.len() * 9)
        .max_by_key(|&(score, _)| score)
        .map(|(_, a)| a)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un `Image` finto con la tabella kallsyms nell'ordine dei kernel
    /// recenti (6.x): nomi senza compressione (ogni token è il suo byte).
    fn fake_image(syms: &[(u32, &str)], zero_base: bool) -> Vec<u8> {
        let base = 0xffff_8000_8000_0000u64;
        let mut img = vec![0u8; 0x100];
        while img.len() % 8 != 0 {
            img.push(0);
        }
        img.extend_from_slice(&(syms.len() as u32).to_le_bytes());
        img.extend_from_slice(&[0; 4]);
        let names_at = img.len();
        let mut markers = Vec::new();
        for (i, (_, n)) in syms.iter().enumerate() {
            if i % 256 == 0 {
                markers.push((img.len() - names_at) as u32);
            }
            img.push(n.len() as u8);
            img.extend_from_slice(n.as_bytes());
        }
        while img.len() % 8 != 0 {
            img.push(0);
        }
        for m in markers {
            img.extend_from_slice(&m.to_le_bytes());
        }
        while img.len() % 8 != 0 {
            img.push(0);
        }
        let table = img.len();
        let mut offs = Vec::new();
        for t in 0..256u32 {
            offs.push((img.len() - table) as u16);
            // Byte stampabili: se stessi; gli altri un token di due lettere.
            if (0x21..0x7f).contains(&t) {
                img.push(t as u8);
            } else {
                img.extend_from_slice(b"zz");
            }
            img.push(0);
        }
        while img.len() % 8 != 0 {
            img.push(0);
        }
        for o in offs {
            img.extend_from_slice(&o.to_le_bytes());
        }
        while img.len() % 8 != 0 {
            img.push(0);
        }
        for (a, _) in syms {
            img.extend_from_slice(&a.to_le_bytes());
        }
        while img.len() % 8 != 0 {
            img.push(0);
        }
        let base_pos = img.len();
        img.extend_from_slice(&(if zero_base { 0 } else { base }).to_le_bytes());
        img.extend_from_slice(&[0; 64]);
        if zero_base {
            // Una voce di .rela.dyn per quel posto.
            img.extend_from_slice(&(base + base_pos as u64).to_le_bytes());
            img.extend_from_slice(&1027u64.to_le_bytes());
            img.extend_from_slice(&base.to_le_bytes());
        }
        img
    }

    #[test]
    fn tabella_finta_e_system_map() {
        let names: Vec<String> = (0..600).map(|i| format!("Tfunc_{i}")).collect();
        let mut syms: Vec<(u32, &str)> =
            names.iter().enumerate().map(|(i, n)| (i as u32 * 16, n.as_str())).collect();
        syms[3].1 = "Dinit_task";
        for zero in [false, true] {
            let img = fake_image(&syms, zero);
            let s = Symbols::from_image(&img).unwrap();
            assert_eq!(s.len(), 600);
            assert_eq!(s.get("init_task"), Some(0xffff_8000_8000_0030));
            assert_eq!(s.get("func_599"), Some(0xffff_8000_8000_0000 + 599 * 16));
            let (k, off) = s.lookup(0xffff_8000_8000_0035).unwrap();
            assert_eq!((k.name.as_str(), k.kind, off), ("init_task", 'D', 5));
        }
        let m = Symbols::parse_system_map("ffff800080000000 T _text\nffff80008062d3c0 D init_task\nzz\n");
        assert_eq!(m.get("init_task"), Some(0xffff80008062d3c0));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn byte_arbitrari_senza_panic() {
        assert!(extract(&[]).is_err());
        assert!(extract(b"0\x001\x002\x003\x004\x005\x006\x007\x008\x009\x00").is_err());
        let names: Vec<String> = (0..300).map(|i| format!("tf{i}")).collect();
        let syms: Vec<(u32, &str)> = names.iter().enumerate().map(|(i, n)| (i as u32, n.as_str())).collect();
        let img = fake_image(&syms, false);
        for cut in (0..img.len()).step_by(97) {
            let _ = extract(&img[..cut]);
        }
    }
}
