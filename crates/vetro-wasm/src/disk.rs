//! virtio-blk disks whose data comes from the JavaScript host (M5): a
//! local file, a URL read in pieces with HTTP Range, an OPFS cache.
//!
//! [`HostDisk`] knows only the disk size and the blocks (of
//! `block_size` bytes, aligned) that JS has already given it. A read that
//! touches a missing block answers [`BlockError::NotReady`] and puts the
//! block in the list of requested ones: virtio-blk keeps the request
//! pending, the machine stops with `Stop::Blocked` without executing other
//! instructions, JS takes the list ([`HostDisk::take_wanted`]), obtains the
//! blocks (OPFS, then network) and delivers them ([`HostDisk::fill`]); at the next
//! quantum the request is repeated from scratch and completes at the same
//! instruction count as a local disk (ADR 0014).
//!
//! The guest's writes go into the [`CowBackend`] on top (in memory, M6
//! will make them persistent): [`HostDisk`] is read-only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use vetro_platform::virtio::{BLK_SECTOR_SIZE, BlockBackend, BlockError};

/// Counters of a disk, in the order of `vetro_disk_stats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStats {
    /// Reads that found a missing block (every repetition counts).
    pub misses: u64,
    /// Blocks delivered by JS.
    pub fills: u64,
    /// Blocks evicted from the cache to make room.
    pub evictions: u64,
    /// Blocks that JS failed to obtain (I/O error to the guest).
    pub failures: u64,
}

/// Read-only disk with the data provided by the host in blocks.
pub struct HostDisk {
    size: u64,
    block: u64,
    /// Blocks present.
    cache: BTreeMap<u64, Box<[u8]>>,
    /// Order of arrival, to evict the oldest beyond `max_blocks`.
    order: VecDeque<u64>,
    /// 0 = no limit.
    max_blocks: usize,
    /// Requested and not yet delivered.
    requested: BTreeSet<u64>,
    /// Requested after the last [`take_wanted`](Self::take_wanted).
    wanted: Vec<u64>,
    /// Blocks that JS could not obtain.
    failed: BTreeSet<u64>,
    pub stats: DiskStats,
}

/// Why [`HostDisk::new`] or [`HostDisk::fill`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    /// `block_size` is not a power of two multiple of 512.
    BadBlockSize,
    /// Block past the end of the disk.
    OutOfRange,
    /// Length other than the block's (the last one may be short).
    BadLength,
}

impl HostDisk {
    /// Disk of `size` bytes (rounded down to 512, as QEMU does for
    /// raw disks) in blocks of `block_size` bytes; at most `max_blocks`
    /// blocks in memory (0 = no limit).
    pub fn new(size: u64, block_size: u32, max_blocks: usize) -> Result<Self, DiskError> {
        let block = u64::from(block_size);
        if !block.is_power_of_two() || block < BLK_SECTOR_SIZE {
            return Err(DiskError::BadBlockSize);
        }
        Ok(HostDisk {
            size: size / BLK_SECTOR_SIZE * BLK_SECTOR_SIZE,
            block,
            cache: BTreeMap::new(),
            order: VecDeque::new(),
            max_blocks,
            requested: BTreeSet::new(),
            wanted: Vec::new(),
            failed: BTreeSet::new(),
            stats: DiskStats::default(),
        })
    }

    pub fn block_size(&self) -> u64 {
        self.block
    }

    /// Number of blocks of the disk (the last one may be short).
    pub fn blocks(&self) -> u64 {
        self.size.div_ceil(self.block)
    }

    /// Bytes of block `b`.
    pub fn block_len(&self, b: u64) -> usize {
        (self.size - b * self.block).min(self.block) as usize
    }

    pub fn cached_blocks(&self) -> usize {
        self.cache.len()
    }

    /// The blocks requested since the last call: each one appears only
    /// once until it is delivered or declared failed.
    pub fn take_wanted(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.wanted)
    }

    /// Puts `b` back into the list of requested ones (not delivered by whoever had
    /// taken it).
    pub fn requeue(&mut self, b: u64) {
        if self.requested.contains(&b) && !self.wanted.contains(&b) {
            self.wanted.push(b);
        }
    }

    /// Delivers block `b` (even if not requested: read-ahead).
    pub fn fill(&mut self, b: u64, data: &[u8]) -> Result<(), DiskError> {
        if b >= self.blocks() {
            return Err(DiskError::OutOfRange);
        }
        if data.len() != self.block_len(b) {
            return Err(DiskError::BadLength);
        }
        self.requested.remove(&b);
        self.failed.remove(&b);
        self.stats.fills += 1;
        if self.cache.insert(b, data.into()).is_none() {
            self.order.push_back(b);
        }
        while self.max_blocks > 0 && self.cache.len() > self.max_blocks {
            let Some(old) = self.order.pop_front() else { break };
            self.cache.remove(&old);
            self.stats.evictions += 1;
        }
        Ok(())
    }

    /// JS failed to obtain block `b`: the request waiting for it
    /// ends with an I/O error (IOERR status to the guest).
    pub fn fail(&mut self, b: u64) {
        self.requested.remove(&b);
        self.failed.insert(b);
        self.stats.failures += 1;
    }
}

impl BlockBackend for HostDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        true
    }

    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let start = sector.checked_mul(BLK_SECTOR_SIZE).ok_or(BlockError::OutOfRange)?;
        let end = start.checked_add(buf.len() as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.size || !(buf.len() as u64).is_multiple_of(BLK_SECTOR_SIZE) {
            return Err(BlockError::OutOfRange);
        }
        if buf.is_empty() {
            return Ok(());
        }
        let (first, last) = (start / self.block, (end - 1) / self.block);
        // First all the missing ones of the range, so JS requests them
        // together.
        let mut missing = false;
        for b in first..=last {
            if self.failed.contains(&b) {
                return Err(BlockError::Io);
            }
            if !self.cache.contains_key(&b) {
                missing = true;
                if self.requested.insert(b) {
                    self.wanted.push(b);
                }
            }
        }
        if missing {
            self.stats.misses += 1;
            return Err(BlockError::NotReady);
        }
        for b in first..=last {
            let data = &self.cache[&b];
            let at = b * self.block;
            let (lo, hi) = (start.max(at), end.min(at + data.len() as u64));
            buf[(lo - start) as usize..(hi - start) as usize]
                .copy_from_slice(&data[(lo - at) as usize..(hi - at) as usize]);
        }
        Ok(())
    }

    fn write_sectors(&mut self, _sector: u64, _data: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::ReadOnly)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 13 + i / 4096) as u8).collect()
    }

    fn feed(d: &mut HostDisk, img: &[u8]) -> usize {
        let wanted = d.take_wanted();
        for &b in &wanted {
            let at = (b * d.block_size()) as usize;
            d.fill(b, &img[at..at + d.block_len(b)]).unwrap();
        }
        wanted.len()
    }

    /// Read straddling two blocks: first NotReady with both
    /// blocks requested only once, then the right data.
    #[test]
    fn blocchi_mancanti_richiesti_poi_letti() {
        let img = image(3 * 4096 + 1024 + 100); // short last block, unaligned tail
        let mut d = HostDisk::new(img.len() as u64, 4096, 0).unwrap();
        assert_eq!(d.size(), 3 * 4096 + 1024);
        assert_eq!(d.blocks(), 4);
        assert_eq!(d.block_len(3), 1024);
        let mut buf = vec![0u8; 2048];
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [0, 1], "every block requested only once");
        assert!(d.take_wanted().is_empty());
        d.fill(0, &img[..4096]).unwrap();
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert!(d.take_wanted().is_empty(), "block 1 has already been requested");
        d.fill(1, &img[4096..8192]).unwrap();
        assert_eq!(d.read_sectors(7, &mut buf), Ok(()));
        assert_eq!(buf, img[7 * 512..7 * 512 + 2048]);
        let mut tail = vec![0u8; 1024];
        assert_eq!(d.read_sectors(24, &mut tail), Err(BlockError::NotReady));
        assert_eq!(feed(&mut d, &img), 1);
        assert_eq!(d.read_sectors(24, &mut tail), Ok(()));
        assert_eq!(tail, img[3 * 4096..3 * 4096 + 1024]);
        assert_eq!(d.read_sectors(26, &mut [0u8; 512]), Err(BlockError::OutOfRange));
        assert_eq!(d.stats.misses, 4);
    }

    #[test]
    fn consegne_sbagliate_e_blocchi_falliti() {
        assert_eq!(HostDisk::new(4096, 1000, 0).err(), Some(DiskError::BadBlockSize));
        assert_eq!(HostDisk::new(4096, 256, 0).err(), Some(DiskError::BadBlockSize));
        let mut d = HostDisk::new(8192, 4096, 0).unwrap();
        assert_eq!(d.fill(2, &[0; 4096]), Err(DiskError::OutOfRange));
        assert_eq!(d.fill(1, &[0; 512]), Err(DiskError::BadLength));
        let mut buf = [0u8; 512];
        assert_eq!(d.read_sectors(8, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [1]);
        d.fail(1);
        assert_eq!(d.read_sectors(8, &mut buf), Err(BlockError::Io));
        assert_eq!(d.write_sectors(0, &buf), Err(BlockError::ReadOnly));
    }

    /// Beyond the limit the oldest blocks are evicted, and are then
    /// requested again.
    #[test]
    fn cache_limitata() {
        let img = image(4 * 4096);
        let mut d = HostDisk::new(img.len() as u64, 4096, 2).unwrap();
        let mut buf = vec![0u8; 4096];
        for s in [0, 8, 16] {
            assert_eq!(d.read_sectors(s, &mut buf), Err(BlockError::NotReady));
            feed(&mut d, &img);
            assert_eq!(d.read_sectors(s, &mut buf), Ok(()));
        }
        assert_eq!(d.cached_blocks(), 2);
        assert_eq!(d.stats.evictions, 1);
        assert_eq!(d.read_sectors(0, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [0]);
    }
}
