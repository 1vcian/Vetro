//! GICv3 per una CPU: distributore, redistributore e interfaccia CPU a
//! registri di sistema (ARM IHI0069).
//!
//! Perimetro e scelte (vedi anche `docs/specs/platform.md`):
//! - **Un solo stato di sicurezza** (GICD_CTLR.DS = 1, RAO/WI) e affinity
//!   routing sempre attivo (ARE = 1, RAO/WI), come vede Linux sotto QEMU
//!   senza EL3.
//! - **Solo gruppo 1 non sicuro**: IGROUPR si memorizza, ma gli interrupt
//!   lasciati in gruppo 0 non vengono mai segnalati (sarebbero FIQ).
//!   Linux mette tutto in gruppo 1. IGRPMODR e NSACR sono RAZ/WI.
//! - Niente LPI/ITS: GICR_CTLR.EnableLPIs, PROPBASER e PENDBASER RAZ/WI.
//! - 256 SPI (INTID 32..287), GICD_TYPER.ITLinesNumber = 8, IDbits = 9.
//! - Interfaccia CPU con 5 bit di priorità (PRIbits = 4, come QEMU):
//!   PMR tiene i 5 bit alti, BPR1 minimo 3. ICC_CTLR_EL1.CBPR è RAZ/WI
//!   (BPR0 non è modellato), EOImode è scrivibile.
//! - GICR_WAKER fa l'handshake (ChildrenAsleep segue ProcessorSleep) ma non
//!   blocca la consegna.
//!
//! Il GIC si mappa sul bus come un'unica regione che parte da GICD_BASE e
//! copre il redistributore (vedi [`MMIO_SIZE`]); lo spazio in mezzo è
//! RAZ/WI.

use crate::bus::MmioDevice;
use crate::map;

/// Numero di SPI implementati.
pub const NUM_SPIS: usize = 256;
/// Numero totale di INTID (SGI + PPI + SPI).
pub const NUM_INTIDS: usize = 32 + NUM_SPIS;
/// INTID restituito da IAR quando non c'è niente da consegnare.
pub const INTID_SPURIOUS: u32 = 1023;
/// Bit di priorità implementati nell'interfaccia CPU.
pub const PRI_BITS: u32 = 5;
/// Maschera dei bit di priorità implementati.
pub const PRI_MASK: u8 = (0xFF00u16 >> PRI_BITS) as u8;
/// Valore minimo di ICC_BPR1_EL1 (8 - PRI_BITS).
pub const MIN_BPR1: u8 = (8 - PRI_BITS) as u8;

/// Dimensione della regione MMIO del GIC sul bus (GICD + buco + un GICR).
pub const MMIO_SIZE: u64 = map::GICR_BASE + map::GICR_SIZE_PER_CPU - map::GICD_BASE;
/// Offset del redistributore dentro la regione.
const GICR_OFFSET: u64 = map::GICR_BASE - map::GICD_BASE;

// Distributore.
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

// Redistributore, frame RD_base.
pub const GICR_CTLR: u64 = 0x0000;
pub const GICR_IIDR: u64 = 0x0004;
pub const GICR_TYPER: u64 = 0x0008;
pub const GICR_WAKER: u64 = 0x0014;
pub const GICR_PIDR2: u64 = 0xFFE8;
pub const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
pub const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
// Redistributore, frame SGI_base (offset relativi al frame).
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

/// Implementatore ARM (JEP106 0x43B), come QEMU.
const IIDR: u32 = 0x43B;
/// PIDR4-7, PIDR0-3, CIDR0-3 a partire da 0xFFD0 (valori di QEMU; PIDR2
/// dice ArchRev = 3, cioè GICv3, e Linux lo controlla).
const GICD_IDS: [u8; 12] = [0x44, 0x00, 0x00, 0x00, 0x92, 0xB4, 0x3B, 0x00, 0x0D, 0xF0, 0x05, 0xB1];
const GICR_IDS: [u8; 12] = [0x44, 0x00, 0x00, 0x00, 0x93, 0xB4, 0x3B, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

/// ICC_CTLR_EL1: bit scrivibili e campi fissi.
pub const ICC_CTLR_EOIMODE: u64 = 1 << 1;
const ICC_CTLR_PRIBITS: u64 = ((PRI_BITS - 1) as u64) << 8;
const ICC_CTLR_A3V: u64 = 1 << 15;

#[derive(Clone, Copy, Debug, Default)]
struct Irq {
    enabled: bool,
    /// Latch di pending (fronte, ISPENDR); per i livelli si somma alla linea.
    pending: bool,
    /// Livello della linea in ingresso.
    level: bool,
    active: bool,
    group1: bool,
    /// true = fronte di salita, false = livello alto.
    edge: bool,
    priority: u8,
    /// GICD_IROUTER (solo SPI).
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

#[derive(Clone, Debug)]
pub struct Gic {
    irqs: Vec<Irq>,
    /// Solo EnableGrp0/EnableGrp1; ARE e DS si aggiungono in lettura.
    gicd_ctlr: u32,
    processor_sleep: bool,
    // Interfaccia CPU.
    pmr: u8,
    bpr1: u8,
    igrpen1: bool,
    eoimode: bool,
    /// Priorità attive (INTID, priorità di gruppo), la più alta in cima.
    active_prio: Vec<(u32, u8)>,
}

impl Default for Gic {
    fn default() -> Self {
        Self::new()
    }
}

impl Gic {
    pub fn new() -> Self {
        let mut irqs = vec![Irq::default(); NUM_INTIDS];
        for irq in &mut irqs[..16] {
            irq.edge = true; // gli SGI sono sempre a fronte
        }
        Self {
            irqs,
            gicd_ctlr: 0,
            processor_sleep: true,
            pmr: 0,
            bpr1: MIN_BPR1,
            igrpen1: false,
            eoimode: false,
            active_prio: Vec::new(),
        }
    }

    // ---- Linee in ingresso -------------------------------------------------

    /// Livello della linea di un PPI (16..31) o SPI (32..). Su un interrupt
    /// configurato a fronte, la salita imposta il pending.
    pub fn set_irq_level(&mut self, intid: u32, level: bool) {
        let Some(irq) = self.irqs.get_mut(intid as usize).filter(|_| intid >= 16) else {
            return;
        };
        if irq.edge && level && !irq.level {
            irq.pending = true;
        }
        irq.level = level;
    }

    /// Livello della linea dello SPI `spi` (INTID `32 + spi`).
    pub fn set_spi_level(&mut self, spi: u32, level: bool) {
        self.set_irq_level(map::SPI_BASE + spi, level);
    }

    /// Rende pendente un SGI (0..15) sulla CPU 0.
    pub fn send_sgi(&mut self, intid: u32) {
        if intid < 16 {
            self.irqs[intid as usize].pending = true;
        }
    }

    /// Stato di un interrupt: (abilitato, pendente, attivo).
    pub fn irq_state(&self, intid: u32) -> Option<(bool, bool, bool)> {
        self.irqs.get(intid as usize).map(|i| (i.enabled, i.is_pending(), i.active))
    }

    // ---- Selezione ---------------------------------------------------------

    fn routed_here(intid: usize, irq: &Irq) -> bool {
        // IRM = 1 (qualunque CPU) oppure affinità 0.0.0.0.
        intid < 32 || irq.router & (1 << 31) != 0 || irq.router & 0xFF_00FF_FFFF == 0
    }

    /// Interrupt pendente di gruppo 1 a priorità più alta (valore più basso;
    /// a parità vince l'INTID più basso), senza guardare PMR né la priorità
    /// in esecuzione.
    pub fn highest_pending(&self) -> Option<(u32, u8)> {
        if self.gicd_ctlr & GICD_CTLR_ENABLE_GRP1 == 0 {
            return None;
        }
        let mut best: Option<(u32, u8)> = None;
        for (i, irq) in self.irqs.iter().enumerate() {
            if irq.enabled
                && irq.is_pending()
                && !irq.active
                && irq.group1
                && Self::routed_here(i, irq)
                && best.is_none_or(|(_, p)| irq.priority < p)
            {
                best = Some((i as u32, irq.priority));
            }
        }
        best
    }

    fn group_prio(&self, prio: u8) -> u8 {
        prio & (0xFFu8 << self.bpr1)
    }

    /// Priorità in esecuzione (0x100 = nessuna attiva, per i confronti).
    fn running(&self) -> u16 {
        self.active_prio.last().map_or(0x100, |&(_, p)| u16::from(p))
    }

    /// L'interrupt che l'interfaccia CPU segnalerebbe adesso.
    fn deliverable(&self) -> Option<(u32, u8)> {
        let (intid, prio) = self.highest_pending()?;
        (self.igrpen1 && prio < self.pmr && u16::from(self.group_prio(prio)) < self.running())
            .then_some((intid, prio))
    }

    /// Linea IRQ verso la CPU: la CPU prende l'eccezione se PSTATE.I è 0.
    pub fn irq_line(&self) -> bool {
        self.deliverable().is_some()
    }

    // ---- Interfaccia CPU (registri di sistema) -----------------------------

    /// MRS ICC_IAR1_EL1: riconosce l'interrupt segnalato (diventa attivo) e
    /// ne restituisce l'INTID, oppure 1023.
    pub fn read_iar1(&mut self) -> u64 {
        let Some((intid, prio)) = self.deliverable() else {
            return u64::from(INTID_SPURIOUS);
        };
        let gp = self.group_prio(prio);
        let irq = &mut self.irqs[intid as usize];
        irq.active = true;
        irq.pending = false;
        self.active_prio.push((intid, gp));
        u64::from(intid)
    }

    /// MRS ICC_HPPIR1_EL1: INTID pendente più prioritario, senza riconoscerlo.
    pub fn read_hppir1(&self) -> u64 {
        u64::from(self.highest_pending().map_or(INTID_SPURIOUS, |(i, _)| i))
    }

    /// MSR ICC_EOIR1_EL1: abbassa la priorità in esecuzione e, con
    /// EOImode = 0, disattiva l'interrupt.
    pub fn write_eoir1(&mut self, value: u64) {
        let intid = (value & 0xFF_FFFF) as u32;
        if (1020..1024).contains(&intid) {
            return;
        }
        self.active_prio.pop();
        if !self.eoimode {
            self.deactivate(intid);
        }
    }

    /// MSR ICC_DIR_EL1: disattivazione separata (usata con EOImode = 1).
    pub fn write_dir(&mut self, value: u64) {
        self.deactivate((value & 0xFF_FFFF) as u32);
    }

    fn deactivate(&mut self, intid: u32) {
        if let Some(irq) = self.irqs.get_mut(intid as usize) {
            irq.active = false;
        }
    }

    /// MSR ICC_SGI1R_EL1: con una sola CPU conta solo l'affinità 0.0.0 con
    /// il bit 0 della TargetList; IRM = 1 ("tutte tranne me") non colpisce
    /// nessuno.
    pub fn write_sgi1r(&mut self, value: u64) {
        let targets = value & 0xFFFF;
        let affs = value & 0x00FF_00FF_00FF_0000;
        let irm = value & (1 << 40) != 0;
        if !irm && affs == 0 && targets & 1 != 0 {
            self.send_sgi(((value >> 24) & 0xF) as u32);
        }
    }

    pub fn read_pmr(&self) -> u64 {
        u64::from(self.pmr)
    }
    pub fn write_pmr(&mut self, value: u64) {
        self.pmr = value as u8 & PRI_MASK;
    }

    pub fn read_bpr1(&self) -> u64 {
        u64::from(self.bpr1)
    }
    pub fn write_bpr1(&mut self, value: u64) {
        self.bpr1 = (value as u8 & 7).max(MIN_BPR1);
    }

    /// MRS ICC_RPR_EL1: priorità in esecuzione, 0xFF se nessuna.
    pub fn read_rpr(&self) -> u64 {
        u64::from(self.running().min(0xFF))
    }

    pub fn read_ctlr(&self) -> u64 {
        ICC_CTLR_A3V | ICC_CTLR_PRIBITS | if self.eoimode { ICC_CTLR_EOIMODE } else { 0 }
    }
    pub fn write_ctlr(&mut self, value: u64) {
        self.eoimode = value & ICC_CTLR_EOIMODE != 0;
    }

    pub fn read_igrpen1(&self) -> u64 {
        u64::from(self.igrpen1)
    }
    pub fn write_igrpen1(&mut self, value: u64) {
        self.igrpen1 = value & 1 != 0;
    }

    /// ICC_SRE_EL1: SRE, DFB e DIB fissi a 1 (solo interfaccia a registri).
    pub fn read_sre(&self) -> u64 {
        0x7
    }
    pub fn write_sre(&mut self, _value: u64) {}

    /// ICC_AP1R0_EL1: un bit per priorità di gruppo attiva (bit = prio >> 3).
    pub fn read_ap1r0(&self) -> u64 {
        self.active_prio.iter().fold(0, |acc, &(_, p)| acc | 1 << (p >> 3))
    }
    /// Scrivere zero (come fa Linux all'avvio) svuota le priorità attive;
    /// altri valori ricostruiscono la pila senza INTID associati.
    pub fn write_ap1r0(&mut self, value: u64) {
        self.active_prio.clear();
        for bit in (0..32).rev() {
            if value & (1 << bit) != 0 {
                self.active_prio.push((INTID_SPURIOUS, (bit << 3) as u8));
            }
        }
    }

    // ---- Registri a bitmap condivisi da GICD e GICR -------------------------

    fn read_bits(&self, word: usize, op: BitOp) -> u32 {
        let mut v = 0;
        for (bit, irq) in self.irqs.iter().skip(word * 32).take(32).enumerate() {
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

    fn write_bits(&mut self, word: usize, op: BitOp, value: u32) {
        for (bit, irq) in self.irqs.iter_mut().skip(word * 32).take(32).enumerate() {
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

    fn read_prio(&self, first: usize, size: u8) -> u64 {
        (0..usize::from(size))
            .map(|i| self.irqs.get(first + i).map_or(0, |irq| u64::from(irq.priority)))
            .enumerate()
            .fold(0, |acc, (i, p)| acc | p << (8 * i))
    }

    fn write_prio(&mut self, first: usize, size: u8, value: u64) {
        for i in 0..usize::from(size) {
            if let Some(irq) = self.irqs.get_mut(first + i) {
                irq.priority = (value >> (8 * i)) as u8;
            }
        }
    }

    /// ICFGRn: due bit per interrupt, il bit alto vale "a fronte".
    fn read_cfg(&self, word: usize) -> u32 {
        self.irqs
            .iter()
            .skip(word * 16)
            .take(16)
            .enumerate()
            .fold(0, |acc, (i, irq)| acc | u32::from(irq.edge) << (2 * i + 1))
    }

    fn write_cfg(&mut self, word: usize, value: u32) {
        for (i, irq) in self.irqs.iter_mut().skip(word * 16).take(16).enumerate() {
            irq.edge = value & (1 << (2 * i + 1)) != 0;
        }
    }

    // ---- Distributore ------------------------------------------------------

    pub fn dist_read(&mut self, offset: u64, size: u8) -> u64 {
        let words = NUM_INTIDS as u64 / 32;
        match (offset, size) {
            (GICD_IPRIORITYR..0x0800, 1 | 2 | 4) if offset >= GICD_IPRIORITYR + 32 => {
                self.read_prio((offset - GICD_IPRIORITYR) as usize, size)
            }
            (GICD_IROUTER..0x8000, 4 | 8) if offset >= GICD_IROUTER + 32 * 8 => {
                let n = ((offset - GICD_IROUTER) / 8) as usize;
                let r = self.irqs.get(n).map_or(0, |i| i.router);
                if size == 8 { r } else { r >> ((offset & 4) * 8) & 0xFFFF_FFFF }
            }
            (_, 4) if offset & 3 == 0 => u64::from(match offset {
                GICD_CTLR => self.gicd_ctlr | GICD_CTLR_ARE | GICD_CTLR_DS,
                GICD_TYPER => (words as u32 - 1) | (9 << 19),
                GICD_IIDR => IIDR,
                GICD_TYPER2 => 0,
                0x0080..0x0400 => {
                    let n = (offset & 0x7F) / 4;
                    match Self::bit_op(offset) {
                        Some(op) if n >= 1 && n < words => self.read_bits(n as usize, op),
                        _ => 0,
                    }
                }
                GICD_ICFGR..GICD_IGRPMODR => {
                    let n = (offset - GICD_ICFGR) / 4;
                    if n >= 2 && n < words * 2 { self.read_cfg(n as usize) } else { 0 }
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
                self.write_prio((offset - GICD_IPRIORITYR) as usize, size, value);
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
                            self.write_bits(n as usize, op, v);
                        }
                    }
                    GICD_ICFGR..GICD_IGRPMODR => {
                        let n = (offset - GICD_ICFGR) / 4;
                        if n >= 2 && n < words * 2 {
                            self.write_cfg(n as usize, v);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---- Redistributore ----------------------------------------------------

    pub fn redist_read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= GICR_SGI_BASE {
            let off = offset - GICR_SGI_BASE;
            return match (off, size) {
                (GICR_IPRIORITYR..0x0420, 1 | 2 | 4) => {
                    self.read_prio((off - GICR_IPRIORITYR) as usize, size)
                }
                (_, 4) => u64::from(match off {
                    GICR_IGROUPR0 | GICR_ISENABLER0 | GICR_ICENABLER0 | GICR_ISPENDR0 | GICR_ICPENDR0
                    | GICR_ISACTIVER0 | GICR_ICACTIVER0 => self.read_bits(0, Self::bit_op(off).unwrap()),
                    GICR_ICFGR0 => self.read_cfg(0),
                    GICR_ICFGR1 => self.read_cfg(1),
                    _ => 0,
                }),
                _ => 0,
            };
        }
        match (offset, size) {
            // TYPER: Last = 1, Processor_Number = 0, affinità 0.0.0.0.
            (GICR_TYPER, 8) => 1 << 4,
            (_, 4) => u64::from(match offset {
                GICR_IIDR => IIDR,
                GICR_TYPER => 1 << 4,
                GICR_WAKER => {
                    if self.processor_sleep {
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
        if offset >= GICR_SGI_BASE {
            let off = offset - GICR_SGI_BASE;
            match (off, size) {
                (GICR_IPRIORITYR..0x0420, 1 | 2 | 4) => {
                    self.write_prio((off - GICR_IPRIORITYR) as usize, size, value);
                }
                (
                    GICR_IGROUPR0 | GICR_ISENABLER0 | GICR_ICENABLER0 | GICR_ISPENDR0 | GICR_ICPENDR0
                    | GICR_ISACTIVER0 | GICR_ICACTIVER0,
                    4,
                ) => {
                    self.write_bits(0, Self::bit_op(off).unwrap(), value as u32);
                }
                // ICFGR0 (SGI) è in sola lettura: sempre a fronte.
                (GICR_ICFGR1, 4) => self.write_cfg(1, value as u32),
                _ => {}
            }
            return;
        }
        if (offset, size) == (GICR_WAKER, 4) {
            self.processor_sleep = value as u32 & GICR_WAKER_PROCESSOR_SLEEP != 0;
        }
    }
}

impl MmioDevice for Gic {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset < map::GICD_SIZE {
            self.dist_read(offset, size)
        } else if (GICR_OFFSET..MMIO_SIZE).contains(&offset) {
            self.redist_read(offset - GICR_OFFSET, size)
        } else {
            0
        }
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if offset < map::GICD_SIZE {
            self.dist_write(offset, size, value);
        } else if (GICR_OFFSET..MMIO_SIZE).contains(&offset) {
            self.redist_write(offset - GICR_OFFSET, size, value);
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

impl vetro_snapshot::Snapshot for Gic {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.len_of(self.irqs.len());
        for i in &self.irqs {
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
        w.u32(self.gicd_ctlr);
        w.bool(self.processor_sleep);
        w.u8(self.pmr);
        w.u8(self.bpr1);
        w.bool(self.igrpen1);
        w.bool(self.eoimode);
        w.seq(&self.active_prio, |w, &(intid, prio)| {
            w.u32(intid);
            w.u8(prio);
        });
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("INTID del GIC", self.irqs.len() as u64)?;
        for i in &mut self.irqs {
            let f = r.u8()?;
            if f >> 6 != 0 {
                return Err(vetro_snapshot::Error::invalid(format!("stato di un interrupt {f:#x}")));
            }
            i.enabled = f & 1 != 0;
            i.pending = f & 2 != 0;
            i.level = f & 4 != 0;
            i.active = f & 8 != 0;
            i.group1 = f & 16 != 0;
            i.edge = f & 32 != 0;
            i.priority = r.u8()?;
            i.router = r.u64()?;
        }
        self.gicd_ctlr = r.u32()?;
        self.processor_sleep = r.bool()?;
        self.pmr = r.u8()?;
        self.bpr1 = r.u8()?;
        self.igrpen1 = r.bool()?;
        self.eoimode = r.bool()?;
        self.active_prio = r.seq(5, |r| Ok((r.u32()?, r.u8()?)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UART: u32 = 33;

    /// GIC inizializzato come fa Linux: gruppo 1 ovunque, distributore e
    /// interfaccia CPU accesi, PMR aperto.
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
        assert_eq!(g.redist_read(GICR_TYPER, 8), 0x10, "Last = 1, affinità 0");
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
        assert_eq!(g.irq_state(UART), Some((true, true, true)), "attivo e pendente: linea alta");
        assert!(!g.irq_line(), "un interrupt attivo non si ripresenta");
        assert_eq!(g.read_iar1(), u64::from(INTID_SPURIOUS));
        g.write_eoir1(33);
        assert_eq!(g.read_rpr(), 0xFF);
        assert!(g.irq_line(), "la linea è ancora alta");
        g.set_spi_level(1, false);
        assert!(!g.irq_line());
        assert_eq!(g.dist_read(GICD_ISPENDR + 4, 4), 0);
    }

    #[test]
    fn spi_a_fronte() {
        let mut g = linux_like();
        enable_spi(&mut g, 48, 0x80);
        g.dist_write(GICD_ICFGR + 4 * 3, 4, 0b10); // INTID 48 a fronte
        assert_eq!(g.dist_read(GICD_ICFGR + 4 * 3, 4), 0b10);
        g.set_irq_level(48, true);
        g.set_irq_level(48, false);
        assert_eq!(g.read_iar1(), 48);
        g.write_eoir1(48);
        assert!(!g.irq_line(), "il fronte è stato consumato");
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
        assert_eq!(g.read_hppir1(), 41, "priorità 0x40, a parità l'INTID più basso");
        g.write_pmr(0x40);
        assert!(!g.irq_line(), "serve priorità strettamente minore di PMR");
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
        assert!(!g.irq_line(), "stessa priorità di gruppo: niente prelazione");
        g.set_irq_level(41, true);
        assert_eq!(g.read_iar1(), 41, "priorità più alta: prelaziona");
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
        assert_eq!(g.irq_state(40), Some((true, true, true)), "ancora attivo fino a DIR");
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
        // Nel distributore gli SGI/PPI sono RAZ/WI con ARE = 1.
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
        assert_eq!(g.redist_read(GICR_SGI_BASE + GICR_ICFGR0, 4), 0xAAAA_AAAA, "ICFGR0 in sola lettura");
        g.write_sgi1r(5 << 24 | 1 << 40 | 1);
        assert!(!g.irq_line(), "IRM: tutte le CPU tranne questa");
        g.write_sgi1r(5 << 24 | 1 << 16 | 1);
        assert!(!g.irq_line(), "Aff1 = 1: nessuna CPU");
        g.write_sgi1r(5 << 24 | 1);
        assert_eq!(g.read_iar1(), 5);
    }

    #[test]
    fn gruppo_0_e_distributore_spento_non_consegnano() {
        let mut g = linux_like();
        enable_spi(&mut g, 40, 0x80);
        g.set_irq_level(40, true);
        g.dist_write(GICD_IGROUPR + 4, 4, 0);
        assert!(!g.irq_line(), "gruppo 0 non supportato");
        g.dist_write(GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
        assert!(g.irq_line());
        g.dist_write(GICD_CTLR, 4, 0);
        assert!(!g.irq_line());
        g.dist_write(GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
        g.write_igrpen1(0);
        assert!(!g.irq_line());
        assert_eq!(g.read_hppir1(), 40, "HPPIR ignora IGRPEN1 e PMR");
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
        assert!(!g.irq_line(), "affinità di un'altra CPU");
        g.dist_write(r, 8, 1 << 31);
        assert!(g.irq_line(), "IRM = 1");
        g.dist_write(r, 4, 0);
        assert!(g.irq_line());
        assert_eq!(g.dist_read(GICD_IROUTER, 8), 0, "IROUTER degli SGI/PPI riservato");
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
        g.write_bpr1(5); // priorità di gruppo = bit [7:5]
        enable_spi(&mut g, 40, 0x50);
        enable_spi(&mut g, 41, 0x48);
        g.set_irq_level(40, true);
        assert_eq!(g.read_iar1(), 40);
        assert_eq!(g.read_rpr(), 0x40, "RPR riporta la priorità di gruppo");
        g.set_irq_level(41, true);
        assert!(!g.irq_line(), "0x48 e 0x50 stanno nello stesso gruppo 0x40");
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
        assert_eq!(MmioDevice::read(&mut g, 0x5_0000, 4), 0, "buco RAZ");
        MmioDevice::write(&mut g, gicr + GICR_WAKER, 4, 0);
        assert_eq!(MmioDevice::read(&mut g, gicr + GICR_WAKER, 4), 0);
        MmioDevice::write(&mut g, GICD_IPRIORITYR + 32, 4, 0x4433_2211);
        assert_eq!(MmioDevice::read(&mut g, GICD_IPRIORITYR + 34, 1), 0x33);
        assert_eq!(MmioDevice::read(&mut g, GICD_IPRIORITYR, 4), 0, "priorità SGI RAZ nel distributore");
    }
}
