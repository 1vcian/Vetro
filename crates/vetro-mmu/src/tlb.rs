//! Software TLB and TLBI instructions of the EL1&0 regime.
//!
//! Direct-mapped cache, deterministic (no random hash):
//! one slot per 4 KiB page, chosen by the low bits of the page number.
//! Each entry however remembers the whole page or block it comes from, so a
//! TLBI by VA within a 2 MiB or 1 GiB block removes all the entries of that
//! block. Only successful walks are cached (translation, AF and address
//! size faults never enter, as the architecture requires); the
//! permissions are checked on every access from the saved AP/XN bits.

pub use vetro_cpu::sys::TlbiOp;

use crate::regs::MmuRegs;
use crate::walk::{Perms, Translation};

/// VA[55:0]: the part of the address that identifies an entry. Bits 63:56
/// are the tag (with TBI) or copies of bit 55 (verified before the lookup).
pub(crate) const VA_MASK: u64 = (1 << 56) - 1;

const ENTRIES: usize = 512;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TlbEntry {
    /// Base of the page or block, in VA[55:0].
    va_base: u64,
    size: u64,
    pa_base: u64,
    asid: u16,
    global: bool,
    level: u8,
    perms: Perms,
    attr_index: u8,
    sh: u8,
}

impl TlbEntry {
    pub(crate) fn new(key: u64, t: &Translation) -> Self {
        let mask = !(t.block_size - 1);
        TlbEntry {
            va_base: key & mask,
            size: t.block_size,
            pa_base: t.pa & mask,
            asid: t.asid,
            global: !t.ng,
            level: t.level,
            perms: t.perms.expect("only walks with the MMU on"),
            attr_index: t.attr_index.expect("only walks with the MMU on"),
            sh: t.sh,
        }
    }

    fn contains(&self, key: u64) -> bool {
        key.wrapping_sub(self.va_base) < self.size
    }

    /// Rebuilds the translation for `key`. MAIR is re-read now
    /// (the architecture allows both this and keeping it cached).
    pub(crate) fn translation(&self, key: u64, regs: &MmuRegs) -> Translation {
        Translation {
            pa: self.pa_base + (key - self.va_base),
            level: self.level,
            block_size: self.size,
            perms: Some(self.perms),
            attr_index: Some(self.attr_index),
            mair_attr: regs.mair_attr(self.attr_index),
            sh: self.sh,
            ng: !self.global,
            asid: self.asid,
        }
    }
}

/// VA[55:12] from the register of a TLBI (Xt[43:0]).
fn tlbi_va(xt: u64) -> u64 {
    (xt & ((1 << 44) - 1)) << 12
}

/// ASID from the register of a TLBI (Xt[63:48]).
fn tlbi_asid(xt: u64) -> u16 {
    (xt >> 48) as u16
}

/// TLB of a core.
#[derive(Clone, Debug)]
pub struct Tlb {
    entries: Vec<Option<TlbEntry>>,
    /// Generation of each slot: grows on every change to the slot
    /// (insertion, TLBI, flush). The cache of recent translations
    /// of the [`Mmu`](crate::Mmu) is valid only as long as the slot it comes from
    /// does not change.
    gens: Box<[u64; ENTRIES]>,
    /// Invalidations performed (TLBI and flushes), even no-op ones: whoever
    /// keeps copies of translations outside the TLB (the JIT's software TLB)
    /// discards them when this changes.
    flushes: u64,
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlb {
    pub fn new() -> Self {
        Tlb { entries: vec![None; ENTRIES], gens: Box::new([0; ENTRIES]), flushes: 0 }
    }

    #[inline]
    pub(crate) fn slot(key: u64) -> usize {
        (key >> 12) as usize & (ENTRIES - 1)
    }

    /// Generation of slot `slot`.
    #[inline]
    pub(crate) fn generation(&self, slot: usize) -> u64 {
        self.gens[slot]
    }

    pub(crate) fn lookup(&self, key: u64, asid: u16) -> Option<&TlbEntry> {
        self.entries[Self::slot(key)].as_ref().filter(|e| e.contains(key) && (e.global || e.asid == asid))
    }

    pub(crate) fn insert(&mut self, key: u64, e: TlbEntry) {
        let s = Self::slot(key);
        self.entries[s] = Some(e);
        self.gens[s] += 1;
    }

    /// Number of invalidations performed so far ([`tlbi`](Self::tlbi) and
    /// [`flush_all`](Self::flush_all)).
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Number of valid entries.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn remove_if(&mut self, pred: impl Fn(&TlbEntry) -> bool) {
        for (slot, g) in self.entries.iter_mut().zip(self.gens.iter_mut()) {
            if slot.as_ref().is_some_and(&pred) {
                *slot = None;
                *g += 1;
            }
        }
    }

    /// Flushes everything (VMALLE1).
    pub fn flush_all(&mut self) {
        self.flushes += 1;
        self.entries.fill(None);
        for g in self.gens.iter_mut() {
            *g += 1;
        }
    }

    /// Entries that contain `va` and are global or of `asid` (VAE1, VALE1).
    pub fn flush_va(&mut self, va: u64, asid: u16) {
        let key = va & VA_MASK;
        self.remove_if(|e| e.contains(key) && (e.global || e.asid == asid));
    }

    /// Non-global entries of `asid` (ASIDE1).
    pub fn flush_asid(&mut self, asid: u16) {
        self.remove_if(|e| !e.global && e.asid == asid);
    }

    /// Entries that contain `va`, of any ASID (VAAE1, VAALE1).
    pub fn flush_va_all_asids(&mut self, va: u64) {
        let key = va & VA_MASK;
        self.remove_if(|e| e.contains(key));
    }

    /// Executes a TLBI with the value of Xt (ignored by VMALLE1). The
    /// "last level" variants coincide with the others because the TLB contains
    /// only leaves (no cache of intermediate levels); the IS variants
    /// act here like the local ones.
    pub fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        use TlbiOp::*;
        self.flushes += 1;
        match op {
            Vmalle1 | Vmalle1is => self.flush_all(),
            Vae1 | Vae1is | Vale1 | Vale1is => self.flush_va(tlbi_va(xt), tlbi_asid(xt)),
            Aside1 | Aside1is => self.flush_asid(tlbi_asid(xt)),
            Vaae1 | Vaae1is | Vaale1 | Vaale1is => self.flush_va_all_asids(tlbi_va(xt)),
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// TLB entries go into the snapshot: they are observable state (a guest
/// that changes the tables without a TLBI still sees the old translation, and
/// an entry missing after restore would change the result). The slot
/// generations do not: they only serve the MMU's cache of recent
/// translations, which restarts empty on restore. Nor does `flushes`:
/// on restore it grows, so whoever keeps copies of translations (the JIT's
/// software TLB) discards them.
impl vetro_snapshot::Snapshot for Tlb {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        let valid = self.entries.iter().enumerate().filter_map(|(i, e)| e.map(|e| (i, e)));
        w.u32(self.len() as u32);
        for (slot, e) in valid {
            w.u32(slot as u32);
            w.u64(e.va_base);
            w.u64(e.size);
            w.u64(e.pa_base);
            w.u16(e.asid);
            w.bool(e.global);
            w.u8(e.level);
            w.u8(e.perms.ap);
            w.bool(e.perms.uxn);
            w.bool(e.perms.pxn);
            w.u8(e.attr_index);
            w.u8(e.sh);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        use vetro_snapshot::Error;
        let n = r.u32()? as usize;
        if n > ENTRIES {
            return Err(Error::invalid(format!("{n} entries in the TLB")));
        }
        self.entries.fill(None);
        let mut last = None;
        for _ in 0..n {
            let slot = r.u32()? as usize;
            if slot >= ENTRIES || last.is_some_and(|l| slot <= l) {
                return Err(Error::invalid(format!("TLB slot {slot}")));
            }
            last = Some(slot);
            let e = TlbEntry {
                va_base: r.u64()?,
                size: r.u64()?,
                pa_base: r.u64()?,
                asid: r.u16()?,
                global: r.bool()?,
                level: r.u8()?,
                perms: Perms { ap: r.u8()?, uxn: r.bool()?, pxn: r.bool()? },
                attr_index: r.u8()?,
                sh: r.u8()?,
            };
            if !e.size.is_power_of_two() || e.va_base & (e.size - 1) != 0 || e.attr_index > 7 {
                return Err(Error::invalid(format!("TLB entry {e:?}")));
            }
            self.entries[slot] = Some(e);
        }
        for g in self.gens.iter_mut() {
            *g += 1;
        }
        self.flushes += 1;
        Ok(())
    }
}
