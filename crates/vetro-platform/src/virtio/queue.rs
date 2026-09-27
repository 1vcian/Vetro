//! Split virtqueue (virtio v1.2, §2.7).
//!
//! Areas in guest RAM, with `n` = Queue Size (a power of 2):
//! - descriptor table: `n` entries of 16 bytes (addr, len, flags, next);
//! - driver area (available ring): flags, idx, `ring[n]`, used_event;
//! - device area (used ring): flags, idx, `ring[n]` of (id, len),
//!   avail_event.
//!
//! Validation rules (a violation is a queue error: the
//! transport puts the device in DEVICE_NEEDS_RESET, as QEMU does with
//! `virtio_error`):
//! - head and `next` inside the table; a chain does not visit more
//!   descriptors than the table has (no loops);
//! - the device-writable buffers all follow the readable ones;
//! - INDIRECT only if negotiated and only on the head descriptor, with a
//!   length that is a non-zero multiple of 16; the NEXT flag on the indirect
//!   descriptor is ignored (the chain ends with the table, like QEMU) and
//!   inside an indirect table INDIRECT is forbidden;
//! - `avail.idx` cannot advance more than `n` past the last one consumed.

use core::fmt;

use super::{GuestRam, GuestRamExt, RamError};

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const DESC_F_INDIRECT: u16 = 4;
/// `avail.flags`: the driver does not want interrupts (without EVENT_IDX).
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Maximum size of a split queue (§2.7).
pub const MAX_QUEUE_SIZE: u16 = 32768;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    Ram(RamError),
    /// `avail.idx` advanced by more than the queue size.
    AvailIdx {
        last: u16,
        avail: u16,
    },
    HeadOutOfRange(u16),
    NextOutOfRange(u16),
    /// Chain longer than the table: there is a loop.
    ChainLoop,
    /// Readable buffer after a writable one.
    ReadableAfterWritable,
    IndirectNotNegotiated,
    /// Indirect table empty or with a length that is not a multiple of 16.
    IndirectLen(u32),
    /// INDIRECT outside the head descriptor or inside a table.
    IndirectMisplaced,
    /// Malformed request for the device (e.g. missing header).
    Malformed(&'static str),
}

impl From<RamError> for QueueError {
    fn from(e: RamError) -> Self {
        QueueError::Ram(e)
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Ram(e) => write!(f, "{e}"),
            QueueError::AvailIdx { last, avail } => {
                write!(f, "avail.idx jumps from {last} to {avail}")
            }
            QueueError::HeadOutOfRange(h) => write!(f, "head {h} outside the table"),
            QueueError::NextOutOfRange(n) => write!(f, "next {n} outside the table"),
            QueueError::ChainLoop => write!(f, "loop in the descriptor chain"),
            QueueError::ReadableAfterWritable => write!(f, "readable buffer after a writable one"),
            QueueError::IndirectNotNegotiated => write!(f, "INDIRECT without VIRTIO_F_INDIRECT_DESC"),
            QueueError::IndirectLen(l) => write!(f, "indirect table of {l} bytes"),
            QueueError::IndirectMisplaced => write!(f, "INDIRECT outside the head descriptor"),
            QueueError::Malformed(m) => write!(f, "malformed request: {m}"),
        }
    }
}

/// A buffer of the chain: physical address and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
}

/// Descriptor chain extracted from the available ring. `head` is the id to
/// return in the used ring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DescChain {
    pub head: u16,
    /// Buffers readable by the device (from the driver to the device).
    pub readable: Vec<Buf>,
    /// Buffers writable by the device (from the device to the driver).
    pub writable: Vec<Buf>,
}

fn total(bufs: &[Buf]) -> u64 {
    bufs.iter().map(|b| u64::from(b.len)).sum()
}

/// Copy between a sequence of buffers seen as contiguous space and `len`
/// bytes starting at `offset`; `f(addr, start, end)` acts on each piece.
fn for_each_piece(
    bufs: &[Buf],
    mut offset: u64,
    len: usize,
    mut f: impl FnMut(u64, usize, usize) -> Result<(), RamError>,
) -> Result<usize, RamError> {
    let mut done = 0usize;
    for b in bufs {
        if done == len {
            break;
        }
        let blen = u64::from(b.len);
        if offset >= blen {
            offset -= blen;
            continue;
        }
        let n = ((blen - offset) as usize).min(len - done);
        f(b.addr + offset, done, done + n)?;
        done += n;
        offset = 0;
    }
    Ok(done)
}

impl DescChain {
    pub fn readable_len(&self) -> u64 {
        total(&self.readable)
    }

    pub fn writable_len(&self) -> u64 {
        total(&self.writable)
    }

    /// Reads from the readable buffers, seen as contiguous space, starting at
    /// `offset`. Returns the bytes read (fewer than `out.len()` at the end).
    pub fn read(&self, ram: &dyn GuestRam, offset: u64, out: &mut [u8]) -> Result<usize, RamError> {
        let len = out.len();
        for_each_piece(&self.readable, offset, len, |addr, a, b| ram.read(addr, &mut out[a..b]))
    }

    /// All the readable bytes from `offset` to the end.
    pub fn read_to_vec(&self, ram: &dyn GuestRam, offset: u64) -> Result<Vec<u8>, RamError> {
        let len = self.readable_len().saturating_sub(offset) as usize;
        let mut v = vec![0; len];
        self.read(ram, offset, &mut v)?;
        Ok(v)
    }

    /// Writes into the writable buffers, seen as contiguous space, starting
    /// at `offset`. Returns the bytes written (fewer if the space runs out).
    pub fn write(&self, ram: &mut dyn GuestRam, offset: u64, data: &[u8]) -> Result<usize, RamError> {
        for_each_piece(&self.writable, offset, data.len(), |addr, a, b| ram.write(addr, &data[a..b]))
    }
}

/// Descriptor as laid out in memory.
#[derive(Clone, Copy, Debug)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

fn read_desc(ram: &dyn GuestRam, table: u64, i: u16) -> Result<Desc, RamError> {
    let mut b = [0u8; 16];
    ram.read(table + 16 * u64::from(i), &mut b)?;
    Ok(Desc {
        addr: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        len: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        flags: u16::from_le_bytes(b[12..14].try_into().unwrap()),
        next: u16::from_le_bytes(b[14..16].try_into().unwrap()),
    })
}

/// `vring_need_event` (§2.7.10): an event is needed if `event` lies in
/// `[old, new)` modulo 2^16.
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// State of a split queue on the device side.
#[derive(Clone, Debug)]
pub struct Virtqueue {
    max_size: u16,
    size: u16,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
    /// Next available ring index to consume.
    last_avail: u16,
    /// Next used ring index to write (copy of `used.idx`).
    used_idx: u16,
    /// `used.idx` at the last interrupt decision.
    signalled_used: u16,
    pub(crate) event_idx: bool,
    pub(crate) indirect: bool,
}

impl Virtqueue {
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc: 0,
            driver: 0,
            device: 0,
            last_avail: 0,
            used_idx: 0,
            signalled_used: 0,
            event_idx: false,
            indirect: false,
        }
    }

    /// Back to the reset state (size = maximum, not ready).
    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub fn max_size(&self) -> u16 {
        self.max_size
    }
    pub fn size(&self) -> u16 {
        self.size
    }
    pub fn ready(&self) -> bool {
        self.ready
    }
    /// Addresses of the table, driver area and device area.
    pub fn addrs(&self) -> (u64, u64, u64) {
        (self.desc, self.driver, self.device)
    }

    pub(crate) fn set_size(&mut self, n: u16) {
        self.size = n;
    }
    pub(crate) fn set_desc(&mut self, a: u64) {
        self.desc = a;
    }
    pub(crate) fn set_driver(&mut self, a: u64) {
        self.driver = a;
    }
    pub(crate) fn set_device(&mut self, a: u64) {
        self.device = a;
    }

    /// QueueReady: the queue becomes ready only with a valid configuration
    /// (size a power of 2 not above the maximum, areas aligned as
    /// §2.7 requires: 16, 2 and 4 bytes). Returns the resulting state.
    pub(crate) fn set_ready(&mut self, ready: bool) -> bool {
        let valid = self.size != 0
            && self.size.is_power_of_two()
            && self.size <= self.max_size
            && self.desc.is_multiple_of(16)
            && self.driver.is_multiple_of(2)
            && self.device.is_multiple_of(4);
        self.ready = ready && valid;
        if self.ready {
            self.last_avail = 0;
            self.used_idx = 0;
            self.signalled_used = 0;
        }
        self.ready
    }

    fn avail_idx(&self, ram: &dyn GuestRam) -> Result<u16, QueueError> {
        let idx = ram.read_u16(self.driver + 2)?;
        if idx.wrapping_sub(self.last_avail) > self.size {
            return Err(QueueError::AvailIdx { last: self.last_avail, avail: idx });
        }
        Ok(idx)
    }

    /// Number of chains available and not yet consumed.
    pub fn available(&self, ram: &dyn GuestRam) -> Result<u16, QueueError> {
        if !self.ready {
            return Ok(0);
        }
        Ok(self.avail_idx(ram)?.wrapping_sub(self.last_avail))
    }

    /// With EVENT_IDX: asks the driver for a notification when it publishes
    /// index `last_avail` (avail_event field of the used ring).
    fn publish_avail_event(&self, ram: &mut dyn GuestRam) -> Result<(), RamError> {
        if self.event_idx {
            ram.write_u16(self.device + 4 + 8 * u64::from(self.size), self.last_avail)?;
        }
        Ok(())
    }

    /// Extracts the next available chain, if any.
    pub fn pop(&mut self, ram: &mut dyn GuestRam) -> Result<Option<DescChain>, QueueError> {
        if !self.ready {
            return Ok(None);
        }
        let avail = self.avail_idx(ram)?;
        if avail == self.last_avail {
            self.publish_avail_event(ram)?;
            return Ok(None);
        }
        let slot = u64::from(self.last_avail % self.size);
        let head = ram.read_u16(self.driver + 4 + 2 * slot)?;
        let chain = self.walk(ram, head)?;
        self.last_avail = self.last_avail.wrapping_add(1);
        self.publish_avail_event(ram)?;
        Ok(Some(chain))
    }

    /// Puts the last `n` extracted chains back into the available ring (the
    /// device did not use them, e.g. a frame that did not fit).
    pub fn rewind(&mut self, ram: &mut dyn GuestRam, n: u16) -> Result<(), RamError> {
        self.last_avail = self.last_avail.wrapping_sub(n);
        self.publish_avail_event(ram)
    }

    fn walk(&self, ram: &dyn GuestRam, head: u16) -> Result<DescChain, QueueError> {
        if head >= self.size {
            return Err(QueueError::HeadOutOfRange(head));
        }
        let mut chain = DescChain { head, ..DescChain::default() };
        let mut d = read_desc(ram, self.desc, head)?;
        let (table, table_len) = if d.flags & DESC_F_INDIRECT != 0 {
            if !self.indirect {
                return Err(QueueError::IndirectNotNegotiated);
            }
            if d.len == 0 || d.len % 16 != 0 || d.len / 16 > u32::from(MAX_QUEUE_SIZE) {
                return Err(QueueError::IndirectLen(d.len));
            }
            let t = (d.addr, (d.len / 16) as u16);
            d = read_desc(ram, t.0, 0)?;
            t
        } else {
            (self.desc, self.size)
        };
        let mut visited = 0u32;
        loop {
            visited += 1;
            if visited > u32::from(table_len) {
                return Err(QueueError::ChainLoop);
            }
            if d.flags & DESC_F_INDIRECT != 0 {
                return Err(QueueError::IndirectMisplaced);
            }
            let buf = Buf { addr: d.addr, len: d.len };
            if d.flags & DESC_F_WRITE != 0 {
                chain.writable.push(buf);
            } else if chain.writable.is_empty() {
                chain.readable.push(buf);
            } else {
                return Err(QueueError::ReadableAfterWritable);
            }
            if d.flags & DESC_F_NEXT == 0 {
                return Ok(chain);
            }
            if d.next >= table_len {
                return Err(QueueError::NextOutOfRange(d.next));
            }
            d = read_desc(ram, table, d.next)?;
        }
    }

    /// Returns chain `head` to the driver with `len` bytes written.
    pub fn push_used(&mut self, ram: &mut dyn GuestRam, head: u16, len: u32) -> Result<(), RamError> {
        let slot = u64::from(self.used_idx % self.size);
        let elem = self.device + 4 + 8 * slot;
        ram.write_u32(elem, u32::from(head))?;
        ram.write_u32(elem + 4, len)?;
        self.used_idx = self.used_idx.wrapping_add(1);
        ram.write_u16(self.device + 2, self.used_idx)
    }

    /// After a series of `push_used`: should the driver be notified? Without
    /// EVENT_IDX `avail.flags` decides (NO_INTERRUPT); with EVENT_IDX
    /// `used_event` must have been passed by this last series.
    pub(crate) fn should_notify(&mut self, ram: &dyn GuestRam) -> Result<bool, RamError> {
        let (old, new) = (self.signalled_used, self.used_idx);
        if !self.ready || old == new {
            return Ok(false);
        }
        self.signalled_used = new;
        if self.event_idx {
            let event = ram.read_u16(self.driver + 4 + 2 * u64::from(self.size))?;
            Ok(need_event(event, new, old))
        } else {
            Ok(ram.read_u16(self.driver)? & AVAIL_F_NO_INTERRUPT == 0)
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// Addresses, size, indices and negotiated features of the queue. The
/// maximum size is device configuration: it is checked.
impl vetro_snapshot::Snapshot for Virtqueue {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u16(self.max_size);
        w.u16(self.size);
        w.bool(self.ready);
        w.u64(self.desc);
        w.u64(self.driver);
        w.u64(self.device);
        w.u16(self.last_avail);
        w.u16(self.used_idx);
        w.u16(self.signalled_used);
        w.bool(self.event_idx);
        w.bool(self.indirect);
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        let max = r.u16()?;
        if max != self.max_size {
            return Err(vetro_snapshot::Error::invalid(format!(
                "queue of {max} in the snapshot, {} in the device",
                self.max_size
            )));
        }
        self.size = r.u16()?;
        self.ready = r.bool()?;
        self.desc = r.u64()?;
        self.driver = r.u64()?;
        self.device = r.u64()?;
        self.last_avail = r.u16()?;
        self.used_idx = r.u16()?;
        self.signalled_used = r.u16()?;
        self.event_idx = r.bool()?;
        self.indirect = r.bool()?;
        Ok(())
    }
}

impl DescChain {
    /// A chain extracted and not yet returned (in-flight request) for
    /// snapshots: the head and the buffers, already validated at extraction.
    pub fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u16(self.head);
        for bufs in [&self.readable, &self.writable] {
            w.seq(bufs, |w, b| {
                w.u64(b.addr);
                w.u32(b.len);
            });
        }
    }

    pub fn restore(r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<Self> {
        let head = r.u16()?;
        let buf = |r: &mut vetro_snapshot::Reader<'_>| Ok(Buf { addr: r.u64()?, len: r.u32()? });
        let readable = r.seq(12, buf)?;
        let writable = r.seq(12, buf)?;
        Ok(DescChain { head, readable, writable })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::VecRam;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    fn ram() -> VecRam {
        VecRam::new(0, 0x10000)
    }

    fn queue(size: u16) -> Virtqueue {
        let mut q = Virtqueue::new(256);
        q.set_size(size);
        q.set_desc(DESC);
        q.set_driver(AVAIL);
        q.set_device(USED);
        assert!(q.set_ready(true));
        q
    }

    fn put_desc(r: &mut VecRam, table: u64, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        r.write(table + 16 * u64::from(i), &b).unwrap();
    }

    fn offer(r: &mut VecRam, size: u16, head: u16) {
        let idx = r.read_u16(AVAIL + 2).unwrap();
        r.write_u16(AVAIL + 4 + 2 * u64::from(idx % size), head).unwrap();
        r.write_u16(AVAIL + 2, idx.wrapping_add(1)).unwrap();
    }

    #[test]
    fn need_event_come_linux() {
        assert!(need_event(0, 1, 0));
        assert!(!need_event(1, 1, 0));
        assert!(need_event(5, 8, 3));
        assert!(!need_event(8, 8, 3));
        assert!(need_event(0xFFFF, 2, 0xFFFE)); // straddling the overflow
    }

    #[test]
    fn ready_rifiuta_configurazioni_invalide() {
        let mut q = Virtqueue::new(8);
        q.set_size(6);
        assert!(!q.set_ready(true), "not a power of 2");
        q.set_size(16);
        assert!(!q.set_ready(true), "above the maximum");
        q.set_size(8);
        q.set_desc(0x1008);
        assert!(!q.set_ready(true), "table not aligned to 16");
        q.set_desc(0x1000);
        q.set_device(0x3002);
        assert!(!q.set_ready(true), "used ring not aligned to 4");
        q.set_device(0x3000);
        assert!(q.set_ready(true));
        assert!(!q.set_ready(false));
    }

    #[test]
    fn catena_diretta_leggibili_poi_scrivibili() {
        let mut r = ram();
        let mut q = queue(8);
        put_desc(&mut r, DESC, 3, 0x5000, 4, DESC_F_NEXT, 5);
        put_desc(&mut r, DESC, 5, 0x6000, 3, DESC_F_NEXT, 1);
        put_desc(&mut r, DESC, 1, 0x7000, 8, DESC_F_WRITE, 0);
        r.write(0x5000, b"abcd").unwrap();
        r.write(0x6000, b"efg").unwrap();
        offer(&mut r, 8, 3);
        assert_eq!(q.available(&r), Ok(1));
        let c = q.pop(&mut r).unwrap().unwrap();
        assert_eq!(c.head, 3);
        assert_eq!(c.readable_len(), 7);
        assert_eq!(c.writable, [Buf { addr: 0x7000, len: 8 }]);
        assert_eq!(c.read_to_vec(&r, 2).unwrap(), b"cdefg");
        assert_eq!(c.write(&mut r, 6, b"xyz").unwrap(), 2);
        assert_eq!(&r.bytes[0x7006..0x7008], b"xy");
        assert_eq!(q.pop(&mut r), Ok(None));
    }

    #[test]
    fn used_ring_e_notifica_senza_event_idx() {
        let mut r = ram();
        let mut q = queue(4);
        for head in [2, 0, 1, 3, 2] {
            put_desc(&mut r, DESC, head, 0x5000, 1, DESC_F_WRITE, 0);
            offer(&mut r, 4, head);
            let c = q.pop(&mut r).unwrap().unwrap();
            q.push_used(&mut r, c.head, 1).unwrap();
        }
        assert_eq!(r.read_u16(USED + 2), Ok(5));
        // The fifth element goes back to slot 0.
        assert_eq!(r.read_u32(USED + 4), Ok(2));
        assert_eq!(r.read_u32(USED + 8), Ok(1));
        assert_eq!(q.should_notify(&r), Ok(true));
        assert_eq!(q.should_notify(&r), Ok(false), "nothing new");
        r.write_u16(AVAIL, AVAIL_F_NO_INTERRUPT).unwrap();
        offer(&mut r, 4, 0);
        let c = q.pop(&mut r).unwrap().unwrap();
        q.push_used(&mut r, c.head, 0).unwrap();
        assert_eq!(q.should_notify(&r), Ok(false));
    }

    #[test]
    fn event_idx_rispetta_used_event_e_pubblica_avail_event() {
        let mut r = ram();
        let mut q = queue(8);
        q.event_idx = true;
        let used_event = AVAIL + 4 + 2 * 8;
        let avail_event = USED + 4 + 8 * 8;
        // The driver wants an interrupt only after the third buffer (idx 2).
        r.write_u16(used_event, 2).unwrap();
        for i in 0..3u16 {
            put_desc(&mut r, DESC, i, 0x5000, 1, 0, 0);
            offer(&mut r, 8, i);
            let c = q.pop(&mut r).unwrap().unwrap();
            assert_eq!(r.read_u16(avail_event), Ok(i + 1));
            q.push_used(&mut r, c.head, 0).unwrap();
            assert_eq!(q.should_notify(&r), Ok(i == 2), "after buffer {i}");
        }
        // Two buffers together that jump over used_event = 4: one notification.
        r.write_u16(used_event, 4).unwrap();
        for i in 3..5u16 {
            put_desc(&mut r, DESC, i, 0x5000, 1, 0, 0);
            offer(&mut r, 8, i);
            let c = q.pop(&mut r).unwrap().unwrap();
            q.push_used(&mut r, c.head, 0).unwrap();
        }
        assert_eq!(q.should_notify(&r), Ok(true));
    }

    #[test]
    fn catena_indiretta() {
        let mut r = ram();
        let mut q = queue(8);
        q.indirect = true;
        let table = 0x8000;
        put_desc(&mut r, table, 0, 0x5000, 2, DESC_F_NEXT, 2);
        put_desc(&mut r, table, 2, 0x5100, 2, DESC_F_NEXT, 1);
        put_desc(&mut r, table, 1, 0x5200, 4, DESC_F_WRITE, 0);
        // WRITE on the indirect descriptor must be ignored, NEXT too.
        put_desc(&mut r, DESC, 6, table, 48, DESC_F_INDIRECT | DESC_F_WRITE | DESC_F_NEXT, 7);
        offer(&mut r, 8, 6);
        let c = q.pop(&mut r).unwrap().unwrap();
        assert_eq!(c.head, 6);
        assert_eq!(c.readable, [Buf { addr: 0x5000, len: 2 }, Buf { addr: 0x5100, len: 2 }]);
        assert_eq!(c.writable, [Buf { addr: 0x5200, len: 4 }]);
    }

    fn pop_err(setup: impl FnOnce(&mut VecRam, &mut Virtqueue)) -> QueueError {
        let mut r = ram();
        let mut q = queue(4);
        setup(&mut r, &mut q);
        q.pop(&mut r).unwrap_err()
    }

    #[test]
    fn errori_delle_catene() {
        assert_eq!(pop_err(|r, _| offer(r, 4, 4)), QueueError::HeadOutOfRange(4));
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 9);
                offer(r, 4, 0);
            }),
            QueueError::NextOutOfRange(9)
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 1);
                put_desc(r, DESC, 1, 0, 1, DESC_F_NEXT, 0);
                offer(r, 4, 0);
            }),
            QueueError::ChainLoop
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_WRITE | DESC_F_NEXT, 1);
                put_desc(r, DESC, 1, 0, 1, 0, 0);
                offer(r, 4, 0);
            }),
            QueueError::ReadableAfterWritable
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectNotNegotiated
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, DESC, 0, 0x8000, 24, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectLen(24)
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                put_desc(r, 0x8000, 0, 0x9000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectMisplaced
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, 0x8000, 0, 0, 1, DESC_F_NEXT, 0);
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::ChainLoop
        );
        assert_eq!(
            pop_err(|r, _| r.write_u16(AVAIL + 2, 5).unwrap()),
            QueueError::AvailIdx { last: 0, avail: 5 }
        );
        assert!(matches!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 1);
                offer(r, 4, 0);
                r.bytes.truncate(0x2000);
            }),
            QueueError::Ram(_)
        ));
    }

    #[test]
    fn rewind_rimette_le_catene() {
        let mut r = ram();
        let mut q = queue(4);
        put_desc(&mut r, DESC, 0, 0x5000, 1, 0, 0);
        put_desc(&mut r, DESC, 1, 0x5000, 1, 0, 0);
        offer(&mut r, 4, 0);
        offer(&mut r, 4, 1);
        q.pop(&mut r).unwrap().unwrap();
        q.pop(&mut r).unwrap().unwrap();
        q.rewind(&mut r, 2).unwrap();
        assert_eq!(q.available(&r), Ok(2));
        assert_eq!(q.pop(&mut r).unwrap().unwrap().head, 0);
    }
}
