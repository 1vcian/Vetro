//! virtio-blk (virtio v1.2, §5.2) and block backends.
//!
//! One request queue. Each request: a readable 16-byte header
//! (type, reserved, sector), data, a writable status byte at the end.
//! Types handled: IN, OUT, FLUSH, GET_ID; the others answer UNSUPP.
//! Sectors are always 512 bytes, whatever `blk_size` is.
//!
//! Choices:
//! - a request without a complete header or without the status byte: queue
//!   error (DEVICE_NEEDS_RESET), like QEMU's `virtio_error`;
//! - data not a multiple of 512, access past the capacity, write to a
//!   read-only disk, backend error: status IOERR;
//! - the length in the used ring is the one actually written (data + status);
//! - I/O proceeds in 64 KiB chunks, without allocating the whole
//!   request;
//! - [`BlockError::NotReady`] (data not arrived yet, e.g. an image
//!   downloaded in pieces in M5) leaves the request pending: it is retried from
//!   scratch at the next `service`, before popping others. Requests are
//!   idempotent, so repeating them is safe.

use core::any::Any;
use std::collections::{BTreeMap, BTreeSet};

use super::*;

pub const BLK_SECTOR_SIZE: u64 = 512;

pub const T_IN: u32 = 0;
pub const T_OUT: u32 = 1;
pub const T_FLUSH: u32 = 4;
pub const T_GET_ID: u32 = 8;

pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

pub const F_SIZE_MAX: u64 = 1 << 1;
pub const F_SEG_MAX: u64 = 1 << 2;
pub const F_RO: u64 = 1 << 5;
pub const F_BLK_SIZE: u64 = 1 << 6;
pub const F_FLUSH: u64 = 1 << 9;

/// Length of the GET_ID identifier.
pub const ID_BYTES: usize = 20;
/// Maximum chunk for each call to the backend.
const CHUNK: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Backend I/O error.
    Io,
    /// Sectors past the end of the disk.
    OutOfRange,
    /// Write to a read-only backend.
    ReadOnly,
    /// Data not available yet: retry later.
    NotReady,
}

/// Disk as seen by the device. `sector` in 512-byte units; the buffers
/// are multiples of 512.
pub trait BlockBackend: Any {
    /// Size in bytes (a multiple of 512).
    fn size(&self) -> u64;
    fn read_only(&self) -> bool {
        false
    }
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError>;
    fn flush(&mut self) -> Result<(), BlockError>;
    /// Backend state in snapshots (M6, ADR 0015). Usually none: the
    /// data lives outside (a file, an image over HTTP) and the backend is a
    /// link that the host recreates before the restore. Backends holding their
    /// own data written by the guest (in-memory disk, copy-on-write layer)
    /// save it here.
    fn save_state(&self, _w: &mut vetro_snapshot::Writer) {}
    fn restore_state(&mut self, _r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        Ok(())
    }
}

/// Byte range of `len` bytes from sector `sector`, if it fits in `size`.
fn byte_range(size: u64, sector: u64, len: usize) -> Result<core::ops::Range<usize>, BlockError> {
    let start = sector.checked_mul(BLK_SECTOR_SIZE).ok_or(BlockError::OutOfRange)?;
    let end = start.checked_add(len as u64).ok_or(BlockError::OutOfRange)?;
    if end > size || !(len as u64).is_multiple_of(BLK_SECTOR_SIZE) {
        return Err(BlockError::OutOfRange);
    }
    Ok(start as usize..end as usize)
}

/// In-memory disk.
#[derive(Clone, Debug, Default)]
pub struct MemBackend {
    data: Vec<u8>,
    read_only: bool,
}

impl MemBackend {
    /// Zeroed disk of `size` bytes (rounded up to 512).
    pub fn new(size: u64) -> Self {
        Self::from_vec(vec![0; size as usize])
    }

    /// Disk with the given contents, padded with zeros up to 512.
    pub fn from_vec(mut data: Vec<u8>) -> Self {
        let pad = data.len().next_multiple_of(BLK_SECTOR_SIZE as usize);
        data.resize(pad, 0);
        Self { data, read_only: false }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl BlockBackend for MemBackend {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, buf.len())?;
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        let r = byte_range(self.size(), sector, data.len())?;
        self.data[r].copy_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
    /// Writable: the whole contents (the guest may have written them), in
    /// compressed blocks. Read-only (the base of a copy-on-write) it is
    /// an image that doesn't change: only its hash, to check that at
    /// restore the same one is attached.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(u64::from(self.read_only));
        if self.read_only {
            w.u64(vetro_snapshot::hash64(&self.data));
        } else {
            vetro_snapshot::compress(w, &self.data);
        }
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("read-only flag of the in-memory disk", u64::from(self.read_only))?;
        if self.read_only {
            return r.expect_u64("hash of the in-memory disk", vetro_snapshot::hash64(&self.data));
        }
        self.data.fill(0);
        vetro_snapshot::decompress_into(r, &mut self.data, |_| {})
    }
}

/// Copy-on-write layer over a backend used read-only (e.g.
/// an image downloaded in pieces): writes end up in in-memory clusters of
/// [`CowBackend::CLUSTER`] bytes, reads prefer the
/// written clusters. A partial write of a cluster copies it first
/// from the base.
///
/// For the persistent overlay (M6, ADR 0017) it also keeps the set of
/// clusters written since the last [`take_dirty`](Self::take_dirty): whoever
/// keeps the clusters in a file (CLI, OPFS in the browser) writes only those.
/// It is host bookkeeping, not guest state: it doesn't go into snapshots,
/// and after a restore it is emptied (whoever persists compares all clusters).
pub struct CowBackend<B: BlockBackend> {
    base: B,
    clusters: BTreeMap<u64, Box<[u8]>>,
    dirty: BTreeSet<u64>,
}

impl<B: BlockBackend> CowBackend<B> {
    pub const CLUSTER: u64 = 4096;

    pub fn new(base: B) -> Self {
        Self { base, clusters: BTreeMap::new(), dirty: BTreeSet::new() }
    }

    pub fn base(&self) -> &B {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut B {
        &mut self.base
    }

    /// Number of written clusters.
    pub fn dirty_clusters(&self) -> usize {
        self.clusters.len()
    }

    /// The clusters written by the guest since the last call, in order.
    pub fn take_dirty(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// The data of cluster `c`, if it has been written.
    pub fn cluster(&self, c: u64) -> Option<&[u8]> {
        self.clusters.get(&c).map(|d| &d[..])
    }

    /// All the written clusters, in index order.
    pub fn clusters(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.clusters.iter().map(|(&c, d)| (c, &d[..]))
    }

    /// Sets cluster `c` with the data of a saved overlay (as long as the
    /// cluster in the disk). It doesn't count as a guest write.
    pub fn load_cluster(&mut self, c: u64, data: &[u8]) -> Result<(), BlockError> {
        if c >= self.size().div_ceil(Self::CLUSTER) {
            return Err(BlockError::OutOfRange);
        }
        if data.len() != self.cluster_len(c) {
            return Err(BlockError::Io);
        }
        self.clusters.insert(c, data.into());
        Ok(())
    }

    /// Size of cluster `c` (the last one may be shorter).
    fn cluster_len(&self, c: u64) -> usize {
        (self.base.size() - c * Self::CLUSTER).min(Self::CLUSTER) as usize
    }

    /// Applies `f(cluster, offset in the cluster, start, end)` to every piece
    /// of the range `[pos, pos + len)` split by cluster.
    fn pieces(
        pos: u64,
        len: usize,
        mut f: impl FnMut(u64, usize, usize, usize) -> Result<(), BlockError>,
    ) -> Result<(), BlockError> {
        let mut done = 0usize;
        while done < len {
            let at = pos + done as u64;
            let (c, off) = (at / Self::CLUSTER, (at % Self::CLUSTER) as usize);
            let n = (Self::CLUSTER as usize - off).min(len - done);
            f(c, off, done, done + n)?;
            done += n;
        }
        Ok(())
    }
}

impl<B: BlockBackend> BlockBackend for CowBackend<B> {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, buf.len())?;
        Self::pieces(r.start as u64, buf.len(), |c, off, a, b| match self.clusters.get(&c) {
            Some(data) => {
                buf[a..b].copy_from_slice(&data[off..off + (b - a)]);
                Ok(())
            }
            None => {
                let at = c * Self::CLUSTER + off as u64;
                self.base.read_sectors(at / BLK_SECTOR_SIZE, &mut buf[a..b])
            }
        })
    }

    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, data.len())?;
        Self::pieces(r.start as u64, data.len(), |c, off, a, b| {
            if !self.clusters.contains_key(&c) {
                let mut fresh = vec![0u8; self.cluster_len(c)].into_boxed_slice();
                if b - a < fresh.len() {
                    self.base.read_sectors(c * Self::CLUSTER / BLK_SECTOR_SIZE, &mut fresh)?;
                }
                self.clusters.insert(c, fresh);
            }
            let cl = self.clusters.get_mut(&c).expect("cluster just inserted");
            cl[off..off + (b - a)].copy_from_slice(&data[a..b]);
            self.dirty.insert(c);
            Ok(())
        })
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }

    /// The clusters written by the guest (in order), then the state of the base
    /// (usually none: the base is a link, checked only by
    /// size).
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.base.size());
        w.seq(&self.clusters, |w, (&c, data)| {
            w.u64(c);
            vetro_snapshot::compress(w, data);
        });
        self.base.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("disk size", self.base.size())?;
        let clusters = self.size().div_ceil(Self::CLUSTER);
        self.clusters.clear();
        self.dirty.clear();
        let n = r.len_of(16)?;
        for _ in 0..n {
            let c = r.u64()?;
            if c >= clusters || self.clusters.contains_key(&c) {
                return Err(vetro_snapshot::Error::invalid(format!("disk cluster {c}")));
            }
            let mut data = vec![0u8; self.cluster_len(c)].into_boxed_slice();
            vetro_snapshot::decompress_into(r, &mut data, |_| {})?;
            self.clusters.insert(c, data);
        }
        self.base.restore_state(r)
    }
}

/// virtio-blk parameters.
#[derive(Clone, Debug)]
pub struct VirtioBlkConfig {
    /// Queue size (QEMU: 256).
    pub queue_size: u16,
    /// Maximum data segments per request (QEMU: queue_size - 2).
    pub seg_max: u32,
    /// Maximum bytes per segment.
    pub size_max: u32,
    /// Logical block size announced to the driver.
    pub blk_size: u32,
    /// Forces read-only even if the backend accepts writes.
    pub read_only: bool,
    /// Identifier for GET_ID (at most 20 bytes, padded with zeros).
    pub serial: Vec<u8>,
}

impl Default for VirtioBlkConfig {
    fn default() -> Self {
        Self {
            queue_size: 256,
            seg_max: 254,
            size_max: 1 << 20,
            blk_size: 512,
            read_only: false,
            serial: b"vetro-blk".to_vec(),
        }
    }
}

/// Outcome of handling a request.
enum Outcome {
    /// Bytes written into the driver's buffers (data + status).
    Done(u32),
    /// The backend is not ready: retry.
    Retry,
}

pub struct VirtioBlk {
    backend: Box<dyn BlockBackend>,
    cfg: VirtioBlkConfig,
    queue_sizes: [u16; 1],
    pending: Option<DescChain>,
}

impl VirtioBlk {
    pub fn new(backend: Box<dyn BlockBackend>, cfg: VirtioBlkConfig) -> Self {
        let queue_sizes = [cfg.queue_size];
        Self { backend, cfg, queue_sizes, pending: None }
    }

    pub fn backend(&self) -> &dyn BlockBackend {
        self.backend.as_ref()
    }

    pub fn backend_mut(&mut self) -> &mut dyn BlockBackend {
        self.backend.as_mut()
    }

    /// Typed access to the backend.
    pub fn backend_as_mut<T: BlockBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    pub fn is_read_only(&self) -> bool {
        self.cfg.read_only || self.backend.read_only()
    }

    /// Capacity in 512-byte sectors.
    pub fn capacity(&self) -> u64 {
        self.backend.size() / BLK_SECTOR_SIZE
    }

    /// A request is waiting for the backend.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn config_bytes(&self) -> [u8; 36] {
        let mut c = [0u8; 36];
        c[0..8].copy_from_slice(&self.capacity().to_le_bytes());
        c[8..12].copy_from_slice(&self.cfg.size_max.to_le_bytes());
        c[12..16].copy_from_slice(&self.cfg.seg_max.to_le_bytes());
        // geometry (16..20) e topology (24..32) a zero, writeback (32) a 0.
        c[20..24].copy_from_slice(&self.cfg.blk_size.to_le_bytes());
        c[34..36].copy_from_slice(&1u16.to_le_bytes()); // num_queues
        c
    }

    fn handle(&mut self, c: &DescChain, ram: &mut dyn GuestRam) -> Result<Outcome, QueueError> {
        let mut hdr = [0u8; 16];
        if c.read(ram, 0, &mut hdr)? < hdr.len() {
            return Err(QueueError::Malformed("incomplete virtio-blk header"));
        }
        let Some(status_at) = c.writable_len().checked_sub(1) else {
            return Err(QueueError::Malformed("virtio-blk status byte missing"));
        };
        let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let (status, written) = match kind {
            T_IN => match self.transfer(c, ram, sector, status_at, false)? {
                Some(Ok(())) => (S_OK, status_at),
                Some(Err(_)) => (S_IOERR, 0),
                None => return Ok(Outcome::Retry),
            },
            T_OUT => {
                let len = c.readable_len() - 16;
                match self.transfer(c, ram, sector, len, true)? {
                    Some(Ok(())) => (S_OK, 0),
                    Some(Err(_)) => (S_IOERR, 0),
                    None => return Ok(Outcome::Retry),
                }
            }
            T_FLUSH => match self.backend.flush() {
                Ok(()) => (S_OK, 0),
                Err(BlockError::NotReady) => return Ok(Outcome::Retry),
                Err(_) => (S_IOERR, 0),
            },
            T_GET_ID => {
                let mut id = [0u8; ID_BYTES];
                let n = self.cfg.serial.len().min(ID_BYTES);
                id[..n].copy_from_slice(&self.cfg.serial[..n]);
                let len = (status_at as usize).min(ID_BYTES);
                c.write(ram, 0, &id[..len])?;
                (S_OK, len as u64)
            }
            _ => (S_UNSUPP, 0),
        };
        c.write(ram, status_at, &[status])?;
        Ok(Outcome::Done(written as u32 + 1))
    }

    /// Transfers `len` bytes between disk (from sector `sector`) and chain:
    /// writable data from offset 0 for IN, readable from offset 16 for
    /// OUT. `None` = backend not ready; `Some(Err)` = IOERR error.
    fn transfer(
        &mut self,
        c: &DescChain,
        ram: &mut dyn GuestRam,
        sector: u64,
        len: u64,
        out: bool,
    ) -> Result<Option<Result<(), BlockError>>, QueueError> {
        if out && self.is_read_only() {
            return Ok(Some(Err(BlockError::ReadOnly)));
        }
        if !len.is_multiple_of(BLK_SECTOR_SIZE) {
            return Ok(Some(Err(BlockError::OutOfRange)));
        }
        let end = sector.checked_add(len / BLK_SECTOR_SIZE);
        if end.is_none_or(|e| e > self.capacity()) {
            return Ok(Some(Err(BlockError::OutOfRange)));
        }
        let mut buf = vec![0u8; len.min(CHUNK) as usize];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(CHUNK) as usize;
            let chunk = &mut buf[..n];
            let s = sector + done / BLK_SECTOR_SIZE;
            let r = if out {
                c.read(ram, 16 + done, chunk)?;
                self.backend.write_sectors(s, chunk)
            } else {
                let r = self.backend.read_sectors(s, chunk);
                if r.is_ok() {
                    c.write(ram, done, chunk)?;
                }
                r
            };
            match r {
                Ok(()) => {}
                Err(BlockError::NotReady) => return Ok(None),
                Err(e) => return Ok(Some(Err(e))),
            }
            done += n as u64;
        }
        Ok(Some(Ok(())))
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        ID_BLOCK
    }

    fn features(&self) -> u64 {
        let ro = if self.is_read_only() { F_RO } else { 0 };
        F_SIZE_MAX | F_SEG_MAX | F_BLK_SIZE | F_FLUSH | ro
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_bytes(), offset, data);
    }

    fn reset(&mut self) {
        self.pending = None;
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        let (q, ram) = (&mut ctx.queues[0], &mut *ctx.ram);
        loop {
            let chain = match self.pending.take() {
                Some(c) => c,
                None => match q.pop(ram)? {
                    Some(c) => c,
                    None => return Ok(()),
                },
            };
            match self.handle(&chain, ram)? {
                Outcome::Done(len) => q.push_used(ram, chain.head, len)?,
                Outcome::Retry => {
                    self.pending = Some(chain);
                    return Ok(());
                }
            }
        }
    }

    /// The pending request (if the backend was not ready) and the backend
    /// state. The configuration (capacity, queue, read-only) is
    /// checked: the disk attached at restore must be the same.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.capacity());
        w.u64(u64::from(self.is_read_only()));
        w.bytes(&self.cfg.serial);
        w.opt(self.pending.as_ref(), |w, c| c.save(w));
        self.backend.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("disk sectors", self.capacity())?;
        r.expect_u64("disk read-only flag", u64::from(self.is_read_only()))?;
        if r.bytes()? != self.cfg.serial.as_slice() {
            return Err(vetro_snapshot::Error::invalid("different disk identifier"));
        }
        self.pending = r.opt(DescChain::restore)?;
        self.backend.restore_state(r)
    }
}

#[cfg(test)]
mod tests;
