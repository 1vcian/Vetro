//! Ciò che sta fuori dalla CPU: RAM, piattaforma e contatore del tempo.
//! Implementa la memoria fisica vista dalla MMU e l'ambiente della CPU
//! ([`CpuEnv`]): timer generico, interfaccia CPU del GIC, linea IRQ.

use core::cell::RefCell;

use vetro_cpu::sys::CpuEnv;
use vetro_cpu::sysreg::EnvReg;
use vetro_mmu::{BusError, PhysMemory};
use vetro_platform::Virt;
use vetro_platform::map;
use vetro_platform::virtio::{GuestRam, RamError};

/// La RAM del guest, da `map::RAM_BASE`.
pub struct Ram {
    pub bytes: Vec<u8>,
}

impl Ram {
    /// Offset in `bytes` di `[pa, pa+len)`, se tutto dentro la RAM.
    #[inline]
    fn range(&self, pa: u64, len: usize) -> Option<usize> {
        let off = pa.checked_sub(map::RAM_BASE)?;
        (off.checked_add(len as u64)? <= self.bytes.len() as u64).then_some(off as usize)
    }

    pub fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        match self.range(pa, buf.len()) {
            Some(o) => {
                buf.copy_from_slice(&self.bytes[o..o + buf.len()]);
                true
            }
            None => false,
        }
    }

    pub fn write(&mut self, pa: u64, data: &[u8]) -> bool {
        match self.range(pa, data.len()) {
            Some(o) => {
                self.bytes[o..o + data.len()].copy_from_slice(data);
                true
            }
            None => false,
        }
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

/// RAM, piattaforma e tempo.
pub struct Board {
    pub ram: Ram,
    pub virt: Virt,
    /// Valore corrente di CNTPCT_EL0.
    pub cntpct: u64,
    /// Qualcosa può aver cambiato il livello di una linea di interrupt
    /// (accesso MMIO, registro del timer): va chiamato `update_irqs`.
    pub(crate) irq_dirty: bool,
    /// Un accesso a uno slot virtio-mmio: il dispositivo va servito.
    pub(crate) virtio_dirty: bool,
    /// Livello della linea IRQ del GIC, se già calcolato: la CPU lo legge
    /// prima di ogni istruzione con PSTATE.I = 0, e `Gic::irq_line` scorre
    /// tutti gli interrupt. Si azzera a ogni operazione che può cambiare lo
    /// stato del GIC (MMIO, ICC_*, `update_irqs`, virtio).
    pub(crate) irq_cache: Option<bool>,
}

impl Board {
    pub fn new(ram_size: u64, now_secs: u64) -> Self {
        Board {
            ram: Ram { bytes: vec![0; ram_size as usize] },
            virt: Virt::new(now_secs),
            cntpct: 0,
            irq_dirty: true,
            virtio_dirty: false,
            irq_cache: None,
        }
    }

    /// Porta al GIC i livelli di tutte le linee.
    pub fn update_irqs(&mut self) {
        self.virt.update_irqs(self.cntpct);
        self.irq_cache = None;
        self.irq_dirty = false;
    }

    /// Fa lavorare i dispositivi virtio sopra la RAM.
    pub fn service_virtio(&mut self) {
        let Board { ram, virt, .. } = self;
        virt.service_virtio(ram);
        self.irq_cache = None;
        self.virtio_dirty = false;
        self.irq_dirty = true;
    }

    /// Pilota la linea d'ingresso `line` del GPIO PL061: la 3
    /// (`vetro_platform::pl061::POWER_KEY_LINE`) è il tasto di spegnimento
    /// (`gpio-keys`, KEY_POWER). L'interrupt arriva al guest prima della
    /// prossima istruzione. È un ingresso dell'host: va registrato per il
    /// replay (M10).
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

/// La memoria fisica: RAM, altrimenti il bus MMIO (un accesso da 1, 2, 4 o
/// 8 byte; ciò che non risponde è un decode error).
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

/// L'ambiente della CPU: linea IRQ del GIC, timer generico, ICC_*.
pub(crate) struct Env<'a>(pub &'a RefCell<Board>);

/// INTID "nessun interrupt" dell'interfaccia CPU.
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
            // Il GIC di Vetro ha solo il gruppo 1 (Linux non usa il gruppo 0).
            IccIar0El1 | IccHppir0El1 => SPURIOUS,
            IccBpr0El1 | IccAp0r0El1 | IccIgrpen0El1 => 0,
            // Registri di sola scrittura: la CPU non li legge mai.
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
            // Gruppo 0 assente; CNTFRQ/CNTPCT/CNTVCT e le letture pure non
            // arrivano qui (la CPU rifiuta la scrittura).
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::gic::*;
    use vetro_platform::timer::CTL_ENABLE;

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

    /// La linea IRQ in cache segue ogni cambiamento del GIC: timer che scade
    /// (`update_irqs`), acknowledge (lettura di ICC_IAR1), EOI (scrittura) e
    /// accessi MMIO. Senza gli azzeramenti la CPU vedrebbe il livello vecchio.
    #[test]
    fn linea_irq_in_cache_segue_il_gic() {
        let b = board_with_vtimer_enabled();
        let mut env = Env(&b);
        b.borrow_mut().update_irqs();
        assert!(!env.irq_line());
        assert_eq!(b.borrow().irq_cache, Some(false), "il livello resta in cache");

        env.write_sysreg(EnvReg::CntvCvalEl0, 100);
        env.write_sysreg(EnvReg::CntvCtlEl0, CTL_ENABLE);
        b.borrow_mut().cntpct = 100;
        b.borrow_mut().update_irqs();
        assert!(env.irq_line(), "il timer scaduto alza la linea");

        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line(), "dopo l'acknowledge l'interrupt è attivo, non più in attesa");

        env.write_sysreg(EnvReg::CntvCtlEl0, 0);
        b.borrow_mut().update_irqs();
        env.write_sysreg(EnvReg::IccEoir1El1, u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line());

        // Un accesso MMIO al GIC azzera la cache: un SGI di nuovo pendente.
        let mut phys = Phys(&b);
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, &1u32.to_le_bytes()).unwrap();
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISPENDR0, &1u32.to_le_bytes()).unwrap();
        assert!(env.irq_line(), "SGI 0 abilitato e reso pendente via MMIO");
    }

    /// Il tasto di spegnimento premuto dall'host: `gpio_input` segna le linee
    /// da aggiornare (il ciclo di `Machine::run` chiama `update_irqs` prima
    /// della prossima istruzione) e l'INTID 39 arriva alla CPU.
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
        assert!(b.borrow().irq_dirty, "le linee vanno riportate al GIC");
        b.borrow_mut().update_irqs();
        assert!(env.irq_line());
        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(intid));
    }
}
