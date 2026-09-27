//! virtio-mmio transport version 2 (virtio v1.2, §4.2.2).
//!
//! Choices, aligned with QEMU (hw/virtio/virtio-mmio.c) where the spec leaves
//! room:
//! - registers below 0x100 are accessed only as aligned 32-bit; other
//!   accesses read 0 and write nothing;
//! - write-only registers (QueueNum, QueueDesc*, ...) read 0;
//! - VIRTIO_F_VERSION_1 is always offered and mandatory: if the driver does not
//!   accept it, or accepts features not offered, or the device rejects the
//!   combination, FEATURES_OK does not stay set in Status;
//! - DriverFeatures after FEATURES_OK and the configuration of a queue already
//!   ready are ignored;
//! - the shared memory region (SHMSel) does not exist: SHMLen and SHMBase
//!   read all ones (-1), as the spec asks for an absent region;
//! - an error in the queues puts the device in DEVICE_NEEDS_RESET with a
//!   configuration interrupt (§2.1.2) and stops servicing until reset;
//! - the interrupt line is `InterruptStatus != 0`; in the device tree it is
//!   declared rising-edge, as in QEMU.

use core::any::Any;

use super::*;

pub struct VirtioMmio {
    device: Option<Box<dyn VirtioDevice>>,
    queues: Vec<Virtqueue>,
    /// Transport features removed from the offer (for tests and compatibility).
    removed: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    interrupt_status: u32,
    status: u32,
    config_generation: u32,
    last_error: Option<QueueError>,
}

impl VirtioMmio {
    /// Slot without a device: behaves like [`VirtioMmioEmpty`].
    pub fn empty() -> Self {
        Self {
            device: None,
            queues: Vec::new(),
            removed: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            interrupt_status: 0,
            status: 0,
            config_generation: 0,
            last_error: None,
        }
    }

    pub fn new(device: Box<dyn VirtioDevice>) -> Self {
        let mut t = Self::empty();
        t.set_device(Some(device));
        t
    }

    /// Removes some transport features from the offer (INDIRECT_DESC,
    /// EVENT_IDX). VERSION_1 cannot be removed.
    pub fn without_features(mut self, mask: u64) -> Self {
        self.removed = mask & !F_VERSION_1;
        self
    }

    /// Attaches (or removes) the device and brings the transport back to reset.
    pub fn set_device(&mut self, device: Option<Box<dyn VirtioDevice>>) {
        self.queues = device
            .as_ref()
            .map(|d| d.queue_max_sizes().iter().map(|&n| Virtqueue::new(n)).collect())
            .unwrap_or_default();
        self.device = device;
        self.reset();
    }

    pub fn device(&self) -> Option<&dyn VirtioDevice> {
        self.device.as_deref()
    }

    /// Typed access to the attached device.
    pub fn device_as<T: VirtioDevice>(&self) -> Option<&T> {
        let d: &dyn Any = self.device.as_deref()?;
        d.downcast_ref()
    }

    pub fn device_as_mut<T: VirtioDevice>(&mut self) -> Option<&mut T> {
        let d: &mut dyn Any = self.device.as_deref_mut()?;
        d.downcast_mut()
    }

    /// Features offered to the driver.
    pub fn offered_features(&self) -> u64 {
        match &self.device {
            Some(d) => (d.features() | F_VERSION_1 | F_INDIRECT_DESC | F_EVENT_IDX) & !self.removed,
            None => 0,
        }
    }

    /// Features accepted by the driver (valid after FEATURES_OK).
    pub fn driver_features(&self) -> u64 {
        self.driver_features
    }

    pub fn status(&self) -> u32 {
        self.status
    }

    pub fn interrupt_status(&self) -> u32 {
        self.interrupt_status
    }

    pub fn config_generation(&self) -> u32 {
        self.config_generation
    }

    pub fn queue(&self, i: usize) -> Option<&Virtqueue> {
        self.queues.get(i)
    }

    /// Last error that put the device in DEVICE_NEEDS_RESET.
    pub fn last_error(&self) -> Option<QueueError> {
        self.last_error
    }

    /// Level of the interrupt line to the GIC.
    pub fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Configuration change decided by the host (e.g. disk
    /// capacity): ConfigGeneration advances and the interrupt fires, if the driver is
    /// already active.
    pub fn signal_config_change(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.status & STATUS_DRIVER_OK != 0 {
            self.interrupt_status |= INT_CONFIG;
        }
    }

    fn reset(&mut self) {
        for q in &mut self.queues {
            q.reset();
        }
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        self.status = 0;
        self.last_error = None;
        if let Some(d) = &mut self.device {
            d.reset();
        }
    }

    fn fail(&mut self, e: QueueError) {
        self.last_error = Some(e);
        self.status |= STATUS_DEVICE_NEEDS_RESET;
        self.signal_config_change();
    }

    /// Makes the device work: consumes the queues (driver requests and
    /// data ready in the backends) and updates InterruptStatus. Without
    /// DRIVER_OK, or in DEVICE_NEEDS_RESET, it does nothing.
    pub fn service(&mut self, ram: &mut dyn GuestRam) {
        if self.status & STATUS_DRIVER_OK == 0 || self.status & STATUS_DEVICE_NEEDS_RESET != 0 {
            return;
        }
        let Some(dev) = self.device.as_mut() else { return };
        let mut ctx = ServiceCtx {
            queues: &mut self.queues,
            ram,
            features: self.driver_features,
            config_changed: false,
        };
        let mut result = dev.service(&mut ctx);
        if ctx.config_changed {
            self.signal_config_change();
        }
        // Even after an error, the buffers already returned must be notified.
        for q in &mut self.queues {
            match q.should_notify(ram) {
                Ok(true) => self.interrupt_status |= INT_VRING,
                Ok(false) => {}
                Err(e) => result = result.and(Err(e.into())),
            }
        }
        if let Err(e) = result {
            self.fail(e);
        }
    }

    fn cur_queue(&mut self) -> Option<&mut Virtqueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn read_reg(&self, offset: u64) -> u32 {
        let q = self.queues.get(self.queue_sel as usize);
        match offset {
            MAGIC_VALUE => MAGIC,
            VERSION => 2,
            DEVICE_ID => self.device.as_ref().map_or(0, |d| d.device_id()),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => match self.device_features_sel {
                0 => self.offered_features() as u32,
                1 => (self.offered_features() >> 32) as u32,
                _ => 0,
            },
            QUEUE_NUM_MAX => q.map_or(0, |q| u32::from(q.max_size())),
            QUEUE_READY => q.is_some_and(|q| q.ready()).into(),
            INTERRUPT_STATUS => self.interrupt_status,
            STATUS => self.status,
            SHM_LEN_LOW | SHM_LEN_HIGH | SHM_BASE_LOW | SHM_BASE_HIGH => u32::MAX,
            CONFIG_GENERATION => self.config_generation,
            _ => 0,
        }
    }

    fn write_reg(&mut self, offset: u64, v: u32) {
        let features_locked = self.status & STATUS_FEATURES_OK != 0;
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            DRIVER_FEATURES if !features_locked => match self.driver_features_sel {
                0 => self.driver_features = (self.driver_features & !0xFFFF_FFFF) | u64::from(v),
                1 => self.driver_features = (self.driver_features & 0xFFFF_FFFF) | (u64::from(v) << 32),
                _ => {}
            },
            QUEUE_SEL => self.queue_sel = v,
            QUEUE_NUM => {
                if let Some(q) = self.cur_queue().filter(|q| !q.ready()) {
                    // A value over 16 bits is invalid anyway: 0 makes it so.
                    q.set_size(u16::try_from(v).unwrap_or(0));
                }
            }
            QUEUE_READY => {
                let f = self.driver_features;
                if let Some(q) = self.cur_queue() {
                    q.event_idx = f & F_EVENT_IDX != 0;
                    q.indirect = f & F_INDIRECT_DESC != 0;
                    q.set_ready(v & 1 != 0);
                }
            }
            QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_DRIVER_LOW | QUEUE_DRIVER_HIGH | QUEUE_DEVICE_LOW
            | QUEUE_DEVICE_HIGH => {
                let Some(q) = self.cur_queue().filter(|q| !q.ready()) else { return };
                let (desc, driver, device) = q.addrs();
                let half = |old: u64| {
                    if offset & 4 == 0 {
                        (old & !0xFFFF_FFFF) | u64::from(v)
                    } else {
                        (old & 0xFFFF_FFFF) | (u64::from(v) << 32)
                    }
                };
                match offset & !4 {
                    QUEUE_DESC_LOW => q.set_desc(half(desc)),
                    QUEUE_DRIVER_LOW => q.set_driver(half(driver)),
                    _ => q.set_device(half(device)),
                }
            }
            INTERRUPT_ACK => self.interrupt_status &= !v,
            STATUS => self.write_status(v),
            // QueueNotify: the work is done in `service`. SHMSel: no region.
            _ => {}
        }
    }

    fn write_status(&mut self, v: u32) {
        if v == 0 {
            self.reset();
            return;
        }
        let mut v = v | (self.status & STATUS_DEVICE_NEEDS_RESET);
        if v & STATUS_FEATURES_OK != 0 && self.status & STATUS_FEATURES_OK == 0 {
            let f = self.driver_features;
            let ok = f & F_VERSION_1 != 0
                && f & !self.offered_features() == 0
                && self.device.as_mut().is_some_and(|d| d.negotiate(f));
            if !ok {
                v &= !STATUS_FEATURES_OK;
            }
        }
        self.status = v;
    }
}

impl MmioDevice for VirtioMmio {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= CONFIG {
            let Some(d) = &self.device else { return 0 };
            let mut b = [0u8; 8];
            let n = usize::from(size.min(8));
            d.read_config(offset - CONFIG, &mut b[..n]);
            return u64::from_le_bytes(b);
        }
        if size != 4 || !offset.is_multiple_of(4) {
            return 0;
        }
        if self.device.is_none() {
            return VirtioMmioEmpty.read(offset, size);
        }
        u64::from(self.read_reg(offset))
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        let Some(d) = &mut self.device else { return };
        if offset >= CONFIG {
            let n = usize::from(size.min(8));
            d.write_config(offset - CONFIG, &value.to_le_bytes()[..n]);
            return;
        }
        if size == 4 && offset.is_multiple_of(4) {
            self.write_reg(offset, value as u32);
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

fn save_queue_error(w: &mut vetro_snapshot::Writer, e: &QueueError) {
    match *e {
        QueueError::Ram(RamError { addr, len }) => {
            w.u8(0);
            w.u64(addr);
            w.len_of(len);
        }
        QueueError::AvailIdx { last, avail } => {
            w.u8(1);
            w.u16(last);
            w.u16(avail);
        }
        QueueError::HeadOutOfRange(h) => {
            w.u8(2);
            w.u16(h);
        }
        QueueError::NextOutOfRange(n) => {
            w.u8(3);
            w.u16(n);
        }
        QueueError::ChainLoop => w.u8(4),
        QueueError::ReadableAfterWritable => w.u8(5),
        QueueError::IndirectNotNegotiated => w.u8(6),
        QueueError::IndirectLen(l) => {
            w.u8(7);
            w.u32(l);
        }
        QueueError::IndirectMisplaced => w.u8(8),
        QueueError::Malformed(m) => {
            w.u8(9);
            w.str(m);
        }
    }
}

fn restore_queue_error(r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<QueueError> {
    Ok(match r.u8()? {
        0 => QueueError::Ram(RamError { addr: r.u64()?, len: r.u64()? as usize }),
        1 => QueueError::AvailIdx { last: r.u16()?, avail: r.u16()? },
        2 => QueueError::HeadOutOfRange(r.u16()?),
        3 => QueueError::NextOutOfRange(r.u16()?),
        4 => QueueError::ChainLoop,
        5 => QueueError::ReadableAfterWritable,
        6 => QueueError::IndirectNotNegotiated,
        7 => QueueError::IndirectLen(r.u32()?),
        8 => QueueError::IndirectMisplaced,
        // The message is a `&'static str`: it is kept (once per
        // restore of a broken device, rare and small).
        9 => QueueError::Malformed(Box::leak(r.string()?.into_boxed_str())),
        v => return Err(vetro_snapshot::Error::invalid(format!("queue error {v}"))),
    })
}

/// Transport state (selectors, negotiated features, status, interrupt,
/// configuration generation, last error), queues and device
/// state. The device type and the features removed from the offer
/// are configuration: they are checked.
impl vetro_snapshot::Snapshot for VirtioMmio {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(u64::from(self.device.as_ref().map_or(0, |d| d.device_id())));
        w.u64(self.removed);
        w.u32(self.device_features_sel);
        w.u32(self.driver_features_sel);
        w.u64(self.driver_features);
        w.u32(self.queue_sel);
        w.u32(self.interrupt_status);
        w.u32(self.status);
        w.u32(self.config_generation);
        w.opt(self.last_error.as_ref(), save_queue_error);
        w.seq(&self.queues, |w, q| w.put(q));
        if let Some(d) = &self.device {
            w.section(b"VDEV", |w| d.save_state(w));
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        let id = self.device.as_ref().map_or(0, |d| d.device_id());
        r.expect_u64("virtio DeviceID", u64::from(id))?;
        r.expect_u64("features removed from the offer", self.removed)?;
        self.device_features_sel = r.u32()?;
        self.driver_features_sel = r.u32()?;
        self.driver_features = r.u64()?;
        self.queue_sel = r.u32()?;
        self.interrupt_status = r.u32()?;
        self.status = r.u32()?;
        self.config_generation = r.u32()?;
        self.last_error = r.opt(restore_queue_error)?;
        r.expect_u64("number of queues", self.queues.len() as u64)?;
        for q in &mut self.queues {
            r.get(q)?;
        }
        if let Some(d) = &mut self.device {
            let mut s = r.section(b"VDEV")?;
            d.restore_state(&mut s)?;
            s.finish()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal device: a queue that returns every chain with len 7.
    struct Eco {
        cfg: [u8; 8],
        rifiuta: bool,
        reset: u32,
    }

    impl VirtioDevice for Eco {
        fn device_id(&self) -> u32 {
            42
        }
        fn features(&self) -> u64 {
            0b101
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &[16, 8]
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            read_config_bytes(&self.cfg, offset, data);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            self.cfg[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        }
        fn negotiate(&mut self, _features: u64) -> bool {
            !self.rifiuta
        }
        fn reset(&mut self) {
            self.reset += 1;
        }
        fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
            let (q, ram) = (&mut ctx.queues[0], &mut *ctx.ram);
            while let Some(c) = q.pop(ram)? {
                q.push_used(ram, c.head, 7)?;
            }
            Ok(())
        }
        fn save_state(&self, w: &mut vetro_snapshot::Writer) {
            w.raw(&self.cfg);
            w.u32(self.reset);
        }
        fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
            self.cfg.copy_from_slice(r.raw(8)?);
            self.reset = r.u32()?;
            Ok(())
        }
    }

    fn eco() -> VirtioMmio {
        VirtioMmio::new(Box::new(Eco { cfg: [1, 2, 3, 4, 5, 6, 7, 8], rifiuta: false, reset: 0 }))
    }

    fn rd(t: &mut VirtioMmio, off: u64) -> u32 {
        t.read(off, 4) as u32
    }

    #[test]
    fn identificazione_e_feature() {
        let mut t = eco();
        assert_eq!(rd(&mut t, MAGIC_VALUE), MAGIC);
        assert_eq!(rd(&mut t, VERSION), 2);
        assert_eq!(rd(&mut t, DEVICE_ID), 42);
        assert_eq!(rd(&mut t, VENDOR_ID), VENDOR);
        assert_eq!(rd(&mut t, DEVICE_FEATURES), 0b101 | (1 << 28) | (1 << 29));
        t.write(DEVICE_FEATURES_SEL, 4, 1);
        assert_eq!(rd(&mut t, DEVICE_FEATURES), 1, "VERSION_1 is bit 32");
        t.write(DEVICE_FEATURES_SEL, 4, 2);
        assert_eq!(rd(&mut t, DEVICE_FEATURES), 0);
        let t = eco().without_features(F_EVENT_IDX | F_VERSION_1);
        assert_eq!(t.offered_features(), 0b101 | F_INDIRECT_DESC | F_VERSION_1);
    }

    #[test]
    fn slot_vuoto_come_quello_di_qemu() {
        let mut t = VirtioMmio::empty();
        assert_eq!(rd(&mut t, MAGIC_VALUE), MAGIC);
        assert_eq!(rd(&mut t, VERSION), 2);
        assert_eq!(rd(&mut t, DEVICE_ID), 0);
        assert_eq!(rd(&mut t, VENDOR_ID), 0);
        t.write(STATUS, 4, 1);
        assert_eq!(rd(&mut t, STATUS), 0);
        assert_eq!(t.read(CONFIG, 4), 0);
    }

    #[test]
    fn accessi_non_a_32_bit_ignorati() {
        let mut t = eco();
        assert_eq!(t.read(MAGIC_VALUE, 2), 0);
        assert_eq!(t.read(MAGIC_VALUE + 1, 4), 0);
        t.write(STATUS, 1, 1);
        assert_eq!(rd(&mut t, STATUS), 0);
    }

    fn negozia(t: &mut VirtioMmio, lo: u32, hi: u32) -> u32 {
        t.write(STATUS, 4, 0);
        t.write(STATUS, 4, u64::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER));
        t.write(DRIVER_FEATURES_SEL, 4, 0);
        t.write(DRIVER_FEATURES, 4, u64::from(lo));
        t.write(DRIVER_FEATURES_SEL, 4, 1);
        t.write(DRIVER_FEATURES, 4, u64::from(hi));
        t.write(STATUS, 4, u64::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK));
        rd(t, STATUS)
    }

    #[test]
    fn features_ok_richiede_version_1_e_sottoinsieme() {
        let mut t = eco();
        assert_eq!(negozia(&mut t, 0b1, 0) & STATUS_FEATURES_OK, 0, "without VERSION_1");
        assert_eq!(negozia(&mut t, 0b10, 1) & STATUS_FEATURES_OK, 0, "bit not offered");
        assert_ne!(negozia(&mut t, 0b1, 1) & STATUS_FEATURES_OK, 0);
        assert_eq!(t.driver_features(), F_VERSION_1 | 1);
        // After FEATURES_OK DriverFeatures no longer changes.
        t.write(DRIVER_FEATURES_SEL, 4, 0);
        t.write(DRIVER_FEATURES, 4, 0b100);
        assert_eq!(t.driver_features(), F_VERSION_1 | 1);
        t.device_as_mut::<Eco>().unwrap().rifiuta = true;
        assert_eq!(negozia(&mut t, 0b1, 1) & STATUS_FEATURES_OK, 0, "rejected by the device");
    }

    #[test]
    fn configurazione_delle_code_e_reset() {
        let mut t = eco();
        negozia(&mut t, 0, 1);
        t.write(QUEUE_SEL, 4, 1);
        assert_eq!(rd(&mut t, QUEUE_NUM_MAX), 8);
        t.write(QUEUE_NUM, 4, 4);
        for (reg, v) in [(QUEUE_DESC_LOW, 0x1000), (QUEUE_DESC_HIGH, 1), (QUEUE_DRIVER_LOW, 0x2000)] {
            t.write(reg, 4, v);
        }
        t.write(QUEUE_DEVICE_LOW, 4, 0x3000);
        t.write(QUEUE_DEVICE_HIGH, 4, 2);
        t.write(QUEUE_READY, 4, 1);
        assert_eq!(rd(&mut t, QUEUE_READY), 1);
        let q = t.queue(1).unwrap();
        assert_eq!(q.size(), 4);
        assert_eq!(q.addrs(), (0x1_0000_1000, 0x2000, 0x2_0000_3000));
        assert_eq!(rd(&mut t, QUEUE_DESC_LOW), 0, "write-only register");
        // Queue ready: the configuration no longer changes.
        t.write(QUEUE_NUM, 4, 2);
        t.write(QUEUE_DESC_LOW, 4, 0x5000);
        assert_eq!(t.queue(1).unwrap().size(), 4);
        assert_eq!(t.queue(1).unwrap().addrs().0, 0x1_0000_1000);
        // Nonexistent queue.
        t.write(QUEUE_SEL, 4, 2);
        assert_eq!(rd(&mut t, QUEUE_NUM_MAX), 0);
        assert_eq!(rd(&mut t, QUEUE_READY), 0);
        // Reset.
        t.write(STATUS, 4, 0);
        assert_eq!(rd(&mut t, STATUS), 0);
        assert_eq!(t.driver_features(), 0);
        assert!(!t.queue(1).unwrap().ready());
        assert_eq!(t.queue(1).unwrap().size(), 8);
        assert_eq!(t.device_as::<Eco>().unwrap().reset, 3, "set_device and Status = 0 twice");
    }

    #[test]
    fn coda_non_potenza_di_2_non_diventa_pronta() {
        let mut t = eco();
        t.write(QUEUE_NUM, 4, 12);
        t.write(QUEUE_READY, 4, 1);
        assert_eq!(rd(&mut t, QUEUE_READY), 0);
        t.write(QUEUE_NUM, 4, 0x1_0010);
        t.write(QUEUE_READY, 4, 1);
        assert_eq!(rd(&mut t, QUEUE_READY), 0);
    }

    #[test]
    fn spazio_di_configurazione() {
        let mut t = eco();
        assert_eq!(t.read(CONFIG, 1), 1);
        assert_eq!(t.read(CONFIG + 2, 2), 0x0403);
        assert_eq!(t.read(CONFIG + 4, 4), 0x0807_0605);
        assert_eq!(t.read(CONFIG, 8), 0x0807_0605_0403_0201);
        assert_eq!(t.read(CONFIG + 6, 4), 0x0807, "past the end it reads 0");
        t.write(CONFIG + 1, 1, 0xAA);
        assert_eq!(t.read(CONFIG, 2), 0xAA01);
    }

    #[test]
    fn shm_assente_e_config_generation() {
        let mut t = eco();
        t.write(SHM_SEL, 4, 0);
        assert_eq!(rd(&mut t, SHM_LEN_LOW), u32::MAX);
        assert_eq!(rd(&mut t, SHM_LEN_HIGH), u32::MAX);
        assert_eq!(rd(&mut t, CONFIG_GENERATION), 0);
        t.signal_config_change();
        assert_eq!(rd(&mut t, CONFIG_GENERATION), 1);
        assert_eq!(rd(&mut t, INTERRUPT_STATUS), 0, "no interrupt without DRIVER_OK");
    }
}
