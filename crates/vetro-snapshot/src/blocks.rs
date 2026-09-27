//! Large data (RAM, disks and GPU images) in blocks of [`BLOCK`] bytes: the
//! format of [`compress`](crate::compress) and of the machine's `RAM `
//! section (ADR 0015, 0031).
//!
//! u64 length of the data, u64 number of non-zero blocks, then entries until
//! all of them are covered. Zero blocks are absent. An entry starts with the
//! u64 index of its (first) block and a u8 encoding:
//! - 0: the block's bytes;
//! - 1: [`lz`] of the block: u32 length, bytes;
//! - 2: the same bytes as an earlier block: u64 index of that block (already
//!   written by an earlier entry);
//! - 3: a frame, several blocks compressed together with [`lzh`]: u16 number
//!   of blocks `k` (1 to 64), `k - 1` × u64 indices of the other blocks, u32
//!   length, bytes of the concatenated blocks.
//!
//! Every block appears once; entries are in increasing order of index except
//! that an encoding-2 entry may come after the frame holding its source.
//! [`Level::Fast`] writes encodings 0, 1 and 2 (the app's own snapshots, saved
//! while the guest waits); [`Level::Small`] writes 0, 2 and 3 (downloaded
//! snapshots: several times slower to write, about a third smaller).

use std::collections::HashMap;

use crate::{BLOCK, Error, Result, hash64, is_zero, lz, lzh};

/// How hard to compress (ADR 0031). The reader accepts both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// Each block with [`lz`] on its own: fast to write and read.
    #[default]
    Fast,
    /// Frames of up to 64 blocks with [`lzh`] (LZ77 and Huffman codes).
    Small,
}

const RAW: u8 = 0;
const LZ: u8 = 1;
const SAME: u8 = 2;
const FRAME: u8 = 3;
/// Blocks per frame.
const FRAME_BLOCKS: usize = lzh::FRAME / BLOCK;
/// Output is handed on in chunks of about this size.
const FLUSH: usize = 1 << 20;

struct Out<'e> {
    buf: Vec<u8>,
    emit: &'e mut dyn FnMut(&[u8]),
}

impl Out<'_> {
    fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }
    fn maybe_flush(&mut self) {
        if self.buf.len() >= FLUSH {
            (self.emit)(&self.buf);
            self.buf.clear();
        }
    }
}

/// Writes data of `len` bytes, whose block `i` is `block(i)` ([`BLOCK`] bytes,
/// the last one possibly shorter), in chunks to `emit`. Deterministic: the
/// same data and level always give the same bytes.
pub fn encode<'a>(len: usize, level: Level, block: &dyn Fn(usize) -> &'a [u8], emit: &mut dyn FnMut(&[u8])) {
    let present: Vec<usize> = (0..len.div_ceil(BLOCK)).filter(|&i| !is_zero(block(i))).collect();
    let mut out = Out { buf: Vec::with_capacity(FLUSH + lzh::FRAME + 1024), emit };
    out.u64(len as u64);
    out.u64(present.len() as u64);
    // First block with each content (by hash, confirmed byte by byte).
    let mut first: HashMap<u64, usize> = HashMap::with_capacity(present.len());
    let mut table = lz::Table::new();
    let mut enc = (level == Level::Small).then(lzh::Encoder::new);
    let mut scratch = Vec::with_capacity(lzh::FRAME + lzh::FRAME / 16);
    let mut frame: Vec<usize> = Vec::with_capacity(FRAME_BLOCKS);
    let mut data: Vec<u8> = Vec::with_capacity(lzh::FRAME);
    // Duplicates of blocks in the pending frame wait for it.
    let mut same: Vec<(usize, usize)> = Vec::new();
    let flush_frame = |out: &mut Out,
                       frame: &mut Vec<usize>,
                       data: &mut Vec<u8>,
                       same: &mut Vec<(usize, usize)>,
                       enc: &mut lzh::Encoder,
                       scratch: &mut Vec<u8>| {
        if !frame.is_empty() {
            scratch.clear();
            enc.compress(data, scratch);
            if scratch.len() < data.len() {
                out.u64(frame[0] as u64);
                out.raw(&[FRAME]);
                out.raw(&(frame.len() as u16).to_le_bytes());
                for &i in &frame[1..] {
                    out.u64(i as u64);
                }
                out.raw(&(scratch.len() as u32).to_le_bytes());
                out.raw(scratch);
            } else {
                let mut at = 0;
                for &i in frame.iter() {
                    let l = BLOCK.min(len - i * BLOCK);
                    out.u64(i as u64);
                    out.raw(&[RAW]);
                    out.raw(&data[at..at + l]);
                    at += l;
                }
            }
            frame.clear();
            data.clear();
        }
        for (i, j) in same.drain(..) {
            out.u64(i as u64);
            out.raw(&[SAME]);
            out.u64(j as u64);
        }
        out.maybe_flush();
    };
    for i in present {
        let b = block(i);
        let h = hash64(b);
        match first.get(&h) {
            Some(&j) if block(j) == b => {
                if frame.is_empty() {
                    out.u64(i as u64);
                    out.raw(&[SAME]);
                    out.u64(j as u64);
                } else {
                    same.push((i, j));
                }
                continue;
            }
            Some(_) => {}
            None => {
                first.insert(h, i);
            }
        }
        match enc.as_mut() {
            None => {
                out.u64(i as u64);
                scratch.clear();
                lz::compress(b, &mut scratch, &mut table);
                if scratch.len() < b.len() {
                    out.raw(&[LZ]);
                    out.raw(&(scratch.len() as u32).to_le_bytes());
                    out.raw(&scratch);
                } else {
                    out.raw(&[RAW]);
                    out.raw(b);
                }
                out.maybe_flush();
            }
            Some(enc) => {
                frame.push(i);
                data.extend_from_slice(b);
                if frame.len() == FRAME_BLOCKS {
                    flush_frame(&mut out, &mut frame, &mut data, &mut same, enc, &mut scratch);
                }
            }
        }
    }
    if let Some(enc) = enc.as_mut() {
        flush_frame(&mut out, &mut frame, &mut data, &mut same, enc, &mut scratch);
    }
    if !out.buf.is_empty() {
        (out.emit)(&out.buf);
    }
}

/// Where the encoded bytes come from: the next `n` bytes.
pub trait Source {
    fn take(&mut self, n: usize) -> Result<&[u8]>;
}

impl Source for crate::Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        self.raw(n)
    }
}

/// Where the blocks go: block `i` ([`BLOCK`] bytes, the last one possibly
/// shorter), writable.
pub trait Target {
    fn block_mut(&mut self, i: usize) -> &mut [u8];
}

/// A [`Target`] over a slice.
pub struct Slice<'a>(pub &'a mut [u8]);

impl Target for Slice<'_> {
    fn block_mut(&mut self, i: usize) -> &mut [u8] {
        let end = (i * BLOCK + BLOCK).min(self.0.len());
        &mut self.0[i * BLOCK..end]
    }
}

fn u64_of(s: &mut dyn Source) -> Result<u64> {
    Ok(u64::from_le_bytes(s.take(8)?.try_into().expect("8 bytes")))
}

/// Reads data written by [`encode`] into `t`, which holds `len` bytes: only
/// the non-zero blocks are written (absent ones are left as they are),
/// `visit(i)` for each. Rejects anything [`encode`] cannot have written.
pub fn decode(
    s: &mut dyn Source,
    len: usize,
    t: &mut dyn Target,
    visit: &mut dyn FnMut(usize),
) -> Result<()> {
    let found = u64_of(s)?;
    if found != len as u64 {
        return Err(Error::invalid(format!("data of {found} bytes, expected {len}")));
    }
    let blocks = len.div_ceil(BLOCK);
    let count = u64_of(s)?;
    if count > blocks as u64 {
        return Err(Error::invalid("more blocks than the data"));
    }
    let block_len = |i: usize| BLOCK.min(len - i * BLOCK);
    let mut done = vec![0u64; blocks.div_ceil(64)];
    let is_done = |d: &[u64], i: usize| d[i / 64] & 1 << (i % 64) != 0;
    // Entries in increasing order (encoding-2 entries aside).
    let mut last: Option<usize> = None;
    let claim = |d: &mut Vec<u64>, i: u64, ordered: bool, last: &mut Option<usize>| -> Result<usize> {
        if i >= blocks as u64 || is_done(d, i as usize) || (ordered && last.is_some_and(|l| i as usize <= l))
        {
            return Err(Error::invalid(format!("block {i} out of place")));
        }
        let i = i as usize;
        d[i / 64] |= 1 << (i % 64);
        if ordered {
            *last = Some(i);
        }
        Ok(i)
    };
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut idx: Vec<usize> = Vec::new();
    let mut covered = 0u64;
    while covered < count {
        let first = u64_of(s)?;
        let enc = s.take(1)?[0];
        match enc {
            RAW | LZ => {
                let i = claim(&mut done, first, true, &mut last)?;
                let dst = t.block_mut(i);
                if enc == RAW {
                    dst.copy_from_slice(s.take(dst.len())?);
                } else {
                    let n = u32::from_le_bytes(s.take(4)?.try_into().expect("4 bytes")) as usize;
                    lz::decompress(s.take(n)?, dst)?;
                }
                visit(i);
                covered += 1;
            }
            SAME => {
                let i = claim(&mut done, first, false, &mut last)?;
                let j = u64_of(s)?;
                if j >= i as u64 || !is_done(&done, j as usize) || block_len(j as usize) != block_len(i) {
                    return Err(Error::invalid(format!("block {i}: copy of block {j}")));
                }
                let mut tmp = [0u8; BLOCK];
                let l = block_len(i);
                tmp[..l].copy_from_slice(t.block_mut(j as usize));
                t.block_mut(i).copy_from_slice(&tmp[..l]);
                visit(i);
                covered += 1;
            }
            FRAME => {
                let k = u16::from_le_bytes(s.take(2)?.try_into().expect("2 bytes")) as usize;
                if k == 0 || k > FRAME_BLOCKS || covered + k as u64 > count {
                    return Err(Error::invalid(format!("frame of {k} blocks")));
                }
                idx.clear();
                idx.push(claim(&mut done, first, true, &mut last)?);
                for _ in 1..k {
                    let i = u64_of(s)?;
                    idx.push(claim(&mut done, i, true, &mut last)?);
                }
                let total: usize = idx.iter().map(|&i| block_len(i)).sum();
                let n = u32::from_le_bytes(s.take(4)?.try_into().expect("4 bytes")) as usize;
                frame_buf.resize(total, 0);
                lzh::decompress(s.take(n)?, &mut frame_buf)?;
                let mut at = 0;
                for &i in &idx {
                    let dst = t.block_mut(i);
                    let l = dst.len();
                    dst.copy_from_slice(&frame_buf[at..at + l]);
                    at += l;
                    visit(i);
                }
                covered += k as u64;
            }
            v => return Err(Error::invalid(format!("block encoding {v}"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Reader;

    fn encode_vec(data: &[u8], level: Level) -> Vec<u8> {
        let mut out = Vec::new();
        encode(data.len(), level, &|i| &data[i * BLOCK..(i * BLOCK + BLOCK).min(data.len())], &mut |c| {
            out.extend_from_slice(c)
        });
        out
    }

    fn decode_vec(enc: &[u8], len: usize) -> Result<(Vec<u8>, Vec<usize>)> {
        let mut out = vec![0u8; len];
        let mut seen = Vec::new();
        let mut r = Reader::new(enc);
        decode(&mut r, len, &mut Slice(&mut out), &mut |i| seen.push(i))?;
        if r.remaining() != 0 {
            return Err(Error::invalid("trailing"));
        }
        Ok((out, seen))
    }

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

    /// 300 blocks and a short one: zeros, text, noise, repeated blocks (also
    /// far apart and within one frame).
    fn sample() -> Vec<u8> {
        let mut d = Vec::new();
        for i in 0..300usize {
            let b: Vec<u8> = match i % 7 {
                0 => vec![0; BLOCK],
                1 => noise(BLOCK, 5),
                2 => noise(BLOCK, i as u64),
                3 => format!("block {i} of the guest's page cache. ").bytes().cycle().take(BLOCK).collect(),
                4 => noise(BLOCK, 5).iter().map(|b| b & 7).collect(),
                5 => vec![0xff; BLOCK],
                _ => noise(BLOCK, (i % 3) as u64 + 100),
            };
            d.extend(b);
        }
        d.extend(noise(1000, 9));
        d
    }

    #[test]
    fn both_levels_round_trip() {
        let data = sample();
        let present: Vec<usize> = (0..data.len().div_ceil(BLOCK)).filter(|&i| i % 7 != 0).collect();
        let mut sizes = Vec::new();
        for level in [Level::Fast, Level::Small] {
            let enc = encode_vec(&data, level);
            assert_eq!(enc, encode_vec(&data, level), "deterministic");
            let (out, mut seen) = decode_vec(&enc, data.len()).unwrap();
            assert!(out == data, "{level:?}: same bytes");
            seen.sort();
            assert_eq!(seen, present, "{level:?}: every non-zero block once");
            sizes.push(enc.len());
        }
        assert!(sizes[1] < sizes[0], "small is smaller: {sizes:?}");
        // Repeated blocks cost an entry each.
        let rep: Vec<u8> = noise(BLOCK, 1).repeat(100);
        for level in [Level::Fast, Level::Small] {
            let enc = encode_vec(&rep, level);
            assert!(enc.len() < BLOCK + 100 * 17 + 400, "{level:?}: {}", enc.len());
            assert!(decode_vec(&enc, rep.len()).unwrap().0 == rep);
        }
    }

    #[test]
    fn empty_zero_and_incompressible() {
        for level in [Level::Fast, Level::Small] {
            assert_eq!(decode_vec(&encode_vec(&[], level), 0).unwrap().0, Vec::<u8>::new());
            let zeros = vec![0u8; 5 * BLOCK];
            assert_eq!(encode_vec(&zeros, level).len(), 16, "only length and count");
            // Noise: raw blocks (a frame that does not shrink is written raw).
            let n = noise(70 * BLOCK + 3, 77);
            let enc = encode_vec(&n, level);
            assert!(enc.len() <= n.len() + 71 * 9 + 16, "{level:?}: {}", enc.len());
            assert!(decode_vec(&enc, n.len()).unwrap().0 == n);
        }
    }

    #[test]
    fn rejects_bad_data() {
        let data = sample();
        for level in [Level::Fast, Level::Small] {
            let enc = encode_vec(&data, level);
            assert!(decode_vec(&enc, data.len() + 1).is_err(), "other length");
            assert!(decode_vec(&enc[..enc.len() - 1], data.len()).is_err(), "truncated");
            // Damage anywhere: an error or different bytes (the file checksum
            // catches the rest), never a panic.
            for at in (0..enc.len()).step_by(97) {
                let mut x = enc.clone();
                x[at] ^= 0x41;
                let _ = decode_vec(&x, data.len());
            }
        }
        // A copy of a block not written yet.
        let mut e = Vec::new();
        e.extend((2 * BLOCK as u64).to_le_bytes());
        e.extend(1u64.to_le_bytes());
        e.extend(0u64.to_le_bytes());
        e.push(SAME);
        e.extend(1u64.to_le_bytes());
        assert!(decode_vec(&e, 2 * BLOCK).is_err());
        // The same block twice.
        let mut e = Vec::new();
        e.extend((2 * BLOCK as u64).to_le_bytes());
        e.extend(2u64.to_le_bytes());
        for _ in 0..2 {
            e.extend(0u64.to_le_bytes());
            e.push(RAW);
            e.extend([1u8; BLOCK]);
        }
        assert!(decode_vec(&e, 2 * BLOCK).is_err());
    }
}
