//! Trasporto virtio-mmio versione 2 (virtio v1.2, §4.2.2).
//!
//! Scelte, allineate a QEMU (hw/virtio/virtio-mmio.c) dove la spec lascia
//! margine:
//! - i registri sotto 0x100 si accedono solo a 32 bit allineati; gli altri
//!   accessi leggono 0 e non scrivono nulla;
//! - i registri di sola scrittura (QueueNum, QueueDesc*, ...) si leggono 0;
//! - VIRTIO_F_VERSION_1 è sempre offerto e obbligatorio: se il driver non
//!   lo accetta, o accetta feature non offerte, o il dispositivo rifiuta la
//!   combinazione, FEATURES_OK non resta impostato in Status;
//! - DriverFeatures dopo FEATURES_OK e la configurazione di una coda già
//!   pronta si ignorano;
//! - la regione di memoria condivisa (SHMSel) non esiste: SHMLen e SHMBase
//!   leggono tutti 1 (-1), come chiede la spec per una regione assente;
//! - un errore nelle code porta il dispositivo in DEVICE_NEEDS_RESET con
//!   interrupt di configurazione (§2.1.2) e ferma il servizio fino al reset;
//! - la linea di interrupt vale `InterruptStatus != 0`; nel device tree è
//!   dichiarata a fronte di salita, come in QEMU.

use core::any::Any;

use super::*;

pub struct VirtioMmio {
    device: Option<Box<dyn VirtioDevice>>,
    queues: Vec<Virtqueue>,
    /// Feature del trasporto tolte dall'offerta (per test e compatibilità).
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
    /// Slot senza dispositivo: si comporta come [`VirtioMmioEmpty`].
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

    /// Toglie dall'offerta alcune feature del trasporto (INDIRECT_DESC,
    /// EVENT_IDX). VERSION_1 non si può togliere.
    pub fn without_features(mut self, mask: u64) -> Self {
        self.removed = mask & !F_VERSION_1;
        self
    }

    /// Monta (o toglie) il dispositivo e riporta il trasporto al reset.
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

    /// Accesso tipizzato al dispositivo montato.
    pub fn device_as<T: VirtioDevice>(&self) -> Option<&T> {
        let d: &dyn Any = self.device.as_deref()?;
        d.downcast_ref()
    }

    pub fn device_as_mut<T: VirtioDevice>(&mut self) -> Option<&mut T> {
        let d: &mut dyn Any = self.device.as_deref_mut()?;
        d.downcast_mut()
    }

    /// Feature offerte al driver.
    pub fn offered_features(&self) -> u64 {
        match &self.device {
            Some(d) => (d.features() | F_VERSION_1 | F_INDIRECT_DESC | F_EVENT_IDX) & !self.removed,
            None => 0,
        }
    }

    /// Feature accettate dal driver (valide dopo FEATURES_OK).
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

    /// Ultimo errore che ha portato il dispositivo in DEVICE_NEEDS_RESET.
    pub fn last_error(&self) -> Option<QueueError> {
        self.last_error
    }

    /// Livello della linea di interrupt verso il GIC.
    pub fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Cambio della configurazione deciso dall'host (es. capacità del
    /// disco): ConfigGeneration avanza e parte l'interrupt, se il driver è
    /// già attivo.
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

    /// Fa lavorare il dispositivo: consuma le code (richieste del driver e
    /// dati pronti nei backend) e aggiorna InterruptStatus. Senza
    /// DRIVER_OK, o in DEVICE_NEEDS_RESET, non fa nulla.
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
        // Anche dopo un errore, i buffer già restituiti vanno notificati.
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
                    // Un valore oltre 16 bit è comunque invalido: 0 lo rende tale.
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
            // QueueNotify: il lavoro si fa in `service`. SHMSel: nessuna regione.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Dispositivo minimo: una coda che restituisce ogni catena con len 7.
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
        assert_eq!(rd(&mut t, DEVICE_FEATURES), 1, "VERSION_1 è il bit 32");
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
        assert_eq!(negozia(&mut t, 0b1, 0) & STATUS_FEATURES_OK, 0, "senza VERSION_1");
        assert_eq!(negozia(&mut t, 0b10, 1) & STATUS_FEATURES_OK, 0, "bit non offerto");
        assert_ne!(negozia(&mut t, 0b1, 1) & STATUS_FEATURES_OK, 0);
        assert_eq!(t.driver_features(), F_VERSION_1 | 1);
        // Dopo FEATURES_OK DriverFeatures non cambia più.
        t.write(DRIVER_FEATURES_SEL, 4, 0);
        t.write(DRIVER_FEATURES, 4, 0b100);
        assert_eq!(t.driver_features(), F_VERSION_1 | 1);
        t.device_as_mut::<Eco>().unwrap().rifiuta = true;
        assert_eq!(negozia(&mut t, 0b1, 1) & STATUS_FEATURES_OK, 0, "rifiutato dal dispositivo");
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
        assert_eq!(rd(&mut t, QUEUE_DESC_LOW), 0, "registro di sola scrittura");
        // Coda pronta: la configurazione non cambia più.
        t.write(QUEUE_NUM, 4, 2);
        t.write(QUEUE_DESC_LOW, 4, 0x5000);
        assert_eq!(t.queue(1).unwrap().size(), 4);
        assert_eq!(t.queue(1).unwrap().addrs().0, 0x1_0000_1000);
        // Coda inesistente.
        t.write(QUEUE_SEL, 4, 2);
        assert_eq!(rd(&mut t, QUEUE_NUM_MAX), 0);
        assert_eq!(rd(&mut t, QUEUE_READY), 0);
        // Reset.
        t.write(STATUS, 4, 0);
        assert_eq!(rd(&mut t, STATUS), 0);
        assert_eq!(t.driver_features(), 0);
        assert!(!t.queue(1).unwrap().ready());
        assert_eq!(t.queue(1).unwrap().size(), 8);
        assert_eq!(t.device_as::<Eco>().unwrap().reset, 3, "set_device e due volte Status = 0");
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
        assert_eq!(t.read(CONFIG + 6, 4), 0x0807, "oltre la fine si legge 0");
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
        assert_eq!(rd(&mut t, INTERRUPT_STATUS), 0, "senza DRIVER_OK niente interrupt");
    }
}
