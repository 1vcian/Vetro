//! Decompressori per il kernel dentro un `boot.img`: gzip (RFC 1951 e 1952)
//! e LZ4 (formato legacy di `lz4 -l`, quello del GKI, e formato frame).
//!
//! Servono solo al kernel: i ramdisk passano al guest compressi così come
//! sono, e li apre il kernel. Senza dipendenze, perché `vetro-machine` va
//! anche nel browser. Scritti per essere corretti, non velocissimi: una
//! tabella diretta a 9 bit per i codici di Huffman corti e la decodifica
//! canonica bit per bit (come `puff.c` di zlib) per gli altri.
//!
//! gzip controlla CRC32 e lunghezza di ogni membro. Del formato frame di LZ4
//! non si verificano i checksum xxHash (opzionali nel formato): un errore nei
//! dati compressi di solito rompe comunque la decodifica, e il kernel
//! risultante viene poi validato dal caricatore (header dell'`Image`).

use std::fmt;

/// Tetto all'uscita: un kernel arm64 sta ampiamente sotto.
pub const MAX_OUTPUT: usize = 512 << 20;

/// Magic dell'`Image` arm64 (`ARM\x64` all'offset 56).
const IMAGE_MAGIC: &[u8; 4] = b"ARM\x64";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// `lz4 -l`: blocchi indipendenti da 8 MiB non compressi.
pub const LZ4_LEGACY_MAGIC: u32 = 0x184c_2102;
pub const LZ4_FRAME_MAGIC: u32 = 0x184d_2204;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompressError {
    /// I dati finiscono prima del previsto.
    Truncated,
    /// Intestazione non valida o funzione del formato non supportata.
    BadHeader(&'static str),
    /// Dati compressi non validi.
    BadData(&'static str),
    /// CRC32 del membro gzip diverso da quello dichiarato.
    Crc { expected: u32, actual: u32 },
    /// Lunghezza del membro gzip (modulo 2^32) diversa da quella dichiarata.
    Length { expected: u32, actual: u32 },
    /// Uscita oltre [`MAX_OUTPUT`].
    TooLarge,
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecompressError::Truncated => write!(f, "dati compressi troncati"),
            DecompressError::BadHeader(w) => write!(f, "intestazione non valida: {w}"),
            DecompressError::BadData(w) => write!(f, "dati compressi non validi: {w}"),
            DecompressError::Crc { expected, actual } => {
                write!(f, "CRC32 sbagliato: atteso {expected:#010x}, calcolato {actual:#010x}")
            }
            DecompressError::Length { expected, actual } => {
                write!(f, "lunghezza sbagliata: attesa {expected}, ottenuta {actual}")
            }
            DecompressError::TooLarge => write!(f, "uscita oltre {} MiB", MAX_OUTPUT >> 20),
        }
    }
}

impl std::error::Error for DecompressError {}

/// Formato riconosciuto dai primi byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `Image` arm64 non compresso.
    Image,
    Gzip,
    Lz4Legacy,
    Lz4Frame,
    Unknown,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Image => "Image",
            Format::Gzip => "gzip",
            Format::Lz4Legacy => "lz4 legacy",
            Format::Lz4Frame => "lz4 frame",
            Format::Unknown => "sconosciuto",
        })
    }
}

pub fn detect(data: &[u8]) -> Format {
    let magic = data.get(..4).map(|m| u32::from_le_bytes(m.try_into().unwrap()));
    if data.len() >= 64 && &data[56..60] == IMAGE_MAGIC {
        Format::Image
    } else if data.starts_with(&GZIP_MAGIC) {
        Format::Gzip
    } else if magic == Some(LZ4_LEGACY_MAGIC) {
        Format::Lz4Legacy
    } else if magic == Some(LZ4_FRAME_MAGIC) {
        Format::Lz4Frame
    } else {
        Format::Unknown
    }
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3, riflesso), quello di gzip.

fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, e) in t.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc32_table();
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

// ---------------------------------------------------------------------------
// Inflate (RFC 1951).

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, cnt: 0 }
    }

    /// Riempie il buffer fin dove ci sono byte.
    fn refill(&mut self) {
        while self.cnt <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }

    fn bits(&mut self, n: u32) -> Result<u32, DecompressError> {
        if n == 0 {
            return Ok(0);
        }
        if self.cnt < n {
            self.refill();
            if self.cnt < n {
                return Err(DecompressError::Truncated);
            }
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Ok(v)
    }

    /// Scarta i bit fino al confine di byte e restituisce i byte già letti
    /// nel buffer allo stream: da qui si legge a byte.
    fn align(&mut self) {
        let drop = self.cnt % 8;
        self.buf >>= drop;
        self.cnt -= drop;
        self.pos -= (self.cnt / 8) as usize;
        self.buf = 0;
        self.cnt = 0;
    }
}

const FAST_BITS: u32 = 9;
const MAX_BITS: usize = 15;

struct Huffman {
    /// Indice: i prossimi 9 bit dello stream. Valore: `simbolo << 4 | lunghezza`,
    /// 0 se il codice è più lungo di 9 bit.
    fast: Vec<u16>,
    counts: [u16; MAX_BITS + 1],
    /// Simboli in ordine canonico (lunghezza, poi valore).
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, DecompressError> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // Codice sovra-sottoscritto: non è un albero.
        let mut left: i32 = 1;
        for &c in &counts[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return Err(DecompressError::BadData("codice di Huffman sovra-sottoscritto"));
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for l in 1..=MAX_BITS {
            offs[l + 1] = offs[l] + counts[l];
        }
        let mut symbols = vec![0u16; offs[MAX_BITS + 1] as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        // Tabella diretta per i codici fino a FAST_BITS bit: codici canonici,
        // bit invertiti (lo stream è LSB per primo, i codici MSB per primi).
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut code: u32 = 0;
        let mut idx = 0usize;
        for len in 1..=MAX_BITS as u32 {
            for _ in 0..counts[len as usize] {
                if len <= FAST_BITS {
                    let rev = code.reverse_bits() >> (32 - len);
                    let entry = (symbols[idx] << 4) | len as u16;
                    let mut k = rev as usize;
                    while k < fast.len() {
                        fast[k] = entry;
                        k += 1 << len;
                    }
                }
                code += 1;
                idx += 1;
            }
            code <<= 1;
        }
        Ok(Huffman { fast, counts, symbols })
    }

    fn decode(&self, br: &mut Bits) -> Result<u16, DecompressError> {
        if br.cnt < MAX_BITS as u32 {
            br.refill();
        }
        let e = self.fast[(br.buf & ((1 << FAST_BITS) - 1)) as usize];
        let len = (e & 15) as u32;
        if len != 0 {
            if len > br.cnt {
                return Err(DecompressError::Truncated);
            }
            br.buf >>= len;
            br.cnt -= len;
            return Ok(e >> 4);
        }
        // Codice lungo (o non valido): decodifica canonica bit per bit.
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(DecompressError::BadData("codice di Huffman non valido"))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195,
    227, 258,
];
const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// Ordine delle lunghezze del codice delle lunghezze nei blocchi dinamici.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    lit[..144].fill(8);
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    lit[280..].fill(8);
    let dist = [5u8; 30];
    (Huffman::new(&lit).expect("tabella fissa"), Huffman::new(&dist).expect("tabella fissa"))
}

fn dynamic_tables(br: &mut Bits) -> Result<(Huffman, Huffman), DecompressError> {
    let nlen = br.bits(5)? as usize + 257;
    let ndist = br.bits(5)? as usize + 1;
    let ncode = br.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(DecompressError::BadData("troppi codici nel blocco dinamico"));
    }
    let mut cl = [0u8; 19];
    for &i in &CL_ORDER[..ncode] {
        cl[i] = br.bits(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = [0u8; 286 + 30];
    let mut i = 0;
    while i < nlen + ndist {
        let sym = clh.decode(br)?;
        let (val, rep) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                if i == 0 {
                    return Err(DecompressError::BadData("ripetizione senza lunghezza precedente"));
                }
                (lengths[i - 1], 3 + br.bits(2)? as usize)
            }
            17 => (0, 3 + br.bits(3)? as usize),
            _ => (0, 11 + br.bits(7)? as usize),
        };
        if i + rep > nlen + ndist {
            return Err(DecompressError::BadData("troppe lunghezze nel blocco dinamico"));
        }
        lengths[i..i + rep].fill(val);
        i += rep;
    }
    if lengths[256] == 0 {
        return Err(DecompressError::BadData("manca il codice di fine blocco"));
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..nlen + ndist])?))
}

fn inflate_block(
    br: &mut Bits,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), DecompressError> {
    loop {
        let sym = lit.decode(br)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let s = sym - 257;
            if s >= 29 {
                return Err(DecompressError::BadData("simbolo di lunghezza non valido"));
            }
            let len = LEN_BASE[s] as usize + br.bits(LEN_EXTRA[s] as u32)? as usize;
            let d = dist.decode(br)? as usize;
            if d >= 30 {
                return Err(DecompressError::BadData("simbolo di distanza non valido"));
            }
            let d = DIST_BASE[d] as usize + br.bits(DIST_EXTRA[d] as u32)? as usize;
            copy_match(out, d, len)?;
        }
        if out.len() > MAX_OUTPUT {
            return Err(DecompressError::TooLarge);
        }
    }
}

/// Copia `len` byte da `dist` byte indietro (le sovrapposizioni ripetono).
fn copy_match(out: &mut Vec<u8>, dist: usize, len: usize) -> Result<(), DecompressError> {
    if dist == 0 || dist > out.len() {
        return Err(DecompressError::BadData("distanza oltre l'inizio dei dati"));
    }
    let start = out.len() - dist;
    if dist >= len {
        out.extend_from_within(start..start + len);
    } else {
        for k in 0..len {
            let b = out[start + k];
            out.push(b);
        }
    }
    Ok(())
}

/// Decomprime uno stream deflate grezzo; restituisce i dati e i byte letti.
pub fn inflate(data: &[u8]) -> Result<(Vec<u8>, usize), DecompressError> {
    let mut out = Vec::new();
    let mut br = Bits::new(data);
    loop {
        let last = br.bits(1)? == 1;
        match br.bits(2)? {
            0 => {
                br.align();
                let p = br.pos;
                let hdr = data.get(p..p + 4).ok_or(DecompressError::Truncated)?;
                let len = u16::from_le_bytes([hdr[0], hdr[1]]);
                let nlen = u16::from_le_bytes([hdr[2], hdr[3]]);
                if len != !nlen {
                    return Err(DecompressError::BadData("blocco non compresso con LEN/NLEN incoerenti"));
                }
                let body = data.get(p + 4..p + 4 + len as usize).ok_or(DecompressError::Truncated)?;
                out.extend_from_slice(body);
                br.pos = p + 4 + len as usize;
            }
            1 => {
                let (lit, dist) = fixed_tables();
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err(DecompressError::BadData("tipo di blocco riservato")),
        }
        if out.len() > MAX_OUTPUT {
            return Err(DecompressError::TooLarge);
        }
        if last {
            br.align();
            return Ok((out, br.pos));
        }
    }
}

/// Decomprime un file gzip (anche più membri concatenati; quello che segue
/// l'ultimo membro, per esempio zeri di riempimento, si ignora).
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    let mut p = 0;
    loop {
        p += gunzip_member(&data[p..], &mut out)?;
        if !data[p..].starts_with(&GZIP_MAGIC) {
            return Ok(out);
        }
    }
}

fn gunzip_member(data: &[u8], out: &mut Vec<u8>) -> Result<usize, DecompressError> {
    let hdr = data.get(..10).ok_or(DecompressError::Truncated)?;
    if hdr[..2] != GZIP_MAGIC {
        return Err(DecompressError::BadHeader("manca il magic di gzip"));
    }
    if hdr[2] != 8 {
        return Err(DecompressError::BadHeader("metodo di compressione diverso da deflate"));
    }
    let flg = hdr[3];
    if flg & 0xe0 != 0 {
        return Err(DecompressError::BadHeader("flag riservati di gzip"));
    }
    let mut p = 10;
    if flg & 4 != 0 {
        let x = data.get(p..p + 2).ok_or(DecompressError::Truncated)?;
        p += 2 + u16::from_le_bytes([x[0], x[1]]) as usize;
    }
    for bit in [8u8, 16] {
        if flg & bit != 0 {
            let z = data
                .get(p..)
                .and_then(|r| r.iter().position(|&b| b == 0))
                .ok_or(DecompressError::Truncated)?;
            p += z + 1;
        }
    }
    if flg & 2 != 0 {
        p += 2;
    }
    let body = data.get(p..).ok_or(DecompressError::Truncated)?;
    let (member, used) = inflate(body)?;
    p += used;
    let tr = data.get(p..p + 8).ok_or(DecompressError::Truncated)?;
    let crc = u32::from_le_bytes(tr[..4].try_into().unwrap());
    let isize = u32::from_le_bytes(tr[4..].try_into().unwrap());
    let actual = crc32(&member);
    if actual != crc {
        return Err(DecompressError::Crc { expected: crc, actual });
    }
    if member.len() as u32 != isize {
        return Err(DecompressError::Length { expected: isize, actual: member.len() as u32 });
    }
    if out.len() + member.len() > MAX_OUTPUT {
        return Err(DecompressError::TooLarge);
    }
    out.extend_from_slice(&member);
    Ok(p + 8)
}

// ---------------------------------------------------------------------------
// LZ4.

fn le32(data: &[u8], p: usize) -> Result<u32, DecompressError> {
    data.get(p..p + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or(DecompressError::Truncated)
}

/// Decodifica un blocco LZ4 in coda a `out` (le distanze possono risalire
/// ai blocchi precedenti, come nei frame a blocchi collegati).
pub fn lz4_block(src: &[u8], out: &mut Vec<u8>) -> Result<(), DecompressError> {
    let mut i = 0;
    let byte = |i: &mut usize| -> Result<u8, DecompressError> {
        let b = *src.get(*i).ok_or(DecompressError::Truncated)?;
        *i += 1;
        Ok(b)
    };
    loop {
        let token = byte(&mut i)?;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = byte(&mut i)?;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        out.extend_from_slice(src.get(i..i + lit).ok_or(DecompressError::Truncated)?);
        i += lit;
        if i == src.len() {
            // L'ultima sequenza ha solo letterali.
            return Ok(());
        }
        let off = u16::from_le_bytes([byte(&mut i)?, byte(&mut i)?]) as usize;
        let mut len = (token & 15) as usize;
        if len == 15 {
            loop {
                let b = byte(&mut i)?;
                len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        copy_match(out, off, len + 4)?;
        if out.len() > MAX_OUTPUT {
            return Err(DecompressError::TooLarge);
        }
    }
}

/// Formato legacy (`lz4 -l`): magic, poi blocchi `dimensione (u32) + dati`
/// fino alla fine; un altro magic apre uno stream concatenato.
pub fn lz4_legacy(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    if le32(data, 0)? != LZ4_LEGACY_MAGIC {
        return Err(DecompressError::BadHeader("manca il magic di lz4 legacy"));
    }
    let mut out = Vec::new();
    let mut p = 4;
    while p + 4 <= data.len() {
        let n = le32(data, p)?;
        p += 4;
        if n == LZ4_LEGACY_MAGIC {
            continue;
        }
        if n == 0 {
            break;
        }
        let block = data.get(p..p + n as usize).ok_or(DecompressError::Truncated)?;
        lz4_block(block, &mut out)?;
        p += n as usize;
    }
    Ok(out)
}

/// Formato frame (`lz4` senza `-l`), anche più frame concatenati e frame
/// saltabili. Niente dizionari esterni.
pub fn lz4_frame(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        let magic = le32(data, p)?;
        if magic & 0xffff_fff0 == 0x184d_2a50 {
            p += 8 + le32(data, p + 4)? as usize;
            continue;
        }
        if magic != LZ4_FRAME_MAGIC {
            if p == 0 {
                return Err(DecompressError::BadHeader("manca il magic del frame lz4"));
            }
            break;
        }
        let flg = *data.get(p + 4).ok_or(DecompressError::Truncated)?;
        if flg >> 6 != 1 {
            return Err(DecompressError::BadHeader("versione del frame lz4 diversa da 1"));
        }
        if flg & 1 != 0 {
            return Err(DecompressError::BadHeader("frame lz4 con dizionario"));
        }
        p += 7 + if flg & 8 != 0 { 8 } else { 0 };
        loop {
            let size = le32(data, p)?;
            p += 4;
            if size == 0 {
                break;
            }
            let n = (size & 0x7fff_ffff) as usize;
            let block = data.get(p..p + n).ok_or(DecompressError::Truncated)?;
            if size & 0x8000_0000 != 0 {
                out.extend_from_slice(block);
            } else {
                lz4_block(block, &mut out)?;
            }
            p += n + if flg & 0x10 != 0 { 4 } else { 0 };
            if out.len() > MAX_OUTPUT {
                return Err(DecompressError::TooLarge);
            }
        }
        if flg & 4 != 0 {
            p += 4;
        }
    }
    Ok(out)
}

/// Decomprime secondo il formato riconosciuto; `Image` resta com'è.
pub fn decompress(data: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, DecompressError> {
    use std::borrow::Cow;
    match detect(data) {
        Format::Image => Ok(Cow::Borrowed(data)),
        Format::Gzip => gunzip(data).map(Cow::Owned),
        Format::Lz4Legacy => lz4_legacy(data).map(Cow::Owned),
        Format::Lz4Frame => lz4_frame(data).map(Cow::Owned),
        Format::Unknown => Err(DecompressError::BadHeader("formato del kernel sconosciuto")),
    }
}
