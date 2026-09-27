//! The MMU of a core: registers, TLB and last fault.

use vetro_cpu::Access;

use crate::fault::Fault;
use crate::regs::{MmuRegs, PAGE_SIZE};
use crate::tlb::{Tlb, TlbEntry, TlbiOp, VA_MASK};
use crate::walk::{
    Perms, PhysMemory, Translation, check, check_device_alignment, fault, mmu_off, select, walk_tables,
};

/// Entries of the cache of recent translations: one per TLB slot.
const RECENT: usize = 512;

/// Recent successful translation with the MMU on, ready for the permission
/// check. It is only a shortcut: valid as long as the translation
/// registers do not change (`epoch`) and as long as the TLB slot it comes from
/// stays as it was (`tlb_gen`). Under those conditions the full path
/// ([`Mmu::translate_checked`]) would find the same entry in the TLB and give
/// the same result: the cache changes nothing observable, not even
/// when the guest modifies the tables without a TLBI.
#[derive(Clone, Copy, Debug)]
struct Recent {
    /// Whole VA[63:12] (tag included: `select` depends on the high bits).
    vpage: u64,
    /// Physical address of the 4 KiB page.
    pa_page: u64,
    perms: Perms,
    /// Device memory (MAIR byte 0b0000xxxx): unaligned data accesses
    /// fault.
    device: bool,
    epoch: u64,
    tlb_gen: u64,
}

const EMPTY: Recent = Recent {
    vpage: u64::MAX,
    pa_page: 0,
    perms: Perms { ap: 0, uxn: true, pxn: true },
    device: true,
    epoch: 0,
    tlb_gen: 0,
};

/// Stage 1 MMU of the EL1&0 regime for one core.
#[derive(Clone, Debug)]
pub struct Mmu {
    /// Translation registers. After changing SCTLR or TCR the system
    /// calls [`Tlb::flush_all`] like QEMU (the architecture allows keeping
    /// those fields in the TLB, but the guest must do a TLBI anyway).
    pub regs: MmuRegs,
    pa_bits: u32,
    tlb: Tlb,
    last_fault: Option<Fault>,
    /// Cache of recent translations (see [`Recent`]), indexed like
    /// the TLB.
    recent: Box<[Recent; RECENT]>,
    /// Registers with which the entries of the current epoch were filled.
    recent_regs: MmuRegs,
    /// Current epoch: grows when the translation registers change.
    recent_epoch: u64,
    /// Hits of the recent cache (tests only).
    #[cfg(test)]
    pub(crate) recent_hits: u64,
}

impl Mmu {
    /// PARange of the Cortex-A53 (ID_AA64MMFR0_EL1.PARange = 0b0010).
    pub const PA_BITS_CORTEX_A53: u32 = 40;

    /// `pa_bits` is PARange (32..=48): limits TCR.IPS and the identity mapping with the MMU
    /// off.
    pub fn new(pa_bits: u32) -> Self {
        assert!((32..=48).contains(&pa_bits), "unsupported PARange: {pa_bits}");
        Mmu {
            regs: MmuRegs::default(),
            pa_bits,
            tlb: Tlb::new(),
            last_fault: None,
            recent: Box::new([EMPTY; RECENT]),
            recent_regs: MmuRegs::default(),
            recent_epoch: 1,
            #[cfg(test)]
            recent_hits: 0,
        }
    }

    pub fn pa_bits(&self) -> u32 {
        self.pa_bits
    }

    /// Translates `va` for an access with privilege `el` (0 or 1), going through the
    /// TLB. Order: identity if SCTLR.M = 0; check of the half and of the high
    /// bits (level 0 translation fault); TLB lookup; walk (which on a
    /// miss honours EPDx); permissions.
    pub fn translate<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
    ) -> Result<Translation, Fault> {
        self.translate_checked(phys, va, access, el, true)
    }

    /// Like [`translate`](Self::translate), but with `aligned = false` a
    /// data access on Device memory (even with the MMU off, where data
    /// is Device-nGnRnE) gives [`FaultKind::Alignment`]: after the walk
    /// faults and before the permissions, like `AArch64.FirstStageTranslate`.
    pub fn translate_checked<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<Translation, Fault> {
        let regs = self.regs;
        if !regs.enabled() {
            let t = mmu_off(&regs, self.pa_bits, va, access, el)?;
            check_device_alignment(&t, access, aligned).map_err(|k| fault(k, va, access, el))?;
            return Ok(t);
        }
        let hi = select(&regs, va).map_err(|k| fault(k, va, access, el))?;
        let key = va & VA_MASK;
        let t = match self.tlb.lookup(key, regs.asid()) {
            Some(e) => e.translation(key, &regs),
            None => {
                let t =
                    walk_tables(&regs, self.pa_bits, phys, va, hi).map_err(|k| fault(k, va, access, el))?;
                self.tlb.insert(key, TlbEntry::new(key, &t));
                t
            }
        };
        check_device_alignment(&t, access, aligned).map_err(|k| fault(k, va, access, el))?;
        check(&regs, &t, access, el).map_err(|k| fault(k, va, access, el))?;
        Ok(t)
    }

    /// Like `translate_pa_with`, with the registers already in `self.regs` (for
    /// tests, which change them directly).
    #[cfg(test)]
    #[inline]
    pub(crate) fn translate_pa<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        let regs = self.regs;
        self.sync_recent(&regs);
        self.translate_recent(phys, va, access, el, aligned)
    }

    /// Like [`translate_checked`](Self::translate_checked) with the registers
    /// `regs`, which become the MMU's, but returns only
    /// the physical address and goes through the cache of recent translations first.
    /// Result, fault and TLB state after the call are identical to
    /// those of the full path. Used by [`MmuBus`](crate::MmuBus), to which
    /// the CPU passes the registers on every access.
    #[inline]
    pub(crate) fn translate_pa_with<P: PhysMemory + ?Sized>(
        &mut self,
        regs: &MmuRegs,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        self.regs = *regs;
        self.sync_recent(regs);
        self.translate_recent(phys, va, access, el, aligned)
    }

    /// New epoch of the recent cache if `regs` (the current registers) are not
    /// the ones it was filled with.
    #[inline]
    fn sync_recent(&mut self, regs: &MmuRegs) {
        if !regs.same(&self.recent_regs) {
            self.recent_regs = *regs;
            self.recent_epoch += 1;
        }
    }

    /// Recent cache, then full path. Requires `self.regs ==
    /// self.recent_regs` (guaranteed by `sync_recent`).
    #[inline]
    fn translate_recent<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        let enabled = self.recent_regs.enabled();
        let vpage = va / PAGE_SIZE;
        let slot = Tlb::slot(va & VA_MASK);
        if enabled {
            let r = &self.recent[slot];
            if r.vpage == vpage
                && r.epoch == self.recent_epoch
                && r.tlb_gen == self.tlb.generation(slot)
                && (aligned || access == Access::Fetch || !r.device)
                && r.perms.allows(access, el, self.recent_regs.wxn())
            {
                #[cfg(test)]
                {
                    self.recent_hits += 1;
                }
                return Ok(r.pa_page | (va & (PAGE_SIZE - 1)));
            }
        }
        self.translate_fill(phys, va, access, el, aligned, slot)
    }

    /// Full path and filling of the recent entry (out of line: the
    /// fast path stays small and gets inlined into the fetch).
    #[inline(never)]
    fn translate_fill<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
        slot: usize,
    ) -> Result<u64, Fault> {
        let t = self.translate_checked(phys, va, access, el, aligned)?;
        if self.regs.enabled()
            && let Some(perms) = t.perms
        {
            self.recent[slot] = Recent {
                vpage: va / PAGE_SIZE,
                pa_page: t.pa & !(PAGE_SIZE - 1),
                perms,
                device: t.mair_attr & 0xf0 == 0,
                epoch: self.recent_epoch,
                tlb_gen: self.tlb.generation(slot),
            };
        }
        Ok(t.pa)
    }

    /// Like [`translate`](Self::translate) but without reading or filling the
    /// TLB: for the debugger and for checking the tables.
    pub fn walk<P: PhysMemory + ?Sized>(
        &self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
    ) -> Result<Translation, Fault> {
        let regs = self.regs;
        if !regs.enabled() {
            return mmu_off(&regs, self.pa_bits, va, access, el);
        }
        let t = select(&regs, va)
            .and_then(|hi| walk_tables(&regs, self.pa_bits, phys, va, hi))
            .map_err(|k| fault(k, va, access, el))?;
        check(&regs, &t, access, el).map_err(|k| fault(k, va, access, el))?;
        Ok(t)
    }

    pub fn tlb(&self) -> &Tlb {
        &self.tlb
    }

    pub fn tlb_mut(&mut self) -> &mut Tlb {
        &mut self.tlb
    }

    /// Executes a TLBI on this core (see [`Tlb::tlbi`]).
    pub fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        self.tlb.tlbi(op, xt);
    }

    /// Detailed fault of the last access that failed through
    /// [`VirtMemory`](crate::VirtMemory).
    pub fn last_fault(&self) -> Option<Fault> {
        self.last_fault
    }

    pub(crate) fn set_last_fault(&mut self, f: Fault) {
        self.last_fault = Some(f);
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// Translation registers and TLB. The cache of recent translations is not
/// saved: it is a shortcut that changes nothing observable (see
/// [`Recent`]) and on restore it restarts empty. The last fault only serves the
/// diagnostics of [`VirtMemory`](crate::VirtMemory) (user mode) and restarts
/// empty.
impl vetro_snapshot::Snapshot for Mmu {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(u64::from(self.pa_bits));
        let r = &self.regs;
        for v in [r.sctlr, r.tcr, r.ttbr0, r.ttbr1, r.mair] {
            w.u64(v);
        }
        w.put(&self.tlb);
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("MMU PARange", u64::from(self.pa_bits))?;
        let g = &mut self.regs;
        for v in [&mut g.sctlr, &mut g.tcr, &mut g.ttbr0, &mut g.ttbr1, &mut g.mair] {
            *v = r.u64()?;
        }
        r.get(&mut self.tlb)?;
        self.last_fault = None;
        self.recent.fill(EMPTY);
        self.recent_regs = MmuRegs::default();
        self.recent_epoch += 1;
        Ok(())
    }
}
