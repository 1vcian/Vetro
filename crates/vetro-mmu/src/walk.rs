//! Stage 1 table walk (4 KiB granule) and permission check.
//!
//! Follows the ARMv8.0 pseudocode `AArch64.TranslationTableWalk`,
//! `AArch64.TranslateAddressS1Off` and `AArch64.CheckPermission` (Arm ARM,
//! D8 and J1), in the same order of checks: so the priority among different
//! faults is the architectural one.

use vetro_cpu::Access;

use crate::fault::{Fault, FaultKind};
use crate::regs::{Granule, MmuRegs, PAGE_SIZE, sctlr};

/// Error response of the physical bus (synchronous external abort).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusError {
    /// Nobody responds at that address (decode error).
    Decode,
    /// The device responded with an error (slave error).
    Slave,
}

/// Physical memory as seen by the MMU: RAM and devices, little-endian.
pub trait PhysMemory {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError>;

    /// Read of a descriptor (8 aligned bytes).
    fn read_u64(&mut self, pa: u64) -> Result<u64, BusError> {
        let mut b = [0u8; 8];
        self.read(pa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Compare-and-exchange of `old.len()` aligned bytes (store-exclusive):
    /// writes `new` if the bytes are `old`; true if it wrote. Memory shared
    /// with cores running in parallel must do it atomically (ADR 0042); the
    /// default reads, compares and writes.
    fn cmpxchg(&mut self, pa: u64, old: &[u8], new: &[u8]) -> Result<bool, BusError> {
        let mut cur = [0u8; 16];
        let cur = &mut cur[..old.len()];
        self.read(pa, cur)?;
        if cur != old {
            return Ok(false);
        }
        self.write(pa, new)?;
        Ok(true)
    }

    /// A broadcast TLBI (the Inner Shareable forms) after the core applied it
    /// to its own TLB: the other cores' TLBs must forget the same entries
    /// before it continues (ADR 0042). Nothing to do with one TLB.
    fn tlbi_broadcast(&mut self, _op: vetro_cpu::sys::TlbiOp, _xt: u64) {}
}

/// Final permissions of a translation: those of the leaf combined with
/// APTable, UXNTable and PXNTable of the tables walked through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    /// AP[2:1]: bit 1 = AP[2] (read-only), bit 0 = AP[1] (access from EL0).
    pub ap: u8,
    /// UXN (XN): no execution at EL0.
    pub uxn: bool,
    /// PXN: no execution at EL1.
    pub pxn: bool,
}

impl Perms {
    /// True if the access is permitted at privilege `el` (0 or 1).
    /// A page writable from EL0 is never executable at EL1.
    pub fn allows(self, access: Access, el: u8, wxn: bool) -> bool {
        let priv_w = self.ap & 0b10 == 0;
        let user_r = self.ap & 0b01 != 0;
        let user_w = self.ap == 0b01;
        if el == 0 {
            match access {
                Access::Read => user_r,
                Access::Write => user_w,
                Access::Fetch => !(self.uxn || (user_w && wxn)),
            }
        } else {
            match access {
                Access::Read => true,
                Access::Write => priv_w,
                Access::Fetch => !(self.pxn || (priv_w && wxn) || user_w),
            }
        }
    }
}

/// Outcome of a successful translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    pub pa: u64,
    /// Level of the leaf descriptor (1, 2, 3); 0 with the MMU off.
    pub level: u8,
    /// Size of the page or block (4 KiB, 2 MiB, 1 GiB); with the MMU
    /// off [`PAGE_SIZE`].
    pub block_size: u64,
    /// `None` with the MMU off, where every access is permitted.
    pub perms: Option<Perms>,
    /// AttrIndx of the descriptor; `None` with the MMU off.
    pub attr_index: Option<u8>,
    /// Memory attributes in MAIR format: the byte chosen by AttrIndx, or
    /// the one fixed by the architecture with the MMU off (0x00 Device-nGnRnE for
    /// data; 0xaa Write-Through or 0x44 Non-cacheable for fetches, depending on
    /// SCTLR.I).
    pub mair_attr: u8,
    /// Shareability SH[1:0]; 0b10 (Outer Shareable) with the MMU off.
    pub sh: u8,
    /// Non-global descriptor: valid only for `asid`.
    pub ng: bool,
    /// Current ASID at the time of the walk.
    pub asid: u16,
}

impl Translation {
    /// Value of PAR_EL1 after a successful AT, as QEMU composes it: ATTR,
    /// PA[47:12], bit 11 (RES1), NS = 1 (Non-secure regime), SH (0b10 for
    /// Device and Normal Non-cacheable).
    pub fn par(&self) -> u64 {
        let attr = u64::from(self.mair_attr);
        let sh = if attr & 0xf0 == 0 || attr == 0x44 || attr == 0x40 { 0b10 } else { u64::from(self.sh) };
        attr << 56 | (self.pa & bits(47, 12)) | 1 << 11 | 1 << 9 | sh << 7
    }
}

/// Mask of bits `hi..=lo`.
pub(crate) const fn bits(hi: u32, lo: u32) -> u64 {
    (u64::MAX >> (63 - hi)) & !((1u64 << lo) - 1)
}

/// Mask of the low `n` bits (n < 64).
const fn low(n: u32) -> u64 {
    (1u64 << n) - 1
}

pub(crate) fn fault(kind: FaultKind, va: u64, access: Access, el: u8) -> Fault {
    Fault { kind, va, access, el }
}

/// Translation with the MMU off: identity, with an address size fault if the VA
/// (without the tag, if there is TBI) exceeds PARange.
pub(crate) fn mmu_off(
    regs: &MmuRegs,
    pa_bits: u32,
    va: u64,
    access: Access,
    el: u8,
) -> Result<Translation, Fault> {
    let top = regs.addr_top(va);
    if va & bits(top, pa_bits) != 0 {
        return Err(fault(FaultKind::AddressSize(0), va, access, el));
    }
    let mair_attr = match access {
        Access::Fetch if regs.sctlr & sctlr::I != 0 => 0xaa,
        Access::Fetch => 0x44,
        Access::Read | Access::Write => 0x00,
    };
    Ok(Translation {
        pa: va & low(pa_bits),
        level: 0,
        block_size: PAGE_SIZE,
        perms: None,
        attr_index: None,
        mair_attr,
        sh: 0b10,
        ng: false,
        asid: regs.asid(),
    })
}

/// Selects the half of the virtual space (true = TTBR1) and checks that the bits
/// between AddrTop and the input size are all equal to the selector;
/// otherwise a level 0 translation fault.
pub(crate) fn select(regs: &MmuRegs, va: u64) -> Result<bool, FaultKind> {
    let top = regs.addr_top(va);
    let hi = va >> top & 1 != 0;
    let field = va & bits(top, regs.input_size(hi));
    let ok = if hi { field == bits(top, regs.input_size(hi)) } else { field == 0 };
    if ok { Ok(hi) } else { Err(FaultKind::Translation(0)) }
}

/// Table walk for a VA already validated by [`select`]. Does not check the
/// permissions ([`check`] does), so the outcome can be put in the TLB.
pub(crate) fn walk_tables<P: PhysMemory + ?Sized>(
    regs: &MmuRegs,
    pa_bits: u32,
    phys: &mut P,
    va: u64,
    hi: bool,
) -> Result<Translation, FaultKind> {
    const STRIDE: u32 = 9;
    const GRAIN: u32 = 12;
    if regs.walk_disabled(hi) {
        return Err(FaultKind::Translation(0));
    }
    if regs.granule(hi) == Granule::K64 {
        return Err(FaultKind::Unimplemented("64 KiB translation granule"));
    }
    if regs.sctlr & sctlr::EE != 0 {
        return Err(FaultKind::Unimplemented("big-endian descriptors (SCTLR_EL1.EE = 1)"));
    }
    let inputsize = regs.input_size(hi);
    let outputsize = regs.output_size(pa_bits);
    let oversize = |addr: u64| outputsize < 48 && addr & bits(47, outputsize) != 0;
    let ttbr = regs.ttbr(hi);
    if oversize(ttbr) {
        return Err(FaultKind::AddressSize(0));
    }
    // Initial level and alignment of the first table.
    let mut level = 4 - (inputsize - GRAIN).div_ceil(STRIDE);
    let baselowerbound = 3 + inputsize - ((3 - level) * STRIDE + GRAIN);
    let mut base = ttbr & bits(47, baselowerbound);
    let mut addrtop = inputsize - 1;
    // APTable[1:0], UXNTable, PXNTable accumulated along the walk.
    let (mut ap_table, mut xn_table, mut pxn_table) = (0u8, false, false);
    let desc = loop {
        let bottom = (3 - level) * STRIDE + GRAIN;
        let index = (va & bits(addrtop, bottom)) >> bottom << 3;
        let desc = phys.read_u64(base | index).map_err(|e| FaultKind::ExternalWalk(level as u8, e))?;
        // Invalid (x0), reserved, or "block" (01) at level 3.
        if desc & 1 == 0 || (desc & 3 == 1 && level == 3) {
            return Err(FaultKind::Translation(level as u8));
        }
        if desc & 3 == 1 || level == 3 {
            break desc;
        }
        // Table descriptor.
        if oversize(desc) {
            return Err(FaultKind::AddressSize(level as u8));
        }
        base = desc & bits(47, GRAIN);
        ap_table |= (desc >> 61 & 3) as u8;
        xn_table |= desc >> 60 & 1 != 0;
        pxn_table |= desc >> 59 & 1 != 0;
        level += 1;
        addrtop = bottom - 1;
    };
    // With 4 KiB the first level with blocks is level 1.
    if level < 1 {
        return Err(FaultKind::Translation(level as u8));
    }
    let bottom = (3 - level) * STRIDE + GRAIN;
    let pa = (desc & bits(47, bottom)) | (va & low(bottom));
    if oversize(pa) {
        return Err(FaultKind::AddressSize(level as u8));
    }
    if desc >> 10 & 1 == 0 {
        return Err(FaultKind::AccessFlag(level as u8));
    }
    let ap = (desc >> 6 & 3) as u8;
    let perms = Perms {
        // APTable[1] forces read-only, APTable[0] removes access from EL0.
        ap: (ap | (ap_table & 0b10)) & !(ap_table & 0b01),
        uxn: desc >> 54 & 1 != 0 || xn_table,
        pxn: desc >> 53 & 1 != 0 || pxn_table,
    };
    let attr_index = (desc >> 2 & 7) as u8;
    Ok(Translation {
        pa,
        level: level as u8,
        block_size: 1 << bottom,
        perms: Some(perms),
        attr_index: Some(attr_index),
        mair_attr: regs.mair_attr(attr_index),
        sh: (desc >> 8 & 3) as u8,
        ng: desc >> 11 & 1 != 0,
        asid: regs.asid(),
    })
}

/// Unaligned data access on Device memory (MAIR byte 0b0000xxxx).
pub(crate) fn check_device_alignment(
    t: &Translation,
    access: Access,
    aligned: bool,
) -> Result<(), FaultKind> {
    if !aligned && access != Access::Fetch && t.mair_attr & 0xf0 == 0 {
        Err(FaultKind::Alignment)
    } else {
        Ok(())
    }
}

/// Permission check; the permission fault reports the level of the
/// leaf.
pub(crate) fn check(regs: &MmuRegs, t: &Translation, access: Access, el: u8) -> Result<(), FaultKind> {
    match t.perms {
        Some(p) if !p.allows(access, el, regs.wxn()) => Err(FaultKind::Permission(t.level)),
        _ => Ok(()),
    }
}
