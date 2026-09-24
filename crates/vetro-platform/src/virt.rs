//! La piattaforma virt montata: bus con GIC, UART, RTC e 32 slot
//! virtio-mmio, più il timer generico della CPU 0 e il cablaggio delle
//! linee IRQ.
//!
//! La CPU (M3) userà `bus` per gli accessi MMIO fuori dalla RAM, `gic_mut`
//! per i registri ICC_* e `timer` per i registri CNT*. Il motore chiama
//! [`Virt::service_virtio`] dopo gli accessi MMIO agli slot virtio e
//! periodicamente (dati in arrivo dai backend), poi [`Virt::update_irqs`];
//! quest'ultimo anche dopo ogni accesso MMIO e quando il contatore supera
//! la prossima scadenza del timer.
//!
//! I dispositivi virtio si montano con [`Virt::attach_virtio`] (slot
//! scelto) o [`Virt::attach_virtio_next`] (primo slot libero dall'alto,
//! come fa QEMU con i `-device` in ordine di riga di comando).

use crate::bus::{Bus, DeviceId};
use crate::gic::{self, Gic};
use crate::map;
use crate::pl011::Pl011;
use crate::pl031::Pl031;
use crate::timer::GenericTimer;
use crate::virtio::{GuestRam, VirtioDevice, VirtioMmio};

/// Errore nel montaggio di un dispositivo virtio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioSlotError {
    /// Lo slot non esiste (validi: 0..32).
    NoSuchSlot(u32),
    /// Lo slot ha già un dispositivo.
    Occupied(u32),
    /// Tutti gli slot sono occupati.
    Full,
}

pub struct Virt {
    pub bus: Bus,
    pub timer: GenericTimer,
    gic: DeviceId,
    uart: DeviceId,
    rtc: DeviceId,
    /// Trasporto dello slot `k` (base `VIRTIO_BASE + k * VIRTIO_SLOT_SIZE`).
    virtio: [DeviceId; map::VIRTIO_SLOTS as usize],
}

impl Virt {
    /// Piattaforma con l'RTC inizializzato a `now_secs` (tempo esterno).
    pub fn new(now_secs: u64) -> Self {
        let mut bus = Bus::new();
        let gic = bus.map(map::GICD_BASE, gic::MMIO_SIZE, "gicv3", Box::new(Gic::new())).unwrap();
        let uart = bus.map(map::UART_BASE, map::UART_SIZE, "pl011", Box::new(Pl011::new())).unwrap();
        let rtc = bus.map(map::RTC_BASE, map::RTC_SIZE, "pl031", Box::new(Pl031::new(now_secs))).unwrap();
        let virtio = core::array::from_fn(|k| {
            let base = map::VIRTIO_BASE + k as u64 * map::VIRTIO_SLOT_SIZE;
            bus.map(base, map::VIRTIO_SLOT_SIZE, "virtio-mmio", Box::new(VirtioMmio::empty())).unwrap()
        });
        Self { bus, timer: GenericTimer::default(), gic, uart, rtc, virtio }
    }

    /// Trasporto virtio-mmio dello slot `slot`.
    pub fn virtio(&self, slot: u32) -> Option<&VirtioMmio> {
        let id = *self.virtio.get(slot as usize)?;
        self.bus.device(id)
    }

    pub fn virtio_mut(&mut self, slot: u32) -> Option<&mut VirtioMmio> {
        let id = *self.virtio.get(slot as usize)?;
        self.bus.device_mut(id)
    }

    /// Monta `dev` nello slot `slot`; la sua linea è lo SPI
    /// `VIRTIO_SPI_BASE + slot`, già descritto nel device tree.
    pub fn attach_virtio(&mut self, slot: u32, dev: Box<dyn VirtioDevice>) -> Result<(), VirtioSlotError> {
        let t = self.virtio_mut(slot).ok_or(VirtioSlotError::NoSuchSlot(slot))?;
        if t.device().is_some() {
            return Err(VirtioSlotError::Occupied(slot));
        }
        t.set_device(Some(dev));
        Ok(())
    }

    /// Monta `dev` nello slot libero più alto e ne restituisce il numero.
    /// Come QEMU virt: il primo `-device virtio-*-device` finisce nello
    /// slot 31 (0x0A00_3E00), il secondo nel 30, e così via.
    pub fn attach_virtio_next(&mut self, dev: Box<dyn VirtioDevice>) -> Result<u32, VirtioSlotError> {
        let slot = (0..map::VIRTIO_SLOTS as u32)
            .rev()
            .find(|&k| self.virtio(k).is_some_and(|t| t.device().is_none()))
            .ok_or(VirtioSlotError::Full)?;
        self.attach_virtio(slot, dev)?;
        Ok(slot)
    }

    /// Fa lavorare tutti i dispositivi virtio montati (vedi
    /// [`VirtioMmio::service`]).
    pub fn service_virtio(&mut self, ram: &mut dyn GuestRam) {
        for id in self.virtio {
            let t: &mut VirtioMmio = self.bus.device_mut(id).unwrap();
            if t.device().is_some() {
                t.service(ram);
            }
        }
    }

    pub fn gic(&self) -> &Gic {
        self.bus.device(self.gic).unwrap()
    }
    pub fn gic_mut(&mut self) -> &mut Gic {
        self.bus.device_mut(self.gic).unwrap()
    }
    pub fn uart(&self) -> &Pl011 {
        self.bus.device(self.uart).unwrap()
    }
    pub fn uart_mut(&mut self) -> &mut Pl011 {
        self.bus.device_mut(self.uart).unwrap()
    }
    pub fn rtc(&self) -> &Pl031 {
        self.bus.device(self.rtc).unwrap()
    }
    pub fn rtc_mut(&mut self) -> &mut Pl031 {
        self.bus.device_mut(self.rtc).unwrap()
    }

    /// Porta al GIC il livello di tutte le linee: timer (PPI 27 e 30),
    /// UART (SPI 1), RTC (SPI 2) e virtio (SPI 16 + slot).
    pub fn update_irqs(&mut self, cntpct: u64) {
        let timer = self.timer.irq_lines(cntpct);
        let uart = self.uart().irq_level();
        let rtc = self.rtc().irq_level();
        let virtio: [bool; map::VIRTIO_SLOTS as usize] =
            core::array::from_fn(|k| self.virtio(k as u32).is_some_and(VirtioMmio::irq_level));
        let gic = self.gic_mut();
        for (intid, level) in timer {
            gic.set_irq_level(intid, level);
        }
        gic.set_spi_level(map::UART_SPI, uart);
        gic.set_spi_level(map::RTC_SPI, rtc);
        for (k, level) in virtio.into_iter().enumerate() {
            gic.set_spi_level(map::VIRTIO_SPI_BASE + k as u32, level);
        }
    }

    /// Linea IRQ verso la CPU 0.
    pub fn irq_line(&self) -> bool {
        self.gic().irq_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gic::*;
    use crate::{pl011, pl031, timer, virtio};

    const GICR: u64 = map::GICR_BASE;

    fn init_gic(v: &mut Virt) {
        let b = &mut v.bus;
        assert!(b.write(GICR + GICR_WAKER, 4, 0));
        b.write(map::GICD_BASE + GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
        b.write(map::GICD_BASE + GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
        b.write(GICR + GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
        let g = v.gic_mut();
        g.write_pmr(0xF0);
        g.write_igrpen1(1);
    }

    #[test]
    fn mappa_della_memoria() {
        let mut v = Virt::new(0);
        assert_eq!(v.bus.read(map::GICD_BASE + GICD_PIDR2, 4), Some(0x3B));
        assert_eq!(v.bus.read(GICR + GICR_PIDR2, 4), Some(0x3B));
        assert_eq!(v.bus.read(map::UART_BASE + 0xFE0, 4), Some(0x11));
        assert_eq!(v.bus.read(map::RTC_BASE + 0xFE0, 4), Some(0x31));
        for k in [0, 31] {
            let base = map::VIRTIO_BASE + k * map::VIRTIO_SLOT_SIZE;
            assert_eq!(v.bus.read(base + virtio::MAGIC_VALUE, 4), Some(u64::from(virtio::MAGIC)));
        }
        assert_eq!(v.bus.read(map::VIRTIO_BASE + 32 * map::VIRTIO_SLOT_SIZE, 4), None);
        assert_eq!(v.bus.read(map::RAM_BASE, 4), None, "la RAM non passa dal bus MMIO");
        assert_eq!(v.bus.read(0x0900_2000, 4), None);
    }

    #[test]
    fn uart_scrive_verso_l_host() {
        let mut v = Virt::new(0);
        for &c in b"ok\n" {
            v.bus.write(map::UART_BASE + pl011::DR, 1, u64::from(c));
        }
        assert_eq!(v.uart_mut().take_output(), b"ok\n");
    }

    #[test]
    fn interrupt_della_uart_arriva_alla_cpu() {
        let mut v = Virt::new(0);
        init_gic(&mut v);
        let intid = map::SPI_BASE + map::UART_SPI;
        v.bus.write(map::GICD_BASE + GICD_ISENABLER + 4, 4, 1 << (intid % 32));
        v.bus.write(map::UART_BASE + pl011::CR, 4, 0x301);
        v.bus.write(map::UART_BASE + pl011::IMSC, 4, u64::from(pl011::INT_RX));
        v.uart_mut().push_input(b"k");
        assert!(!v.irq_line(), "serve update_irqs");
        v.update_irqs(0);
        assert!(v.irq_line());
        assert_eq!(v.gic_mut().read_iar1(), u64::from(intid));
        assert_eq!(v.bus.read(map::UART_BASE + pl011::DR, 4), Some(u64::from(b'k')));
        v.update_irqs(0);
        v.gic_mut().write_eoir1(u64::from(intid));
        assert!(!v.irq_line());
    }

    #[test]
    fn timer_virtuale_sul_ppi_27() {
        let mut v = Virt::new(0);
        init_gic(&mut v);
        v.bus.write(GICR + GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << map::PPI_VTIMER);
        v.timer.set_cntv_cval(1000);
        v.timer.set_cntv_ctl(timer::CTL_ENABLE);
        v.update_irqs(999);
        assert!(!v.irq_line());
        v.update_irqs(1000);
        assert_eq!(v.gic_mut().read_iar1(), u64::from(map::PPI_VTIMER));
        // Il guest spegne il timer nell'handler, poi EOI.
        v.timer.set_cntv_ctl(0);
        v.update_irqs(1001);
        v.gic_mut().write_eoir1(u64::from(map::PPI_VTIMER));
        assert!(!v.irq_line());
    }

    #[test]
    fn allarme_rtc_sullo_spi_2() {
        let mut v = Virt::new(100);
        init_gic(&mut v);
        let intid = map::SPI_BASE + map::RTC_SPI;
        v.bus.write(map::GICD_BASE + GICD_ISENABLER + 4, 4, 1 << (intid % 32));
        v.bus.write(map::RTC_BASE + pl031::MR, 4, 105);
        v.bus.write(map::RTC_BASE + pl031::IMSC, 4, 1);
        v.rtc_mut().set_time(106);
        v.update_irqs(0);
        assert_eq!(v.gic_mut().read_hppir1(), u64::from(intid));
        assert_eq!(v.bus.read(map::RTC_BASE + pl031::DR, 4), Some(106));
    }

    // ---- virtio -------------------------------------------------------------

    use crate::virtio::testdrv::{Driver, Transport};
    use crate::virtio::{self as vio, MemBackend, VecRam, VirtioBlk};

    fn disco() -> Box<VirtioBlk> {
        Box::new(VirtioBlk::new(Box::new(MemBackend::new(4096)), Default::default()))
    }

    fn slot_base(k: u32) -> u64 {
        map::VIRTIO_BASE + u64::from(k) * map::VIRTIO_SLOT_SIZE
    }

    #[test]
    fn scelta_degli_slot_virtio() {
        let mut v = Virt::new(0);
        assert_eq!(v.attach_virtio_next(disco()), Ok(31), "come QEMU: dall'alto");
        assert_eq!(v.attach_virtio_next(disco()), Ok(30));
        assert_eq!(v.attach_virtio(31, disco()), Err(VirtioSlotError::Occupied(31)));
        assert_eq!(v.attach_virtio(32, disco()), Err(VirtioSlotError::NoSuchSlot(32)));
        assert_eq!(v.attach_virtio(5, disco()), Ok(()));
        assert_eq!(v.bus.read(slot_base(5) + vio::DEVICE_ID, 4), Some(u64::from(vio::ID_BLOCK)));
        assert_eq!(v.bus.read(slot_base(31) + vio::DEVICE_ID, 4), Some(u64::from(vio::ID_BLOCK)));
        assert_eq!(v.bus.read(slot_base(4) + vio::DEVICE_ID, 4), Some(0), "slot libero");
        assert_eq!(v.bus.read(slot_base(4) + vio::MAGIC_VALUE, 4), Some(u64::from(vio::MAGIC)));
        assert!(v.virtio(5).unwrap().device_as::<VirtioBlk>().is_some());
        for k in 0..32 {
            if ![5, 30, 31].contains(&k) {
                v.attach_virtio(k, disco()).unwrap();
            }
        }
        assert_eq!(v.attach_virtio_next(disco()), Err(VirtioSlotError::Full));
    }

    /// Il driver di prova che passa dal bus della piattaforma.
    struct Porta {
        v: Virt,
        slot: u32,
    }

    impl Transport for Porta {
        fn rd(&mut self, off: u64) -> u32 {
            self.v.bus.read(slot_base(self.slot) + off, 4).unwrap() as u32
        }
        fn wr(&mut self, off: u64, val: u32) {
            assert!(self.v.bus.write(slot_base(self.slot) + off, 4, u64::from(val)));
        }
        fn cfg(&mut self, off: u64, size: u8) -> u64 {
            self.v.bus.read(slot_base(self.slot) + vio::CONFIG + off, size).unwrap()
        }
        fn cfg_wr(&mut self, off: u64, size: u8, val: u64) {
            self.v.bus.write(slot_base(self.slot) + vio::CONFIG + off, size, val);
        }
        fn service(&mut self, ram: &mut VecRam) {
            self.v.service_virtio(ram);
            self.v.update_irqs(0);
        }
    }

    #[test]
    fn richiesta_virtio_blk_fino_all_interrupt_del_gic() {
        let mut v = Virt::new(0);
        let slot = v.attach_virtio_next(disco()).unwrap();
        init_gic(&mut v);
        let intid = map::SPI_BASE + map::VIRTIO_SPI_BASE + slot;
        assert_eq!(intid, 79);
        let (reg, bit) = (u64::from(intid / 32) * 4, intid % 32);
        v.bus.write(map::GICD_BASE + GICD_IGROUPR + reg, 4, 0xFFFF_FFFF);
        v.bus.write(map::GICD_BASE + GICD_ISENABLER + reg, 4, 1 << bit);
        // A fronte di salita, come dichiarato nel device tree.
        let cfg_reg = u64::from(intid / 16) * 4;
        v.bus.write(map::GICD_BASE + GICD_ICFGR + cfg_reg, 4, 2 << ((intid % 16) * 2));

        let mut d = Driver::new(Porta { v, slot });
        d.init(u64::MAX, 8);
        let h = d.buf(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // IN, settore 1
        let data = d.alloc(512, 8);
        let st = d.buf(&[0xFF]);
        d.add(0, &[(h, 16, false), (data, 512, true), (st, 1, true)]);
        assert!(!d.t.v.irq_line(), "prima del servizio niente interrupt");
        d.service();
        assert!(d.t.v.irq_line());
        assert_eq!(d.t.v.gic_mut().read_iar1(), u64::from(intid));
        // Handler del guest: legge e riconosce InterruptStatus, consuma lo used ring.
        assert_eq!(d.irq(), vio::INT_VRING);
        assert_eq!(d.pop_used(0).map(|u| u.1), Some(513));
        assert_eq!(d.mem(st, 1), [0]);
        d.t.v.update_irqs(0);
        d.t.v.gic_mut().write_eoir1(u64::from(intid));
        assert!(!d.t.v.irq_line());
    }
}
