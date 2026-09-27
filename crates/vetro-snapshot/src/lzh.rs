//! LZ77 with Huffman coding over a frame of up to [`FRAME`] bytes (a group
//! of pages): the codec of [`Level::Small`](crate::Level::Small) (ADR 0031),
//! for snapshots that are downloaded (the prebuilt Android snapshot). No
//! dependencies, deterministic (the same bytes always give the same output).
//!
//! A compressed frame is:
//! - the code lengths of the two Huffman codes, 4 bits each (low nibble
//!   first), [`LL_CODES`] for literals and match lengths then [`DIST_CODES`]
//!   for distances; 0 = symbol unused, at most [`MAX_BITS`];
//! - a bit stream, least significant bit first, of symbols until the frame
//!   (whose length the caller knows) is full: a literal/length symbol `s`
//!   below 256 is the byte `s`; otherwise `s - 256` is a length code (value
//!   `len - MIN_MATCH`), followed by its extra bits, a distance symbol (value
//!   `distance - 1`) and its extra bits. Codes are canonical (by length, then
//!   symbol); a code's bits are written from its first bit.
//!
//! Values (lengths and distances) are coded as buckets: `v < 16` is code `v`
//! with no extra bits; otherwise, with `n` = index of the highest bit of `v`
//! and `m` the bit below it, code `16 + 2 (n - 4) + m` with the `n - 1` low
//! bits of `v` as extra bits.
//!
//! The compressor uses hash chains over the whole frame and one step of lazy
//! matching.

use crate::{Error, Result};

/// Largest frame (64 pages of 4 KiB).
pub const FRAME: usize = 64 * 4096;
/// Shortest match.
pub const MIN_MATCH: usize = 4;
/// Longest match (`len - MIN_MATCH` below 2^16).
pub const MAX_MATCH: usize = MIN_MATCH + (1 << 16) - 1;
/// Longest code, in bits.
pub const MAX_BITS: u32 = 12;
/// Length codes (values below 2^16).
pub const LEN_CODES: usize = 40;
/// Literal/length alphabet: 256 bytes and the length codes.
pub const LL_CODES: usize = 256 + LEN_CODES;
/// Distance codes (values below 2^18 = [`FRAME`]).
pub const DIST_CODES: usize = 44;
/// Bytes of the code lengths at the start of a frame.
pub const HEADER: usize = (LL_CODES + DIST_CODES) / 2;

const HASH_BITS: u32 = 16;
const MAX_CHAIN: usize = 48;
/// Matches at least this long are taken without looking one byte further.
const LAZY_MAX: usize = 64;
const TABLE: usize = 1 << MAX_BITS;

/// Bucket of value `v`: (code, extra bits, extra value).
fn bucket(v: u32) -> (usize, u32, u32) {
    if v < 16 {
        return (v as usize, 0, 0);
    }
    let n = 31 - v.leading_zeros();
    let m = (v >> (n - 1)) & 1;
    (16 + 2 * (n as usize - 4) + m as usize, n - 1, v & ((1 << (n - 1)) - 1))
}

/// Base value and extra bits of a code.
const fn code_base(code: usize) -> (u32, u32) {
    if code < 16 {
        return (code as u32, 0);
    }
    let n = ((code - 16) / 2 + 4) as u32;
    let m = ((code - 16) % 2) as u32;
    ((2 | m) << (n - 1), n - 1)
}

const fn bases<const N: usize>() -> [(u32, u32); N] {
    let mut t = [(0, 0); N];
    let mut i = 0;
    while i < N {
        t[i] = code_base(i);
        i += 1;
    }
    t
}

const LEN_BASE: [(u32, u32); LEN_CODES] = bases();
const DIST_BASE: [(u32, u32); DIST_CODES] = bases();

// ---- Huffman ------------------------------------------------------------------

/// Code lengths for the frequencies `freq`, at most `max` bits (canonical
/// Huffman lengths, limited with the method of JPEG Annex K.3). Deterministic.
fn code_lengths(freq: &[u32], max: u32) -> Vec<u8> {
    let mut len = vec![0u8; freq.len()];
    let mut syms: Vec<usize> = (0..freq.len()).filter(|&s| freq[s] > 0).collect();
    match syms.len() {
        0 => return len,
        1 => {
            len[syms[0]] = 1;
            return len;
        }
        _ => {}
    }
    syms.sort_by_key(|&s| (freq[s], s));
    let m = syms.len();
    // Two queues: leaves in order of weight, internal nodes in creation order
    // (their weights never decrease).
    let mut weight: Vec<u64> = syms.iter().map(|&s| u64::from(freq[s])).collect();
    let mut parent = vec![usize::MAX; 2 * m - 1];
    let (mut leaf, mut node) = (0usize, m);
    let pick = |weight: &Vec<u64>, leaf: &mut usize, node: &mut usize| {
        let take_leaf = *leaf < m && (*node >= weight.len() || weight[*leaf] <= weight[*node]);
        if take_leaf {
            *leaf += 1;
            *leaf - 1
        } else {
            *node += 1;
            *node - 1
        }
    };
    for _ in 0..m - 1 {
        let a = pick(&weight, &mut leaf, &mut node);
        let b = pick(&weight, &mut leaf, &mut node);
        let id = weight.len();
        weight.push(weight[a] + weight[b]);
        parent[a] = id;
        parent[b] = id;
    }
    let mut depth = vec![0u32; 2 * m - 1];
    for i in (0..2 * m - 2).rev() {
        depth[i] = depth[parent[i]] + 1;
    }
    let deepest = depth[..m].iter().copied().max().unwrap_or(0) as usize;
    let mut count = vec![0u32; deepest.max(max as usize) + 1];
    for &d in &depth[..m] {
        count[d as usize] += 1;
    }
    for i in (max as usize + 1..=deepest).rev() {
        while count[i] > 0 {
            let mut j = i - 2;
            while count[j] == 0 {
                j -= 1;
            }
            count[i] -= 2;
            count[i - 1] += 1;
            count[j + 1] += 2;
            count[j] -= 1;
        }
    }
    // The most frequent symbols get the shortest codes.
    let mut l = 1usize;
    for &s in syms.iter().rev() {
        while count[l] == 0 {
            l += 1;
        }
        len[s] = l as u8;
        count[l] -= 1;
    }
    len
}

/// Canonical codes for `len`, bit-reversed (written from their first bit).
fn codes(len: &[u8]) -> Vec<u16> {
    let mut count = [0u16; MAX_BITS as usize + 1];
    for &l in len {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u16; MAX_BITS as usize + 2];
    let mut code = 0u16;
    for b in 1..=MAX_BITS as usize {
        code = (code + count[b - 1]) << 1;
        next[b] = code;
    }
    len.iter()
        .map(|&l| {
            if l == 0 {
                return 0;
            }
            let c = next[l as usize];
            next[l as usize] += 1;
            c.reverse_bits() >> (16 - u32::from(l))
        })
        .collect()
}

/// Decoding table: for every `MAX_BITS` bits, symbol << 4 | length (0 =
/// no code starts with those bits).
fn table(len: &[u8], out: &mut [u16; TABLE]) -> Result<()> {
    out.fill(0);
    let mut used = 0usize;
    for &l in len {
        if l as u32 > MAX_BITS {
            return Err(Error::invalid("frame: code longer than 12 bits"));
        }
        if l > 0 {
            used += TABLE >> l;
        }
    }
    if used > TABLE {
        return Err(Error::invalid("frame: over-subscribed code"));
    }
    for (s, (&l, &c)) in len.iter().zip(codes(len).iter()).enumerate() {
        if l == 0 {
            continue;
        }
        let e = (s as u16) << 4 | u16::from(l);
        let mut i = c as usize;
        while i < TABLE {
            out[i] = e;
            i += 1 << l;
        }
    }
    Ok(())
}

// ---- Bits ------------------------------------------------------------------------

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter<'_> {
    #[inline]
    fn put(&mut self, v: u32, bits: u32) {
        self.acc |= u64::from(v) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn finish(self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
    }
}

struct BitReader<'a> {
    src: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
}

impl BitReader<'_> {
    /// At least 56 bits in `acc` (zeros past the end: [`Self::check`] tells).
    #[inline]
    fn refill(&mut self) {
        if self.pos + 8 <= self.src.len() {
            let v = u64::from_le_bytes(self.src[self.pos..self.pos + 8].try_into().expect("8 bytes"));
            self.acc |= v << self.n;
            self.pos += ((63 - self.n) >> 3) as usize;
            self.n |= 56;
        } else {
            while self.n <= 56 {
                let b = self.src.get(self.pos).copied().unwrap_or(0);
                self.acc |= u64::from(b) << self.n;
                self.pos += 1;
                self.n += 8;
            }
        }
    }

    #[inline]
    fn bits(&mut self, k: u32) -> u32 {
        let v = (self.acc & ((1u64 << k) - 1)) as u32;
        self.acc >>= k;
        self.n -= k;
        v
    }

    #[inline]
    fn symbol(&mut self, t: &[u16; TABLE]) -> Result<usize> {
        let e = t[(self.acc as usize) & (TABLE - 1)];
        let l = u32::from(e & 15);
        if l == 0 {
            return Err(Error::invalid("frame: invalid code"));
        }
        self.acc >>= l;
        self.n -= l;
        Ok(usize::from(e >> 4))
    }

    /// Error if more bits were used than the stream has.
    fn check(&self) -> Result<()> {
        let used = self.pos * 8 - self.n as usize;
        if used > self.src.len() * 8 { Err(Error::Truncated) } else { Ok(()) }
    }
}

// ---- Compression ----------------------------------------------------------------

/// Working memory of the compressor, reusable from one frame to the next (it
/// does not change the result).
pub struct Encoder {
    head: Vec<u32>,
    prev: Vec<u32>,
    /// Tokens: `dist == 0` is the literal `len`.
    toks: Vec<(u32, u32)>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn hash4(b: &[u8], i: usize) -> usize {
    let v = u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"));
    (v.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
}

/// Common length of `b[a..]` and `b[i..]` (a < i), at most `max`.
#[inline]
fn common(b: &[u8], a: usize, i: usize, max: usize) -> usize {
    let mut l = 0;
    while l + 8 <= max {
        let x = u64::from_le_bytes(b[a + l..a + l + 8].try_into().expect("8 bytes"));
        let y = u64::from_le_bytes(b[i + l..i + l + 8].try_into().expect("8 bytes"));
        if x != y {
            return l + ((x ^ y).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < max && b[a + l] == b[i + l] {
        l += 1;
    }
    l
}

impl Encoder {
    pub fn new() -> Self {
        Encoder { head: vec![0; 1 << HASH_BITS], prev: vec![0; FRAME], toks: Vec::new() }
    }

    #[inline]
    fn insert(&mut self, b: &[u8], i: usize) {
        if i + MIN_MATCH <= b.len() {
            let h = hash4(b, i);
            self.prev[i] = self.head[h];
            self.head[h] = i as u32 + 1;
        }
    }

    /// Longest earlier match for position `i`: (length, distance).
    #[inline]
    fn find(&self, b: &[u8], i: usize) -> (usize, usize) {
        if i + MIN_MATCH > b.len() {
            return (0, 0);
        }
        let max = (b.len() - i).min(MAX_MATCH);
        let mut cand = self.head[hash4(b, i)];
        let (mut best, mut dist) = (0usize, 0usize);
        let mut chain = MAX_CHAIN;
        while cand != 0 && chain > 0 {
            let c = cand as usize - 1;
            if best < max && b[c + best] == b[i + best] {
                let l = common(b, c, i, max);
                if l > best {
                    best = l;
                    dist = i - c;
                    if l == max {
                        break;
                    }
                }
            }
            cand = self.prev[c];
            chain -= 1;
        }
        if best < MIN_MATCH { (0, 0) } else { (best, dist) }
    }

    /// Compresses `src` (at most [`FRAME`] bytes) appending to `out`.
    pub fn compress(&mut self, src: &[u8], out: &mut Vec<u8>) {
        assert!(src.len() <= FRAME, "frame of {} bytes", src.len());
        self.head.fill(0);
        self.toks.clear();
        let n = src.len();
        let mut i = 0;
        while i < n {
            let (l0, d0) = self.find(src, i);
            self.insert(src, i);
            if l0 == 0 {
                self.toks.push((u32::from(src[i]), 0));
                i += 1;
                continue;
            }
            let (mut l, mut d) = (l0, d0);
            if l0 < LAZY_MAX && i + 1 < n {
                let (l1, d1) = self.find(src, i + 1);
                if l1 > l0 {
                    self.toks.push((u32::from(src[i]), 0));
                    i += 1;
                    self.insert(src, i);
                    (l, d) = (l1, d1);
                }
            }
            self.toks.push((l as u32, d as u32));
            for p in i + 1..i + l {
                self.insert(src, p);
            }
            i += l;
        }
        let mut ll = [0u32; LL_CODES];
        let mut dd = [0u32; DIST_CODES];
        for &(l, d) in &self.toks {
            if d == 0 {
                ll[l as usize] += 1;
            } else {
                ll[256 + bucket(l - MIN_MATCH as u32).0] += 1;
                dd[bucket(d - 1).0] += 1;
            }
        }
        let ll_len = code_lengths(&ll, MAX_BITS);
        let dd_len = code_lengths(&dd, MAX_BITS);
        let all: Vec<u8> = ll_len.iter().chain(dd_len.iter()).copied().collect();
        for p in all.chunks(2) {
            out.push(p[0] | p.get(1).copied().unwrap_or(0) << 4);
        }
        let (ll_code, dd_code) = (codes(&ll_len), codes(&dd_len));
        let mut w = BitWriter { out, acc: 0, n: 0 };
        for &(l, d) in &self.toks {
            if d == 0 {
                w.put(u32::from(ll_code[l as usize]), u32::from(ll_len[l as usize]));
            } else {
                let (c, bits, extra) = bucket(l - MIN_MATCH as u32);
                w.put(u32::from(ll_code[256 + c]), u32::from(ll_len[256 + c]));
                w.put(extra, bits);
                let (c, bits, extra) = bucket(d - 1);
                w.put(u32::from(dd_code[c]), u32::from(dd_len[c]));
                w.put(extra, bits);
            }
        }
        w.finish();
    }
}

// ---- Decompression --------------------------------------------------------------

/// Decompresses the frame `src` into `dst`, which must come out exactly full.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<()> {
    if dst.len() > FRAME {
        return Err(Error::invalid("frame too long"));
    }
    if src.len() < HEADER {
        return Err(Error::Truncated);
    }
    let mut lens = [0u8; LL_CODES + DIST_CODES];
    for (i, l) in lens.iter_mut().enumerate() {
        *l = (src[i / 2] >> (4 * (i % 2))) & 15;
    }
    let mut ll = [0u16; TABLE];
    let mut dd = [0u16; TABLE];
    table(&lens[..LL_CODES], &mut ll)?;
    table(&lens[LL_CODES..], &mut dd)?;
    let mut r = BitReader { src: &src[HEADER..], pos: 0, acc: 0, n: 0 };
    let mut o = 0usize;
    let end = dst.len();
    while o < end {
        r.refill();
        let s = r.symbol(&ll)?;
        if s < 256 {
            dst[o] = s as u8;
            o += 1;
            continue;
        }
        let code = s - 256;
        if code >= LEN_CODES {
            return Err(Error::invalid("frame: length code"));
        }
        let (base, bits) = LEN_BASE[code];
        let len = (base + r.bits(bits)) as usize + MIN_MATCH;
        let dc = r.symbol(&dd)?;
        let (base, bits) = DIST_BASE[dc];
        let dist = (base + r.bits(bits)) as usize + 1;
        if dist > o || len > end - o {
            return Err(Error::invalid("frame: copy outside the frame"));
        }
        if dist == 1 {
            let v = dst[o - 1];
            dst[o..o + len].fill(v);
        } else if dist >= len {
            dst.copy_within(o - dist..o - dist + len, o);
        } else {
            for k in 0..len {
                dst[o + k] = dst[o + k - dist];
            }
        }
        o += len;
    }
    r.check()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &[u8]) -> usize {
        let mut e = Encoder::new();
        let mut c = Vec::new();
        e.compress(src, &mut c);
        let mut d = vec![0u8; src.len()];
        decompress(&c, &mut d).unwrap();
        assert_eq!(d, src);
        let mut c2 = Vec::new();
        e.compress(src, &mut c2);
        assert_eq!(c, c2, "deterministic, whatever the encoder did before");
        c.len()
    }

    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn buckets() {
        for v in (0..300_000u32).step_by(7).chain([15, 16, 23, 24, 31, 32, 65535, 262_143]) {
            let (c, bits, extra) = bucket(v);
            let (base, b) = code_base(c);
            assert_eq!((b, base + extra), (bits, v), "value {v}");
            assert!(extra < 1 << bits || bits == 0);
        }
        assert_eq!(bucket((MAX_MATCH - MIN_MATCH) as u32).0, LEN_CODES - 1);
        assert_eq!(bucket(FRAME as u32 - 1).0, DIST_CODES - 1);
    }

    #[test]
    fn lengths_are_limited_and_complete() {
        // Fibonacci frequencies give the deepest trees.
        let mut f = vec![1u32, 1];
        while f.len() < 40 {
            f.push(f[f.len() - 1] + f[f.len() - 2]);
        }
        let l = code_lengths(&f, MAX_BITS);
        assert!(l.iter().all(|&x| (1..=12).contains(&x)));
        let kraft: f64 = l.iter().map(|&x| 0.5f64.powi(x.into())).sum();
        assert!((kraft - 1.0).abs() < 1e-9, "complete code: {kraft}");
        assert_eq!(code_lengths(&[0, 5, 0], 12), vec![0, 1, 0]);
        assert_eq!(code_lengths(&[3, 5], 12), vec![1, 1]);
    }

    #[test]
    fn roundtrips() {
        assert_eq!(roundtrip(&[]), HEADER);
        roundtrip(b"a");
        roundtrip(b"abcd");
        assert!(roundtrip(&[0u8; FRAME]) < HEADER + 64, "one long run");
        let text: Vec<u8> = b"the machine is deterministic. ".iter().cycle().take(FRAME).copied().collect();
        assert!(roundtrip(&text) < HEADER + 400);
        let n = noise(FRAME, 7);
        assert!(roundtrip(&n) < FRAME + FRAME / 50);
        // Pages repeated far apart: long distances.
        let mut far = noise(4096, 3);
        far.extend(noise(FRAME - 8192, 9).iter().map(|b| b & 3));
        far.extend(noise(4096, 3));
        assert!(roundtrip(&far) < FRAME / 3);
        // Skewed bytes: fewer bits than 8 per literal.
        let skew: Vec<u8> = noise(FRAME, 11).iter().map(|b| b.leading_zeros() as u8).collect();
        assert!(roundtrip(&skew) < FRAME / 3);
        for len in [1, 5, 4095, 4097, 100_003] {
            roundtrip(&noise(len, len as u32));
        }
    }

    #[test]
    fn rejects_damaged_frames() {
        let src: Vec<u8> = b"vetro ".iter().cycle().take(20_000).copied().collect();
        let mut c = Vec::new();
        Encoder::new().compress(&src, &mut c);
        let mut d = vec![0u8; src.len()];
        assert!(decompress(&c[..c.len() / 2], &mut d).is_err(), "truncated");
        assert!(decompress(&c[..10], &mut d).is_err(), "no header");
        let mut bad = c.clone();
        bad[0] = 0xff;
        bad[1] = 0xff;
        assert!(decompress(&bad, &mut d).is_err() || d != src, "over-subscribed or wrong");
        let mut longer = vec![0u8; src.len() + 1];
        assert!(decompress(&c, &mut longer).is_err(), "stream ends early");
        // Any damage is either rejected or detected by the snapshot checksum;
        // it never panics.
        for i in 0..c.len() {
            let mut x = c.clone();
            x[i] ^= 0x5a;
            let _ = decompress(&x, &mut d);
        }
    }
}
