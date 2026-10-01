//! Interface to guest memory and user address space.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Fetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault {
    pub addr: u64,
    pub access: Access,
}

/// Memory as seen by the CPU (virtual addresses, little-endian).
///
/// The methods with a default implementation serve system
/// mode; in user mode they coincide with `read` and `write`.
pub trait Memory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault>;
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault>;
    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault>;

    /// LDTR read: at EL1 the permissions are those of EL0.
    fn read_unpriv(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        self.read(addr, buf)
    }

    /// STTR write: at EL1 the permissions are those of EL0.
    fn write_unpriv(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.write(addr, data)
    }

    /// DC ZVA: zeroes the aligned 64-byte block that starts at `addr`.
    /// On Device memory it gives an alignment fault (system mode).
    fn zero_block(&mut self, addr: u64) -> Result<(), MemFault> {
        self.write(addr, &[0u8; 64])
    }

    /// The store of a store-exclusive whose monitor matched: writes `new` at
    /// `addr` if the bytes there are still `old` (same length, aligned), in
    /// one atomic step when other cores run at the same time (ADR 0041).
    /// True if it wrote. The default reads, compares and writes.
    fn cmpxchg(&mut self, addr: u64, old: &[u8], new: &[u8]) -> Result<bool, MemFault> {
        let mut cur = [0u8; 16];
        let cur = &mut cur[..old.len()];
        self.read(addr, cur)?;
        if cur != old {
            return Ok(false);
        }
        self.write(addr, new)?;
        Ok(true)
    }
}

/// Permissions of a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const NONE: Perm = Perm { read: false, write: false, exec: false };
    pub const R: Perm = Perm { read: true, write: false, exec: false };
    pub const RW: Perm = Perm { read: true, write: true, exec: false };
    pub const RX: Perm = Perm { read: true, write: false, exec: true };
    pub const RWX: Perm = Perm { read: true, write: true, exec: true };

    /// From the bits PROT_READ (1), PROT_WRITE (2), PROT_EXEC (4).
    pub fn from_prot(prot: u32) -> Perm {
        Perm { read: prot & 1 != 0, write: prot & 2 != 0, exec: prot & 4 != 0 }
    }

    fn allows(self, a: Access) -> bool {
        match a {
            Access::Read => self.read,
            Access::Write => self.write,
            Access::Fetch => self.exec,
        }
    }
}

const PAGE: usize = 4096;

/// Memory behind a region.
#[derive(Clone)]
enum Backing {
    /// Private and lazy: only written pages exist, the others read as
    /// zero. A fork copies only the present pages.
    Pages { pages: BTreeMap<usize, Box<[u8; PAGE]>>, base: usize },
    /// Shared (MAP_SHARED): stays the same after a fork.
    Shared(Rc<RefCell<Vec<u8>>>, usize),
}

#[derive(Clone)]
struct Region {
    backing: Backing,
    len: usize,
    perm: Perm,
    /// False for MAP_SHARED of files opened read-only: mprotect
    /// cannot add write (Linux's VM_MAYWRITE).
    may_write: bool,
    /// MAP_GROWSDOWN: an access just below extends it (VM_GROWSDOWN).
    grows_down: bool,
    /// All pages already present (content from a file, or MAP_POPULATE):
    /// only for /proc/<pid>/pagemap.
    populated: bool,
}

impl Region {
    /// Private region with initial content (zero pages are not
    /// allocated).
    fn own(data: Vec<u8>, perm: Perm) -> Region {
        let mut pages = BTreeMap::new();
        for (i, chunk) in data.chunks(PAGE).enumerate() {
            if chunk.iter().any(|&b| b != 0) {
                let mut p = Box::new([0u8; PAGE]);
                p[..chunk.len()].copy_from_slice(chunk);
                pages.insert(i, p);
            }
        }
        Region {
            len: data.len(),
            backing: Backing::Pages { pages, base: 0 },
            perm,
            may_write: true,
            grows_down: false,
            populated: true,
        }
    }

    /// All-zero private region, without allocating anything.
    fn zeroed(len: usize, perm: Perm) -> Region {
        Region {
            len,
            backing: Backing::Pages { pages: BTreeMap::new(), base: 0 },
            perm,
            may_write: true,
            grows_down: false,
            populated: false,
        }
    }

    fn read(&self, off: usize, out: &mut [u8]) {
        match &self.backing {
            Backing::Pages { pages, base } => {
                let mut done = 0;
                while done < out.len() {
                    let abs = base + off + done;
                    let (pi, po) = (abs / PAGE, abs % PAGE);
                    let n = (PAGE - po).min(out.len() - done);
                    match pages.get(&pi) {
                        Some(p) => out[done..done + n].copy_from_slice(&p[po..po + n]),
                        None => out[done..done + n].fill(0),
                    }
                    done += n;
                }
            }
            Backing::Shared(b, base) => {
                let b = b.borrow();
                let start = base + off;
                for (i, o) in out.iter_mut().enumerate() {
                    *o = b.get(start + i).copied().unwrap_or(0);
                }
            }
        }
    }

    fn write(&mut self, off: usize, data: &[u8]) {
        match &mut self.backing {
            Backing::Pages { pages, base } => {
                let mut done = 0;
                while done < data.len() {
                    let abs = *base + off + done;
                    let (pi, po) = (abs / PAGE, abs % PAGE);
                    let n = (PAGE - po).min(data.len() - done);
                    let p = pages.entry(pi).or_insert_with(|| Box::new([0u8; PAGE]));
                    p[po..po + n].copy_from_slice(&data[done..done + n]);
                    done += n;
                }
            }
            Backing::Shared(b, base) => {
                let mut b = b.borrow_mut();
                let start = *base + off;
                if b.len() < start + data.len() {
                    b.resize(start + data.len(), 0);
                }
                b[start..start + data.len()].copy_from_slice(data);
            }
        }
    }

    /// True if byte `off` of the region is in a page that starts beyond
    /// the end of the shared buffer.
    fn past_end(&self, off: u64) -> bool {
        match &self.backing {
            Backing::Shared(buf, base) => {
                let page = (*base as u64 + off) & !(PAGE as u64 - 1);
                page >= buf.borrow().len() as u64
            }
            Backing::Pages { .. } => false,
        }
    }

    /// Splits the region `k` bytes from the start; returns the tail.
    fn split_off(&mut self, k: usize) -> Region {
        let tail = match &mut self.backing {
            Backing::Pages { pages, base } => {
                // The tail has the same logical pages, shifted by k bytes.
                let abs = *base + k;
                let first = abs / PAGE;
                let tail_pages = pages.split_off(&first);
                if !abs.is_multiple_of(PAGE)
                    && let Some(p) = tail_pages.get(&first)
                {
                    pages.insert(first, p.clone());
                }
                Backing::Pages { pages: tail_pages, base: abs }
            }
            Backing::Shared(b, base) => Backing::Shared(b.clone(), *base + k),
        };
        let r = Region {
            backing: tail,
            len: self.len - k,
            perm: self.perm,
            may_write: self.may_write,
            grows_down: self.grows_down,
            populated: self.populated,
        };
        self.len = k;
        r
    }
}

/// Hash for page numbers (Fibonacci multiplication): watched pages
/// are checked on every write, SipHash would cost too much.
#[derive(Default)]
struct PageHasher(u64);

impl Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type PageSet = HashSet<u64, BuildHasherDefault<PageHasher>>;

/// Identity of address spaces (see [`UserMemory::space_id`]).
static NEXT_SPACE: AtomicU64 = AtomicU64::new(1);

/// Watching of code pages for the JIT (ADR 0012,
/// "Invalidation"). The JIT marks with [`UserMemory::watch_code`] the pages
/// it has translated blocks from; every change to the content or the
/// mapping of one of them (write by the guest or by the emulated kernel, mmap,
/// munmap, mprotect, mremap...) removes it from watching and puts it among
/// the dirty pages, which the JIT collects with
/// [`UserMemory::take_code_dirty`] and invalidates.
#[derive(Default)]
struct CodeWatch {
    pages: PageSet,
    dirty: Vec<u64>,
}

/// Address space of a user mode process: disjoint regions
/// with permissions, which can be overwritten, removed and re-protected in
/// pieces (mmap MAP_FIXED, munmap, mprotect). It is the implementation of
/// [`Memory`] for the Linux user mode level; from M3 the MMU does the translation.
pub struct UserMemory {
    regions: BTreeMap<u64, Region>,
    /// Unique identity of this space: a copy (fork) has another one.
    space: u64,
    watch: CodeWatch,
}

impl Default for UserMemory {
    fn default() -> Self {
        UserMemory {
            regions: BTreeMap::new(),
            space: NEXT_SPACE.fetch_add(1, Ordering::Relaxed),
            watch: CodeWatch::default(),
        }
    }
}

/// The copy is another space (fork): new identity and no watched
/// pages, because the JIT's translated blocks are per space.
impl Clone for UserMemory {
    fn clone(&self) -> Self {
        UserMemory {
            regions: self.regions.clone(),
            space: NEXT_SPACE.fetch_add(1, Ordering::Relaxed),
            watch: CodeWatch::default(),
        }
    }
}

impl fmt::Debug for UserMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut l = f.debug_list();
        for (b, r) in &self.regions {
            l.entry(&format_args!("{:#x}..{:#x} {:?}", b, b + r.len as u64, r.perm));
        }
        l.finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub base: u64,
}

impl UserMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Identity of the address space, unique in the host process (a
    /// copy receives a new one). Key of the JIT's block cache.
    pub fn space_id(&self) -> u64 {
        self.space
    }

    /// Watches page `page` (address >> 12): the next
    /// change to its content or its mapping marks it dirty.
    pub fn watch_code(&mut self, page: u64) {
        self.watch.pages.insert(page);
    }

    /// True if there are watched pages that became dirty.
    #[inline]
    pub fn code_dirty(&self) -> bool {
        !self.watch.dirty.is_empty()
    }

    /// Watched pages changed since the last call (and no longer
    /// watched).
    pub fn take_code_dirty(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.watch.dirty)
    }

    /// True if the code at `addr` can live in a cache of translated
    /// blocks: private memory only. Shared mappings (MAP_SHARED)
    /// change also from other processes or from files, outside the view of
    /// this space.
    pub fn is_private(&self, addr: u64) -> bool {
        match self.find(addr) {
            Some((b, _)) => matches!(self.regions[&b].backing, Backing::Pages { .. }),
            None => false,
        }
    }

    /// Marks dirty the watched pages that touch `[start, end)`.
    fn touch(&mut self, start: u64, end: u64) {
        if self.watch.pages.is_empty() || start >= end {
            return;
        }
        let (first, last) = (start >> 12, (end - 1) >> 12);
        if last - first < 64 {
            for p in first..=last {
                if self.watch.pages.remove(&p) {
                    self.watch.dirty.push(p);
                }
            }
        } else {
            let hit: Vec<u64> =
                self.watch.pages.iter().copied().filter(|p| (first..=last).contains(p)).collect();
            for p in hit {
                self.watch.pages.remove(&p);
                self.watch.dirty.push(p);
            }
        }
    }

    /// Like [`touch`](Self::touch) for a write of `len` bytes at `addr`.
    #[inline]
    fn touch_write(&mut self, addr: u64, len: usize) {
        if !self.watch.pages.is_empty() {
            self.touch(addr, addr.saturating_add(len as u64));
        }
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        if let Some((b, r)) = self.regions.range(..end).next_back() {
            return b + r.len as u64 > start;
        }
        false
    }

    /// Maps `[base, base+data.len())`, which must not overlap anything else.
    pub fn map(&mut self, base: u64, data: Vec<u8>, perm: Perm) -> Result<(), Overlap> {
        let end = base.checked_add(data.len() as u64).ok_or(Overlap { base })?;
        if data.is_empty() || self.overlaps(base, end) {
            return Err(Overlap { base });
        }
        self.touch(base, end);
        self.regions.insert(base, Region::own(data, perm));
        Ok(())
    }

    /// Like [`map`](Self::map) but replaces what was there (MAP_FIXED).
    pub fn map_fixed(&mut self, base: u64, data: Vec<u8>, perm: Perm) {
        if data.is_empty() {
            return;
        }
        self.unmap(base, base + data.len() as u64);
        self.regions.insert(base, Region::own(data, perm));
    }

    /// Maps `len` zero bytes without allocating (private anonymous mmap).
    pub fn map_zeroed(&mut self, base: u64, len: usize, perm: Perm) {
        if len == 0 {
            return;
        }
        self.unmap(base, base + len as u64);
        self.regions.insert(base, Region::zeroed(len, perm));
    }

    /// Maps `len` bytes of the shared buffer `buf` from `off` onwards at `base`,
    /// replacing what was there (MAP_SHARED). With `may_write` false the
    /// region can never become writable.
    pub fn map_shared(
        &mut self,
        base: u64,
        buf: Rc<RefCell<Vec<u8>>>,
        off: usize,
        len: usize,
        perm: Perm,
        may_write: bool,
    ) {
        if len == 0 {
            return;
        }
        self.unmap(base, base + len as u64);
        self.regions.insert(
            base,
            Region {
                backing: Backing::Shared(buf, off),
                len,
                perm,
                may_write,
                grows_down: false,
                populated: true,
            },
        );
    }

    /// True if no region in `[start, end)` forbids writing.
    pub fn may_write(&self, start: u64, end: u64) -> bool {
        let first = self.regions.range(..=start).next_back().map_or(start, |(&b, _)| b);
        self.regions.range(first..end).all(|(&b, r)| b + r.len as u64 <= start || r.may_write)
    }

    /// Moves the regions of `[old, old+len)` to `dst` with their content and
    /// their memory (shared too), replacing what was there (mremap).
    pub fn remap(&mut self, old: u64, len: u64, dst: u64) {
        self.touch(old, old.saturating_add(len));
        self.touch(dst, dst.saturating_add(len));
        self.split_at(old);
        self.split_at(old + len);
        let keys: Vec<u64> = self.regions.range(old..old + len).map(|(&k, _)| k).collect();
        let moved: Vec<(u64, Region)> =
            keys.into_iter().map(|k| (k - old, self.regions.remove(&k).unwrap())).collect();
        self.unmap(dst, dst + len);
        for (d, r) in moved {
            self.regions.insert(dst + d, r);
        }
    }

    /// Marks as MAP_GROWSDOWN the region that starts at `base`.
    pub fn set_grows_down(&mut self, base: u64) {
        if let Some(r) = self.regions.get_mut(&base) {
            r.grows_down = true;
        }
    }

    /// Fault at unmapped `addr`: if just above there is a
    /// MAP_GROWSDOWN region it extends it down to the page of `addr`, as long as at least
    /// `gap` bytes remain from the previous mapping (stack_guard_gap).
    /// True if it extended it.
    pub fn grow_down(&mut self, addr: u64, gap: u64) -> bool {
        let page = addr & !(PAGE as u64 - 1);
        let Some((&b, r)) = self.regions.range(page + 1..).next() else { return false };
        if !r.grows_down || addr >= b {
            return false;
        }
        let (perm, may_write) = (r.perm, r.may_write);
        if let Some((&pb, p)) = self.regions.range(..=page).next_back() {
            let pend = pb + p.len as u64;
            let accessible = p.perm.read || p.perm.write || p.perm.exec;
            if pend > page || accessible && !p.grows_down && page - pend < gap {
                return false;
            }
        }
        let len = (b - page) as usize;
        self.touch(page, b);
        self.regions.insert(
            page,
            Region {
                backing: Backing::Pages { pages: BTreeMap::new(), base: 0 },
                len,
                perm,
                may_write,
                grows_down: true,
                populated: false,
            },
        );
        true
    }

    /// Like [`remap`](Self::remap), but the old range stays mapped
    /// (MREMAP_DONTUNMAP): private regions stay with the same permissions
    /// and without pages (they read as zero), shared ones stay on the
    /// same memory.
    pub fn remap_dontunmap(&mut self, old: u64, len: u64, dst: u64) {
        self.split_at(old);
        self.split_at(old + len);
        let left: Vec<(u64, Region)> = self
            .regions
            .range(old..old + len)
            .map(|(&b, r)| {
                let (backing, populated) = match &r.backing {
                    Backing::Shared(buf, base) => (Backing::Shared(buf.clone(), *base), true),
                    Backing::Pages { .. } => (Backing::Pages { pages: BTreeMap::new(), base: 0 }, false),
                };
                (b, Region { backing, populated, ..*r })
            })
            .collect();
        self.remap(old, len, dst);
        for (b, r) in left {
            self.regions.insert(b, r);
        }
    }

    /// Extends by `extra` bytes the region that ends at `end`: a shared one
    /// continues in the same buffer, a private one with zero pages.
    pub fn extend(&mut self, end: u64, extra: usize) {
        let Some((&b, r)) = self.regions.range(..end).next_back() else { return };
        if b + r.len as u64 != end || extra == 0 {
            return;
        }
        let backing = match &r.backing {
            Backing::Shared(buf, base) => Backing::Shared(buf.clone(), base + r.len),
            Backing::Pages { .. } => Backing::Pages { pages: BTreeMap::new(), base: 0 },
        };
        let tail = Region {
            backing,
            len: extra,
            perm: r.perm,
            may_write: r.may_write,
            grows_down: r.grows_down,
            populated: false,
        };
        self.unmap(end, end + extra as u64);
        self.regions.insert(end, tail);
    }

    /// Splits the region that contains `at` (if any) so that `at` is a
    /// boundary.
    fn split_at(&mut self, at: u64) {
        let Some((&b, r)) = self.regions.range(..at).next_back() else { return };
        let end = b + r.len as u64;
        if at <= b || at >= end {
            return;
        }
        let r = self.regions.get_mut(&b).unwrap();
        let tail = r.split_off((at - b) as usize);
        self.regions.insert(at, tail);
    }

    /// Removes every mapping in `[start, end)`.
    pub fn unmap(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.touch(start, end);
        self.split_at(start);
        self.split_at(end);
        let keys: Vec<u64> = self.regions.range(start..end).map(|(&k, _)| k).collect();
        for k in keys {
            self.regions.remove(&k);
        }
    }

    /// Changes the permissions of `[start, end)`. Fails (without changing anything)
    /// if part of the range is not mapped.
    pub fn protect(&mut self, start: u64, end: u64, perm: Perm) -> Result<(), MemFault> {
        if !self.is_mapped(start, end) {
            return Err(MemFault { addr: start, access: Access::Read });
        }
        self.touch(start, end);
        self.split_at(start);
        self.split_at(end);
        for (_, r) in self.regions.range_mut(start..end) {
            r.perm = perm;
        }
        Ok(())
    }

    /// True if every byte of `[start, end)` is mapped.
    pub fn is_mapped(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        while at < end {
            match self.find(at) {
                Some((b, len)) => at = b + len,
                None => return false,
            }
        }
        true
    }

    /// Region that contains `addr`: (base, length).
    fn find(&self, addr: u64) -> Option<(u64, u64)> {
        let (&b, r) = self.regions.range(..=addr).next_back()?;
        let len = r.len as u64;
        (addr < b + len).then_some((b, len))
    }

    /// "Physical" identity of an address in a shared region: (buffer,
    /// offset in the buffer). Needed for futexes shared between processes, which Linux
    /// recognises by the page and not by the virtual address.
    pub fn shared_key(&self, addr: u64) -> Option<(usize, u64)> {
        let (b, _) = self.find(addr)?;
        match &self.regions[&b].backing {
            Backing::Shared(buf, base) => {
                Some((Rc::as_ptr(buf) as *const u8 as usize, (*base as u64) + (addr - b)))
            }
            Backing::Pages { .. } => None,
        }
    }

    /// Permissions of the page that contains `addr`.
    pub fn perm_at(&self, addr: u64) -> Option<Perm> {
        let (b, _) = self.find(addr)?;
        Some(self.regions[&b].perm)
    }

    /// Searches from top to bottom for a hole of `len` bytes within
    /// `[bottom, top)`.
    pub fn find_free(&self, len: u64, bottom: u64, top: u64) -> Option<u64> {
        let mut end = top;
        for (&b, r) in self.regions.range(..top).rev() {
            let r_end = b + r.len as u64;
            let start = r_end.max(bottom);
            if end >= start && end - start >= len {
                return Some(end - len);
            }
            end = end.min(b);
            if end < bottom.saturating_add(len) {
                return None;
            }
        }
        (end >= bottom.saturating_add(len)).then(|| end - len)
    }

    /// Like [`ranges`](Self::ranges) plus whether the memory is shared
    /// (the `s` of /proc/self/maps).
    pub fn maps(&self) -> impl Iterator<Item = (u64, u64, Perm, bool)> + '_ {
        self.regions
            .iter()
            .map(|(&b, r)| (b, b + r.len as u64, r.perm, matches!(r.backing, Backing::Shared(..))))
    }

    /// Mapped ranges, for /proc/self/maps and debugging.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64, Perm)> + '_ {
        self.regions.iter().map(|(&b, r)| (b, b + r.len as u64, r.perm))
    }

    /// True if the page of `addr` has memory (for /proc/<pid>/pagemap):
    /// private pages exist only after the first write or if they have
    /// initial content (file, MAP_POPULATE); shared ones always.
    pub fn page_present(&self, addr: u64) -> bool {
        let Some((b, _)) = self.find(addr) else { return false };
        let r = &self.regions[&b];
        match &r.backing {
            Backing::Pages { pages, base } => {
                r.populated || pages.contains_key(&((base + (addr - b) as usize) / PAGE))
            }
            Backing::Shared(..) => true,
        }
    }

    /// Allocates the pages of `[start, end)` (MAP_POPULATE).
    pub fn populate(&mut self, start: u64, end: u64) {
        for (&b, r) in self.regions.range_mut(..end) {
            if b + r.len as u64 > start {
                r.populated = true;
            }
        }
    }

    /// True if `addr` falls in a page of a shared mapping that
    /// starts beyond the end of the file (or object): Linux gives SIGBUS.
    pub fn beyond_eof(&self, addr: u64) -> bool {
        match self.find(addr) {
            Some((b, _)) => self.regions[&b].past_end(addr - b),
            None => false,
        }
    }

    /// Region that contains all of `[addr, addr+len)`, with permission `a`.
    /// `Ok(None)` if the range crosses several regions (slow path).
    fn locate(&self, addr: u64, len: usize, a: Access) -> Result<Option<(u64, usize)>, MemFault> {
        match self.find(addr) {
            Some((b, rlen)) => {
                let off = (addr - b) as usize;
                let r = &self.regions[&b];
                if !r.perm.allows(a) || r.past_end(off as u64 + len.max(1) as u64 - 1) {
                    return Err(MemFault { addr, access: a });
                }
                if off + len <= rlen as usize { Ok(Some((b, off))) } else { Ok(None) }
            }
            None => Err(MemFault { addr, access: a }),
        }
    }

    /// Checks byte by byte a range that crosses several regions.
    fn check_slow(&self, addr: u64, len: usize, a: Access) -> Result<(), MemFault> {
        for k in 0..len {
            let p = addr.wrapping_add(k as u64);
            match self.find(p) {
                Some((b, _)) if self.regions[&b].perm.allows(a) && !self.regions[&b].past_end(p - b) => {}
                _ => return Err(MemFault { addr: p, access: a }),
            }
        }
        Ok(())
    }

    /// Write that ignores permissions (ELF loader, kernel preparing
    /// the stack on already mapped pages).
    pub fn poke(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.touch_write(addr, data.len());
        for (k, &byte) in data.iter().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let Some((b, _)) = self.find(p) else {
                return Err(MemFault { addr: p, access: Access::Write });
            };
            let r = self.regions.get_mut(&b).unwrap();
            r.write((p - b) as usize, &[byte]);
        }
        Ok(())
    }
}

impl Memory for UserMemory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), MemFault> {
        let len = buf.len();
        if len == 0 {
            return Ok(()); // an empty access does not touch memory (e.g. iovec {NULL, 0})
        }
        if let Some((b, off)) = self.locate(addr, len, Access::Read)? {
            self.regions[&b].read(off, buf);
            return Ok(());
        }
        self.check_slow(addr, len, Access::Read)?;
        for (k, o) in buf.iter_mut().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("checked above");
            let mut one = [0u8; 1];
            self.regions[&b].read((p - b) as usize, &mut one);
            *o = one[0];
        }
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        if data.is_empty() {
            return Ok(());
        }
        if let Some((b, off)) = self.locate(addr, data.len(), Access::Write)? {
            self.regions.get_mut(&b).unwrap().write(off, data);
            self.touch_write(addr, data.len());
            return Ok(());
        }
        self.check_slow(addr, data.len(), Access::Write)?;
        self.touch_write(addr, data.len());
        for (k, &byte) in data.iter().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("checked above");
            self.regions.get_mut(&b).unwrap().write((p - b) as usize, &[byte]);
        }
        Ok(())
    }

    fn fetch(&mut self, addr: u64) -> Result<u32, MemFault> {
        let mut w = [0u8; 4];
        if let Some((b, off)) = self.locate(addr, 4, Access::Fetch)? {
            self.regions[&b].read(off, &mut w);
            return Ok(u32::from_le_bytes(w));
        }
        self.check_slow(addr, 4, Access::Fetch)?;
        for (k, o) in w.iter_mut().enumerate() {
            let p = addr.wrapping_add(k as u64);
            let (b, _) = self.find(p).expect("checked above");
            let mut one = [0u8; 1];
            self.regions[&b].read((p - b) as usize, &mut one);
            *o = one[0];
        }
        Ok(u32::from_le_bytes(w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_and_access() {
        let mut m = UserMemory::new();
        m.map(0x1000, vec![0; 0x1000], Perm::RW).unwrap();
        m.map(0x2000, vec![0; 0x1000], Perm::RW).unwrap();
        assert!(m.map(0x1800, vec![0; 16], Perm::R).is_err());
        // crosses two regions
        m.write(0x1ffe, &[1, 2, 3, 4]).unwrap();
        let mut b = [0; 4];
        m.read(0x1ffe, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4]);
        assert!(m.read(0x2ffe, &mut b).is_err());
        assert!(m.fetch(0x1000).is_err());
    }

    #[test]
    fn unmap_protect_split() {
        let mut m = UserMemory::new();
        m.map(0x10000, vec![7; 0x4000], Perm::RW).unwrap();
        m.unmap(0x11000, 0x12000);
        assert!(!m.is_mapped(0x10000, 0x14000));
        assert!(m.is_mapped(0x12000, 0x14000));
        m.protect(0x12000, 0x13000, Perm::R).unwrap();
        assert!(m.write(0x12000, &[1]).is_err());
        assert!(m.write(0x13000, &[1]).is_ok());
        let mut b = [0u8; 1];
        m.read(0x13fff, &mut b).unwrap();
        assert_eq!(b[0], 7);
        m.map_fixed(0x10800, vec![9; 0x1000], Perm::R);
        m.read(0x10800, &mut b).unwrap();
        assert_eq!(b[0], 9);
        assert_eq!(m.find_free(0x1000, 0x1000, 0x10000), Some(0xf000));
        assert_eq!(m.find_free(0x2000, 0x10000, 0x14000), None);
        m.poke(0x12000, &[5]).unwrap();
        m.read(0x12000, &mut b).unwrap();
        assert_eq!(b[0], 5);
    }

    #[test]
    fn empty_access_never_faults() {
        let mut m = UserMemory::new();
        m.read(0, &mut []).unwrap();
        m.write(0, &[]).unwrap();
        assert!(m.read(0, &mut [0u8; 1]).is_err());
    }

    #[test]
    fn remap_keeps_backing_and_extend_follows_it() {
        let mut m = UserMemory::new();
        let buf = Rc::new(RefCell::new(vec![0u8; 0x3000]));
        m.map_shared(0x40000, buf.clone(), 0, 0x1000, Perm { read: false, write: true, exec: false }, false);
        m.poke(0x40010, &[5]).unwrap();
        m.remap(0x40000, 0x1000, 0x80000);
        assert!(!m.is_mapped(0x40000, 0x41000));
        m.extend(0x81000, 0x1000);
        // The moved memory and the extension stay those of the buffer.
        m.poke(0x81004, &[9]).unwrap();
        assert_eq!(buf.borrow()[0x10], 5);
        assert_eq!(buf.borrow()[0x1004], 9);
        assert!(!m.may_write(0x80000, 0x82000));
        m.map_zeroed(0x90000, 0x1000, Perm::RW);
        assert!(m.may_write(0x90000, 0x91000));
        m.extend(0x91000, 0x1000);
        assert!(m.is_mapped(0x90000, 0x92000));
    }

    #[test]
    fn shared_survives_clone_and_split() {
        let mut a = UserMemory::new();
        let buf = Rc::new(RefCell::new(vec![0u8; 0x2000]));
        a.map_shared(0x40000, buf.clone(), 0, 0x2000, Perm::RW, true);
        let mut b = a.clone(); // like a fork
        b.write(0x41000, &[42]).unwrap();
        let mut x = [0u8; 1];
        a.read(0x41000, &mut x).unwrap();
        assert_eq!(x[0], 42);
        a.protect(0x40000, 0x41000, Perm::R).unwrap(); // splits the region
        b.write(0x41001, &[7]).unwrap();
        a.read(0x41001, &mut x).unwrap();
        assert_eq!(x[0], 7);
        assert_eq!(buf.borrow()[0x1001], 7);
    }

    /// Code watching for the JIT: every change to a watched page
    /// marks it dirty only once, the others do not.
    #[test]
    fn code_watch_reports_every_kind_of_change() {
        let mut m = UserMemory::new();
        m.map(0x10000, vec![1; 0x4000], Perm::RWX).unwrap();
        let watch_all = |m: &mut UserMemory| {
            for p in 0x10..0x14 {
                m.watch_code(p);
            }
        };
        watch_all(&mut m);
        assert!(!m.code_dirty());
        m.write(0x10ffe, &[0; 4]).unwrap(); // straddling two pages
        assert_eq!(m.take_code_dirty(), [0x10, 0x11]);
        m.write(0x10000, &[0]).unwrap(); // no longer watched
        assert!(!m.code_dirty());
        assert!(m.write(0x20000, &[0]).is_err());
        assert!(!m.code_dirty());
        m.poke(0x12000, &[5]).unwrap();
        assert_eq!(m.take_code_dirty(), [0x12]);
        watch_all(&mut m);
        m.protect(0x13000, 0x14000, Perm::RW).unwrap();
        assert_eq!(m.take_code_dirty(), [0x13]);
        m.unmap(0x11000, 0x12000);
        assert_eq!(m.take_code_dirty(), [0x11]);
        m.map_fixed(0x10000, vec![2; 0x1000], Perm::RX);
        assert_eq!(m.take_code_dirty(), [0x10]);
        m.remap(0x12000, 0x1000, 0x30000);
        assert_eq!(m.take_code_dirty(), [0x12]);
        // A copy (fork) is another space, without watched pages.
        m.watch_code(0x10);
        let mut c = m.clone();
        assert_ne!(c.space_id(), m.space_id());
        c.poke(0x10000, &[9]).unwrap();
        assert!(!c.code_dirty());
        assert!(!m.code_dirty());
        // Only private memory can live in the block cache.
        assert!(m.is_private(0x10000));
        m.map_shared(0x50000, Rc::new(RefCell::new(vec![0; 0x1000])), 0, 0x1000, Perm::RX, false);
        assert!(!m.is_private(0x50000));
        assert!(!m.is_private(0x90000));
    }
}
