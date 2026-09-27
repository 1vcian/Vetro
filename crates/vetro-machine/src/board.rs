//! What lies outside the CPU: RAM, platform and time counter.
//! Implements the physical memory seen by the MMU and the CPU environment
//! ([`CpuEnv`]): generic timer, GIC CPU interface, IRQ line.

use core::cell::RefCell;

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

    #[inline]
    fn get_mut(&mut self, o: usize, n: usize) -> &mut [u8] {
        debug_assert!(o + n <= self.len);
        // SAFETY: as in `get`, with `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.wrapping_add(o), n) }
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
pub struct Ram {
    bytes: Store,
    /// One bit per 4 KiB page: watched.
    code: Vec<u64>,
    /// Watched pages.
    watched: usize,
    /// Physical pages (`pa >> 12`) watched and then written.
    dirty: Vec<u64>,
}

impl Ram {
    pub fn new(size: u64) -> Self {
        let pages = size.div_ceil(4096) as usize;
        Ram { bytes: Store::new(size), code: vec![0; pages.div_ceil(64)], watched: 0, dirty: Vec::new() }
    }

    /// RAM bytes.
    pub fn size(&self) -> u64 {
        self.bytes.len as u64
    }

    /// The RAM bytes as one slice (read-only). Always on 64-bit hosts; on
    /// wasm32 only up to 2 GiB - 1 (beyond that, [`chunks`](Self::chunks)).
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
    pub fn watch_code(&mut self, page: u64) -> bool {
        let Some(i) = (page << 12).checked_sub(map::RAM_BASE).map(|o| (o >> 12) as usize) else {
            return false;
        };
        if (i as u64) << 12 >= self.size() {
            return false;
        }
        let (w, b) = (i / 64, 1u64 << (i % 64));
        if self.code[w] & b == 0 {
            self.code[w] |= b;
            self.watched += 1;
        }
        true
    }

    /// True if the physical page `page` is watched.
    pub fn is_watched(&self, page: u64) -> bool {
        match (page << 12).checked_sub(map::RAM_BASE) {
            Some(o) if o < self.size() => {
                let i = (o >> 12) as usize;
                self.code[i / 64] & 1 << (i % 64) != 0
            }
            _ => false,
        }
    }

    /// Appends to `out` the watched pages written since then.
    pub fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        out.append(&mut self.dirty);
    }

    /// Marks dirty (and no longer watched) the pages of `[o, o+len)`
    /// (offset into RAM); true if there was at least one.
    #[inline]
    fn touch(&mut self, o: usize, len: usize) -> bool {
        if self.watched == 0 || len == 0 {
            return false;
        }
        let mut hit = false;
        for i in o >> 12..=(o + len - 1) >> 12 {
            let (w, b) = (i / 64, 1u64 << (i % 64));
            if self.code[w] & b != 0 {
                self.code[w] &= !b;
                self.watched -= 1;
                self.dirty.push((map::RAM_BASE >> 12) + i as u64);
                hit = true;
            }
        }
        hit
    }

    /// A write that also says whether it touched watched code: `None`
    /// outside RAM.
    pub fn write_watched(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        let o = self.range(pa, data.len())?;
        self.bytes.get_mut(o, data.len()).copy_from_slice(data);
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
                buf.copy_from_slice(self.bytes.get(o, buf.len()));
                true
            }
            None => false,
        }
    }

    pub fn write(&mut self, pa: u64, data: &[u8]) -> bool {
        self.write_watched(pa, data).is_some()
    }
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
    /// [`RamSource`].
    pub fn restore_from(&mut self, r: &mut dyn RamSource) -> vetro_snapshot::Result<()> {
        const PAGE: usize = vetro_snapshot::BLOCK;
        struct Src<'a>(&'a mut dyn RamSource);
        impl vetro_snapshot::blocks::Source for Src<'_> {
            fn take(&mut self, n: usize) -> vetro_snapshot::Result<&[u8]> {
                self.0.take(n)
            }
        }
        struct Dst<'a>(&'a mut Ram);
        impl vetro_snapshot::blocks::Target for Dst<'_> {
            fn block_mut(&mut self, i: usize) -> &mut [u8] {
                let len = self.0.bytes.len;
                self.0.bytes.get_mut(i * PAGE, (len - i * PAGE).min(PAGE))
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
                let p = self.bytes.get_mut(i * PAGE, (len - i * PAGE).min(PAGE));
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

/// RAM, platform and time.
pub struct Board {
    pub ram: Ram,
    pub virt: Virt,
    /// Current value of CNTPCT_EL0.
    pub cntpct: u64,
    /// Something may have changed the level of an interrupt line
    /// (MMIO access, timer register): `update_irqs` must be called.
    pub(crate) irq_dirty: bool,
    /// An access to a virtio-mmio slot: the device must be serviced.
    pub(crate) virtio_dirty: bool,
    /// Level of the GIC IRQ line, if already computed: the CPU reads it
    /// before every instruction with PSTATE.I = 0, and `Gic::irq_line` walks
    /// all interrupts. It is cleared by every operation that can change the
    /// GIC state (MMIO, ICC_*, `update_irqs`, virtio).
    pub(crate) irq_cache: Option<bool>,
    /// After the last virtio service a virtio-blk request is waiting for
    /// data from the host (`BlockError::NotReady`): the machine executes no
    /// instructions until they arrive (`Stop::Blocked`).
    pub(crate) host_wait: bool,
}

impl Board {
    pub fn new(ram_size: u64, now_secs: u64) -> Self {
        Board {
            ram: Ram::new(ram_size),
            virt: Virt::new(now_secs),
            cntpct: 0,
            irq_dirty: true,
            virtio_dirty: false,
            irq_cache: None,
            host_wait: false,
        }
    }

    /// Brings the levels of all lines to the GIC.
    pub fn update_irqs(&mut self) {
        self.virt.update_irqs(self.cntpct);
        self.irq_cache = None;
        self.irq_dirty = false;
    }

    /// Runs the virtio devices on top of RAM.
    pub fn service_virtio(&mut self) {
        let Board { ram, virt, .. } = self;
        virt.service_virtio(ram);
        self.host_wait = (0..map::VIRTIO_SLOTS as u32).any(|k| {
            virt.virtio(k).and_then(|t| t.device_as::<VirtioBlk>()).is_some_and(VirtioBlk::has_pending)
        });
        self.irq_cache = None;
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
        self.irq_cache = None;
        self.irq_dirty = true;
    }

    fn mmio_touched(&mut self, pa: u64) {
        self.irq_cache = None;
        self.irq_dirty = true;
        let virtio_end = map::VIRTIO_BASE + map::VIRTIO_SLOTS * map::VIRTIO_SLOT_SIZE;
        if (map::VIRTIO_BASE..virtio_end).contains(&pa) {
            self.virtio_dirty = true;
        }
    }
}

fn mmio_size(len: usize) -> Option<u8> {
    matches!(len, 1 | 2 | 4 | 8).then_some(len as u8)
}

/// Physical memory: RAM, otherwise the MMIO bus (an access of 1, 2, 4 or
/// 8 bytes; whatever does not respond is a decode error).
pub(crate) struct Phys<'a>(pub &'a RefCell<Board>);

impl PhysMemory for Phys<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let mut b = self.0.borrow_mut();
        if b.ram.read(pa, buf) {
            return Ok(());
        }
        let size = mmio_size(buf.len()).ok_or(BusError::Slave)?;
        let v = b.virt.bus.read(pa, size).ok_or(BusError::Decode)?;
        buf.copy_from_slice(&v.to_le_bytes()[..buf.len()]);
        b.mmio_touched(pa);
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        let mut b = self.0.borrow_mut();
        if b.ram.write(pa, data) {
            return Ok(());
        }
        let size = mmio_size(data.len()).ok_or(BusError::Slave)?;
        let mut v = [0u8; 8];
        v[..data.len()].copy_from_slice(data);
        if !b.virt.bus.write(pa, size, u64::from_le_bytes(v)) {
            return Err(BusError::Decode);
        }
        b.mmio_touched(pa);
        Ok(())
    }
}

/// Physical memory for the JIT: RAM only, with the code pages
/// watched.
impl SysPhys for Phys<'_> {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool {
        self.0.borrow().ram.read(pa, buf)
    }
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        self.0.borrow_mut().ram.write_watched(pa, data)
    }
    fn watch_code(&mut self, page: u64) -> bool {
        self.0.borrow_mut().ram.watch_code(page)
    }
    fn is_watched(&self, page: u64) -> bool {
        self.0.borrow().ram.is_watched(page)
    }
    fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        self.0.borrow_mut().ram.take_code_dirty(out)
    }
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        let b = self.0.borrow();
        Some((map::RAM_BASE, b.ram.bytes.ptr, b.ram.bytes.len))
    }
}

/// The CPU environment: GIC IRQ line, generic timer, ICC_*.
pub(crate) struct Env<'a>(pub &'a RefCell<Board>);

/// The CPU interface's "no interrupt" INTID.
const SPURIOUS: u64 = 1023;

impl CpuEnv for Env<'_> {
    fn irq_line(&mut self) -> bool {
        let mut b = self.0.borrow_mut();
        if let Some(l) = b.irq_cache {
            return l;
        }
        let l = b.virt.irq_line();
        b.irq_cache = Some(l);
        l
    }

    fn read_sysreg(&mut self, reg: EnvReg) -> u64 {
        use EnvReg::*;
        let mut b = self.0.borrow_mut();
        let c = b.cntpct;
        b.irq_cache = None;
        let v = &mut b.virt;
        match reg {
            CntfrqEl0 => u64::from(map::CNTFRQ_HZ),
            CntpctEl0 => c,
            CntvctEl0 => v.timer.cntvct(c),
            CntpTvalEl0 => v.timer.cntp_tval(c),
            CntpCtlEl0 => v.timer.cntp_ctl(c),
            CntpCvalEl0 => v.timer.cntp_cval(),
            CntvTvalEl0 => v.timer.cntv_tval(c),
            CntvCtlEl0 => v.timer.cntv_ctl(c),
            CntvCvalEl0 => v.timer.cntv_cval(),
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
        }
    }

    fn write_sysreg(&mut self, reg: EnvReg, value: u64) {
        use EnvReg::*;
        let mut b = self.0.borrow_mut();
        let c = b.cntpct;
        b.irq_cache = None;
        b.irq_dirty = true;
        let v = &mut b.virt;
        match reg {
            CntpTvalEl0 => v.timer.set_cntp_tval(c, value),
            CntpCtlEl0 => v.timer.set_cntp_ctl(value),
            CntpCvalEl0 => v.timer.set_cntp_cval(value),
            CntvTvalEl0 => v.timer.set_cntv_tval(c, value),
            CntvCtlEl0 => v.timer.set_cntv_ctl(value),
            CntvCvalEl0 => v.timer.set_cntv_cval(value),
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
        let mut ram = Ram::new(size as u64);
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

    fn board_with_vtimer_enabled() -> RefCell<Board> {
        let b = RefCell::new(Board::new(1 << 20, 0));
        {
            let mut bb = b.borrow_mut();
            let bus = &mut bb.virt.bus;
            bus.write(map::GICR_BASE + GICR_WAKER, 4, 0);
            bus.write(map::GICD_BASE + GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << map::PPI_VTIMER);
        }
        let mut env = Env(&b);
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
        let mut env = Env(&b);
        b.borrow_mut().update_irqs();
        assert!(!env.irq_line());
        assert_eq!(b.borrow().irq_cache, Some(false), "the level stays cached");

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
        let mut phys = Phys(&b);
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
        let mut env = Env(&b);
        assert!(!env.irq_line());
        b.borrow_mut().gpio_input(pl061::POWER_KEY_LINE, true);
        assert!(b.borrow().irq_dirty, "the lines must be brought to the GIC");
        b.borrow_mut().update_irqs();
        assert!(env.irq_line());
        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(intid));
    }
}
