//! DEFLATE (RFC 1951) con gli involucri gzip (RFC 1952) e zlib (RFC 1950),
//! per `Content-Encoding: gzip` e `deflate`. Decodificatore canonico alla
//! maniera di `puff.c` di zlib: lento ma piccolo e senza dipendenze. Le
//! somme (CRC-32, Adler-32) e le lunghezze si verificano; l'uscita è
//! limitata (bombe di compressione).

/// Uscita massima di una decompressione.
pub const MAX_OUTPUT: usize = 256 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InflateError(pub &'static str);

impl std::fmt::Display for InflateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InflateError {}

type Result<T> = std::result::Result<T, InflateError>;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    nbits: u32,
}

impl Bits<'_> {
    fn need(&mut self, n: u32) -> Result<u32> {
        let mut v = self.bit;
        while self.nbits < n {
            let b = *self.data.get(self.pos).ok_or(InflateError("dati DEFLATE troncati"))?;
            self.pos += 1;
            v |= u32::from(b) << self.nbits;
            self.nbits += 8;
        }
        self.bit = if n == 32 { 0 } else { v >> n };
        self.nbits -= n;
        Ok(v & ((1u64 << n) - 1) as u32)
    }

    fn align(&mut self) {
        self.bit = 0;
        self.nbits = 0;
    }
}

struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    /// Codice canonico dalle lunghezze; ammette codici incompleti solo
    /// con un solo simbolo (come puff).
    fn new(lengths: &[u8]) -> Result<Huffman> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }
        if usize::from(count[0]) == lengths.len() {
            return Ok(Huffman { count, symbol: Vec::new() });
        }
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err(InflateError("codice di Huffman sovrabbondante"));
            }
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[usize::from(offs[usize::from(l)])] = s as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, b: &mut Bits<'_>) -> Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.need(1)? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                return self
                    .symbol
                    .get((index + code - first) as usize)
                    .copied()
                    .ok_or(InflateError("simbolo non valido"));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(InflateError("codice di Huffman non valido"))
    }
}

const LBASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195,
    227, 258,
];
const LEXT: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn codes(b: &mut Bits<'_>, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman) -> Result<()> {
    loop {
        let s = lit.decode(b)?;
        match s {
            0..=255 => {
                if out.len() >= MAX_OUTPUT {
                    return Err(InflateError("uscita oltre il limite"));
                }
                out.push(s as u8);
            }
            256 => return Ok(()),
            _ => {
                let i = usize::from(s - 257);
                let len = usize::from(*LBASE.get(i).ok_or(InflateError("lunghezza non valida"))?)
                    + b.need(u32::from(LEXT[i]))? as usize;
                let d = usize::from(dist.decode(b)?);
                let dd = usize::from(*DBASE.get(d).ok_or(InflateError("distanza non valida"))?)
                    + b.need(u32::from(DEXT[d]))? as usize;
                if dd > out.len() {
                    return Err(InflateError("distanza oltre l'inizio"));
                }
                if out.len() + len > MAX_OUTPUT {
                    return Err(InflateError("uscita oltre il limite"));
                }
                let start = out.len() - dd;
                for k in 0..len {
                    out.push(out[start + k]);
                }
            }
        }
    }
}

/// Decomprime un flusso DEFLATE grezzo; restituisce i dati e i byte letti.
pub fn inflate_raw(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut b = Bits { data, pos: 0, bit: 0, nbits: 0 };
    let mut out = Vec::new();
    loop {
        let last = b.need(1)?;
        match b.need(2)? {
            0 => {
                b.align();
                let h = data.get(b.pos..b.pos + 4).ok_or(InflateError("blocco non compresso troncato"))?;
                let len = usize::from(u16::from_le_bytes([h[0], h[1]]));
                if u16::from_le_bytes([h[2], h[3]]) != !(len as u16) {
                    return Err(InflateError("lunghezza del blocco non compresso incoerente"));
                }
                let d = data
                    .get(b.pos + 4..b.pos + 4 + len)
                    .ok_or(InflateError("blocco non compresso troncato"))?;
                if out.len() + len > MAX_OUTPUT {
                    return Err(InflateError("uscita oltre il limite"));
                }
                out.extend_from_slice(d);
                b.pos += 4 + len;
            }
            1 => {
                let mut l = [0u8; 288];
                l[..144].fill(8);
                l[144..256].fill(9);
                l[256..280].fill(7);
                l[280..].fill(8);
                let lit = Huffman::new(&l)?;
                let dist = Huffman::new(&[5u8; 30])?;
                codes(&mut b, &mut out, &lit, &dist)?;
            }
            2 => {
                let nlen = b.need(5)? as usize + 257;
                let ndist = b.need(5)? as usize + 1;
                let ncode = b.need(4)? as usize + 4;
                if nlen > 286 || ndist > 30 {
                    return Err(InflateError("troppe lunghezze"));
                }
                const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl = [0u8; 19];
                for &o in &ORDER[..ncode] {
                    cl[o] = b.need(3)? as u8;
                }
                let lencode = Huffman::new(&cl)?;
                let mut lengths = vec![0u8; nlen + ndist];
                let mut i = 0;
                while i < nlen + ndist {
                    let s = lencode.decode(&mut b)?;
                    let (val, rep) = match s {
                        0..=15 => (s as u8, 1),
                        16 => {
                            let prev = *lengths
                                .get(i.wrapping_sub(1))
                                .ok_or(InflateError("ripetizione senza precedente"))?;
                            (prev, 3 + b.need(2)? as usize)
                        }
                        17 => (0, 3 + b.need(3)? as usize),
                        _ => (0, 11 + b.need(7)? as usize),
                    };
                    if i + rep > lengths.len() {
                        return Err(InflateError("troppe lunghezze"));
                    }
                    lengths[i..i + rep].fill(val);
                    i += rep;
                }
                if lengths[256] == 0 {
                    return Err(InflateError("manca il codice di fine blocco"));
                }
                let lit = Huffman::new(&lengths[..nlen])?;
                let dist = Huffman::new(&lengths[nlen..])?;
                codes(&mut b, &mut out, &lit, &dist)?;
            }
            _ => return Err(InflateError("tipo di blocco non valido")),
        }
        if last == 1 {
            return Ok((out, b.pos));
        }
    }
}

/// CRC-32 (IEEE, quello di gzip e di Ethernet).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// Decomprime gzip (uno o più membri concatenati).
pub fn gunzip(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
            return Err(InflateError("intestazione gzip non valida"));
        }
        let flg = data[3];
        let mut pos = 10;
        if flg & 4 != 0 {
            let x = data.get(pos..pos + 2).ok_or(InflateError("gzip troncato"))?;
            pos += 2 + usize::from(u16::from_le_bytes([x[0], x[1]]));
        }
        for bit in [8u8, 16] {
            if flg & bit != 0 {
                let z = data
                    .get(pos..)
                    .and_then(|d| d.iter().position(|&c| c == 0))
                    .ok_or(InflateError("gzip troncato"))?;
                pos += z + 1;
            }
        }
        if flg & 2 != 0 {
            pos += 2;
        }
        let (d, used) = inflate_raw(data.get(pos..).ok_or(InflateError("gzip troncato"))?)?;
        let t = data.get(pos + used..pos + used + 8).ok_or(InflateError("coda gzip mancante"))?;
        if u32::from_le_bytes([t[0], t[1], t[2], t[3]]) != crc32(&d) {
            return Err(InflateError("CRC gzip errato"));
        }
        if u32::from_le_bytes([t[4], t[5], t[6], t[7]]) != d.len() as u32 {
            return Err(InflateError("lunghezza gzip errata"));
        }
        if out.len() + d.len() > MAX_OUTPUT {
            return Err(InflateError("uscita oltre il limite"));
        }
        out.extend(d);
        data = &data[pos + used + 8..];
        // Zeri di riempimento dopo l'ultimo membro sono tollerati.
        if data.iter().all(|&b| b == 0) {
            return Ok(out);
        }
    }
}

/// Decomprime zlib (`Content-Encoding: deflate` secondo RFC 9110); se
/// l'intestazione non è zlib, prova DEFLATE grezzo (come fanno i browser
/// con i server che lo mandano così).
pub fn zlib_or_raw(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 6
        && data[0] & 0x0f == 8
        && (u16::from(data[0]) << 8 | u16::from(data[1])) % 31 == 0
        && data[1] & 0x20 == 0
    {
        let (d, used) = inflate_raw(&data[2..])?;
        let t = data.get(2 + used..6 + used).ok_or(InflateError("coda zlib mancante"))?;
        if u32::from_be_bytes([t[0], t[1], t[2], t[3]]) != adler32(&d) {
            return Err(InflateError("Adler-32 errato"));
        }
        return Ok(d);
    }
    inflate_raw(data).map(|(d, _)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printf 'ciao ciao ciao vetro\n' | gzip -9 -n` (gzip dell'host).
    const GZ_SHORT: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x4b, 0xce, 0x4c, 0xcc, 0x57, 0x48, 0x86,
        0x13, 0x65, 0xa9, 0x25, 0x45, 0xf9, 0x5c, 0x00, 0xb6, 0x65, 0xcc, 0x5d, 0x15, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn gzip_breve_a_codici_fissi() {
        assert_eq!(gunzip(GZ_SHORT).unwrap(), b"ciao ciao ciao vetro\n");
    }

    #[test]
    fn gzip_con_codici_dinamici() {
        // `seq 1 2000 | gzip -9 -n`: blocco con codici dinamici.
        let expect: Vec<u8> = (1..=2000).flat_map(|i| format!("{i}\n").into_bytes()).collect();
        let gz = include_bytes!("testdata/seq2000.gz");
        assert_eq!((gz[10] >> 1) & 3, 2, "primo blocco a codici dinamici");
        assert_eq!(gunzip(gz).unwrap(), expect);
        // Due membri concatenati.
        let mut two = GZ_SHORT.to_vec();
        two.extend_from_slice(gz);
        let mut e2 = b"ciao ciao ciao vetro\n".to_vec();
        e2.extend(&expect);
        assert_eq!(gunzip(&two).unwrap(), e2);
    }

    #[test]
    fn blocco_non_compresso_e_zlib() {
        // zlib con un blocco stored: 78 01, BFINAL=1 BTYPE=00, LEN, NLEN.
        let mut z = vec![0x78, 0x01, 0x01, 5, 0, 0xfa, 0xff];
        z.extend(b"hello");
        z.extend(adler32(b"hello").to_be_bytes());
        assert_eq!(zlib_or_raw(&z).unwrap(), b"hello");
        assert_eq!(zlib_or_raw(&z[2..z.len() - 4]).unwrap(), b"hello", "DEFLATE grezzo");
        let mut bad = z.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(zlib_or_raw(&bad).is_err());
    }

    #[test]
    fn errori_senza_panic() {
        assert!(gunzip(b"").is_err());
        let mut bad = GZ_SHORT.to_vec();
        bad[20] ^= 0x55;
        assert!(gunzip(&bad).is_err());
        for n in 0..GZ_SHORT.len() {
            assert!(gunzip(&GZ_SHORT[..n]).is_err(), "{n}");
        }
        let mut x = 99u32;
        for _ in 0..500 {
            let v: Vec<u8> = (0..64)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (x >> 16) as u8
                })
                .collect();
            let _ = inflate_raw(&v);
        }
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
