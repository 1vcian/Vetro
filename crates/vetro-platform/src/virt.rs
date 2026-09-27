//! The assembled virt platform: bus with GIC, UART, RTC, GPIO and 32
//! virtio-mmio slots, plus CPU 0's generic timer and the wiring of the
//! IRQ lines.
//!
//! The CPU (M3) will use `bus` for MMIO accesses outside RAM, `gic_mut`
//! for the ICC_* registers and `timer` for the CNT* registers. The engine calls
//! [`Virt::service_virtio`] after MMIO accesses to the virtio slots and
//! periodically (incoming data from the backends), then [`Virt::update_irqs`];
//! the latter also after every MMIO access and when the counter passes
//! the next timer deadline.
//!
//! virtio devices are attached with [`Virt::attach_virtio`] (chosen
//! slot) or [`Virt::attach_virtio_next`] (first free slot from the top,
//! as QEMU does with `-device` in command-line order).

use crate::bus::{Bus, DeviceId};
use crate::gic::{self, Gic};
use crate::map;
use crate::pl011::Pl011;
use crate::pl031::Pl031;
use crate::pl061::Pl061;
use crate::timer::GenericTimer;
use crate::virtio::{GuestRam, VirtioDevice, VirtioMmio};

/// Error attaching a virtio device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioSlotError {
    /// The slot does not exist (valid: 0..32).
    NoSuchSlot(u32),
    /// The slot already has a device.
    Occupied(u32),
    /// All slots are occupied.
    Full,
}

pub struct Virt {
    pub bus: Bus,
    pub timer: GenericTimer,
    gic: DeviceId,
    uart: DeviceId,
    rtc: DeviceId,
    gpio: DeviceId,
    /// Transport of slot `k` (base `VIRTIO_BASE + k * VIRTIO_SLOT_SIZE`).
    virtio: [DeviceId; map::VIRTIO_SLOTS as usize],
}

impl Virt {
    /// Platform with the RTC initialised to `now_secs` (external time).
    pub fn new(now_secs: u64) -> Self {
        let mut bus = Bus::new();
        let gic = bus.map(map::GICD_BASE, gic::MMIO_SIZE, "gicv3", Box::new(Gic::new())).unwrap();
        let uart = bus.map(map::UART_BASE, map::UART_SIZE, "pl011", Box::new(Pl011::new())).unwrap();
        let rtc = bus.map(map::RTC_BASE, map::RTC_SIZE, "pl031", Box::new(Pl031::new(now_secs))).unwrap();
        let gpio = bus.map(map::GPIO_BASE, map::GPIO_SIZE, "pl061", Box::new(Pl061::new())).unwrap();
        let virtio = core::array::from_fn(|k| {
            let base = map::VIRTIO_BASE + k as u64 * map::VIRTIO_SLOT_SIZE;
            bus.map(base, map::VIRTIO_SLOT_SIZE, "virtio-mmio", Box::new(VirtioMmio::empty())).unwrap()
        });
        Self { bus, timer: GenericTimer::default(), gic, uart, rtc, gpio, virtio }
    }

    /// virtio-mmio transport of slot `slot`.
    pub fn virtio(&self, slot: u32) -> Option<&VirtioMmio> {
        let id = *self.virtio.get(slot as usize)?;
        self.bus.device(id)
    }

    pub fn virtio_mut(&mut self, slot: u32) -> Option<&mut VirtioMmio> {
        let id = *self.virtio.get(slot as usize)?;
        self.bus.device_mut(id)
    }

    /// Attaches `dev` to slot `slot`; its line is SPI
    /// `VIRTIO_SPI_BASE + slot`, already described in the device tree.
    pub fn attach_virtio(&mut self, slot: u32, dev: Box<dyn VirtioDevice>) -> Result<(), VirtioSlotError> {
        let t = self.virtio_mut(slot).ok_or(VirtioSlotError::NoSuchSlot(slot))?;
        if t.device().is_some() {
            return Err(VirtioSlotError::Occupied(slot));
        }
        t.set_device(Some(dev));
        Ok(())
    }

    /// Attaches `dev` to the highest free slot and returns its number.
    /// Like QEMU virt: the first `-device virtio-*-device` ends up in
    /// slot 31 (0x0A00_3E00), the second in 30, and so on.
    pub fn attach_virtio_next(&mut self, dev: Box<dyn VirtioDevice>) -> Result<u32, VirtioSlotError> {
        let slot = (0..map::VIRTIO_SLOTS as u32)
            .rev()
            .find(|&k| self.virtio(k).is_some_and(|t| t.device().is_none()))
            .ok_or(VirtioSlotError::Full)?;
        self.attach_virtio(slot, dev)?;
        Ok(slot)
    }

    /// Makes all attached virtio devices do their work (see
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
    pub fn gpio(&self) -> &Pl061 {
        self.bus.device(self.gpio).unwrap()
    }
    /// The GPIO: the host presses and releases the power key with
    /// `set_input(pl061::POWER_KEY_LINE, ..)`, then calls `update_irqs`.
    pub fn gpio_mut(&mut self) -> &mut Pl061 {
        self.bus.device_mut(self.gpio).unwrap()
    }

    /// Brings the level of all lines to the GIC: timer (PPI 27 and 30),
    /// UART (SPI 1), RTC (SPI 2), GPIO (SPI 7) and virtio (SPI 16 + slot).
    pub fn update_irqs(&mut self, cntpct: u64) {
        let timer = self.timer.irq_lines(cntpct);
        let uart = self.uart().irq_level();
        let rtc = self.rtc().irq_level();
        let gpio = self.gpio().irq_level();
        let virtio: [bool; map::VIRTIO_SLOTS as usize] =
            core::array::from_fn(|k| self.virtio(k as u32).is_some_and(VirtioMmio::irq_level));
        let gic = self.gic_mut();
        for (intid, level) in timer {
            gic.set_irq_level(intid, level);
        }
        gic.set_spi_level(map::UART_SPI, uart);
        gic.set_spi_level(map::RTC_SPI, rtc);
        gic.set_spi_level(map::GPIO_SPI, gpio);
        for (k, level) in virtio.into_iter().enumerate() {
            gic.set_spi_level(map::VIRTIO_SPI_BASE + k as u32, level);
        }
    }

    /// IRQ line to CPU 0.
    pub fn irq_line(&self) -> bool {
        self.gic().irq_line()
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// All platform devices, each in its own section: timer,
/// GIC, UART, RTC, GPIO and the 32 virtio slots (an empty slot saves only its
/// DeviceID 0). The bus has no state: the regions are fixed by `Virt::new`.
impl vetro_snapshot::Snapshot for Virt {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.section(b"TIMR", |w| w.put(&self.timer));
        w.section(b"GIC3", |w| w.put(self.gic()));
        w.section(b"UART", |w| w.put(self.uart()));
        w.section(b"RTC ", |w| w.put(self.rtc()));
        w.section(b"GPIO", |w| w.put(self.gpio()));
        for k in 0..map::VIRTIO_SLOTS as u32 {
            w.section(b"VIO ", |w| {
                w.u64(u64::from(k));
                w.put(self.virtio(k).expect("32 slots"));
            });
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        fn part<S: vetro_snapshot::Snapshot + ?Sized>(
            r: &mut vetro_snapshot::Reader<'_>,
            tag: &[u8; 4],
            s: &mut S,
        ) -> vetro_snapshot::Result<()> {
            let mut sec = r.section(tag)?;
            sec.get(s)?;
            sec.finish()
        }
        part(r, b"TIMR", &mut self.timer)?;
        part(r, b"GIC3", self.gic_mut())?;
        part(r, b"UART", self.uart_mut())?;
        part(r, b"RTC ", self.rtc_mut())?;
        part(r, b"GPIO", self.gpio_mut())?;
        for k in 0..map::VIRTIO_SLOTS as u32 {
            let mut sec = r.section(b"VIO ")?;
            sec.expect_u64("virtio slot", u64::from(k))?;
            sec.get(self.virtio_mut(k).expect("32 slots"))?;
            sec.finish()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gic::*;
    use crate::{pl011, pl031, pl061, timer, virtio};

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
        assert_eq!(v.bus.read(map::GPIO_BASE + 0xFE0, 1), Some(0x61));
        for k in [0, 31] {
            let base = map::VIRTIO_BASE + k * map::VIRTIO_SLOT_SIZE;
            assert_eq!(v.bus.read(base + virtio::MAGIC_VALUE, 4), Some(u64::from(virtio::MAGIC)));
        }
        assert_eq!(v.bus.read(map::VIRTIO_BASE + 32 * map::VIRTIO_SLOT_SIZE, 4), None);
        assert_eq!(v.bus.read(map::RAM_BASE, 4), None, "RAM does not go through the MMIO bus");
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
        assert!(!v.irq_line(), "update_irqs is needed");
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
        // The guest turns the timer off in the handler, then EOI.
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

    /// Power key (line 3 of the PL061) programmed the way Linux does
    /// for gpio-keys (both edges): press and release arrive
    /// at INTID 39.
    #[test]
    fn tasto_di_spegnimento_sullo_spi_7() {
        let mut v = Virt::new(0);
        init_gic(&mut v);
        let intid = map::SPI_BASE + map::GPIO_SPI;
        v.bus.write(map::GICD_BASE + GICD_ISENABLER + 4, 4, 1 << (intid % 32));
        let m = 1u64 << pl061::POWER_KEY_LINE;
        v.bus.write(map::GPIO_BASE + pl061::IBE, 1, m);
        v.bus.write(map::GPIO_BASE + pl061::IE, 1, m);
        v.update_irqs(0);
        assert!(!v.irq_line());
        for pressed in [true, false] {
            v.gpio_mut().set_input(pl061::POWER_KEY_LINE, pressed);
            v.update_irqs(0);
            assert_eq!(v.gic_mut().read_iar1(), u64::from(intid));
            let data = v.bus.read(map::GPIO_BASE + (m << 2), 1);
            assert_eq!(data, Some(if pressed { m } else { 0 }));
            v.bus.write(map::GPIO_BASE + pl061::IC, 1, m);
            v.update_irqs(0);
            v.gic_mut().write_eoir1(u64::from(intid));
            assert!(!v.irq_line());
        }
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
        assert_eq!(v.attach_virtio_next(disco()), Ok(31), "like QEMU: from the top");
        assert_eq!(v.attach_virtio_next(disco()), Ok(30));
        assert_eq!(v.attach_virtio(31, disco()), Err(VirtioSlotError::Occupied(31)));
        assert_eq!(v.attach_virtio(32, disco()), Err(VirtioSlotError::NoSuchSlot(32)));
        assert_eq!(v.attach_virtio(5, disco()), Ok(()));
        assert_eq!(v.bus.read(slot_base(5) + vio::DEVICE_ID, 4), Some(u64::from(vio::ID_BLOCK)));
        assert_eq!(v.bus.read(slot_base(31) + vio::DEVICE_ID, 4), Some(u64::from(vio::ID_BLOCK)));
        assert_eq!(v.bus.read(slot_base(4) + vio::DEVICE_ID, 4), Some(0), "free slot");
        assert_eq!(v.bus.read(slot_base(4) + vio::MAGIC_VALUE, 4), Some(u64::from(vio::MAGIC)));
        assert!(v.virtio(5).unwrap().device_as::<VirtioBlk>().is_some());
        for k in 0..32 {
            if ![5, 30, 31].contains(&k) {
                v.attach_virtio(k, disco()).unwrap();
            }
        }
        assert_eq!(v.attach_virtio_next(disco()), Err(VirtioSlotError::Full));
    }

    /// The test driver that goes through the platform bus.
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
        // Rising edge, as declared in the device tree.
        let cfg_reg = u64::from(intid / 16) * 4;
        v.bus.write(map::GICD_BASE + GICD_ICFGR + cfg_reg, 4, 2 << ((intid % 16) * 2));

        let mut d = Driver::new(Porta { v, slot });
        d.init(u64::MAX, 8);
        let h = d.buf(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // IN, sector 1
        let data = d.alloc(512, 8);
        let st = d.buf(&[0xFF]);
        d.add(0, &[(h, 16, false), (data, 512, true), (st, 1, true)]);
        assert!(!d.t.v.irq_line(), "no interrupt before servicing");
        d.service();
        assert!(d.t.v.irq_line());
        assert_eq!(d.t.v.gic_mut().read_iar1(), u64::from(intid));
        // Guest handler: reads and acknowledges InterruptStatus, consumes the used ring.
        assert_eq!(d.irq(), vio::INT_VRING);
        assert_eq!(d.pop_used(0).map(|u| u.1), Some(513));
        assert_eq!(d.mem(st, 1), [0]);
        d.t.v.update_irqs(0);
        d.t.v.gic_mut().write_eoir1(u64::from(intid));
        assert!(!d.t.v.irq_line());
    }
}
