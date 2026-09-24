//! La piattaforma virt montata: bus con GIC, UART, RTC e slot virtio vuoti,
//! più il timer generico della CPU 0 e il cablaggio delle linee IRQ.
//!
//! La CPU (M3) userà `bus` per gli accessi MMIO fuori dalla RAM, `gic_mut`
//! per i registri ICC_* e `timer` per i registri CNT*. Il motore chiama
//! [`Virt::update_irqs`] dopo ogni accesso MMIO e quando il contatore
//! supera la prossima scadenza del timer.

use crate::bus::{Bus, DeviceId};
use crate::gic::{self, Gic};
use crate::map;
use crate::pl011::Pl011;
use crate::pl031::Pl031;
use crate::timer::GenericTimer;
use crate::virtio::VirtioMmioEmpty;

pub struct Virt {
    pub bus: Bus,
    pub timer: GenericTimer,
    gic: DeviceId,
    uart: DeviceId,
    rtc: DeviceId,
}

impl Virt {
    /// Piattaforma con l'RTC inizializzato a `now_secs` (tempo esterno).
    pub fn new(now_secs: u64) -> Self {
        let mut bus = Bus::new();
        let gic = bus.map(map::GICD_BASE, gic::MMIO_SIZE, "gicv3", Box::new(Gic::new())).unwrap();
        let uart = bus.map(map::UART_BASE, map::UART_SIZE, "pl011", Box::new(Pl011::new())).unwrap();
        let rtc = bus.map(map::RTC_BASE, map::RTC_SIZE, "pl031", Box::new(Pl031::new(now_secs))).unwrap();
        for k in 0..map::VIRTIO_SLOTS {
            let base = map::VIRTIO_BASE + k * map::VIRTIO_SLOT_SIZE;
            bus.map(base, map::VIRTIO_SLOT_SIZE, "virtio-mmio", Box::new(VirtioMmioEmpty)).unwrap();
        }
        Self { bus, timer: GenericTimer::default(), gic, uart, rtc }
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
    /// UART (SPI 1) e RTC (SPI 2).
    pub fn update_irqs(&mut self, cntpct: u64) {
        let timer = self.timer.irq_lines(cntpct);
        let uart = self.uart().irq_level();
        let rtc = self.rtc().irq_level();
        let gic = self.gic_mut();
        for (intid, level) in timer {
            gic.set_irq_level(intid, level);
        }
        gic.set_spi_level(map::UART_SPI, uart);
        gic.set_spi_level(map::RTC_SPI, rtc);
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
}
