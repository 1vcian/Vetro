//! Saving and restoring the machine state (M6, ADR 0015).
//!
//! This crate does not know the machine: it provides the file format and the
//! tools to write and read it, with no dependencies (it also compiles for
//! wasm32). Each crate implements [`Snapshot`] for its own state; it is
//! `vetro-machine` that puts the sections together (`Machine::save`).
//!
//! - [`Writer`] / [`Reader`]: fixed-width little endian integers,
//!   booleans, length-prefixed bytes, sections with tag and length;
//! - [`compress`]: large data (RAM, GPU images, in-memory disks)
//!   in 4 KiB blocks, all-zero blocks omitted, the others compressed with a
//!   simple LZ ([`lz`]);
//! - [`encode_file`] / [`decode_file`]: header (magic, format
//!   version, configuration hash, length, checksum) and
//!   content;
//! - [`overlay`]: the file of a disk's persistent copy-on-write
//!   overlay (ADR 0016).
//!
//! Determinism: the same machine in the same state gives the same bytes.
//! No hash tables, no clocks, no pointers in the format.
//! Specification in `docs/specs/snapshot.md`.

pub mod blocks;
pub mod deferred;
pub mod lz;
pub mod lzh;
pub mod overlay;

pub use blocks::Level;

use core::fmt;

/// First 8 bytes of every snapshot.
pub const MAGIC: [u8; 8] = *b"VETROSNP";

/// Format version. Changes with every modification of what is written (new
/// fields, order, encodings): a snapshot of another version is rejected
/// with [`Error::Version`], without attempting conversions.
///
/// - 1: M6 (ADR 0015).
/// - 2: M5, port forwarding (host connections in the network stack).
/// - 3: M10, the host-to-guest frames queued in the network link
///   of `vetro-machine` (`NetLink`, ADR 0019).
/// - 4: M6, blocks equal to an earlier one and frames of blocks compressed
///   with LZ77 and Huffman codes ([`blocks`], ADR 0031).
pub const FORMAT_VERSION: u32 = 4;

/// Header bytes: magic, version, configuration hash,
/// content length, content checksum.
pub const HEADER_LEN: usize = 8 + 4 + 8 + 8 + 8;

/// Block size of [`compress`] (one guest page).
pub const BLOCK: usize = 4096;

/// Why a snapshot cannot be read or applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Does not start with [`MAGIC`]: not a Vetro snapshot.
    BadMagic,
    /// Format of another Vetro version.
    Version { found: u32, expected: u32 },
    /// Snapshot of a machine configured differently (RAM,
    /// devices, seed, ...).
    Config { found: u64, expected: u64 },
    /// The content does not match the checksum (corrupted file).
    Checksum,
    /// Ended earlier than expected.
    Truncated,
    /// Expected section and section found.
    Section { expected: [u8; 4], found: [u8; 4] },
    /// Leftover bytes at the end of a section.
    Trailing { section: [u8; 4], bytes: usize },
    /// A value the state cannot have.
    Invalid(String),
}

impl Error {
    /// Error for a value outside the domain.
    pub fn invalid(what: impl Into<String>) -> Self {
        Error::Invalid(what.into())
    }
}

fn tag_str(t: &[u8; 4]) -> String {
    String::from_utf8_lossy(t).into_owned()
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic => write!(f, "not a Vetro snapshot (unknown header)"),
            Error::Version { found, expected } => write!(
                f,
                "snapshot in format version {found}, this version of Vetro only reads version {expected}: \
                 it must be taken again with this version"
            ),
            Error::Config { found, expected } => write!(
                f,
                "snapshot of a machine configured differently (hash {found:016x}, this machine \
                 {expected:016x}): the same RAM, devices and seed are required"
            ),
            Error::Checksum => write!(f, "corrupted snapshot (wrong checksum)"),
            Error::Truncated => write!(f, "truncated snapshot"),
            Error::Section { expected, found } => {
                write!(f, "section {:?} instead of {:?}", tag_str(found), tag_str(expected))
            }
            Error::Trailing { section, bytes } => {
                write!(f, "{bytes} extra bytes at the end of section {:?}", tag_str(section))
            }
            Error::Invalid(what) => write!(f, "invalid value in the snapshot: {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

/// State that is saved and restored.
///
/// `restore` starts from an object built with the same configuration
/// (the part that is not saved: sizes, external backends) and brings it to the
/// saved state. After `restore`, `save` must give the same bytes that were read.
pub trait Snapshot {
    fn save(&self, w: &mut Writer);
    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()>;
}

// ---- Writing ----------------------------------------------------------------

/// Write buffer: everything little endian, fixed width.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
    level: Level,
    /// A deferred writer ([`Writer::deferred`]): the large data and the
    /// section lengths that depend on it, left to [`deferred::Assembler`].
    marks: Option<Vec<deferred::Mark>>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer { buf: Vec::with_capacity(n), level: Level::Fast, marks: None }
    }

    /// A deferred writer (ADR 0046): [`compress`] copies the data as it is
    /// and notes where it is, [`Writer::section`] notes where its length
    /// goes; [`Writer::into_plan`] gives those marks, and
    /// [`deferred::Assembler`] (on another thread) turns the bytes into the
    /// file a normal writer at [`Level::Fast`] would have written.
    pub fn deferred(n: usize) -> Self {
        Writer { buf: Vec::with_capacity(n), level: Level::Fast, marks: Some(Vec::new()) }
    }

    /// True for a [`Writer::deferred`].
    pub fn is_deferred(&self) -> bool {
        self.marks.is_some()
    }

    /// The last part of a deferred writer's content: section `tag` holding
    /// `len` bytes of large data (as [`compress`] writes it) that are not in
    /// this writer: the caller streams them after its bytes (the machine's
    /// RAM, page by page).
    pub fn external_section(&mut self, tag: &[u8; 4], len: u64) {
        self.raw(tag);
        let at = self.buf.len() as u64;
        self.u64(0);
        let marks = self.marks.as_mut().expect("external_section on a deferred writer");
        marks.push(deferred::Mark::Section { at, end: at + 8 + len });
        marks.push(deferred::Mark::Blocks { at: at + 8, len });
    }

    /// The marks and bytes of a deferred writer, with `extra` bytes streamed
    /// after them ([`Writer::external_section`]) and the configuration hash
    /// of the file header.
    pub fn into_plan(self, config_hash: u64, extra: u64) -> (deferred::Plan, Vec<u8>) {
        let marks = self.marks.expect("into_plan on a deferred writer");
        (deferred::Plan { config_hash, marks, raw_len: self.buf.len() as u64 + extra }, self.buf)
    }

    /// How [`compress`] compresses in this writer (default [`Level::Fast`]).
    pub fn set_level(&mut self, level: Level) {
        self.level = level;
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    /// A length or a number of elements (u64).
    pub fn len_of(&mut self, n: usize) {
        self.u64(n as u64);
    }
    /// Bytes without a length (the reader knows how many there are).
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    /// Bytes preceded by their length.
    pub fn bytes(&mut self, b: &[u8]) {
        self.len_of(b.len());
        self.raw(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    /// `None` = 0; `Some` = 1 followed by the value.
    pub fn opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Self, T)) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                f(self, x);
            }
        }
    }
    pub fn opt_u64(&mut self, v: Option<u64>) {
        self.opt(v, Self::u64);
    }
    /// A sequence: number of elements, then `f` for each one.
    pub fn seq<I: IntoIterator>(&mut self, items: I, mut f: impl FnMut(&mut Self, I::Item))
    where
        I::IntoIter: ExactSizeIterator,
    {
        let it = items.into_iter();
        self.len_of(it.len());
        for x in it {
            f(self, x);
        }
    }
    /// Section: 4-byte tag, length (u64), content written by `f`.
    pub fn section(&mut self, tag: &[u8; 4], f: impl FnOnce(&mut Self)) {
        self.raw(tag);
        let at = self.buf.len();
        self.u64(0);
        let mark = self.marks.as_mut().map(|m| {
            m.push(deferred::Mark::Section { at: at as u64, end: 0 });
            m.len() - 1
        });
        f(self);
        let len = (self.buf.len() - at - 8) as u64;
        self.buf[at..at + 8].copy_from_slice(&len.to_le_bytes());
        if let (Some(i), Some(m)) = (mark, self.marks.as_mut()) {
            m[i] = deferred::Mark::Section { at: at as u64, end: self.buf.len() as u64 };
        }
    }
    /// A state that implements [`Snapshot`].
    pub fn put<S: Snapshot + ?Sized>(&mut self, s: &S) {
        s.save(self);
    }
}

// ---- Reading ----------------------------------------------------------------

/// Read buffer; every read past the end is [`Error::Truncated`].
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Tag of the section (for error messages).
    tag: [u8; 4],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0, tag: *b"    " }
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated);
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.raw(N)?.try_into().expect("length checked"))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.raw(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        self.array().map(u16::from_le_bytes)
    }
    pub fn u32(&mut self) -> Result<u32> {
        self.array().map(u32::from_le_bytes)
    }
    pub fn u64(&mut self) -> Result<u64> {
        self.array().map(u64::from_le_bytes)
    }
    pub fn u128(&mut self) -> Result<u128> {
        self.array().map(u128::from_le_bytes)
    }
    /// Only 0 or 1.
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(Error::invalid(format!("boolean {v}"))),
        }
    }
    /// A length, which cannot exceed the remaining bytes divided by `min_item`
    /// (every element takes at least `min_item` bytes): a corrupted file does not
    /// make us allocate memory at random.
    pub fn len_of(&mut self, min_item: usize) -> Result<usize> {
        let n = self.u64()?;
        let max = (self.remaining() / min_item.max(1)) as u64;
        if n > max {
            return Err(Error::Truncated);
        }
        Ok(n as usize)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.len_of(1)?;
        self.raw(n)
    }
    pub fn vec(&mut self) -> Result<Vec<u8>> {
        self.bytes().map(<[u8]>::to_vec)
    }
    pub fn string(&mut self) -> Result<String> {
        String::from_utf8(self.vec()?).map_err(|_| Error::invalid("non-UTF-8 string"))
    }
    pub fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        match self.u8()? {
            0 => Ok(None),
            1 => f(self).map(Some),
            v => Err(Error::invalid(format!("option {v}"))),
        }
    }
    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        self.opt(Self::u64)
    }
    /// A sequence written with [`Writer::seq`]; every element takes at least
    /// `min_item` bytes.
    pub fn seq<T>(&mut self, min_item: usize, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let n = self.len_of(min_item)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }
    /// The next section, which must have tag `tag`: the reader of its
    /// content. It must be closed with [`Reader::finish`].
    pub fn section(&mut self, tag: &[u8; 4]) -> Result<Reader<'a>> {
        let found: [u8; 4] = self.array()?;
        if &found != tag {
            return Err(Error::Section { expected: *tag, found });
        }
        let n = self.u64()?;
        if n > self.remaining() as u64 {
            return Err(Error::Truncated);
        }
        let body = self.raw(n as usize)?;
        Ok(Reader { buf: body, pos: 0, tag: *tag })
    }
    /// Tag of the next section, without consuming it.
    pub fn peek_tag(&self) -> Option<[u8; 4]> {
        self.buf.get(self.pos..self.pos + 4).map(|t| t.try_into().expect("4 bytes"))
    }
    /// Checks that the section has been read completely.
    pub fn finish(&self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            bytes => Err(Error::Trailing { section: self.tag, bytes }),
        }
    }
    /// Restores a state that implements [`Snapshot`].
    pub fn get<S: Snapshot + ?Sized>(&mut self, s: &mut S) -> Result<()> {
        s.restore(self)
    }
    /// Checks that a saved configuration value is the expected one.
    pub fn expect_u64(&mut self, what: &str, expected: u64) -> Result<()> {
        let v = self.u64()?;
        if v != expected {
            return Err(Error::invalid(format!("{what}: {v} in the snapshot, {expected} in this machine")));
        }
        Ok(())
    }
}

// ---- Checksum and hash -----------------------------------------------------

/// Non-cryptographic 64-bit hash (FNV-1a over 8-byte words with a
/// final mix): content checksum and configuration
/// hash. Stable across platforms and compiler versions.
pub fn hash64(data: &[u8]) -> u64 {
    const P: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ data.len() as u64;
    let (words, rest) = data.as_chunks::<8>();
    for c in words {
        h = (h ^ u64::from_le_bytes(*c)).wrapping_mul(P).rotate_left(23);
    }
    for &b in rest {
        h = (h ^ u64::from(b)).wrapping_mul(P);
    }
    // SplitMix64 mixing.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// [`hash64`] in pieces, with the total length known upfront (the chunked
/// saves, ADR 0028 and 0046).
#[derive(Clone, Debug)]
pub struct Hash64 {
    h: u64,
    carry: [u8; 8],
    n: usize,
}

impl Hash64 {
    const P: u64 = 0x0000_0100_0000_01b3;

    /// For data of `total` bytes in all.
    pub fn new(total: u64) -> Self {
        Hash64 { h: 0xcbf2_9ce4_8422_2325 ^ total, carry: [0; 8], n: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        if self.n > 0 {
            let k = (8 - self.n).min(data.len());
            self.carry[self.n..self.n + k].copy_from_slice(&data[..k]);
            self.n += k;
            data = &data[k..];
            if self.n < 8 {
                return;
            }
            self.h = (self.h ^ u64::from_le_bytes(self.carry)).wrapping_mul(Self::P).rotate_left(23);
            self.n = 0;
        }
        let (words, rest) = data.as_chunks::<8>();
        for c in words {
            self.h = (self.h ^ u64::from_le_bytes(*c)).wrapping_mul(Self::P).rotate_left(23);
        }
        self.carry[..rest.len()].copy_from_slice(rest);
        self.n = rest.len();
    }

    pub fn finish(self) -> u64 {
        let mut h = self.h;
        for &b in &self.carry[..self.n] {
            h = (h ^ u64::from(b)).wrapping_mul(Self::P);
        }
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^ (h >> 31)
    }
}

// ---- File -------------------------------------------------------------------

/// Complete snapshot: header and `payload` (the sections).
pub fn encode_file(config_hash: u64, payload: &[u8]) -> Vec<u8> {
    encode_container(&MAGIC, FORMAT_VERSION, config_hash, payload)
}

/// A file with the snapshot header but another magic and another
/// version (e.g. M10's recording log, `b"VETROREC"`): magic, u32
/// version, u64 configuration hash, u64 length, u64 [`hash64`]
/// of the content, content.
pub fn encode_container(magic: &[u8; 8], version: u32, config_hash: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&config_hash.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&hash64(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Header read by [`decode_file`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub config_hash: u64,
}

/// Reads the header and checks magic, version, length and
/// checksum; returns the content. The configuration is checked by the
/// caller (comparison of `config_hash`).
pub fn decode_file(bytes: &[u8]) -> Result<(Header, &[u8])> {
    decode_container(&MAGIC, FORMAT_VERSION, bytes)
}

/// Like [`decode_file`] for a file of [`encode_container`] with magic
/// `magic` and version `version`.
pub fn decode_container<'a>(magic: &[u8; 8], version: u32, bytes: &'a [u8]) -> Result<(Header, &'a [u8])> {
    if bytes.len() < 8 || bytes[..8] != *magic {
        return Err(Error::BadMagic);
    }
    // The version before everything else: a different format may have
    // a different header.
    let mut r = Reader::new(&bytes[8..]);
    let found = r.u32()?;
    if found != version {
        return Err(Error::Version { found, expected: version });
    }
    let config_hash = r.u64()?;
    let len = r.u64()?;
    let sum = r.u64()?;
    if len != r.remaining() as u64 {
        return Err(Error::Truncated);
    }
    let payload = r.raw(len as usize)?;
    if hash64(payload) != sum {
        return Err(Error::Checksum);
    }
    Ok((Header { version: found, config_hash }, payload))
}

// ---- Large data in blocks ---------------------------------------------------

/// Writes `data` in blocks of [`BLOCK`] bytes ([`blocks`]): zero blocks take
/// nothing, the others are compressed at the writer's [`Level`].
pub fn compress(w: &mut Writer, data: &[u8]) {
    if let Some(m) = w.marks.as_mut() {
        m.push(deferred::Mark::Blocks { at: w.buf.len() as u64, len: data.len() as u64 });
        w.buf.extend_from_slice(data);
        return;
    }
    let level = w.level;
    blocks::encode(data.len(), level, &|i| &data[i * BLOCK..(i * BLOCK + BLOCK).min(data.len())], &mut |c| {
        w.raw(c)
    });
}

/// Reads data written with [`compress`] into `out`, which must have the same
/// length and be zero already (zero blocks are not written). `visit(index)`
/// is called for every block written.
pub fn decompress_into(r: &mut Reader<'_>, out: &mut [u8], mut visit: impl FnMut(usize)) -> Result<()> {
    blocks::decode(r, out.len(), &mut blocks::Slice(out), &mut visit)
}

/// Like [`decompress_into`], into a new vector.
pub fn decompress(r: &mut Reader<'_>) -> Result<Vec<u8>> {
    let len = r.clone().u64()?;
    // Limit against corrupted files (the checksum already stops them).
    if len > 1 << 40 {
        return Err(Error::invalid(format!("data of {len} bytes")));
    }
    let mut out = vec![0u8; len as usize];
    decompress_into(r, &mut out, |_| {})?;
    Ok(out)
}

/// True if `b` is all zero.
pub fn is_zero(b: &[u8]) -> bool {
    let (words, rest) = b.as_chunks::<8>();
    let acc = words.iter().fold(0u64, |a, c| a | u64::from_le_bytes(*c));
    acc == 0 && rest.iter().all(|&x| x == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interi_sezioni_e_opzioni() {
        let mut w = Writer::new();
        w.section(b"PROV", |w| {
            w.u8(7);
            w.u16(0x1234);
            w.u32(0xdead_beef);
            w.u64(u64::MAX);
            w.u128(1 << 100);
            w.bool(true);
            w.opt_u64(None);
            w.opt_u64(Some(5));
            w.str("città");
            w.seq([1u32, 2, 3], |w, x| w.u32(x));
        });
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        let mut s = r.section(b"PROV").unwrap();
        assert_eq!(s.u8().unwrap(), 7);
        assert_eq!(s.u16().unwrap(), 0x1234);
        assert_eq!(s.u32().unwrap(), 0xdead_beef);
        assert_eq!(s.u64().unwrap(), u64::MAX);
        assert_eq!(s.u128().unwrap(), 1 << 100);
        assert!(s.bool().unwrap());
        assert_eq!(s.opt_u64().unwrap(), None);
        assert_eq!(s.opt_u64().unwrap(), Some(5));
        assert_eq!(s.string().unwrap(), "città");
        assert_eq!(s.seq(4, |r| r.u32()).unwrap(), [1, 2, 3]);
        s.finish().unwrap();
        r.finish().unwrap();
        assert_eq!(
            Reader::new(&bytes).section(b"ALTR").unwrap_err(),
            Error::Section { expected: *b"ALTR", found: *b"PROV" }
        );
    }

    #[test]
    fn letture_oltre_la_fine_e_valori_invalidi() {
        let mut r = Reader::new(&[1, 2]);
        assert_eq!(r.u32(), Err(Error::Truncated));
        let mut r = Reader::new(&[2]);
        assert!(matches!(r.bool(), Err(Error::Invalid(_))));
        // A huge length does not allocate: it is rejected.
        let mut w = Writer::new();
        w.u64(u64::MAX / 2);
        let b = w.into_bytes();
        assert_eq!(Reader::new(&b).bytes(), Err(Error::Truncated));
        let mut w = Writer::new();
        w.section(b"ABCD", |w| w.u8(1));
        let b = w.into_bytes();
        let s = Reader::new(&b).section(b"ABCD").unwrap();
        assert_eq!(s.finish(), Err(Error::Trailing { section: *b"ABCD", bytes: 1 }));
    }

    #[test]
    fn file_con_versione_configurazione_e_somma() {
        let f = encode_file(42, b"contenuto");
        let (h, p) = decode_file(&f).unwrap();
        assert_eq!(h, Header { version: FORMAT_VERSION, config_hash: 42 });
        assert_eq!(p, b"contenuto");

        let mut other = f.clone();
        other[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let e = decode_file(&other).unwrap_err();
        assert_eq!(e, Error::Version { found: FORMAT_VERSION + 1, expected: FORMAT_VERSION });
        assert!(e.to_string().contains("version"), "{e}");

        let mut bad = f.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(decode_file(&bad).unwrap_err(), Error::Checksum);
        assert_eq!(decode_file(&f[..f.len() - 1]).unwrap_err(), Error::Truncated);
        assert_eq!(decode_file(b"ELF\x7f....").unwrap_err(), Error::BadMagic);
    }

    /// A container with another magic and another version: same
    /// header, and a snapshot is not mistaken for a container (nor
    /// vice versa).
    #[test]
    fn contenitore_con_altra_magia() {
        let f = encode_container(b"VETROREC", 7, 9, b"eventi");
        let (h, p) = decode_container(b"VETROREC", 7, &f).unwrap();
        assert_eq!(h, Header { version: 7, config_hash: 9 });
        assert_eq!(p, b"eventi");
        assert_eq!(
            decode_container(b"VETROREC", 8, &f).unwrap_err(),
            Error::Version { found: 7, expected: 8 }
        );
        assert_eq!(decode_file(&f).unwrap_err(), Error::BadMagic);
        assert_eq!(decode_container(b"VETROREC", 7, &encode_file(9, b"x")).unwrap_err(), Error::BadMagic);
        assert_eq!(&encode_file(3, b"abc")[..], &encode_container(&MAGIC, FORMAT_VERSION, 3, b"abc")[..]);
    }

    const HASH_VETRO: u64 = 0x2e59_3998_1c8e_031d;

    #[test]
    fn hash_stabile() {
        // Fixed values: the hash is part of the format and must not change.
        assert_eq!(hash64(b""), hash64(b""));
        assert_ne!(hash64(b"a"), hash64(b"b"));
        assert_ne!(hash64(&[0; 8]), hash64(&[0; 9]));
        assert_eq!(hash64(b"vetro-snapshot"), HASH_VETRO, "{:#x}", hash64(b"vetro-snapshot"));
    }

    #[test]
    fn blocchi_a_zero_omessi_e_compressione() {
        let mut data = vec![0u8; 10 * BLOCK + 100];
        data[BLOCK * 3..BLOCK * 3 + 11].copy_from_slice(b"hello world");
        for (i, b) in data[BLOCK * 7..BLOCK * 8].iter_mut().enumerate() {
            *b = ((i * 2_654_435_761usize) >> 7) as u8; // barely compressible
        }
        data[10 * BLOCK + 50] = 9; // short last block
        let mut w = Writer::new();
        compress(&mut w, &data);
        let b = w.into_bytes();
        assert!(b.len() < 2 * BLOCK, "{} bytes", b.len());
        let mut seen = Vec::new();
        let mut out = vec![0u8; data.len()];
        decompress_into(&mut Reader::new(&b), &mut out, |i| seen.push(i)).unwrap();
        assert_eq!(out, data);
        assert_eq!(seen, [3, 7, 10]);
        assert_eq!(decompress(&mut Reader::new(&b)).unwrap(), data);
        // Different length: clear error.
        let mut short = vec![0u8; 5];
        assert!(matches!(decompress_into(&mut Reader::new(&b), &mut short, |_| {}), Err(Error::Invalid(_))));
    }
}
