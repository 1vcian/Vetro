//! GICv3: distributor, one redistributor and one system-register CPU
//! interface per core (ARM IHI0069); up to 16 cores (ADR 0042).
//!
//! Scope and choices (see also `docs/specs/platform.md`):
//! - **A single security state** (GICD_CTLR.DS = 1, RAO/WI) and affinity
//!   routing always on (ARE = 1, RAO/WI), as Linux sees it under QEMU
//!   without EL3.
//! - **Non-secure group 1 only**: IGROUPR is stored, but interrupts
//!   left in group 0 are never signalled (they would be FIQs).
//!   Linux puts everything in group 1. IGRPMODR and NSACR are RAZ/WI.
//! - No LPI/ITS: GICR_CTLR.EnableLPIs, PROPBASER and PENDBASER RAZ/WI.
//! - 256 SPI (INTID 32..287), GICD_TYPER.ITLinesNumber = 8, IDbits = 9.
//! - CPU interface with 5 priority bits (PRIbits = 4, like QEMU):
//!   PMR holds the top 5 bits, BPR1 minimum 3. ICC_CTLR_EL1.CBPR is RAZ/WI
//!   (BPR0 is not modelled), EOImode is writable.
//! - GICR_WAKER does the handshake (ChildrenAsleep follows ProcessorSleep) but does not
//!   block delivery.
//!
//! - SPIs go to the core named by GICD_IROUTER (IRM = 1: core 0, QEMU has no
//!   1-of-N either); SGIs from ICC_SGI1R_EL1 to the cores of its target list
//!   (or all but the sender with IRM).
//!
//! The GIC is mapped on the bus as a single region starting at GICD_BASE and
//! covering the redistributors (see [`Gic::mmio_size`]); the space in between is
//! RAZ/WI.

use crate::bus::MmioDevice;
use crate::map;

/// Number of implemented SPIs.
pub const NUM_SPIS: usize = 256;
/// Total number of INTIDs (SGI + PPI + SPI).
pub const NUM_INTIDS: usize = 32 + NUM_SPIS;
/// INTID returned by IAR when there is nothing to deliver.
pub const INTID_SPURIOUS: u32 = 1023;
/// Priority bits implemented in the CPU interface.
pub const PRI_BITS: u32 = 5;
/// Mask of the implemented priority bits.
pub const PRI_MASK: u8 = (0xFF00u16 >> PRI_BITS) as u8;
/// Minimum value of ICC_BPR1_EL1 (8 - PRI_BITS).
pub const MIN_BPR1: u8 = (8 - PRI_BITS) as u8;

/// Size of the GIC MMIO region on the bus for one core (GICD + hole + one
/// GICR); [`Gic::mmio_size`] for more.
pub const MMIO_SIZE: u64 = map::GICR_BASE + map::GICR_SIZE_PER_CPU - map::GICD_BASE;
/// Offset of the redistributor inside the region.
const GICR_OFFSET: u64 = map::GICR_BASE - map::GICD_BASE;

// Distributor.
pub const GICD_CTLR: u64 = 0x0000;
pub const GICD_TYPER: u64 = 0x0004;
pub const GICD_IIDR: u64 = 0x0008;
pub const GICD_TYPER2: u64 = 0x000C;
pub const GICD_IGROUPR: u64 = 0x0080;
pub const GICD_ISENABLER: u64 = 0x0100;
pub const GICD_ICENABLER: u64 = 0x0180;
pub const GICD_ISPENDR: u64 = 0x0200;
pub const GICD_ICPENDR: u64 = 0x0280;
pub const GICD_ISACTIVER: u64 = 0x0300;
pub const GICD_ICACTIVER: u64 = 0x0380;
pub const GICD_IPRIORITYR: u64 = 0x0400;
pub const GICD_ICFGR: u64 = 0x0C00;
pub const GICD_IGRPMODR: u64 = 0x0D00;
pub const GICD_IROUTER: u64 = 0x6000;
pub const GICD_PIDR2: u64 = 0xFFE8;

pub const GICD_CTLR_ENABLE_GRP0: u32 = 1 << 0;
pub const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1;
pub const GICD_CTLR_ARE: u32 = 1 << 4;
pub const GICD_CTLR_DS: u32 = 1 << 6;

// Redistributor, RD_base frame.
pub const GICR_CTLR: u64 = 0x0000;
pub const GICR_IIDR: u64 = 0x0004;
pub const GICR_TYPER: u64 = 0x0008;
pub const GICR_WAKER: u64 = 0x0014;
pub const GICR_PIDR2: u64 = 0xFFE8;
pub const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
pub const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
// Redistributor, SGI_base frame (offsets relative to the frame).
pub const GICR_SGI_BASE: u64 = 0x1_0000;
pub const GICR_IGROUPR0: u64 = 0x0080;
pub const GICR_ISENABLER0: u64 = 0x0100;
pub const GICR_ICENABLER0: u64 = 0x0180;
pub const GICR_ISPENDR0: u64 = 0x0200;
pub const GICR_ICPENDR0: u64 = 0x0280;
pub const GICR_ISACTIVER0: u64 = 0x0300;
pub const GICR_ICACTIVER0: u64 = 0x0380;
pub const GICR_IPRIORITYR: u64 = 0x0400;
pub const GICR_ICFGR0: u64 = 0x0C00;
pub const GICR_ICFGR1: u64 = 0x0C04;

/// ARM implementer (JEP106 0x43B), like QEMU.
const IIDR: u32 = 0x43B;
/// PIDR4-7, PIDR0-3, CIDR0-3 starting at 0xFFD0 (QEMU's values; PIDR2
/// says ArchRev = 3, i.e. GICv3, and Linux checks it).
const GICD_IDS: [u8; 12] = [0x44, 0x00, 0x00, 0x00, 0x92, 0xB4, 0x3B, 0x00, 0x0D, 0xF0, 0x05, 0xB1];
const GICR_IDS: [u8; 12] = [0x44, 0x00, 0x00, 0x00, 0x93, 0xB4, 0x3B, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

/// ICC_CTLR_EL1: writable bits and fixed fields.
pub const ICC_CTLR_EOIMODE: u64 = 1 << 1;
const ICC_CTLR_PRIBITS: u64 = ((PRI_BITS - 1) as u64) << 8;
const ICC_CTLR_A3V: u64 = 1 << 15;

#[derive(Clone, Copy, Debug, Default)]
struct Irq {
    enabled: bool,
    /// Pending latch (edge, ISPENDR); for levels it is added to the line.
    pending: bool,
    /// Level of the input line.
    level: bool,
    active: bool,
    group1: bool,
    /// true = rising edge, false = level high.
    edge: bool,
    priority: u8,
    /// GICD_IROUTER (SPIs only).
    router: u64,
}

impl Irq {
    fn is_pending(&self) -> bool {
        self.pending || (!self.edge && self.level)
    }
}

#[derive(Clone, Copy)]
enum BitOp {
    Group,
    SetEnable,
    ClearEnable,
    SetPending,
    ClearPending,
    SetActive,
    ClearActive,
}

/// The CPU interface of one core and the wake state of its redistributor.
#[derive(Clone, Debug)]
struct CpuIf {
    processor_sleep: bool,
    pmr: u8,
    bpr1: u8,
    igrpen1: bool,
    eoimode: bool,
    /// Active priorities (INTID, group priority), the highest on top.
    active_prio: Vec<(u32, u8)>,
}

impl CpuIf {
    fn new() -> Self {
        CpuIf {
            processor_sleep: true,
            pmr: 0,
            bpr1: MIN_BPR1,
            igrpen1: false,
            eoimode: false,
            active_prio: Vec::new(),
        }
    }

    fn group_prio(&self, prio: u8) -> u8 {
        prio & (0xFFu8 << self.bpr1)
    }

    /// Running priority (0x100 = none active, for comparisons).
    fn running(&self) -> u16 {
        self.active_prio.last().map_or(0x100, |&(_, p)| u16::from(p))
    }
}

/// SGIs and PPIs of one core (INTID 0..31), banked per redistributor.
fn private_irqs() -> [Irq; 32] {
    let mut p = [Irq::default(); 32];
    for irq in &mut p[..16] {
        irq.edge = true; // SGIs are always edge-triggered
    }
    p
}

/// Affinity of core `cpu` as QEMU virt numbers it with a GICv3
/// (`virt_cpu_mp_affinity`): 16 cores per cluster, Aff0 = core in the
/// cluster, Aff1 = cluster. MPIDR_EL1 is this value with bit 31 set.
pub fn cpu_affinity(cpu: usize) -> u64 {
    ((cpu / 16) as u64) << 8 | (cpu % 16) as u64
}

/// Affinity field of GICD_IROUTER (Aff3 in bits 39:32, Aff2..Aff0 in 23:0)
/// for an MPIDR-style affinity value.
fn router_affinity(aff: u64) -> u64 {
    aff & 0xFF_00FF_FFFF
}

/// GICv3 with `n` cores: one distributor, one redistributor (RD and SGI
/// frames, banked SGIs and PPIs, wake state) and one system-register CPU
/// interface per core.
///
/// The CPU interface registers (ICC_*) are those of the *current* core
/// ([`Gic::set_current`]): the machine sets it to the core that runs. A
/// redistributor is chosen by its address, like on hardware.
#[derive(Clone, Debug)]
pub struct Gic {
    /// INTIDs of core 0 (its SGIs and PPIs) and the SPIs: for one core
    /// exactly the state of the single-core model (same snapshot).
    irqs: Vec<Irq>,
    /// SGIs and PPIs of cores 1..n.
    banked: Vec<[Irq; 32]>,
    /// Only EnableGrp0/EnableGrp1; ARE and DS are added on read.
    gicd_ctlr: u32,
    /// CPU interfaces of cores 0..n.
    cpus: Vec<CpuIf>,
    /// Core whose ICC_* registers are accessed.
    cur: usize,
}

impl Default for Gic {
    fn default() -> Self {
        Self::new()
    }
}

impl Gic {
    /// A GIC for one core.
    pub fn new() -> Self {
        Self::with_cpus(1)
    }

    /// A GIC for `n` cores (1..=16, one cluster as QEMU virt numbers them;
    /// more would need Aff1 in the SGI target lists).
    pub fn with_cpus(n: usize) -> Self {
        assert!((1..=16).contains(&n), "{n} cores: 1..=16 supported");
        let mut irqs = vec![Irq::default(); NUM_INTIDS];
        irqs[..32].copy_from_slice(&private_irqs());
        Self { irqs, banked: vec![private_irqs(); n - 1], gicd_ctlr: 0, cpus: vec![CpuIf::new(); n], cur: 0 }
    }

    /// Number of cores.
    pub fn cpus(&self) -> usize {
        self.cpus.len()
    }

    /// Size of the MMIO region on the bus: GICD, the hole, one redistributor
    /// (two 64 KiB frames) per core.
    pub fn mmio_size(n: usize) -> u64 {
        map::GICR_BASE + n as u64 * map::GICR_SIZE_PER_CPU - map::GICD_BASE
    }

    /// The core whose CPU interface (ICC_*) the system registers reach.
    pub fn set_current(&mut self, cpu: usize) {
        assert!(cpu < self.cpus.len());
        self.cur = cpu;
    }

    pub fn current(&self) -> usize {
        self.cur
    }

    fn irq(&self, cpu: usize, intid: usize) -> Option<&Irq> {
        if intid < 32 && cpu > 0 { self.banked.get(cpu - 1).map(|b| &b[intid]) } else { self.irqs.get(intid) }
    }

    fn irq_mut(&mut self, cpu: usize, intid: usize) -> Option<&mut Irq> {
        if intid < 32 && cpu > 0 {
            self.banked.get_mut(cpu - 1).map(|b| &mut b[intid])
        } else {
            self.irqs.get_mut(intid)
        }
    }

    // ---- Input lines -------------------------------------------------------

    /// Line level of a PPI (16..31, of core 0) or SPI (32..). On an
    /// interrupt configured as edge-triggered, the rising edge sets pending.
    pub fn set_irq_level(&mut self, intid: u32, level: bool) {
        self.set_private_level(0, intid, level);
    }

    /// Line level of a PPI (16..31) of core `cpu`, or of an SPI (32..).
    pub fn set_private_level(&mut self, cpu: usize, intid: u32, level: bool) {
        let Some(irq) = self.irq_mut(cpu, intid as usize).filter(|_| intid >= 16) else {
            return;
        };
        if irq.edge && level && !irq.level {
            irq.pending = true;
        }
        irq.level = level;
    }

    /// Line level of SPI `spi` (INTID `32 + spi`).
    pub fn set_spi_level(&mut self, spi: u32, level: bool) {
        self.set_irq_level(map::SPI_BASE + spi, level);
    }

    /// Makes an SGI (0..15) pending on core 0.
    pub fn send_sgi(&mut self, intid: u32) {
        self.send_sgi_to(0, intid);
    }

    /// Makes an SGI (0..15) pending on core `cpu`.
    pub fn send_sgi_to(&mut self, cpu: usize, intid: u32) {
        if intid < 16
            && let Some(irq) = self.irq_mut(cpu, intid as usize)
        {
            irq.pending = true;
        }
    }

    /// State of an interrupt of core 0: (enabled, pending, active).
    pub fn irq_state(&self, intid: u32) -> Option<(bool, bool, bool)> {
        self.irq(0, intid as usize).map(|i| (i.enabled, i.is_pending(), i.active))
    }

    // ---- Selection ---------------------------------------------------------

    /// An SPI goes to the core whose affinity is in GICD_IROUTER; with
    /// IRM = 1 (1 of N, which QEMU does not implement either) to core 0.
    fn routed_to(&self, cpu: usize, irq: &Irq) -> bool {
        if irq.router & (1 << 31) != 0 {
            return cpu == 0;
        }
        router_affinity(irq.router) == router_affinity(cpu_affinity(cpu))
    }

    /// Highest-priority pending group 1 interrupt of core 0 (lowest value;
    /// on a tie the lowest INTID wins), without looking at PMR or the running
    /// priority.
    pub fn highest_pending(&self) -> Option<(u32, u8)> {
        self.highest_pending_of(0)
    }

    fn highest_pending_of(&self, cpu: usize) -> Option<(u32, u8)> {
        if self.gicd_ctlr & GICD_CTLR_ENABLE_GRP1 == 0 {
            return None;
        }
        let ok = |irq: &Irq| irq.enabled && irq.is_pending() && !irq.active && irq.group1;
        let mut best: Option<(u32, u8)> = None;
        let private = if cpu == 0 { &self.irqs[..32] } else { &self.banked[cpu - 1][..] };
        for (i, irq) in private.iter().enumerate() {
            if ok(irq) && best.is_none_or(|(_, p)| irq.priority < p) {
                best = Some((i as u32, irq.priority));
            }
        }
        for (i, irq) in self.irqs.iter().enumerate().skip(32) {
            if ok(irq) && self.routed_to(cpu, irq) && best.is_none_or(|(_, p)| irq.priority < p) {
                best = Some((i as u32, irq.priority));
            }
        }
        best
    }

    /// The interrupt the CPU interface of `cpu` would signal now.
    fn deliverable(&self, cpu: usize) -> Option<(u32, u8)> {
        let (intid, prio) = self.highest_pending_of(cpu)?;
        let c = &self.cpus[cpu];
        (c.igrpen1 && prio < c.pmr && u16::from(c.group_prio(prio)) < c.running()).then_some((intid, prio))
    }

    /// IRQ line to the current core: the core takes the exception if
    /// PSTATE.I is 0.
    pub fn irq_line(&self) -> bool {
        self.deliverable(self.cur).is_some()
    }

    /// IRQ line to core `cpu`.
    pub fn irq_line_of(&self, cpu: usize) -> bool {
        self.deliverable(cpu).is_some()
    }

    // ---- CPU interface (system registers of the current core) ---------------

    /// MRS ICC_IAR1_EL1: acknowledges the signalled interrupt (it becomes active) and
    /// returns its INTID, or 1023.
    pub fn read_iar1(&mut self) -> u64 {
        let cpu = self.cur;
        let Some((intid, prio)) = self.deliverable(cpu) else {
            return u64::from(INTID_SPURIOUS);
        };
        let gp = self.cpus[cpu].group_prio(prio);
        let irq = self.irq_mut(cpu, intid as usize).expect("deliverable INTID");
        irq.active = true;
        irq.pending = false;
        self.cpus[cpu].active_prio.push((intid, gp));
        u64::from(intid)
    }

    /// MRS ICC_HPPIR1_EL1: highest-priority pending INTID, without acknowledging it.
    pub fn read_hppir1(&self) -> u64 {
        u64::from(self.highest_pending_of(self.cur).map_or(INTID_SPURIOUS, |(i, _)| i))
    }

    /// MSR ICC_EOIR1_EL1: drops the running priority and, with
    /// EOImode = 0, deactivates the interrupt.
    pub fn write_eoir1(&mut self, value: u64) {
        let intid = (value & 0xFF_FFFF) as u32;
        if (1020..1024).contains(&intid) {
            return;
        }
        let c = &mut self.cpus[self.cur];
        c.active_prio.pop();
        if !c.eoimode {
            self.deactivate(intid);
        }
    }

    /// MSR ICC_DIR_EL1: separate deactivation (used with EOImode = 1).
    pub fn write_dir(&mut self, value: u64) {
        self.deactivate((value & 0xFF_FFFF) as u32);
    }

    fn deactivate(&mut self, intid: u32) {
        if let Some(irq) = self.irq_mut(self.cur, intid as usize) {
            irq.active = false;
        }
    }

    /// MSR ICC_SGI1R_EL1: SGI `INTID` (bits 27:24) to the cores of cluster
    /// Aff3.Aff2.Aff1 whose Aff0 is in TargetList (bits 15:0, offset by
    /// RS × 16), or with IRM = 1 to every core but the current one.
    pub fn write_sgi1r(&mut self, value: u64) {
        let intid = ((value >> 24) & 0xF) as u32;
        let irm = value & (1 << 40) != 0;
        let targets = value & 0xFFFF;
        let rs = (value >> 44) & 0xF;
        // Aff3 (55:48), Aff2 (39:32), Aff1 (23:16) as an MPIDR-style value.
        let cluster = (value >> 48 & 0xFF) << 32 | (value >> 32 & 0xFF) << 16 | (value >> 16 & 0xFF) << 8;
        for cpu in 0..self.cpus.len() {
            let aff = cpu_affinity(cpu);
            let hit = if irm {
                cpu != self.cur
            } else {
                let aff0 = aff & 0xFF;
                aff & !0xFF == cluster && aff0 >> 4 == rs && targets & 1 << (aff0 & 0xF) != 0
            };
            if hit {
                self.send_sgi_to(cpu, intid);
            }
        }
    }

    pub fn read_pmr(&self) -> u64 {
        u64::from(self.cpus[self.cur].pmr)
    }
    pub fn write_pmr(&mut self, value: u64) {
        self.cpus[self.cur].pmr = value as u8 & PRI_MASK;
    }

    pub fn read_bpr1(&self) -> u64 {
        u64::from(self.cpus[self.cur].bpr1)
    }
    pub fn write_bpr1(&mut self, value: u64) {
        self.cpus[self.cur].bpr1 = (value as u8 & 7).max(MIN_BPR1);
    }

    /// MRS ICC_RPR_EL1: running priority, 0xFF if none.
    pub fn read_rpr(&self) -> u64 {
        u64::from(self.cpus[self.cur].running().min(0xFF))
    }

    pub fn read_ctlr(&self) -> u64 {
        ICC_CTLR_A3V | ICC_CTLR_PRIBITS | if self.cpus[self.cur].eoimode { ICC_CTLR_EOIMODE } else { 0 }
    }
    pub fn write_ctlr(&mut self, value: u64) {
        self.cpus[self.cur].eoimode = value & ICC_CTLR_EOIMODE != 0;
    }

    pub fn read_igrpen1(&self) -> u64 {
        u64::from(self.cpus[self.cur].igrpen1)
    }
    pub fn write_igrpen1(&mut self, value: u64) {
        self.cpus[self.cur].igrpen1 = value & 1 != 0;
    }

    /// ICC_SRE_EL1: SRE, DFB and DIB fixed at 1 (register interface only).
    pub fn read_sre(&self) -> u64 {
        0x7
    }
    pub fn write_sre(&mut self, _value: u64) {}

    /// ICC_AP1R0_EL1: one bit per active group priority (bit = prio >> 3).
    pub fn read_ap1r0(&self) -> u64 {
        self.cpus[self.cur].active_prio.iter().fold(0, |acc, &(_, p)| acc | 1 << (p >> 3))
    }
    /// Writing zero (as Linux does at boot) empties the active priorities;
    /// other values rebuild the stack without associated INTIDs.
    pub fn write_ap1r0(&mut self, value: u64) {
        let a = &mut self.cpus[self.cur].active_prio;
        a.clear();
        for bit in (0..32).rev() {
            if value & (1 << bit) != 0 {
                a.push((INTID_SPURIOUS, (bit << 3) as u8));
            }
        }
    }

    // ---- Bitmap registers shared by GICD and GICR ---------------------------

    /// Word `word` (32 INTIDs) of a bitmap register; word 0 is the private
    /// one of core `cpu`.
    fn read_bits(&self, cpu: usize, word: usize, op: BitOp) -> u32 {
        let mut v = 0;
        for bit in 0..32 {
            let Some(irq) = self.irq(cpu, word * 32 + bit) else { break };
            let set = match op {
                BitOp::Group => irq.group1,
                BitOp::SetEnable | BitOp::ClearEnable => irq.enabled,
                BitOp::SetPending | BitOp::ClearPending => irq.is_pending(),
                BitOp::SetActive | BitOp::ClearActive => irq.active,
            };
            v |= u32::from(set) << bit;
        }
        v
    }

    fn write_bits(&mut self, cpu: usize, word: usize, op: BitOp, value: u32) {
        for bit in 0..32 {
            let Some(irq) = self.irq_mut(cpu, word * 32 + bit) else { break };
            let one = value & (1 << bit) != 0;
            match op {
                BitOp::Group => irq.group1 = one,
                _ if !one => {}
                BitOp::SetEnable => irq.enabled = true,
                BitOp::ClearEnable => irq.enabled = false,
                BitOp::SetPending => irq.pending = true,
                BitOp::ClearPending => irq.pending = false,
                BitOp::SetActive => irq.active = true,
                BitOp::ClearActive => irq.active = false,
            }
        }
    }

    fn bit_op(reg: u64) -> Option<BitOp> {
        Some(match reg & !0x7F {
            GICD_IGROUPR => BitOp::Group,
            GICD_ISENABLER => BitOp::SetEnable,
            GICD_ICENABLER => BitOp::ClearEnable,
            GICD_ISPENDR => BitOp::SetPending,
            GICD_ICPENDR => BitOp::ClearPending,
            GICD_ISACTIVER => BitOp::SetActive,
            GICD_ICACTIVER => BitOp::ClearActive,
            _ => return None,
        })
    }

    fn read_prio(&self, cpu: usize, first: usize, size: u8) -> u64 {
        (0..usize::from(size))
            .map(|i| self.irq(cpu, first + i).map_or(0, |irq| u64::from(irq.priority)))
            .enumerate()
            .fold(0, |acc, (i, p)| acc | p << (8 * i))
    }

    fn write_prio(&mut self, cpu: usize, first: usize, size: u8, value: u64) {
        for i in 0..usize::from(size) {
            if let Some(irq) = self.irq_mut(cpu, first + i) {
                irq.priority = (value >> (8 * i)) as u8;
            }
        }
    }

    /// ICFGRn: two bits per interrupt, the high bit means "edge-triggered".
    fn read_cfg(&self, cpu: usize, word: usize) -> u32 {
        (0..16)
            .map_while(|i| self.irq(cpu, word * 16 + i))
            .enumerate()
            .fold(0, |acc, (i, irq)| acc | u32::from(irq.edge) << (2 * i + 1))
    }

    fn write_cfg(&mut self, cpu: usize, word: usize, value: u32) {
        for i in 0..16 {
            let Some(irq) = self.irq_mut(cpu, word * 16 + i) else { break };
            irq.edge = value & (1 << (2 * i + 1)) != 0;
        }
    }

    // ---- Distributor -------------------------------------------------------

    pub fn dist_read(&mut self, offset: u64, size: u8) -> u64 {
        let words = NUM_INTIDS as u64 / 32;
        match (offset, size) {
            (GICD_IPRIORITYR..0x0800, 1 | 2 | 4) if offset >= GICD_IPRIORITYR + 32 => {
                self.read_prio(0, (offset - GICD_IPRIORITYR) as usize, size)
            }
            (GICD_IROUTER..0x8000, 4 | 8) if offset >= GICD_IROUTER + 32 * 8 => {
                let n = ((offset - GICD_IROUTER) / 8) as usize;
                let r = self.irqs.get(n).map_or(0, |i| i.router);
                if size == 8 { r } else { r >> ((offset & 4) * 8) & 0xFFFF_FFFF }
            }
            (_, 4) if offset & 3 == 0 => u64::from(match offset {
                GICD_CTLR => self.gicd_ctlr | GICD_CTLR_ARE | GICD_CTLR_DS,
                // ITLinesNumber, CPUNumber (cores - 1, at most 7), IDbits.
                GICD_TYPER => (words as u32 - 1) | ((self.cpus.len() as u32 - 1).min(7) << 5) | (9 << 19),
                GICD_IIDR => IIDR,
                GICD_TYPER2 => 0,
                0x0080..0x0400 => {
                    let n = (offset & 0x7F) / 4;
                    match Self::bit_op(offset) {
                        Some(op) if n >= 1 && n < words => self.read_bits(0, n as usize, op),
                        _ => 0,
                    }
                }
                GICD_ICFGR..GICD_IGRPMODR => {
                    let n = (offset - GICD_ICFGR) / 4;
                    if n >= 2 && n < words * 2 { self.read_cfg(0, n as usize) } else { 0 }
                }
                0xFFD0..=0xFFFC => u32::from(GICD_IDS[((offset - 0xFFD0) / 4) as usize]),
                _ => 0,
            }),
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, offset: u64, size: u8, value: u64) {
        let words = NUM_INTIDS as u64 / 32;
        match (offset, size) {
            (GICD_IPRIORITYR..0x0800, 1 | 2 | 4) if offset >= GICD_IPRIORITYR + 32 => {
                self.write_prio(0, (offset - GICD_IPRIORITYR) as usize, size, value);
            }
            (GICD_IROUTER..0x8000, 4 | 8) if offset >= GICD_IROUTER + 32 * 8 => {
                let n = ((offset - GICD_IROUTER) / 8) as usize;
                if let Some(irq) = self.irqs.get_mut(n) {
                    let r = match (size, offset & 4) {
                        (8, _) => value,
                        (_, 0) => irq.router & !0xFFFF_FFFF | value,
                        _ => irq.router & 0xFFFF_FFFF | value << 32,
                    };
                    irq.router = r & 0xFF_80FF_FFFF;
                }
            }
            (_, 4) if offset & 3 == 0 => {
                let v = value as u32;
                match offset {
                    GICD_CTLR => self.gicd_ctlr = v & (GICD_CTLR_ENABLE_GRP0 | GICD_CTLR_ENABLE_GRP1),
                    0x0080..0x0400 => {
                        let n = (offset & 0x7F) / 4;
                        if let Some(op) = Self::bit_op(offset).filter(|_| n >= 1 && n < words) {
                            self.write_bits(0, n as usize, op, v);
                        }
                    }
                    GICD_ICFGR..GICD_IGRPMODR => {
                        let n = (offset - GICD_ICFGR) / 4;
                        if n >= 2 && n < words * 2 {
                            self.write_cfg(0, n as usize, v);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---- Redistributors ----------------------------------------------------

    /// Read at `offset` from the start of the redistributors: core
    /// `offset / GICR_SIZE_PER_CPU`, its RD frame then its SGI frame.
    pub fn redist_read(&mut self, offset: u64, size: u8) -> u64 {
        let cpu = (offset / map::GICR_SIZE_PER_CPU) as usize;
        let offset = offset % map::GICR_SIZE_PER_CPU;
        if cpu >= self.cpus.len() {
            return 0;
        }
        if offset >= GICR_SGI_BASE {
            let off = offset - GICR_SGI_BASE;
            return match (off, size) {
                (GICR_IPRIORITYR..0x0420, 1 | 2 | 4) => {
                    self.read_prio(cpu, (off - GICR_IPRIORITYR) as usize, size)
                }
                (_, 4) => u64::from(match off {
                    GICR_IGROUPR0 | GICR_ISENABLER0 | GICR_ICENABLER0 | GICR_ISPENDR0 | GICR_ICPENDR0
                    | GICR_ISACTIVER0 | GICR_ICACTIVER0 => self.read_bits(cpu, 0, Self::bit_op(off).unwrap()),
                    GICR_ICFGR0 => self.read_cfg(cpu, 0),
                    GICR_ICFGR1 => self.read_cfg(cpu, 1),
                    _ => 0,
                }),
                _ => 0,
            };
        }
        // TYPER: affinity in 63:32, Processor_Number in 23:8, Last in bit 4.
        let typer = cpu_affinity(cpu) << 32 | (cpu as u64) << 8 | u64::from(cpu + 1 == self.cpus.len()) << 4;
        match (offset, size) {
            (GICR_TYPER, 8) => typer,
            (_, 4) => u64::from(match offset {
                GICR_IIDR => IIDR,
                GICR_TYPER => typer as u32,
                o if o == GICR_TYPER + 4 => (typer >> 32) as u32,
                GICR_WAKER => {
                    if self.cpus[cpu].processor_sleep {
                        GICR_WAKER_PROCESSOR_SLEEP | GICR_WAKER_CHILDREN_ASLEEP
                    } else {
                        0
                    }
                }
                0xFFD0..=0xFFFC => u32::from(GICR_IDS[((offset - 0xFFD0) / 4) as usize]),
                _ => 0,
            }),
            _ => 0,
        }
    }

    pub fn redist_write(&mut self, offset: u64, size: u8, value: u64) {
        let cpu = (offset / map::GICR_SIZE_PER_CPU) as usize;
        let offset = offset % map::GICR_SIZE_PER_CPU;
        if cpu >= self.cpus.len() {
            return;
        }
        if offset >= GICR_SGI_BASE {
            let off = offset - GICR_SGI_BASE;
            match (off, size) {
                (GICR_IPRIORITYR..0x0420, 1 | 2 | 4) => {
                    self.write_prio(cpu, (off - GICR_IPRIORITYR) as usize, size, value);
                }
                (
                    GICR_IGROUPR0 | GICR_ISENABLER0 | GICR_ICENABLER0 | GICR_ISPENDR0 | GICR_ICPENDR0
                    | GICR_ISACTIVER0 | GICR_ICACTIVER0,
                    4,
                ) => {
                    self.write_bits(cpu, 0, Self::bit_op(off).unwrap(), value as u32);
                }
                // ICFGR0 (SGI) is read-only: always edge-triggered.
                (GICR_ICFGR1, 4) => self.write_cfg(cpu, 1, value as u32),
                _ => {}
            }
            return;
        }
        if (offset, size) == (GICR_WAKER, 4) {
            self.cpus[cpu].processor_sleep = value as u32 & GICR_WAKER_PROCESSOR_SLEEP != 0;
        }
    }
}

impl MmioDevice for Gic {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset < map::GICD_SIZE {
            self.dist_read(offset, size)
        } else if (GICR_OFFSET..Self::mmio_size(self.cpus.len())).contains(&offset) {
            self.redist_read(offset - GICR_OFFSET, size)
        } else {
            0
        }
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if offset < map::GICD_SIZE {
            self.dist_write(offset, size, value);
        } else if (GICR_OFFSET..Self::mmio_size(self.cpus.len())).contains(&offset) {
            self.redist_write(offset - GICR_OFFSET, size, value);
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

fn save_irq(w: &mut vetro_snapshot::Writer, i: &Irq) {
    let flags = u8::from(i.enabled)
        | u8::from(i.pending) << 1
        | u8::from(i.level) << 2
        | u8::from(i.active) << 3
        | u8::from(i.group1) << 4
        | u8::from(i.edge) << 5;
    w.u8(flags);
    w.u8(i.priority);
    w.u64(i.router);
}

fn restore_irq(r: &mut vetro_snapshot::Reader<'_>, i: &mut Irq) -> vetro_snapshot::Result<()> {
    let f = r.u8()?;
    if f >> 6 != 0 {
        return Err(vetro_snapshot::Error::invalid(format!("interrupt state {f:#x}")));
    }
    i.enabled = f & 1 != 0;
    i.pending = f & 2 != 0;
    i.level = f & 4 != 0;
    i.active = f & 8 != 0;
    i.group1 = f & 16 != 0;
    i.edge = f & 32 != 0;
    i.priority = r.u8()?;
    i.router = r.u64()?;
    Ok(())
}

fn save_cpuif(w: &mut vetro_snapshot::Writer, c: &CpuIf, with_sleep: bool) {
    if with_sleep {
        w.bool(c.processor_sleep);
    }
    w.u8(c.pmr);
    w.u8(c.bpr1);
    w.bool(c.igrpen1);
    w.bool(c.eoimode);
    w.seq(&c.active_prio, |w, &(intid, prio)| {
        w.u32(intid);
        w.u8(prio);
    });
}

fn restore_cpuif(
    r: &mut vetro_snapshot::Reader<'_>,
    c: &mut CpuIf,
    with_sleep: bool,
) -> vetro_snapshot::Result<()> {
    if with_sleep {
        c.processor_sleep = r.bool()?;
    }
    c.pmr = r.u8()?;
    c.bpr1 = r.u8()?;
    c.igrpen1 = r.bool()?;
    c.eoimode = r.bool()?;
    c.active_prio = r.seq(5, |r| Ok((r.u32()?, r.u8()?)))?;
    Ok(())
}

/// One core: the single-core layout (INTIDs, distributor, core 0's
/// redistributor and CPU interface). More cores: then, per core 1..n, its
/// SGIs/PPIs, wake state and CPU interface. The current core is not state
/// (the machine sets it).
impl vetro_snapshot::Snapshot for Gic {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.len_of(self.irqs.len());
        for i in &self.irqs {
            save_irq(w, i);
        }
        w.u32(self.gicd_ctlr);
        w.bool(self.cpus[0].processor_sleep);
        save_cpuif(w, &self.cpus[0], false);
        for (b, c) in self.banked.iter().zip(&self.cpus[1..]) {
            for i in b {
                save_irq(w, i);
            }
            save_cpuif(w, c, true);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("GIC INTIDs", self.irqs.len() as u64)?;
        for i in &mut self.irqs {
            restore_irq(r, i)?;
        }
        self.gicd_ctlr = r.u32()?;
        let (first, rest) = self.cpus.split_at_mut(1);
        first[0].processor_sleep = r.bool()?;
        restore_cpuif(r, &mut first[0], false)?;
        for (b, c) in self.banked.iter_mut().zip(rest) {
            for i in b.iter_mut() {
                restore_irq(r, i)?;
            }
            restore_cpuif(r, c, true)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UART: u32 = 33;

    /// GIC initialised the way Linux does it: group 1 everywhere, distributor and
    /// CPU interface enabled, PMR open.
    fn linux_like() -> Gic {
        let mut g = Gic::new();
        g.redist_write(GICR_WAKER, 4, 0);
        g.dist_write(GICD_CTLR, 4, u64::from(GICD_CTLR_ARE | GICD_CTLR_ENABLE_GRP1 | GICD_CTLR_ENABLE_GRP0));
        for n in 1..(NUM_INTIDS as u64 / 32) {
            g.dist_write(GICD_IGROUPR + 4 * n, 4, 0xFFFF_FFFF);
        }
        g.redist_write(GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
        g.write_pmr(0xF0);
        g.write_igrpen1(1);
        g
    }

    fn enable_spi(g: &mut Gic, intid: u32, prio: u8) {
        let n = u64::from(intid / 32);
        g.dist_write(GICD_ISENABLER + 4 * n, 4, 1 << (intid % 32));
        g.dist_write(GICD_IPRIORITYR + u64::from(intid), 1, u64::from(prio));
    }

    #[test]
    fn identificazione_e_typer() {
        let mut g = Gic::new();
        assert_eq!(g.dist_read(GICD_PIDR2, 4) >> 4 & 0xF, 3, "ArchRev GICv3");
        assert_eq!(g.redist_read(GICR_PIDR2, 4) >> 4 & 0xF, 3);
        let typer = g.dist_read(GICD_TYPER, 4);
        assert_eq!(typer & 0x1F, 8);
        assert_eq!(typer >> 19 & 0x1F, 9);
        assert_eq!(g.dist_read(GICD_IIDR, 4), 0x43B);
        assert_eq!(g.redist_read(GICR_TYPER, 8), 0x10, "Last = 1, affinity 0");
        assert_eq!(g.redist_read(GICR_TYPER + 4, 4), 0);
    }

    #[test]
    fn ctlr_con_are_e_ds_fissi() {
        let mut g = Gic::new();
        assert_eq!(g.dist_read(GICD_CTLR, 4), u64::from(GICD_CTLR_ARE | GICD_CTLR_DS));
        g.dist_write(GICD_CTLR, 4, 0);
        assert_eq!(g.dist_read(GICD_CTLR, 4) as u32 & GICD_CTLR_ARE, GICD_CTLR_ARE);
        g.dist_write(GICD_CTLR, 4, 0xFFFF_FFFF);
        assert_eq!(g.dist_read(GICD_CTLR, 4), 0x53);
    }

    #[test]
    fn handshake_di_waker() {
        let mut g = Gic::new();
        assert_eq!(g.redist_read(GICR_WAKER, 4), 0x6);
        g.redist_write(GICR_WAKER, 4, 0);
        assert_eq!(g.redist_read(GICR_WAKER, 4), 0);
        g.redist_write(GICR_WAKER, 4, 2);
        assert_eq!(g.redist_read(GICR_WAKER, 4), 0x6);
    }

    #[test]
    fn spi_a_livello_ciclo_completo() {
        let mut g = linux_like();
        enable_spi(&mut g, UART, 0xA0);
        assert!(!g.irq_line());
        g.set_spi_level(1, true);
        assert!(g.irq_line());
        assert_eq!(g.read_hppir1(), 33);
        assert_eq!(g.read_iar1(), 33);
        assert_eq!(g.read_rpr(), 0xA0);
        assert_eq!(g.irq_state(UART), Some((true, true, true)), "active and pending: line high");
        assert!(!g.irq_line(), "an active interrupt does not come back");
        assert_eq!(g.read_iar1(), u64::from(INTID_SPURIOUS));
        g.write_eoir1(33);
        assert_eq!(g.read_rpr(), 0xFF);
        assert!(g.irq_line(), "the line is still high");
        g.set_spi_level(1, false);
        assert!(!g.irq_line());
        assert_eq!(g.dist_read(GICD_ISPENDR + 4, 4), 0);
    }

    #[test]
    fn spi_a_fronte() {
        let mut g = linux_like();
        enable_spi(&mut g, 48, 0x80);
        g.dist_write(GICD_ICFGR + 4 * 3, 4, 0b10); // INTID 48 edge-triggered
        assert_eq!(g.dist_read(GICD_ICFGR + 4 * 3, 4), 0b10);
        g.set_irq_level(48, true);
        g.set_irq_level(48, false);
        assert_eq!(g.read_iar1(), 48);
        g.write_eoir1(48);
        assert!(!g.irq_line(), "the edge has been consumed");
        g.set_irq_level(48, true);
        assert!(g.irq_line());
    }

    #[test]
    fn ispendr_e_icpendr() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x10);
        g.dist_write(GICD_ISPENDR + 4, 4, 1 << 8);
        assert_eq!(g.dist_read(GICD_ISPENDR + 4, 4), 1 << 8);
        g.dist_write(GICD_ICPENDR + 4, 4, 1 << 8);
        assert!(!g.irq_line());
        g.dist_write(GICD_ISACTIVER + 4, 4, 1 << 8);
        assert_eq!(g.dist_read(GICD_ICACTIVER + 4, 4), 1 << 8);
        g.dist_write(GICD_ICACTIVER + 4, 4, 1 << 8);
        assert_eq!(g.dist_read(GICD_ISACTIVER + 4, 4), 0);
        g.dist_write(GICD_ICENABLER + 4, 4, 1 << 8);
        assert_eq!(g.dist_read(GICD_ISENABLER + 4, 4), 0);
    }

    #[test]
    fn priorita_parita_e_pmr() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x80);
        enable_spi(&mut g, 41, 0x40);
        enable_spi(&mut g, 42, 0x40);
        for i in [40, 41, 42] {
            g.set_irq_level(i, true);
        }
        assert_eq!(g.read_hppir1(), 41, "priority 0x40, on a tie the lowest INTID");
        g.write_pmr(0x40);
        assert!(!g.irq_line(), "priority strictly lower than PMR is needed");
        assert_eq!(g.read_iar1(), u64::from(INTID_SPURIOUS));
        g.write_pmr(0x48);
        assert_eq!(g.read_pmr(), 0x48);
        assert_eq!(g.read_iar1(), 41);
    }

    #[test]
    fn prelazione_e_eoi_annidati() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x80);
        enable_spi(&mut g, 41, 0x40);
        g.set_irq_level(40, true);
        assert_eq!(g.read_iar1(), 40);
        enable_spi(&mut g, 42, 0x80);
        g.set_irq_level(42, true);
        assert!(!g.irq_line(), "same group priority: no preemption");
        g.set_irq_level(41, true);
        assert_eq!(g.read_iar1(), 41, "higher priority: preempts");
        assert_eq!(g.read_rpr(), 0x40);
        assert_eq!(g.read_ap1r0(), (1 << (0x40 >> 3)) | (1 << (0x80 >> 3)));
        g.set_irq_level(41, false);
        g.write_eoir1(41);
        assert_eq!(g.read_rpr(), 0x80);
        g.set_irq_level(40, false);
        g.write_eoir1(40);
        assert_eq!(g.read_iar1(), 42);
    }

    #[test]
    fn eoimode_separa_la_disattivazione() {
        let mut g = linux_like();
        g.write_ctlr(ICC_CTLR_EOIMODE);
        assert_eq!(g.read_ctlr() & ICC_CTLR_EOIMODE, ICC_CTLR_EOIMODE);
        enable_spi(&mut g, 40, 0x80);
        g.set_irq_level(40, true);
        assert_eq!(g.read_iar1(), 40);
        g.write_eoir1(40);
        assert_eq!(g.read_rpr(), 0xFF);
        assert_eq!(g.irq_state(40), Some((true, true, true)), "still active until DIR");
        g.write_dir(40);
        assert_eq!(g.irq_state(40), Some((true, true, false)));
    }

    #[test]
    fn ppi_del_timer_dal_redistributore() {
        let mut g = linux_like();
        g.redist_write(GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << 27);
        g.redist_write(GICR_SGI_BASE + GICR_IPRIORITYR + 27, 1, 0xA0);
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_IPRIORITYR + 24, 4), 0xA0 << 24);
        g.set_irq_level(27, true);
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_ISPENDR0, 4), 1 << 27);
        assert_eq!(g.read_iar1(), 27);
        g.write_eoir1(27);
        // In the distributor SGIs/PPIs are RAZ/WI with ARE = 1.
        assert_eq!(g.dist_read(GICD_ISENABLER, 4), 0);
        g.dist_write(GICD_ICENABLER, 4, 1 << 27);
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_ISENABLER0, 4), 1 << 27);
    }

    #[test]
    fn sgi_da_sgi1r() {
        let mut g = linux_like();
        g.redist_write(GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << 5);
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_ICFGR0, 4), 0xAAAA_AAAA);
        g.redist_write(GICR_SGI_BASE + GICR_ICFGR0, 4, 0);
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_ICFGR0, 4), 0xAAAA_AAAA, "ICFGR0 read-only");
        g.write_sgi1r(5 << 24 | 1 << 40 | 1);
        assert!(!g.irq_line(), "IRM: all CPUs but this one");
        g.write_sgi1r(5 << 24 | 1 << 16 | 1);
        assert!(!g.irq_line(), "Aff1 = 1: no CPU");
        g.write_sgi1r(5 << 24 | 1);
        assert_eq!(g.read_iar1(), 5);
    }

    #[test]
    fn gruppo_0_e_distributore_spento_non_consegnano() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x80);
        g.set_irq_level(40, true);
        g.dist_write(GICD_IGROUPR + 4, 4, 0);
        assert!(!g.irq_line(), "group 0 not supported");
        g.dist_write(GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
        assert!(g.irq_line());
        g.dist_write(GICD_CTLR, 4, 0);
        assert!(!g.irq_line());
        g.dist_write(GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
        g.write_igrpen1(0);
        assert!(!g.irq_line());
        assert_eq!(g.read_hppir1(), 40, "HPPIR ignores IGRPEN1 and PMR");
    }

    #[test]
    fn irouter_instrada_verso_questa_cpu() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x80);
        g.set_irq_level(40, true);
        let r = GICD_IROUTER + 8 * 40;
        g.dist_write(r, 8, 0x0000_0001_0000_0100);
        assert_eq!(g.dist_read(r, 8), 0x0000_0001_0000_0100);
        assert_eq!(g.dist_read(r + 4, 4), 1);
        assert!(!g.irq_line(), "affinity of another CPU");
        g.dist_write(r, 8, 1 << 31);
        assert!(g.irq_line(), "IRM = 1");
        g.dist_write(r, 4, 0);
        assert!(g.irq_line());
        assert_eq!(g.dist_read(GICD_IROUTER, 8), 0, "SGI/PPI IROUTER reserved");
    }

    #[test]
    fn registri_icc() {
        let mut g = Gic::new();
        assert_eq!(g.read_sre(), 7);
        g.write_sre(0);
        assert_eq!(g.read_sre(), 7);
        assert_eq!(g.read_ctlr() >> 8 & 7, 4, "PRIbits = 5 bit - 1");
        g.write_bpr1(0);
        assert_eq!(g.read_bpr1(), 3);
        g.write_bpr1(5);
        assert_eq!(g.read_bpr1(), 5);
        g.write_pmr(0xFF);
        assert_eq!(g.read_pmr(), 0xF8);
        assert_eq!(g.read_rpr(), 0xFF);
        assert_eq!(g.read_igrpen1(), 0);
        g.write_ap1r0(0);
        assert_eq!(g.read_ap1r0(), 0);
    }

    #[test]
    fn bpr1_raggruppa_le_priorita() {
        let mut g = linux_like();
        g.write_bpr1(5); // group priority = bits [7:5]
        enable_spi(&mut g, 40, 0x50);
        enable_spi(&mut g, 41, 0x48);
        g.set_irq_level(40, true);
        assert_eq!(g.read_iar1(), 40);
        assert_eq!(g.read_rpr(), 0x40, "RPR reports the group priority");
        g.set_irq_level(41, true);
        assert!(!g.irq_line(), "0x48 and 0x50 are in the same group 0x40");
        g.write_bpr1(3);
        g.write_eoir1(40);
        g.set_irq_level(40, false);
        assert_eq!(g.read_iar1(), 41);
    }

    #[test]
    fn accesso_mmio_dalla_regione_unica() {
        let mut g = Gic::new();
        let gicr = map::GICR_BASE - map::GICD_BASE;
        assert_eq!(MmioDevice::read(&mut g, GICD_PIDR2, 4), 0x3B);
        assert_eq!(MmioDevice::read(&mut g, gicr + GICR_PIDR2, 4), 0x3B);
        assert_eq!(MmioDevice::read(&mut g, 0x5_0000, 4), 0, "RAZ hole");
        MmioDevice::write(&mut g, gicr + GICR_WAKER, 4, 0);
        assert_eq!(MmioDevice::read(&mut g, gicr + GICR_WAKER, 4), 0);
        MmioDevice::write(&mut g, GICD_IPRIORITYR + 32, 4, 0x4433_2211);
        assert_eq!(MmioDevice::read(&mut g, GICD_IPRIORITYR + 34, 1), 0x33);
        assert_eq!(MmioDevice::read(&mut g, GICD_IPRIORITYR, 4), 0, "SGI priority RAZ in the distributor");
    }

    /// Two cores (ADR 0042): a redistributor each (TYPER with affinity,
    /// processor number and Last), banked SGIs and PPIs, a CPU interface each,
    /// SPIs routed by IROUTER, SGIs by ICC_SGI1R_EL1 target list or IRM.
    #[test]
    fn two_cores() {
        let mut g = Gic::with_cpus(2);
        let f = map::GICR_SIZE_PER_CPU;
        assert_eq!(g.dist_read(GICD_TYPER, 4) >> 5 & 7, 1, "CPUNumber");
        assert_eq!(g.redist_read(GICR_TYPER, 8), 0, "core 0: affinity 0, not the last");
        assert_eq!(
            g.redist_read(f + GICR_TYPER, 8),
            1 << 32 | 1 << 8 | 1 << 4,
            "core 1: affinity 1, number 1, Last"
        );
        assert_eq!(g.redist_read(2 * f + GICR_TYPER, 8), 0, "no third redistributor");
        assert_eq!(Gic::mmio_size(2), MMIO_SIZE + f);
        g.dist_write(GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
        for c in 0..2u64 {
            let base = c * f;
            g.redist_write(base + GICR_WAKER, 4, 0);
            g.redist_write(base + GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
            g.redist_write(base + GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << 2 | 1 << map::PPI_VTIMER);
            g.set_current(c as usize);
            g.write_pmr(0xF0);
            g.write_igrpen1(1);
        }
        assert_eq!(g.redist_read(f + GICR_WAKER, 4), 0, "core 1 awake");
        // A PPI of core 1 only.
        g.set_private_level(1, map::PPI_VTIMER, true);
        assert!(!g.irq_line_of(0) && g.irq_line_of(1));
        g.set_private_level(1, map::PPI_VTIMER, false);
        // SGI 2 from core 0 to core 1 (TargetList bit 1).
        g.set_current(0);
        g.write_sgi1r(2 << 24 | 0b10);
        assert!(!g.irq_line_of(0) && g.irq_line_of(1));
        g.set_current(1);
        assert_eq!(g.read_iar1(), 2, "core 1 acknowledges its SGI");
        g.write_eoir1(2);
        // IRM: every core but the sender.
        g.write_sgi1r(2 << 24 | 1 << 40);
        assert!(g.irq_line_of(0) && !g.irq_line_of(1));
        g.set_current(0);
        assert_eq!(g.read_iar1(), 2);
        g.write_eoir1(2);
        // An SPI goes where IROUTER says.
        g.dist_write(GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
        enable_spi(&mut g, 40, 0x80);
        g.dist_write(GICD_IROUTER + 8 * 40, 8, 1);
        g.set_irq_level(40, true);
        assert!(!g.irq_line_of(0) && g.irq_line_of(1));
        g.dist_write(GICD_IROUTER + 8 * 40, 8, 0);
        assert!(g.irq_line_of(0) && !g.irq_line_of(1));
        // The snapshot keeps both cores, and one core keeps its old layout.
        let mut w = vetro_snapshot::Writer::new();
        vetro_snapshot::Snapshot::save(&g, &mut w);
        let mut h = Gic::with_cpus(2);
        vetro_snapshot::Snapshot::restore(&mut h, &mut vetro_snapshot::Reader::new(w.as_bytes())).unwrap();
        let mut w2 = vetro_snapshot::Writer::new();
        vetro_snapshot::Snapshot::save(&h, &mut w2);
        assert_eq!(w.as_bytes(), w2.as_bytes());
        let mut one = vetro_snapshot::Writer::new();
        vetro_snapshot::Snapshot::save(&Gic::new(), &mut one);
        assert_eq!(one.len(), 8 + NUM_INTIDS * 10 + 4 + 1 + 4 + 8, "single-core layout");
    }
}
