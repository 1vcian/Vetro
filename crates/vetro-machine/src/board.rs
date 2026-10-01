//! What lies outside the CPU: RAM, platform and time counter.
//! Implements the physical memory seen by the MMU and the CPU environment
//! ([`CpuEnv`]): generic timer, GIC CPU interface, IRQ line.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use vetro_cpu::sys::TlbiOp;

use vetro_cpu::sys::CpuEnv;
use vetro_cpu::sysreg::EnvReg;
use vetro_jit::SysPhys;
use vetro_mmu::{BusError, PhysMemory};
use vetro_platform::Virt;
use vetro_platform::map;
use vetro_platform::virtio::{GuestRam, RamError, VirtioBlk};

/// The RAM bytes in one contiguous block of host memory.
///
/// Usually a `Vec<u8>`. On wasm32, however, no Rust allocation (and no
/// slice) can exceed `isize::MAX` = 2 GiB - 1, while linear memory goes up to
/// 4 GiB: a larger RAM (Android wants 2–3 GiB, ADR 0028) is a region taken
/// directly with `memory.grow`, outside the allocator, and is read and written
/// only in small pieces. Contiguous in both cases: the JIT's software TLB
/// points into it ([`SysPhys::ram_region`]).
struct Store {
    ptr: *mut u8,
    len: usize,
    /// The vector owning the bytes (absent for the wasm32 region).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    vec: Option<Vec<u8>>,
}

// SAFETY: `Store` owns its bytes (the vector, or the region nobody else
// uses) like a `Vec<u8>`.
unsafe impl Send for Store {}

impl Store {
    fn new(size: u64) -> Self {
        let len = usize::try_from(size).expect("RAM larger than the host address space");
        if len <= isize::MAX as usize {
            let mut v = vec![0u8; len];
            return Store { ptr: v.as_mut_ptr(), len, vec: Some(v) };
        }
        Self::region(len)
    }

    /// A zeroed region of `len` bytes outside the allocator (wasm32 only:
    /// elsewhere `isize::MAX` is always enough). New `memory.grow` pages are
    /// zero by the spec; a region freed by an earlier machine is reused
    /// (linear memory is never given back) after zeroing it.
    #[cfg(target_arch = "wasm32")]
    fn region(len: usize) -> Self {
        let mut free = FREE_REGIONS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = free.iter().position(|&(_, l)| l >= len) {
            let (base, _) = free.swap_remove(i);
            let ptr = core::ptr::with_exposed_provenance_mut::<u8>(base);
            // In pieces: no write longer than isize::MAX.
            let mut at = 0;
            while at < len {
                let n = (len - at).min(1 << 30);
                // SAFETY: the region is ours and at least `len` bytes long.
                unsafe { core::ptr::write_bytes(ptr.wrapping_add(at), 0, n) };
                at += n;
            }
            return Store { ptr, len, vec: None };
        }
        drop(free);
        let pages = len.div_ceil(65536);
        let old = core::arch::wasm32::memory_grow(0, pages);
        assert!(old != usize::MAX, "linear memory exhausted: RAM of {len} bytes");
        Store { ptr: core::ptr::with_exposed_provenance_mut(old * 65536), len, vec: None }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn region(len: usize) -> Self {
        unreachable!("RAM of {len} bytes beyond isize::MAX on a 64-bit host")
    }
    /// `[o, o+n)` (already checked by the caller, `n` small).
    #[inline]
    fn get(&self, o: usize, n: usize) -> &[u8] {
        debug_assert!(o + n <= self.len);
        // SAFETY: within the owned bytes; a slice of length `n` <= isize::MAX.
        unsafe { core::slice::from_raw_parts(self.ptr.wrapping_add(o), n) }
    }

    /// `[o, o+n)` writable through a shared reference.
    ///
    /// # Safety
    /// Nothing else may access those bytes while the slice lives (a
    /// restore, which has the RAM to itself).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut_shared(&self, o: usize, n: usize) -> &mut [u8] {
        debug_assert!(o + n <= self.len);
        // SAFETY: within the owned bytes; exclusivity is the caller's.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.wrapping_add(o), n) }
    }

    /// Copies `[o, o+buf.len())` into `buf` (checked by the caller). An
    /// aligned access of 1, 2, 4 or 8 bytes is a single atomic load.
    #[inline]
    fn load(&self, o: usize, buf: &mut [u8]) {
        debug_assert!(o + buf.len() <= self.len);
        let p = self.ptr.wrapping_add(o);
        let n = buf.len();
        // SAFETY: within the owned bytes; the atomics are aligned (checked).
        unsafe {
            match n {
                8 if p.addr().is_multiple_of(8) => {
                    buf.copy_from_slice(&AtomicU64::from_ptr(p.cast()).load(Ordering::Relaxed).to_le_bytes())
                }
                4 if p.addr().is_multiple_of(4) => {
                    buf.copy_from_slice(&AtomicU32::from_ptr(p.cast()).load(Ordering::Relaxed).to_le_bytes())
                }
                2 if p.addr().is_multiple_of(2) => {
                    buf.copy_from_slice(&AtomicU16::from_ptr(p.cast()).load(Ordering::Relaxed).to_le_bytes())
                }
                1 => buf[0] = AtomicU8::from_ptr(p).load(Ordering::Relaxed),
                _ => core::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), n),
            }
        }
    }

    /// Copies `data` to `[o, o+data.len())` (checked by the caller), like
    /// [`Store::load`].
    #[inline]
    fn store(&self, o: usize, data: &[u8]) {
        debug_assert!(o + data.len() <= self.len);
        let p = self.ptr.wrapping_add(o);
        let n = data.len();
        // SAFETY: as in `load`.
        unsafe {
            match n {
                8 if p.addr().is_multiple_of(8) => AtomicU64::from_ptr(p.cast())
                    .store(u64::from_le_bytes(data.try_into().expect("8 bytes")), Ordering::Relaxed),
                4 if p.addr().is_multiple_of(4) => AtomicU32::from_ptr(p.cast())
                    .store(u32::from_le_bytes(data.try_into().expect("4 bytes")), Ordering::Relaxed),
                2 if p.addr().is_multiple_of(2) => AtomicU16::from_ptr(p.cast())
                    .store(u16::from_le_bytes(data.try_into().expect("2 bytes")), Ordering::Relaxed),
                1 => AtomicU8::from_ptr(p).store(data[0], Ordering::Relaxed),
                _ => core::ptr::copy_nonoverlapping(data.as_ptr(), p, n),
            }
        }
    }

    /// Atomic compare-and-exchange of `n` (1, 2, 4, 8) bytes at `o`, as
    /// little-endian values; true if it wrote. Unaligned host addresses (a RAM
    /// vector that is not 8-aligned) fall back to a plain compare and write.
    fn cmpxchg(&self, o: usize, n: usize, old: u64, new: u64) -> bool {
        let p = self.ptr.wrapping_add(o);
        if !p.addr().is_multiple_of(n) {
            let mut cur = [0u8; 8];
            self.load(o, &mut cur[..n]);
            if u64::from_le_bytes(cur) != old {
                return false;
            }
            self.store(o, &new.to_le_bytes()[..n]);
            return true;
        }
        let (s, f) = (Ordering::SeqCst, Ordering::SeqCst);
        // SAFETY: within the owned bytes, aligned.
        unsafe {
            match n {
                8 => AtomicU64::from_ptr(p.cast()).compare_exchange(old, new, s, f).is_ok(),
                4 => AtomicU32::from_ptr(p.cast()).compare_exchange(old as u32, new as u32, s, f).is_ok(),
                2 => AtomicU16::from_ptr(p.cast()).compare_exchange(old as u16, new as u16, s, f).is_ok(),
                _ => AtomicU8::from_ptr(p).compare_exchange(old as u8, new as u8, s, f).is_ok(),
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for Store {
    fn drop(&mut self) {
        if self.vec.is_none() {
            FREE_REGIONS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((self.ptr.expose_provenance(), self.len));
        }
    }
}

/// RAM regions of destroyed machines, to reuse (wasm32).
#[cfg(target_arch = "wasm32")]
static FREE_REGIONS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// RAM piece size for whole-RAM operations (hash, comparisons).
pub const RAM_CHUNK: usize = 1 << 28;

/// The guest RAM, from `map::RAM_BASE`.
///
/// Watches the pages from which the JIT translated code
/// ([`watch_code`](Self::watch_code)): every write that goes through here (CPU,
/// device DMA, image loading) marks them dirty. That is
/// why bytes are written only with [`write`](Self::write).
///
/// Shared (ADR 0042): every method takes `&self`, so the cores of a parallel
/// machine read and write it from their threads without a lock, as the
/// JIT's regions do with plain WebAssembly accesses. The watch bitmap is
/// atomic, and each JIT (consumer) has its own list of written pages. Aligned
/// accesses of 1, 2, 4 and 8 bytes are single atomic accesses (the
/// architecture's single-copy atomicity); concurrent accesses to the same bytes
/// are the guest's business, as on hardware and in QEMU.
pub struct Ram {
    bytes: Store,
    /// One bit per 4 KiB page: watched.
    code: Vec<AtomicU64>,
    /// Watched pages.
    watched: AtomicUsize,
    /// Physical pages (`pa >> 12`) watched and then written, for each
    /// consumer (one per JIT, [`Ram::set_consumers`]); only the first
    /// `consumers` receive them.
    dirty: Vec<Mutex<Vec<u64>>>,
    consumers: AtomicUsize,
}

// SAFETY: the bytes are only reached through `&self` methods that copy in and
// out (atomically for the small aligned accesses), and the bookkeeping is
// atomic or behind mutexes; like guest RAM shared by cores (or by a CPU and
// devices) on hardware.
unsafe impl Sync for Ram {}

impl Ram {
    pub fn new(size: u64) -> Self {
        Self::with_consumers(size, 1)
    }

    /// A RAM whose written code pages up to `n` JITs can follow (one per core
    /// running in parallel, ADR 0042); one at first.
    pub fn with_consumers(size: u64, n: usize) -> Self {
        let pages = size.div_ceil(4096) as usize;
        Ram {
            bytes: Store::new(size),
            code: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            watched: AtomicUsize::new(0),
            dirty: (0..n.max(1)).map(|_| Mutex::new(Vec::new())).collect(),
            consumers: AtomicUsize::new(1),
        }
    }

    /// Number of JITs following the written code pages (at most the number
    /// given to [`Ram::with_consumers`]): [`take_code_dirty_for`](Self::take_code_dirty_for)
    /// takes consumer `i`'s list. The lists of the new consumers start empty
    /// (their JITs have no blocks yet); those of the dropped ones are emptied.
    pub fn set_consumers(&self, n: usize) {
        let n = n.clamp(1, self.dirty.len());
        for d in &self.dirty[1..] {
            lock(d).clear();
        }
        self.consumers.store(n, Ordering::SeqCst);
    }

    /// RAM bytes.
    pub fn size(&self) -> u64 {
        self.bytes.len as u64
    }

    /// The RAM bytes as one slice (read-only, with nothing else writing).
    /// Always on 64-bit hosts; on wasm32 only up to 2 GiB - 1 (beyond that,
    /// [`chunks`](Self::chunks)).
    pub fn bytes(&self) -> &[u8] {
        assert!(self.bytes.len <= isize::MAX as usize, "RAM beyond isize::MAX: use Ram::chunks");
        self.bytes.get(0, self.bytes.len)
    }

    /// The RAM in pieces of [`RAM_CHUNK`] bytes (the last one shorter), in
    /// order: beyond 2 GiB on wasm32 there is no slice of the whole RAM.
    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        let len = self.bytes.len;
        (0..len.div_ceil(RAM_CHUNK))
            .map(move |i| self.bytes.get(i * RAM_CHUNK, (len - i * RAM_CHUNK).min(RAM_CHUNK)))
    }

    /// [`vetro_snapshot::hash64`] of the whole RAM, without a slice of the
    /// whole RAM (same value: the pieces are multiples of 8 bytes).
    pub fn hash(&self) -> u64 {
        const P: u64 = 0x0000_0100_0000_01b3;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ self.bytes.len as u64;
        for c in self.chunks() {
            let (words, rest) = c.as_chunks::<8>();
            for w in words {
                h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(P).rotate_left(23);
            }
            for &b in rest {
                h = (h ^ u64::from(b)).wrapping_mul(P);
            }
        }
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^ (h >> 31)
    }

    /// The content of a snapshot's `RAM ` section ([`vetro_snapshot::blocks`]
    /// format, as [`vetro_snapshot::compress`] at `level`) in chunks of about
    /// 1 MiB, in order: no buffer as large as the snapshot (ADR 0028). Two
    /// calls give the same bytes.
    pub fn save_chunks(&self, level: vetro_snapshot::Level, emit: &mut dyn FnMut(&[u8])) {
        const PAGE: usize = vetro_snapshot::BLOCK;
        let len = self.bytes.len;
        vetro_snapshot::blocks::encode(
            len,
            level,
            &|i| self.bytes.get(i * PAGE, (len - i * PAGE).min(PAGE)),
            emit,
        );
    }

    /// True if the two RAMs have the same bytes.
    pub fn same_bytes(&self, other: &Ram) -> bool {
        self.size() == other.size() && self.chunks().zip(other.chunks()).all(|(a, b)| a == b)
    }

    /// Watches the physical page `page` (`pa >> 12`); false if it is not RAM.
    pub fn watch_code(&self, page: u64) -> bool {
        let Some(i) = (page << 12).checked_sub(map::RAM_BASE).map(|o| (o >> 12) as usize) else {
            return false;
        };
        if (i as u64) << 12 >= self.size() {
            return false;
        }
        let (w, b) = (i / 64, 1u64 << (i % 64));
        if self.code[w].fetch_or(b, Ordering::SeqCst) & b == 0 {
            self.watched.fetch_add(1, Ordering::SeqCst);
        }
        true
    }

    /// True if the physical page `page` is watched.
    pub fn is_watched(&self, page: u64) -> bool {
        match (page << 12).checked_sub(map::RAM_BASE) {
            Some(o) if o < self.size() => {
                let i = (o >> 12) as usize;
                self.code[i / 64].load(Ordering::SeqCst) & 1 << (i % 64) != 0
            }
            _ => false,
        }
    }

    /// Appends to `out` the watched pages written since then (consumer 0).
    pub fn take_code_dirty(&self, out: &mut Vec<u64>) {
        self.take_code_dirty_for(0, out);
    }

    /// Like [`take_code_dirty`](Self::take_code_dirty) for consumer `i`.
    pub fn take_code_dirty_for(&self, i: usize, out: &mut Vec<u64>) {
        let mut d = lock(&self.dirty[i]);
        if !d.is_empty() {
            out.append(&mut d);
        }
    }

    /// Marks dirty (and no longer watched) the pages of `[o, o+len)`
    /// (offset into RAM); true if there was at least one.
    #[inline]
    fn touch(&self, o: usize, len: usize) -> bool {
        if self.watched.load(Ordering::SeqCst) == 0 || len == 0 {
            return false;
        }
        let mut hit = false;
        for i in o >> 12..=(o + len - 1) >> 12 {
            let (w, b) = (i / 64, 1u64 << (i % 64));
            if self.code[w].load(Ordering::SeqCst) & b != 0
                && self.code[w].fetch_and(!b, Ordering::SeqCst) & b != 0
            {
                self.watched.fetch_sub(1, Ordering::SeqCst);
                let page = (map::RAM_BASE >> 12) + i as u64;
                for d in &self.dirty[..self.consumers.load(Ordering::SeqCst)] {
                    lock(d).push(page);
                }
                hit = true;
            }
        }
        hit
    }

    /// A write that also says whether it touched watched code: `None`
    /// outside RAM.
    pub fn write_watched(&self, pa: u64, data: &[u8]) -> Option<bool> {
        let o = self.range(pa, data.len())?;
        self.bytes.store(o, data);
        Some(self.touch(o, data.len()))
    }

    /// Offset in `bytes` of `[pa, pa+len)`, if entirely inside RAM.
    #[inline]
    fn range(&self, pa: u64, len: usize) -> Option<usize> {
        let off = pa.checked_sub(map::RAM_BASE)?;
        (off.checked_add(len as u64)? <= self.bytes.len as u64).then_some(off as usize)
    }

    pub fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        match self.range(pa, buf.len()) {
            Some(o) => {
                self.bytes.load(o, buf);
                true
            }
            None => false,
        }
    }

    pub fn write(&self, pa: u64, data: &[u8]) -> bool {
        self.write_watched(pa, data).is_some()
    }

    /// Atomic compare-and-exchange of `old.len()` (1, 2, 4 or 8, aligned)
    /// bytes at `pa` (exclusives, ADR 0042): writes `new` if the bytes are
    /// `old`. `None` outside RAM or for another size.
    pub fn cmpxchg(&self, pa: u64, old: &[u8], new: &[u8]) -> Option<bool> {
        let n = old.len();
        if n != new.len() || !matches!(n, 1 | 2 | 4 | 8) || !pa.is_multiple_of(n as u64) {
            return None;
        }
        let o = self.range(pa, n)?;
        let le = |b: &[u8]| {
            let mut v = [0u8; 8];
            v[..n].copy_from_slice(b);
            u64::from_le_bytes(v)
        };
        let ok = self.bytes.cmpxchg(o, n, le(old), le(new));
        if ok {
            self.touch(o, n);
        }
        Some(ok)
    }

    /// Host address of the RAM bytes (for the JIT's software TLB).
    pub fn host_ptr(&self) -> *mut u8 {
        self.bytes.ptr
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A mutex of the cores' slots, ignoring poisoning.
pub(crate) fn lock_slot<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock(m)
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// The RAM bytes in 4 KiB pages: zero pages take no space,
/// the others are compressed (`vetro_snapshot::compress`). Watching
/// code pages is not guest state: on restore every watched page
/// counts as written, so the JIT discards the blocks translated from the
/// earlier RAM.
impl vetro_snapshot::Snapshot for Ram {
    /// The same format as [`vetro_snapshot::compress`] on the whole RAM,
    /// written page by page ([`Ram::save_chunks`]).
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        let level = w.level();
        self.save_chunks(level, &mut |c| w.raw(c));
    }

    /// Like [`vetro_snapshot::decompress_into`] on the whole RAM. Absent pages
    /// must be zero: only those that are not already zero are written, so a
    /// freshly allocated RAM stays untouched (in the browser, pages never
    /// written take no memory).
    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.restore_from(r)
    }
}

/// Where the content of a snapshot's `RAM ` section comes from: a
/// [`vetro_snapshot::Reader`] over the whole file, or the chunks of a file read
/// little by little ([`Machine::load_state_stream`](crate::Machine::load_state_stream)).
pub trait RamSource {
    /// The next `n` bytes.
    fn take(&mut self, n: usize) -> vetro_snapshot::Result<&[u8]>;
}

impl RamSource for vetro_snapshot::Reader<'_> {
    fn take(&mut self, n: usize) -> vetro_snapshot::Result<&[u8]> {
        self.raw(n)
    }
}

impl Ram {
    /// Restores the RAM from the content of the `RAM ` section (see
    /// [`vetro_snapshot::Snapshot::restore`] for [`Ram`]), from any
    /// [`RamSource`]. Nothing else may use the RAM meanwhile.
    pub fn restore_from(&self, r: &mut dyn RamSource) -> vetro_snapshot::Result<()> {
        const PAGE: usize = vetro_snapshot::BLOCK;
        struct Src<'a>(&'a mut dyn RamSource);
        impl vetro_snapshot::blocks::Source for Src<'_> {
            fn take(&mut self, n: usize) -> vetro_snapshot::Result<&[u8]> {
                self.0.take(n)
            }
        }
        struct Dst<'a>(&'a Ram);
        impl vetro_snapshot::blocks::Target for Dst<'_> {
            fn block_mut(&mut self, i: usize) -> &mut [u8] {
                let len = self.0.bytes.len;
                // SAFETY: the restore has the RAM to itself (see above).
                unsafe { self.0.bytes.get_mut_shared(i * PAGE, (len - i * PAGE).min(PAGE)) }
            }
        }
        let len = self.bytes.len;
        let pages = len.div_ceil(PAGE);
        let mut present = vec![0u64; pages.div_ceil(64)];
        vetro_snapshot::blocks::decode(&mut Src(r), len, &mut Dst(self), &mut |i| {
            present[i / 64] |= 1 << (i % 64)
        })?;
        for i in 0..pages {
            if present[i / 64] & 1 << (i % 64) == 0 {
                // SAFETY: as above.
                let p = unsafe { self.bytes.get_mut_shared(i * PAGE, (len - i * PAGE).min(PAGE)) };
                if !vetro_snapshot::is_zero(p) {
                    p.fill(0);
                }
            }
        }
        self.touch(0, len);
        Ok(())
    }
}

impl GuestRam for Ram {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError> {
        if Ram::read(self, addr, buf) { Ok(()) } else { Err(RamError { addr, len: buf.len() }) }
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError> {
        if Ram::write(self, addr, data) { Ok(()) } else { Err(RamError { addr, len: data.len() }) }
    }
}

/// A shared reference to the RAM as a [`GuestRam`]: the devices of a
/// parallel machine, whose cores keep using it (ADR 0042).
pub struct RamRef<'a>(pub &'a Ram);

impl GuestRam for RamRef<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError> {
        if self.0.read(addr, buf) { Ok(()) } else { Err(RamError { addr, len: buf.len() }) }
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError> {
        if self.0.write(addr, data) { Ok(()) } else { Err(RamError { addr, len: data.len() }) }
    }
}

// SAFETY: the board is reached only through `BoardCell`'s lock; its devices
// and their backends are Rust data (disks fed by the host, displays in
// memory), so any core's thread may service them while holding the lock.
unsafe impl Send for Board {}

/// RAM, platform and time.
pub struct Board {
    /// Shared with the cores of a parallel machine (ADR 0042).
    pub ram: Arc<Ram>,
    pub virt: Virt,
    /// Current value of CNTPCT_EL0.
    pub cntpct: u64,
    /// Something may have changed the level of an interrupt line
    /// (MMIO access, timer register): `update_irqs` must be called.
    pub(crate) irq_dirty: bool,
    /// An access to a virtio-mmio slot: the device must be serviced.
    pub(crate) virtio_dirty: bool,
    /// After the last virtio service a virtio-blk request is waiting for
    /// data from the host (`BlockError::NotReady`): the machine executes no
    /// instructions until they arrive (`Stop::Blocked`).
    pub(crate) host_wait: bool,
    /// The cores' IRQ lines as seen without the lock, and their wakeups
    /// (ADR 0042).
    pub(crate) cores: Arc<Cores>,
}

impl Board {
    pub fn new(ram_size: u64, now_secs: u64) -> Self {
        Self::with_cpus(ram_size, now_secs, 1)
    }

    /// A board for `cpus` cores (a redistributor and a timer each).
    pub fn with_cpus(ram_size: u64, now_secs: u64, cpus: usize) -> Self {
        Board {
            ram: Arc::new(Ram::with_consumers(ram_size, cpus)),
            virt: Virt::with_cpus(now_secs, cpus),
            cntpct: 0,
            irq_dirty: true,
            virtio_dirty: false,
            host_wait: false,
            cores: Arc::new(Cores::new(cpus)),
        }
    }

    /// Something may have changed the GIC's state: the cached lines are
    /// stale. With the cores in parallel the lines are recomputed at once
    /// and a core whose line rose is woken (ADR 0042).
    pub(crate) fn lines_changed(&mut self) {
        let c = &self.cores;
        if !c.parallel.load(Ordering::SeqCst) {
            for s in &c.slots {
                s.irq.store(IRQ_UNKNOWN, Ordering::SeqCst);
            }
            return;
        }
        for (i, s) in c.slots.iter().enumerate() {
            let l = if self.virt.irq_line_of(i) { IRQ_HIGH } else { IRQ_LOW };
            if s.irq.swap(l, Ordering::SeqCst) != IRQ_HIGH && l == IRQ_HIGH {
                c.kick(i);
            }
        }
    }

    /// Brings the levels of all lines to the GIC.
    pub fn update_irqs(&mut self) {
        self.virt.update_irqs(self.cntpct);
        self.lines_changed();
        self.irq_dirty = false;
        if self.cores.parallel.load(Ordering::Relaxed) {
            // Each core's next timer deadline, which it watches without the lock.
            for (i, s) in self.cores.slots.iter().enumerate() {
                let d = self.virt.timers[i].next_deadline(self.cntpct).unwrap_or(u64::MAX);
                s.deadline.store(d, Ordering::SeqCst);
            }
        }
    }

    /// With the cores in parallel, a change is applied at once: devices
    /// serviced, lines and deadlines updated (nobody polls `irq_dirty`).
    fn settle(&mut self) {
        if self.cores.parallel.load(Ordering::Relaxed) {
            if self.virtio_dirty {
                self.service_virtio();
            }
            self.update_irqs();
        }
    }

    /// Runs the virtio devices on top of RAM.
    pub fn service_virtio(&mut self) {
        let Board { ram, virt, .. } = self;
        virt.service_virtio(&mut RamRef(ram));
        self.host_wait = (0..map::VIRTIO_SLOTS as u32).any(|k| {
            self.virt.virtio(k).and_then(|t| t.device_as::<VirtioBlk>()).is_some_and(VirtioBlk::has_pending)
        });
        self.lines_changed();
        self.virtio_dirty = false;
        self.irq_dirty = true;
    }

    /// Drives input line `line` of the PL061 GPIO: line 3
    /// (`vetro_platform::pl061::POWER_KEY_LINE`) is the power key
    /// (`gpio-keys`, KEY_POWER). The interrupt reaches the guest before the
    /// next instruction. It is a host input: the host goes through
    /// `Machine::gpio_input` (or `Machine::input`), which records it for
    /// replay (M10, ADR 0019); called here directly it escapes the log.
    pub fn gpio_input(&mut self, line: u32, level: bool) {
        self.virt.gpio_mut().set_input(line, level);
        self.lines_changed();
        self.irq_dirty = true;
        self.settle();
    }

    fn mmio_touched(&mut self, pa: u64) {
        self.lines_changed();
        self.irq_dirty = true;
        let virtio_end = map::VIRTIO_BASE + map::VIRTIO_SLOTS * map::VIRTIO_SLOT_SIZE;
        if (map::VIRTIO_BASE..virtio_end).contains(&pa) {
            self.virtio_dirty = true;
        }
        self.settle();
    }
}

/// [`Slot::irq`]: not computed since the last change.
const IRQ_UNKNOWN: u8 = 0;
const IRQ_LOW: u8 = 1;
const IRQ_HIGH: u8 = 2;

/// What every core needs to see of the others without the board's lock
/// (ADR 0042): its IRQ line, a way to wake it, and with the cores in parallel
/// their coordination (`machine::parallel`).
pub(crate) struct Cores {
    pub slots: Vec<Slot>,
    /// The cores run on host threads at the same time: lines are computed
    /// eagerly and TLBIs and code watches are broadcast.
    pub parallel: AtomicBool,
    /// With the cores in parallel: the clock (steps of every core plus the
    /// time skipped when all wait; CNTPCT is that of `clock / n`).
    pub clock: AtomicU64,
    /// Cores waiting in WFI.
    pub idle: AtomicUsize,
    /// The host wants the cores to stop (`Machine::stop_parallel`).
    pub stop: AtomicBool,
    /// A core powered the machine off (1) or reset it (2).
    pub halt: AtomicU8,
    /// Clock value of the network stack's next deadline (`u64::MAX` none),
    /// for the jump of time when every core waits.
    pub net_deadline: AtomicU64,
}

/// One core's shared state.
#[allow(dead_code)] // the parallel cores' fields: machine::parallel
pub(crate) struct Slot {
    /// Its IRQ line (`IRQ_*`).
    irq: AtomicU8,
    /// Bumped by every kick; a core waiting in WFI wakes when it changes.
    pub kick: AtomicU32,
    /// The kick count the core last looked at ([`Slot::clear_kick`]).
    seen: AtomicU32,
    /// Set by a kick: the core's JIT run ends at the next region boundary.
    pub abort: Arc<AtomicBool>,
    /// Address of the core's `JitState` step limit (0 = none): a kick writes
    /// zero there, so the dispatcher returns at the next block.
    pub limit: AtomicUsize,
    /// For sleeping in WFI.
    pub sleep: Mutex<()>,
    pub wake: Condvar,
    /// Requests from the other cores ([`Request`]) and how many were applied.
    pub inbox: Mutex<Vec<Request>>,
    pub posted: AtomicU64,
    pub applied: AtomicU64,
    /// Powered on, and the CPU_ON that started it (entry, context).
    pub on: AtomicBool,
    pub start: Mutex<Option<(u64, u64)>>,
    /// Clock value at which this core, waiting in WFI, wants to wake (its
    /// timer deadline), `u64::MAX` = none, 0 = not waiting.
    pub wait_until: AtomicU64,
    /// With the cores in parallel: its generic timer's next deadline
    /// (CNTPCT, `u64::MAX` = none), as of the last `update_irqs`.
    pub deadline: AtomicU64,
}

impl Slot {
    /// The core looks at what kicked it (requests, lines, stop).
    pub fn clear_kick(&self) {
        self.seen.store(self.kick.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Kicked since [`Slot::clear_kick`].
    pub fn kick_seen(&self) -> bool {
        self.kick.load(Ordering::SeqCst) != self.seen.load(Ordering::SeqCst)
    }
}

/// What a core asks of another one in parallel (applied between runs, then
/// acknowledged through [`Slot::applied`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// A TLBI that was broadcast (the local form).
    Tlbi(TlbiOp, u64),
    /// A page now holds translated code: no more direct writes to it from the
    /// JIT's software TLB.
    Watch(u64),
}

#[allow(dead_code)] // the coordination of the parallel cores: machine::parallel
impl Cores {
    fn new(n: usize) -> Self {
        Cores {
            slots: (0..n)
                .map(|i| Slot {
                    irq: AtomicU8::new(IRQ_UNKNOWN),
                    kick: AtomicU32::new(0),
                    seen: AtomicU32::new(0),
                    abort: Arc::new(AtomicBool::new(false)),
                    limit: AtomicUsize::new(0),
                    sleep: Mutex::new(()),
                    wake: Condvar::new(),
                    inbox: Mutex::new(Vec::new()),
                    posted: AtomicU64::new(0),
                    applied: AtomicU64::new(0),
                    on: AtomicBool::new(i == 0),
                    start: Mutex::new(None),
                    wait_until: AtomicU64::new(0),
                    deadline: AtomicU64::new(u64::MAX),
                })
                .collect(),
            parallel: AtomicBool::new(false),
            clock: AtomicU64::new(0),
            idle: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            halt: AtomicU8::new(0),
            net_deadline: AtomicU64::new(u64::MAX),
        }
    }

    /// Wakes core `i`: out of a WFI wait, and out of its JIT run at the next
    /// region boundary.
    pub fn kick(&self, i: usize) {
        let s = &self.slots[i];
        s.kick.fetch_add(1, Ordering::SeqCst);
        s.abort.store(true, Ordering::SeqCst);
        let at = s.limit.load(Ordering::SeqCst);
        if at != 0 {
            // SAFETY: the core published the address of its JitState's
            // limit, an aligned u64 that lives as long as its JIT
            // (`machine::parallel`).
            unsafe { AtomicU64::from_ptr(at as *mut u64) }.store(0, Ordering::SeqCst);
        }
        let _g = lock(&s.sleep);
        s.wake.notify_all();
    }

    /// Posts `r` to every other core that is on, kicking them; returns the
    /// count each must reach (`(core, posted)`), for [`Cores::acked`].
    pub fn broadcast(&self, from: usize, r: Request) -> Vec<(usize, u64)> {
        let mut out = Vec::new();
        for (i, s) in self.slots.iter().enumerate() {
            if i == from || !s.on.load(Ordering::SeqCst) {
                continue;
            }
            let n = {
                let mut q = lock(&s.inbox);
                q.push(r);
                s.posted.fetch_add(1, Ordering::SeqCst) + 1
            };
            out.push((i, n));
            self.kick(i);
        }
        out
    }

    /// True if every core in `waits` has applied its requests up to the count.
    pub fn acked(&self, waits: &[(usize, u64)]) -> bool {
        waits.iter().all(|&(i, n)| {
            let s = &self.slots[i];
            s.applied.load(Ordering::SeqCst) >= n || !s.on.load(Ordering::SeqCst)
        })
    }

    /// Takes core `i`'s pending requests; [`Cores::done`] acknowledges them.
    pub fn take(&self, i: usize) -> Vec<Request> {
        core::mem::take(&mut *lock(&self.slots[i].inbox))
    }

    pub fn done(&self, i: usize, n: usize) {
        self.slots[i].applied.fetch_add(n as u64, Ordering::SeqCst);
    }

    /// Core `i`'s IRQ line from the cache, or `None` if it must be computed.
    fn cached_irq(&self, i: usize) -> Option<bool> {
        match self.slots[i].irq.load(Ordering::SeqCst) {
            IRQ_HIGH => Some(true),
            IRQ_LOW => Some(false),
            _ => None,
        }
    }
}

/// The board shared by the cores (ADR 0042): the devices behind a lock, the
/// RAM and the cores' lines outside it. `borrow` and `borrow_mut` take the
/// lock (the names of the single-threaded `RefCell` it replaces).
pub struct BoardCell {
    dev: Mutex<Board>,
    ram: Arc<Ram>,
    pub(crate) cores: Arc<Cores>,
    /// Debug builds: the thread holding the lock, so that taking it again on
    /// the same thread panics instead of hanging.
    #[cfg(debug_assertions)]
    owner: AtomicUsize,
}

/// A guard of [`BoardCell`]: the board, locked.
pub struct BoardGuard<'a> {
    g: MutexGuard<'a, Board>,
    #[cfg(debug_assertions)]
    owner: &'a AtomicUsize,
}

impl core::ops::Deref for BoardGuard<'_> {
    type Target = Board;
    fn deref(&self) -> &Board {
        &self.g
    }
}

impl core::ops::DerefMut for BoardGuard<'_> {
    fn deref_mut(&mut self) -> &mut Board {
        &mut self.g
    }
}

#[cfg(debug_assertions)]
impl Drop for BoardGuard<'_> {
    fn drop(&mut self) {
        self.owner.store(0, Ordering::SeqCst);
    }
}

#[cfg(debug_assertions)]
fn thread_tag() -> usize {
    thread_local!(static TAG: u8 = const { 0 });
    TAG.with(|t| core::ptr::from_ref(t).addr())
}

impl BoardCell {
    pub fn new(b: Board) -> Self {
        BoardCell {
            ram: b.ram.clone(),
            cores: b.cores.clone(),
            dev: Mutex::new(b),
            #[cfg(debug_assertions)]
            owner: AtomicUsize::new(0),
        }
    }

    /// Takes the board's lock (one at a time: the guard must be dropped
    /// before the same thread takes it again).
    pub fn borrow(&self) -> BoardGuard<'_> {
        #[cfg(debug_assertions)]
        assert_ne!(
            self.owner.load(Ordering::SeqCst),
            thread_tag(),
            "board lock taken twice by the same thread"
        );
        let g = lock(&self.dev);
        #[cfg(debug_assertions)]
        self.owner.store(thread_tag(), Ordering::SeqCst);
        BoardGuard {
            g,
            #[cfg(debug_assertions)]
            owner: &self.owner,
        }
    }

    pub fn borrow_mut(&self) -> BoardGuard<'_> {
        self.borrow()
    }

    /// The RAM, without the lock.
    pub fn ram(&self) -> &Ram {
        &self.ram
    }

    #[allow(dead_code)] // machine::parallel
    pub(crate) fn ram_arc(&self) -> &Arc<Ram> {
        &self.ram
    }
}

fn mmio_size(len: usize) -> Option<u8> {
    matches!(len, 1 | 2 | 4 | 8).then_some(len as u8)
}

/// Physical memory of core `core`: RAM (without the lock), otherwise the MMIO
/// bus (an access of 1, 2, 4 or 8 bytes; whatever does not respond is a
/// decode error). `consumer` is the JIT whose written code pages it takes.
pub(crate) struct Phys<'a> {
    pub cell: &'a BoardCell,
    pub core: usize,
    pub consumer: usize,
    /// Broadcast requests posted by this core whose acknowledgement it
    /// still has to wait for (parallel cores, `machine::parallel`).
    pub waits: &'a mut Vec<(usize, u64)>,
}

impl<'a> Phys<'a> {
    /// Core `core` with consumer 0 and no broadcasts (one JIT, cores in turns).
    pub fn single(cell: &'a BoardCell, core: usize, waits: &'a mut Vec<(usize, u64)>) -> Self {
        Phys { cell, core, consumer: 0, waits }
    }

    fn parallel(&self) -> bool {
        self.cell.cores.parallel.load(Ordering::Relaxed)
    }
}

impl PhysMemory for Phys<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.cell.ram.read(pa, buf) {
            return Ok(());
        }
        let size = mmio_size(buf.len()).ok_or(BusError::Slave)?;
        let mut b = self.cell.borrow_mut();
        b.virt.set_current_cpu(self.core);
        let v = b.virt.bus.read(pa, size).ok_or(BusError::Decode)?;
        buf.copy_from_slice(&v.to_le_bytes()[..buf.len()]);
        b.mmio_touched(pa);
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        if self.cell.ram.write(pa, data) {
            return Ok(());
        }
        let size = mmio_size(data.len()).ok_or(BusError::Slave)?;
        let mut v = [0u8; 8];
        v[..data.len()].copy_from_slice(data);
        let mut b = self.cell.borrow_mut();
        b.virt.set_current_cpu(self.core);
        if !b.virt.bus.write(pa, size, u64::from_le_bytes(v)) {
            return Err(BusError::Decode);
        }
        b.mmio_touched(pa);
        Ok(())
    }

    fn cmpxchg(&mut self, pa: u64, old: &[u8], new: &[u8]) -> Result<bool, BusError> {
        if let Some(ok) = self.cell.ram.cmpxchg(pa, old, new) {
            return Ok(ok);
        }
        // 16 bytes (STXP of two doublewords), or not RAM: read, compare and
        // write under the board's lock, which every 16-byte exclusive takes.
        let _g = self.cell.borrow_mut();
        let mut cur = [0u8; 16];
        let cur = &mut cur[..old.len()];
        if !self.cell.ram.read(pa, cur) {
            return Err(BusError::Decode);
        }
        if cur != old {
            return Ok(false);
        }
        self.cell.ram.write(pa, new);
        Ok(true)
    }

    fn tlbi_broadcast(&mut self, op: TlbiOp, xt: u64) {
        if self.parallel() {
            let local = local_tlbi(op);
            let w = self.cell.cores.broadcast(self.core, Request::Tlbi(local, xt));
            self.waits.extend(w);
        }
    }
}

/// The local form of a broadcast TLBI.
fn local_tlbi(op: TlbiOp) -> TlbiOp {
    use TlbiOp::*;
    match op {
        Vmalle1is => Vmalle1,
        Vae1is => Vae1,
        Aside1is => Aside1,
        Vaae1is => Vaae1,
        Vale1is => Vale1,
        Vaale1is => Vaale1,
        other => other,
    }
}

/// Physical memory for the JIT: RAM only, with the code pages
/// watched.
impl SysPhys for Phys<'_> {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool {
        self.cell.ram.read(pa, buf)
    }
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        self.cell.ram.write_watched(pa, data)
    }
    fn watch_code(&mut self, page: u64) -> bool {
        let fresh = !self.cell.ram.is_watched(page);
        let ok = self.cell.ram.watch_code(page);
        if ok && fresh && self.parallel() {
            let w = self.cell.cores.broadcast(self.core, Request::Watch(page));
            self.waits.extend(w);
        }
        ok
    }
    fn watch_ready(&mut self, _page: u64) -> bool {
        // With the cores in parallel a newly watched page is translated only
        // once the others no longer write to it directly.
        self.waits.is_empty()
    }
    fn is_watched(&self, page: u64) -> bool {
        self.cell.ram.is_watched(page)
    }
    fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        self.cell.ram.take_code_dirty_for(self.consumer, out)
    }
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        let r = &self.cell.ram;
        Some((map::RAM_BASE, r.bytes.ptr, r.bytes.len))
    }
}

/// The environment of core `core`: its IRQ line, its generic timer, its GIC
/// CPU interface (ICC_*).
pub(crate) struct Env<'a> {
    pub cell: &'a BoardCell,
    pub core: usize,
    /// CNTPCT for the counter reads, if the caller computes it (the parallel
    /// cores' shared clock); otherwise the board's.
    pub now: Option<u64>,
}

impl<'a> Env<'a> {
    pub fn new(cell: &'a BoardCell, core: usize) -> Self {
        Env { cell, core, now: None }
    }
}

/// The CPU interface's "no interrupt" INTID.
const SPURIOUS: u64 = 1023;

impl CpuEnv for Env<'_> {
    fn irq_line(&mut self) -> bool {
        let cores = &self.cell.cores;
        if let Some(l) = cores.cached_irq(self.core) {
            return l;
        }
        // Computed and stored under the lock, so a change made meanwhile by
        // another core cannot be overwritten with a stale level.
        let b = self.cell.borrow();
        let l = b.virt.irq_line_of(self.core);
        cores.slots[self.core].irq.store(if l { IRQ_HIGH } else { IRQ_LOW }, Ordering::SeqCst);
        l
    }

    fn read_sysreg(&mut self, reg: EnvReg) -> u64 {
        use EnvReg::*;
        let mut b = self.cell.borrow_mut();
        if let Some(n) = self.now {
            b.cntpct = b.cntpct.max(n);
        }
        let c = self.now.unwrap_or(b.cntpct);
        b.virt.set_current_cpu(self.core);
        let v = &mut b.virt;
        let r = match reg {
            CntfrqEl0 => u64::from(map::CNTFRQ_HZ),
            CntpctEl0 => c,
            CntvctEl0 => v.timer_mut().cntvct(c),
            CntpTvalEl0 => v.timer_mut().cntp_tval(c),
            CntpCtlEl0 => v.timer_mut().cntp_ctl(c),
            CntpCvalEl0 => v.timer_mut().cntp_cval(),
            CntvTvalEl0 => v.timer_mut().cntv_tval(c),
            CntvCtlEl0 => v.timer_mut().cntv_ctl(c),
            CntvCvalEl0 => v.timer_mut().cntv_cval(),
            IccPmrEl1 => v.gic().read_pmr(),
            IccIar1El1 => v.gic_mut().read_iar1(),
            IccHppir1El1 => v.gic().read_hppir1(),
            IccBpr1El1 => v.gic().read_bpr1(),
            IccRprEl1 => v.gic().read_rpr(),
            IccCtlrEl1 => v.gic().read_ctlr(),
            IccSreEl1 => v.gic().read_sre(),
            IccIgrpen1El1 => v.gic().read_igrpen1(),
            IccAp1r0El1 => v.gic().read_ap1r0(),
            // Vetro's GIC has only group 1 (Linux does not use group 0).
            IccIar0El1 | IccHppir0El1 => SPURIOUS,
            IccBpr0El1 | IccAp0r0El1 | IccIgrpen0El1 => 0,
            // Write-only registers: the CPU never reads them.
            IccEoir0El1 | IccEoir1El1 | IccDirEl1 | IccSgi1rEl1 | IccAsgi1rEl1 | IccSgi0rEl1 => 0,
        };
        if matches!(reg, IccIar1El1) {
            b.lines_changed();
        }
        r
    }

    fn write_sysreg(&mut self, reg: EnvReg, value: u64) {
        use EnvReg::*;
        let mut b = self.cell.borrow_mut();
        if let Some(n) = self.now {
            b.cntpct = b.cntpct.max(n);
        }
        let c = self.now.unwrap_or(b.cntpct);
        b.irq_dirty = true;
        b.virt.set_current_cpu(self.core);
        let v = &mut b.virt;
        match reg {
            CntpTvalEl0 => v.timer_mut().set_cntp_tval(c, value),
            CntpCtlEl0 => v.timer_mut().set_cntp_ctl(value),
            CntpCvalEl0 => v.timer_mut().set_cntp_cval(value),
            CntvTvalEl0 => v.timer_mut().set_cntv_tval(c, value),
            CntvCtlEl0 => v.timer_mut().set_cntv_ctl(value),
            CntvCvalEl0 => v.timer_mut().set_cntv_cval(value),
            IccPmrEl1 => v.gic_mut().write_pmr(value),
            IccEoir1El1 => v.gic_mut().write_eoir1(value),
            IccDirEl1 => v.gic_mut().write_dir(value),
            IccSgi1rEl1 | IccAsgi1rEl1 => v.gic_mut().write_sgi1r(value),
            IccBpr1El1 => v.gic_mut().write_bpr1(value),
            IccCtlrEl1 => v.gic_mut().write_ctlr(value),
            IccSreEl1 => v.gic_mut().write_sre(value),
            IccIgrpen1El1 => v.gic_mut().write_igrpen1(value),
            IccAp1r0El1 => v.gic_mut().write_ap1r0(value),
            // Group 0 absent; CNTFRQ/CNTPCT/CNTVCT and the read-only ones do not
            // get here (the CPU rejects the write).
            _ => {}
        }
        b.lines_changed();
        b.settle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::gic::*;
    use vetro_platform::timer::CTL_ENABLE;

    /// The RAM in pieces (hash, page-by-page snapshot) gives the same bytes as
    /// the `vetro-snapshot` functions on the whole RAM: snapshots and logs stay
    /// the same between the host and wasm32 with more than 2 GiB.
    #[test]
    fn ram_a_pezzi_come_la_ram_intera() {
        use vetro_snapshot::{Reader, Snapshot, Writer};
        // More than one RAM_CHUNK piece is not needed: the piece logic is the
        // same, and an odd length exercises the short last piece.
        let size = 3 * 4096 * 64 + 4096 * 3 + 520;
        let ram = Ram::new(size as u64);
        for (i, pa) in [0u64, 4096 * 7 + 13, 4096 * 100, size as u64 - 9].into_iter().enumerate() {
            let data: Vec<u8> = (0..9).map(|k| (i * 31 + k * 7 + 1) as u8).collect();
            assert!(ram.write(map::RAM_BASE + pa, &data));
        }
        let incompressible: Vec<u8> =
            (0..4096u32).map(|k| (k.wrapping_mul(2654435761) >> 13) as u8).collect();
        assert!(ram.write(map::RAM_BASE + 4096 * 50, &incompressible));
        assert_eq!(ram.hash(), vetro_snapshot::hash64(ram.bytes()));
        let mut a = Writer::new();
        ram.save(&mut a);
        let mut b = Writer::new();
        vetro_snapshot::compress(&mut b, ram.bytes());
        assert_eq!(a.as_bytes(), b.as_bytes(), "same format as compress");
        // Restore over a dirty RAM: absent pages go back to zero.
        let mut other = Ram::new(size as u64);
        assert!(other.write(map::RAM_BASE + 4096 * 9, &[0xaa; 100]));
        other.restore(&mut Reader::new(a.as_bytes())).unwrap();
        assert!(other.same_bytes(&ram));
        assert_eq!(other.hash(), ram.hash());
        // A different length is rejected.
        let mut small = Ram::new(4096);
        assert!(small.restore(&mut Reader::new(a.as_bytes())).is_err());
    }

    fn board_with_vtimer_enabled() -> BoardCell {
        let b = BoardCell::new(Board::new(1 << 20, 0));
        {
            let mut bb = b.borrow_mut();
            let bus = &mut bb.virt.bus;
            bus.write(map::GICR_BASE + GICR_WAKER, 4, 0);
            bus.write(map::GICD_BASE + GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << map::PPI_VTIMER);
        }
        let mut env = Env::new(&b, 0);
        env.write_sysreg(EnvReg::IccPmrEl1, 0xF0);
        env.write_sysreg(EnvReg::IccIgrpen1El1, 1);
        b
    }

    /// The cached IRQ line follows every GIC change: timer expiring
    /// (`update_irqs`), acknowledge (read of ICC_IAR1), EOI (write) and
    /// MMIO accesses. Without the clears the CPU would see the stale level.
    #[test]
    fn linea_irq_in_cache_segue_il_gic() {
        let b = board_with_vtimer_enabled();
        let mut env = Env::new(&b, 0);
        b.borrow_mut().update_irqs();
        assert!(!env.irq_line());
        assert_eq!(b.cores.cached_irq(0), Some(false), "the level stays cached");

        env.write_sysreg(EnvReg::CntvCvalEl0, 100);
        env.write_sysreg(EnvReg::CntvCtlEl0, CTL_ENABLE);
        b.borrow_mut().cntpct = 100;
        b.borrow_mut().update_irqs();
        assert!(env.irq_line(), "the expired timer raises the line");

        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line(), "after the acknowledge the interrupt is active, no longer pending");

        env.write_sysreg(EnvReg::CntvCtlEl0, 0);
        b.borrow_mut().update_irqs();
        env.write_sysreg(EnvReg::IccEoir1El1, u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line());

        // An MMIO access to the GIC clears the cache: an SGI pending again.
        let mut waits = Vec::new();
        let mut phys = Phys::single(&b, 0, &mut waits);
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, &1u32.to_le_bytes()).unwrap();
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISPENDR0, &1u32.to_le_bytes()).unwrap();
        assert!(env.irq_line(), "SGI 0 enabled and made pending via MMIO");
    }

    /// The power key pressed by the host: `gpio_input` marks the lines
    /// to update (the `Machine::run` loop calls `update_irqs` before
    /// the next instruction) and INTID 39 reaches the CPU.
    #[test]
    fn tasto_di_spegnimento_dall_host() {
        use vetro_platform::pl061;
        let b = board_with_vtimer_enabled();
        let intid = map::SPI_BASE + map::GPIO_SPI;
        {
            let mut bb = b.borrow_mut();
            let bus = &mut bb.virt.bus;
            bus.write(map::GICD_BASE + GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
            bus.write(map::GICD_BASE + GICD_ISENABLER + 4, 4, 1 << (intid % 32));
            let m = 1u64 << pl061::POWER_KEY_LINE;
            bus.write(map::GPIO_BASE + pl061::IBE, 1, m);
            bus.write(map::GPIO_BASE + pl061::IE, 1, m);
            bb.update_irqs();
        }
        let mut env = Env::new(&b, 0);
        assert!(!env.irq_line());
        b.borrow_mut().gpio_input(pl061::POWER_KEY_LINE, true);
        assert!(b.borrow().irq_dirty, "the lines must be brought to the GIC");
        b.borrow_mut().update_irqs();
        assert!(env.irq_line());
        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(intid));
    }
}
