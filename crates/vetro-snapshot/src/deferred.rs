//! Deferred saves (ADR 0046): the machine's thread copies the state as it is
//! (a [`Writer::deferred`](crate::Writer::deferred): large data uncompressed,
//! section lengths still those of the uncompressed content), and another
//! thread turns that copy into the very file a normal save at
//! [`Level::Fast`](crate::Level::Fast) writes: compressed blocks, section
//! lengths, header with length and checksum.
//!
//! The copy is a *plan* ([`Plan`]: configuration hash, total length of the
//! copy, [`Mark`]s) and a stream of bytes, the *raw stream*, handed to
//! [`Assembler::push`] in pieces of any size. The output goes to an [`Out`]
//! (a file with positioned writes and reads): section lengths and block
//! counts are written once known, earlier blocks are read back to confirm a
//! repeated block byte by byte (as [`blocks::encode`](crate::blocks::encode)
//! does in memory), and at the end the content is read back once for the
//! checksum, whose seed is the content length.

use std::collections::HashMap;

use crate::{BLOCK, Error, HEADER_LEN, Hash64, Result, hash64, is_zero, lz};

/// What a position of the raw stream holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The u64 length of a section at raw position `at`; the section's
    /// content ends at raw position `end`.
    Section { at: u64, end: u64 },
    /// `len` bytes of large data from raw position `at`, written by
    /// [`compress`](crate::compress) in the file.
    Blocks { at: u64, len: u64 },
}

impl Mark {
    fn at(&self) -> u64 {
        match *self {
            Mark::Section { at, .. } | Mark::Blocks { at, .. } => at,
        }
    }
}

/// What the assembler needs besides the raw stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Configuration hash of the file header.
    pub config_hash: u64,
    /// The marks in increasing order of `at` (a section before what it holds).
    pub marks: Vec<Mark>,
    /// Bytes of the raw stream.
    pub raw_len: u64,
}

impl Plan {
    /// The plan as bytes (to hand it to another thread or module): u64
    /// configuration hash, u64 raw length, u64 marks, each u8 kind (0
    /// section, 1 blocks) and two u64.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.marks.len() * 17);
        out.extend_from_slice(&self.config_hash.to_le_bytes());
        out.extend_from_slice(&self.raw_len.to_le_bytes());
        out.extend_from_slice(&(self.marks.len() as u64).to_le_bytes());
        for m in &self.marks {
            let (k, a, b) = match *m {
                Mark::Section { at, end } => (0u8, at, end),
                Mark::Blocks { at, len } => (1u8, at, len),
            };
            out.push(k);
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&b.to_le_bytes());
        }
        out
    }

    /// Reads [`Plan::encode`] and checks that the marks are consistent.
    pub fn decode(bytes: &[u8]) -> Result<Plan> {
        let mut r = crate::Reader::new(bytes);
        let config_hash = r.u64()?;
        let raw_len = r.u64()?;
        let n = r.len_of(17)?;
        let mut marks = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.u8()?;
            let (a, b) = (r.u64()?, r.u64()?);
            marks.push(match k {
                0 => Mark::Section { at: a, end: b },
                1 => Mark::Blocks { at: a, len: b },
                _ => return Err(Error::invalid(format!("plan mark {k}"))),
            });
        }
        r.finish()?;
        let plan = Plan { config_hash, marks, raw_len };
        plan.check()?;
        Ok(plan)
    }

    /// Marks in order, sections nested, blocks inside their section and not
    /// overlapping anything that follows.
    fn check(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::invalid(format!("plan: {what}")));
        let mut open: Vec<u64> = Vec::new();
        let mut last = 0u64;
        // End of the last blocks (nothing else may start inside them).
        let mut busy = 0u64;
        for m in &self.marks {
            let at = m.at();
            if at < last || at < busy {
                return bad("marks out of order");
            }
            last = at;
            while open.last().is_some_and(|&e| e <= at) {
                open.pop();
            }
            let end = match *m {
                Mark::Section { at, end } if end >= at + 8 => end,
                Mark::Blocks { at, len } => {
                    let end = at.checked_add(len).ok_or(Error::Truncated)?;
                    busy = end;
                    end
                }
                _ => return bad("section shorter than its length"),
            };
            if end > self.raw_len || open.last().is_some_and(|&e| end > e) {
                return bad("mark past its section");
            }
            if let Mark::Section { .. } = m {
                open.push(end);
            }
        }
        Ok(())
    }
}

/// Where the assembled file goes: positioned writes, and reads of what was
/// written (repeated blocks, the checksum).
pub trait Out {
    fn write_at(&mut self, at: u64, bytes: &[u8]) -> Result<()>;
    fn read_at(&mut self, at: u64, buf: &mut [u8]) -> Result<()>;
}

impl Out for Vec<u8> {
    fn write_at(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        let (at, end) = (at as usize, at as usize + bytes.len());
        if self.len() < end {
            self.resize(end, 0);
        }
        self[at..end].copy_from_slice(bytes);
        Ok(())
    }
    fn read_at(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        let at = at as usize;
        let src = self.get(at..at + buf.len()).ok_or(Error::Truncated)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// Output written in large pieces, with patches and reads that may fall in
/// the part not yet written.
struct Buffered {
    buf: Vec<u8>,
    /// File position of `buf[0]`.
    at: u64,
}

const FLUSH: usize = 1 << 20;

impl Buffered {
    fn pos(&self) -> u64 {
        self.at + self.buf.len() as u64
    }
    fn put(&mut self, out: &mut dyn Out, b: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(b);
        if self.buf.len() >= FLUSH {
            self.flush(out)?;
        }
        Ok(())
    }
    fn flush(&mut self, out: &mut dyn Out) -> Result<()> {
        if !self.buf.is_empty() {
            out.write_at(self.at, &self.buf)?;
            self.at += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }
    fn patch(&mut self, out: &mut dyn Out, at: u64, b: &[u8]) -> Result<()> {
        if at >= self.at {
            let o = (at - self.at) as usize;
            self.buf[o..o + b.len()].copy_from_slice(b);
            Ok(())
        } else if at + b.len() as u64 <= self.at {
            out.write_at(at, b)
        } else {
            self.flush(out)?;
            out.write_at(at, b)
        }
    }
    fn read(&mut self, out: &mut dyn Out, at: u64, dst: &mut [u8]) -> Result<()> {
        if at < self.at {
            self.flush(out)?;
            return out.read_at(at, dst);
        }
        let o = (at - self.at) as usize;
        let src = self.buf.get(o..o + dst.len()).ok_or(Error::Truncated)?;
        dst.copy_from_slice(src);
        Ok(())
    }
}

/// Where the first block with a hash is: its index, the file position of its
/// bytes, and their length if compressed with [`lz`] (0: raw).
#[derive(Clone, Copy)]
struct First {
    index: usize,
    at: u64,
    lz: u32,
}

/// [`blocks::encode`](crate::blocks::encode) at [`Level::Fast`](crate::Level::Fast)
/// one block at a time.
struct BlockStream {
    len: u64,
    /// Bytes still to come.
    left: u64,
    /// Next block index and its bytes so far.
    index: usize,
    block: Vec<u8>,
    count: u64,
    count_at: u64,
    first: HashMap<u64, First>,
    table: lz::Table,
    scratch: Vec<u8>,
    other: Vec<u8>,
    packed: Vec<u8>,
}

impl BlockStream {
    fn start(o: &mut Buffered, out: &mut dyn Out, len: u64) -> Result<Self> {
        o.put(out, &len.to_le_bytes())?;
        let count_at = o.pos();
        o.put(out, &0u64.to_le_bytes())?;
        Ok(BlockStream {
            len,
            left: len,
            index: 0,
            block: Vec::with_capacity(BLOCK),
            count: 0,
            count_at,
            first: HashMap::new(),
            table: lz::Table::new(),
            scratch: Vec::with_capacity(BLOCK + BLOCK / 8),
            other: vec![0; BLOCK],
            packed: Vec::with_capacity(BLOCK + BLOCK / 8),
        })
    }

    fn block_len(&self, i: usize) -> usize {
        (self.len - (i * BLOCK) as u64).min(BLOCK as u64) as usize
    }

    /// Takes up to the end of the data from `data`; returns how many bytes.
    fn feed(&mut self, o: &mut Buffered, out: &mut dyn Out, mut data: &[u8]) -> Result<usize> {
        let n = data.len().min(self.left as usize);
        data = &data[..n];
        self.left -= n as u64;
        while !data.is_empty() {
            let want = self.block_len(self.index);
            if self.block.is_empty() && data.len() >= want {
                let (b, rest) = data.split_at(want);
                self.one(o, out, b)?;
                data = rest;
                continue;
            }
            let k = (want - self.block.len()).min(data.len());
            self.block.extend_from_slice(&data[..k]);
            data = &data[k..];
            if self.block.len() == want {
                let b = core::mem::take(&mut self.block);
                self.one(o, out, &b)?;
                self.block = b;
                self.block.clear();
            }
        }
        Ok(n)
    }

    /// Block `self.index` with bytes `b`.
    fn one(&mut self, o: &mut Buffered, out: &mut dyn Out, b: &[u8]) -> Result<()> {
        let i = self.index;
        self.index += 1;
        if is_zero(b) {
            return Ok(());
        }
        self.count += 1;
        let h = hash64(b);
        match self.first.get(&h).copied() {
            Some(f) if self.block_len(f.index) == b.len() && self.same(o, out, f, b)? => {
                o.put(out, &(i as u64).to_le_bytes())?;
                o.put(out, &[2])?;
                return o.put(out, &(f.index as u64).to_le_bytes());
            }
            Some(_) => {}
            None => {
                self.first.insert(h, First { index: i, at: 0, lz: 0 });
            }
        }
        o.put(out, &(i as u64).to_le_bytes())?;
        self.scratch.clear();
        lz::compress(b, &mut self.scratch, &mut self.table);
        let first = self.first.get_mut(&h).filter(|f| f.index == i);
        if self.scratch.len() < b.len() {
            o.put(out, &[1])?;
            o.put(out, &(self.scratch.len() as u32).to_le_bytes())?;
            if let Some(f) = first {
                *f = First { index: i, at: o.pos(), lz: self.scratch.len() as u32 };
            }
            o.put(out, &self.scratch)
        } else {
            o.put(out, &[0])?;
            if let Some(f) = first {
                *f = First { index: i, at: o.pos(), lz: 0 };
            }
            o.put(out, b)
        }
    }

    /// True if the earlier block `f` has the bytes `b` (read back from the
    /// output).
    fn same(&mut self, o: &mut Buffered, out: &mut dyn Out, f: First, b: &[u8]) -> Result<bool> {
        let n = b.len();
        if f.lz == 0 {
            o.read(out, f.at, &mut self.other[..n])?;
        } else {
            self.packed.resize(f.lz as usize, 0);
            o.read(out, f.at, &mut self.packed)?;
            lz::decompress(&self.packed, &mut self.other[..n])?;
        }
        Ok(self.other[..n] == *b)
    }

    fn finish(self, o: &mut Buffered, out: &mut dyn Out) -> Result<()> {
        debug_assert!(self.left == 0 && self.block.is_empty());
        o.patch(out, self.count_at, &self.count.to_le_bytes())
    }
}

/// Turns a plan and its raw stream into the snapshot file (see the module).
pub struct Assembler {
    plan: Plan,
    /// Raw bytes consumed.
    rp: u64,
    next: usize,
    /// Open sections: raw end, file position of their length.
    open: Vec<(u64, u64)>,
    blocks: Option<BlockStream>,
    o: Buffered,
}

impl Assembler {
    /// For `plan`; the file content starts after the header.
    pub fn new(plan: Plan) -> Self {
        Assembler {
            plan,
            rp: 0,
            next: 0,
            open: Vec::new(),
            blocks: None,
            o: Buffered { buf: Vec::with_capacity(FLUSH + 2 * BLOCK), at: HEADER_LEN as u64 },
        }
    }

    /// Raw bytes taken so far.
    pub fn consumed(&self) -> u64 {
        self.rp
    }

    /// What happens at the current raw position: blocks that end, sections
    /// that close, marks that start.
    fn events(&mut self, out: &mut dyn Out) -> Result<()> {
        loop {
            if self.blocks.as_ref().is_some_and(|b| b.left == 0) {
                let b = self.blocks.take().expect("blocks");
                b.finish(&mut self.o, out)?;
                continue;
            }
            if self.blocks.is_none()
                && let Some(&(end, at)) = self.open.last()
                && end == self.rp
            {
                self.open.pop();
                let len = self.o.pos() - at - 8;
                self.o.patch(out, at, &len.to_le_bytes())?;
                continue;
            }
            match self.plan.marks.get(self.next) {
                Some(&Mark::Section { at, end }) if at == self.rp && self.blocks.is_none() => {
                    self.next += 1;
                    self.open.push((end, self.o.pos()));
                }
                Some(&Mark::Blocks { at, len }) if at == self.rp && self.blocks.is_none() => {
                    self.next += 1;
                    self.blocks = Some(BlockStream::start(&mut self.o, out, len)?);
                }
                _ => return Ok(()),
            }
        }
    }

    /// The next bytes of the raw stream.
    pub fn push(&mut self, mut data: &[u8], out: &mut dyn Out) -> Result<()> {
        if data.len() as u64 > self.plan.raw_len - self.rp {
            return Err(Error::invalid("deferred save: more bytes than the plan"));
        }
        loop {
            self.events(out)?;
            if data.is_empty() {
                return Ok(());
            }
            if let Some(b) = self.blocks.as_mut() {
                let n = b.feed(&mut self.o, out, data)?;
                self.rp += n as u64;
                data = &data[n..];
                continue;
            }
            // Raw bytes up to the next mark or section end.
            let mut stop = self.plan.raw_len;
            if let Some(m) = self.plan.marks.get(self.next) {
                stop = stop.min(m.at());
            }
            if let Some(&(end, _)) = self.open.last() {
                stop = stop.min(end);
            }
            let n = ((stop - self.rp) as usize).min(data.len());
            self.o.put(out, &data[..n])?;
            self.rp += n as u64;
            data = &data[n..];
        }
    }

    /// After the whole raw stream: the header (length and checksum, read back
    /// from `out`) at position 0. Returns the file length.
    pub fn finish(mut self, out: &mut dyn Out) -> Result<u64> {
        if self.rp != self.plan.raw_len {
            return Err(Error::Truncated);
        }
        self.events(out)?;
        if self.blocks.is_some() || !self.open.is_empty() || self.next != self.plan.marks.len() {
            return Err(Error::invalid("deferred save: plan not complete"));
        }
        self.o.flush(out)?;
        let end = self.o.pos();
        let len = end - HEADER_LEN as u64;
        let mut h = Hash64::new(len);
        let mut chunk = vec![0u8; 4 << 20];
        let mut at = HEADER_LEN as u64;
        while at < end {
            let n = ((end - at) as usize).min(chunk.len());
            out.read_at(at, &mut chunk[..n])?;
            h.update(&chunk[..n]);
            at += n as u64;
        }
        let mut head = crate::encode_file(self.plan.config_hash, &[]);
        head.truncate(HEADER_LEN);
        head[20..28].copy_from_slice(&len.to_le_bytes());
        head[28..36].copy_from_slice(&h.finish().to_le_bytes());
        out.write_at(0, &head)?;
        Ok(end)
    }
}

/// Assembles a whole plan from its raw stream in pieces of `piece` bytes
/// (tests and tools).
pub fn assemble(plan: Plan, raw: &[u8], piece: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut a = Assembler::new(plan);
    for c in raw.chunks(piece.max(1)) {
        a.push(c, &mut out)?;
    }
    let n = a.finish(&mut out)?;
    out.truncate(n as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Writer, compress, encode_file};

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    /// Large data with zeros, repeats (near and far), text and noise, and a
    /// short last block.
    fn data(blocks: usize, seed: u64) -> Vec<u8> {
        let mut d = Vec::new();
        for i in 0..blocks {
            let b: Vec<u8> = match i % 6 {
                0 => vec![0; BLOCK],
                1 => noise(BLOCK, seed),
                2 => noise(BLOCK, i as u64 + seed),
                3 => format!("page {i} ").bytes().cycle().take(BLOCK).collect(),
                4 => noise(BLOCK, seed).iter().map(|b| b & 3).collect(),
                _ => vec![0x5a; BLOCK],
            };
            d.extend(b);
        }
        d.extend(noise(777, seed + 1));
        d
    }

    /// The same content written normally and deferred: nested sections,
    /// several compressed pieces (also empty and all zero), and an external
    /// section at the end.
    fn both(ext: &[u8]) -> (Vec<u8>, Plan, Vec<u8>) {
        let write = |w: &mut Writer| {
            w.section(b"ONE ", |w| {
                w.u64(7);
                w.section(b"IN  ", |w| {
                    compress(w, &data(20, 3));
                    w.str("between");
                    compress(w, &[]);
                    compress(w, &[0; 3 * BLOCK]);
                });
                w.section(b"NONE", |_| {});
                compress(w, &data(9, 4));
            });
            w.section(b"TWO ", |w| w.u32(9));
        };
        let mut n = Writer::new();
        write(&mut n);
        n.section(b"EXT ", |w| compress(w, ext));
        let normal = encode_file(0x1234, n.as_bytes());
        let mut d = Writer::deferred(0);
        write(&mut d);
        d.external_section(b"EXT ", ext.len() as u64);
        let (plan, head) = d.into_plan(0x1234, ext.len() as u64);
        (normal, plan, [head, ext.to_vec()].concat())
    }

    #[test]
    fn deferred_gives_the_same_file() {
        let ext = data(70, 11);
        let (normal, plan, raw) = both(&ext);
        let back = Plan::decode(&plan.encode()).unwrap();
        assert_eq!(back, plan);
        for piece in [1, 7, 4096, 4097, 100_000, raw.len()] {
            let file = assemble(plan.clone(), &raw, piece).unwrap();
            assert!(file == normal, "pieces of {piece}: {} vs {} bytes", file.len(), normal.len());
        }
        // The file reads back.
        crate::decode_file(&normal).unwrap();
    }

    /// Repeats whose source was flushed to `out` long before (confirmed by
    /// reading it back), a near-repeat that is not one, the same file.
    #[test]
    fn repeats_far_apart_and_long_outputs() {
        let mut ext = noise(400 * BLOCK, 5);
        let first = ext[..BLOCK].to_vec();
        for k in [399usize, 300, 2] {
            ext[k * BLOCK..(k + 1) * BLOCK].copy_from_slice(&first);
        }
        // One byte off: not a repeat.
        ext[350 * BLOCK..351 * BLOCK].copy_from_slice(&first);
        ext[350 * BLOCK + 9] ^= 1;
        let (normal, plan, raw) = both(&ext);
        assert!(assemble(plan, &raw, 65536).unwrap() == normal);
    }

    #[test]
    fn rejects_bad_plans_and_streams() {
        let (_, plan, raw) = both(&data(3, 1));
        let mut short = assemble(plan.clone(), &raw[..raw.len() - 1], 999);
        assert!(short.is_err(), "stream too short");
        let mut a = Assembler::new(plan.clone());
        let mut out = Vec::new();
        assert!(a.push(&[raw.clone(), vec![0]].concat(), &mut out).is_err(), "too long");
        let mut p = plan.clone();
        p.marks.swap(0, 1);
        assert!(Plan::decode(&p.encode()).is_err(), "out of order");
        let mut p = plan.clone();
        p.raw_len -= 1;
        assert!(Plan::decode(&p.encode()).is_err(), "past the end");
        let mut b = plan.encode();
        b[24] = 9;
        assert!(Plan::decode(&b).is_err(), "unknown mark");
        short = Plan::decode(&plan.encode()[..30]).map(|_| Vec::new());
        assert!(short.is_err(), "truncated plan");
    }
}
