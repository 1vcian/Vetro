//! virtio-input (virtio v1.2, §5.8): tastiera, tablet e touchscreen.
//!
//! Code: 0 = eventi (dispositivo → driver, buffer da 8 byte), 1 = stato
//! (driver → dispositivo, es. i LED della tastiera). Nessuna feature.
//!
//! Lo spazio di configurazione è una finestra: il driver scrive `select` e
//! `subsel` e legge `size` e il contenuto (nome, seriale, identificativi,
//! bitmap delle proprietà e dei tipi di evento, intervalli degli assi). Le
//! voci sono dati ([`InputConfig`]); i profili [`InputConfig::keyboard`],
//! [`InputConfig::tablet`] e [`InputConfig::multitouch`] riproducono
//! `virtio-keyboard-device`, `virtio-tablet-device` (con `wheel-axis`, il
//! default) e `virtio-multitouch-device` di QEMU 10.0 (hw/input/
//! virtio-input-hid.c), così il guest vede gli stessi valori.
//!
//! Scelte, allineate a QEMU (hw/input/virtio-input.c) dove la spec lascia
//! margine:
//! - con `select`/`subsel` senza voce si legge tutto 0, anche `select`;
//! - gli eventi iniettati prima che il driver sia attivo (DRIVER_OK) si
//!   scartano, come fa QEMU;
//! - gli eventi si consegnano a rapporti interi (fino a un `EV_SYN/
//!   SYN_REPORT`) e solo se ci sono buffer per tutto il rapporto. QEMU in
//!   quel caso scarta il rapporto; Vetro lo tiene in coda (al più
//!   [`MAX_PENDING`] eventi, oltre si scarta e si conta in
//!   [`VirtioInput::dropped`]) e lo consegna quando il driver restituisce
//!   buffer: l'ingresso dell'host non si perde per una questione di tempi;
//! - la coda di stato registra gli eventi ricevuti (tipo `EV_LED` aggiorna
//!   [`VirtioInput::leds`]); la lunghezza nello used ring è 0 (nessun byte
//!   scritto, come chiede §2.7.8; QEMU mette i byte letti).

use std::collections::VecDeque;

use super::*;

pub const CFG_UNSET: u8 = 0x00;
pub const CFG_ID_NAME: u8 = 0x01;
pub const CFG_ID_SERIAL: u8 = 0x02;
pub const CFG_ID_DEVIDS: u8 = 0x03;
pub const CFG_PROP_BITS: u8 = 0x10;
pub const CFG_EV_BITS: u8 = 0x11;
pub const CFG_ABS_INFO: u8 = 0x12;

// Tipi e codici di evento di Linux (include/uapi/linux/input-event-codes.h).
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_LED: u16 = 0x11;
pub const EV_REP: u16 = 0x14;
pub const SYN_REPORT: u16 = 0;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_GEAR_DOWN: u16 = 0x150;
pub const BTN_GEAR_UP: u16 = 0x151;
pub const LED_NUML: u16 = 0;
pub const LED_CAPSL: u16 = 1;
pub const LED_SCROLLL: u16 = 2;
pub const INPUT_PROP_DIRECT: u16 = 0x01;
/// `BUS_VIRTUAL` di linux/input.h.
pub const BUS_VIRTUAL: u16 = 0x06;

/// Coordinate assolute di tablet e touchscreen: 0..=0x7FFF, come QEMU
/// (INPUT_EVENT_ABS_MAX), indipendenti dalla risoluzione dello schermo.
pub const ABS_MAX_VALUE: u32 = 0x7FFF;
/// Slot del touchscreen: 0..=10 (INPUT_EVENT_SLOTS_MAX di QEMU).
pub const MT_SLOTS_MAX: u32 = 10;

/// Eventi al più in attesa di buffer del driver.
pub const MAX_PENDING: usize = 4096;

const EVENTQ: usize = 0;
const STATUSQ: usize = 1;
const EVENT_LEN: usize = 8;

/// `struct virtio_input_event`: tipo, codice e valore di un evento evdev.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub ty: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    pub fn new(ty: u16, code: u16, value: i32) -> Self {
        Self { ty, code, value: value as u32 }
    }

    pub fn syn() -> Self {
        Self::new(EV_SYN, SYN_REPORT, 0)
    }

    fn is_report(&self) -> bool {
        self.ty == EV_SYN && self.code == SYN_REPORT
    }

    fn to_bytes(self) -> [u8; EVENT_LEN] {
        let mut b = [0u8; EVENT_LEN];
        b[0..2].copy_from_slice(&self.ty.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; EVENT_LEN]) -> Self {
        Self {
            ty: u16::from_le_bytes([b[0], b[1]]),
            code: u16::from_le_bytes([b[2], b[3]]),
            value: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
}

/// `struct virtio_input_absinfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: u32,
    pub max: u32,
    pub fuzz: u32,
    pub flat: u32,
    pub res: u32,
}

/// Una voce dello spazio di configurazione: (select, subsel) -> contenuto.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CfgEntry {
    select: u8,
    subsel: u8,
    data: Vec<u8>,
}

/// Descrizione del dispositivo come la vede il driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputConfig {
    entries: Vec<CfgEntry>,
}

/// Bitmap con i bit `codes` accesi, lunga fino al byte dell'ultimo bit
/// (come `virtio_input_extend_config` di QEMU).
fn bitmap(codes: &[u16]) -> Vec<u8> {
    let len = codes.iter().map(|&c| usize::from(c) / 8 + 1).max().unwrap_or(0);
    let mut b = vec![0u8; len];
    for &c in codes {
        b[usize::from(c) / 8] |= 1 << (c % 8);
    }
    b
}

/// Tasti di `virtio-keyboard-device`, a intervalli chiusi: i codici Linux
/// della tabella qcode → linux di QEMU (`qemu_input_map_qcode_to_linux`),
/// letti dal guest sotto QEMU 10.0 e 8.2 con EVIOCGBIT(EV_KEY): 159 tasti.
pub const KEYBOARD_KEY_RANGES: &[(u16, u16)] = &[
    (0x01, 0x53),
    (0x56, 0x59),
    (0x5b, 0x5e),
    (0x60, 0x6f),
    (0x71, 0x75),
    (0x77, 0x77),
    (0x79, 0x8c),
    (0x8e, 0x8f),
    (0x9b, 0x9f),
    (0xa3, 0xa6),
    (0xac, 0xad),
    (0xb7, 0xc2),
    (0xe2, 0xe2),
];

/// Pulsanti di tablet e touchscreen (`keymap_button` di QEMU).
pub const POINTER_BUTTONS: &[u16] =
    &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_GEAR_UP, BTN_GEAR_DOWN, BTN_SIDE, BTN_EXTRA, BTN_TOUCH];

impl InputConfig {
    /// Dispositivo con il solo nome.
    pub fn new(name: &str) -> Self {
        let mut c = Self { entries: Vec::new() };
        c.set(CFG_ID_NAME, 0, name.as_bytes().iter().copied().chain([0]).take(128).collect());
        c
    }

    fn set(&mut self, select: u8, subsel: u8, data: Vec<u8>) {
        self.entries.retain(|e| (e.select, e.subsel) != (select, subsel));
        self.entries.push(CfgEntry { select, subsel, data });
    }

    fn get(&self, select: u8, subsel: u8) -> Option<&CfgEntry> {
        self.entries.iter().find(|e| (e.select, e.subsel) == (select, subsel))
    }

    /// Numero di serie (ID_SERIAL), senza terminatore come QEMU.
    pub fn serial(mut self, serial: &str) -> Self {
        self.set(CFG_ID_SERIAL, 0, serial.as_bytes().iter().copied().take(127).collect());
        self
    }

    /// Identificativi (ID_DEVIDS).
    pub fn devids(mut self, bustype: u16, vendor: u16, product: u16, version: u16) -> Self {
        let mut d = Vec::with_capacity(8);
        for v in [bustype, vendor, product, version] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        self.set(CFG_ID_DEVIDS, 0, d);
        self
    }

    /// Proprietà (PROP_BITS), es. [`INPUT_PROP_DIRECT`].
    pub fn props(mut self, props: &[u16]) -> Self {
        self.set(CFG_PROP_BITS, 0, bitmap(props));
        self
    }

    /// Codici del tipo `ty` (EV_BITS). Una lista vuota con `min_len` > 0
    /// dichiara il tipo senza codici (come EV_REP della tastiera di QEMU).
    pub fn events(mut self, ty: u16, codes: &[u16], min_len: usize) -> Self {
        let mut b = bitmap(codes);
        if b.len() < min_len {
            b.resize(min_len, 0);
        }
        self.set(CFG_EV_BITS, ty as u8, b);
        self
    }

    /// Intervallo dell'asse assoluto `axis` (ABS_INFO).
    pub fn abs(mut self, axis: u16, info: AbsInfo) -> Self {
        let mut d = Vec::with_capacity(20);
        for v in [info.min, info.max, info.fuzz, info.flat, info.res] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        self.set(CFG_ABS_INFO, axis as u8, d);
        self
    }

    /// `virtio-keyboard-device` di QEMU.
    pub fn keyboard() -> Self {
        Self::new("QEMU Virtio Keyboard")
            .devids(BUS_VIRTUAL, 0x0627, 0x0001, 0x0001)
            .events(EV_REP, &[], 1)
            .events(EV_LED, &[LED_NUML, LED_CAPSL, LED_SCROLLL], 1)
            .events(EV_KEY, &KEYBOARD_KEY_RANGES.iter().flat_map(|&(a, b)| a..=b).collect::<Vec<_>>(), 0)
    }

    /// `virtio-tablet-device` di QEMU (con la rotella, `wheel-axis=on`):
    /// puntatore assoluto con pulsanti.
    pub fn tablet() -> Self {
        let axis = AbsInfo { max: ABS_MAX_VALUE, ..AbsInfo::default() };
        Self::new("QEMU Virtio Tablet")
            .devids(BUS_VIRTUAL, 0x0627, 0x0003, 0x0002)
            .events(EV_ABS, &[ABS_X, ABS_Y], 1)
            .events(EV_REL, &[REL_WHEEL], 2)
            .abs(ABS_X, axis)
            .abs(ABS_Y, axis)
            .events(EV_KEY, POINTER_BUTTONS, 0)
    }

    /// `virtio-multitouch-device` di QEMU: touchscreen diretto a più
    /// contatti (protocollo B di Linux, con slot e tracking id).
    pub fn multitouch() -> Self {
        let axis = AbsInfo { max: ABS_MAX_VALUE, ..AbsInfo::default() };
        let slots = AbsInfo { max: MT_SLOTS_MAX, ..AbsInfo::default() };
        Self::new("QEMU Virtio MultiTouch")
            .devids(BUS_VIRTUAL, 0x0627, 0x0003, 0x0001)
            .abs(ABS_MT_SLOT, slots)
            .abs(ABS_MT_TRACKING_ID, slots)
            .abs(ABS_MT_POSITION_X, axis)
            .abs(ABS_MT_POSITION_Y, axis)
            .events(EV_KEY, POINTER_BUTTONS, 0)
            .props(&[INPUT_PROP_DIRECT])
            .events(EV_ABS, &[ABS_MT_SLOT, ABS_MT_TRACKING_ID, ABS_MT_POSITION_X, ABS_MT_POSITION_Y], 0)
    }
}

pub struct VirtioInput {
    config: InputConfig,
    select: u8,
    subsel: u8,
    /// Il driver ha completato l'inizializzazione (primo `service` dopo
    /// DRIVER_OK) e non c'è stato un reset.
    active: bool,
    pending: VecDeque<InputEvent>,
    /// Scarta fino al prossimo SYN_REPORT (rapporto troncato dal limite).
    discarding: bool,
    dropped: u64,
    leds: u32,
    status: Vec<InputEvent>,
    queue_sizes: [u16; 2],
}

impl VirtioInput {
    /// Code da 64 come QEMU.
    pub fn new(config: InputConfig) -> Self {
        Self {
            config,
            select: 0,
            subsel: 0,
            active: false,
            pending: VecDeque::new(),
            discarding: false,
            dropped: 0,
            leds: 0,
            status: Vec::new(),
            queue_sizes: [64, 64],
        }
    }

    pub fn config(&self) -> &InputConfig {
        &self.config
    }

    /// Accoda eventi per il guest; partono al prossimo `service`, a
    /// rapporti interi. Senza driver attivo si scartano (come QEMU).
    pub fn inject(&mut self, events: &[InputEvent]) {
        for &e in events {
            if !self.active {
                self.dropped += 1;
                continue;
            }
            if self.discarding || self.pending.len() >= MAX_PENDING {
                // Il rapporto non entra: si scarta fino alla sua fine,
                // compresi gli eventi già accodati dopo l'ultimo SYN_REPORT.
                if !self.discarding {
                    while self.pending.back().is_some_and(|p| !p.is_report()) {
                        self.pending.pop_back();
                        self.dropped += 1;
                    }
                }
                self.discarding = !e.is_report();
                self.dropped += 1;
                continue;
            }
            self.pending.push_back(e);
        }
    }

    /// Un tasto (codice Linux `KEY_*`/`BTN_*`) premuto o rilasciato, con
    /// SYN_REPORT.
    pub fn key(&mut self, code: u16, down: bool) {
        self.inject(&[InputEvent::new(EV_KEY, code, down.into()), InputEvent::syn()]);
    }

    /// Posizione assoluta (0..=[`ABS_MAX_VALUE`]) del tablet, con SYN_REPORT.
    pub fn move_abs(&mut self, x: u32, y: u32) {
        self.inject(&[
            InputEvent { ty: EV_ABS, code: ABS_X, value: x },
            InputEvent { ty: EV_ABS, code: ABS_Y, value: y },
            InputEvent::syn(),
        ]);
    }

    /// Un contatto del touchscreen nello slot `slot`: `Some((x, y))` lo
    /// mette o lo sposta (tracking id = slot), `None` lo toglie. Con
    /// BTN_TOUCH e SYN_REPORT, come QEMU.
    pub fn touch(&mut self, slot: u32, pos: Option<(u32, u32)>) {
        let mut ev = vec![
            InputEvent { ty: EV_ABS, code: ABS_MT_SLOT, value: slot },
            InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, if pos.is_some() { slot as i32 } else { -1 }),
        ];
        if let Some((x, y)) = pos {
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_X, value: x });
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_Y, value: y });
        }
        ev.push(InputEvent::new(EV_KEY, BTN_TOUCH, pos.is_some().into()));
        ev.push(InputEvent::syn());
        self.inject(&ev);
    }

    /// Eventi in attesa di buffer del driver.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Eventi scartati (driver non attivo o coda piena).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// LED accesi dal guest: bit `LED_*` (NUML = bit 0, CAPSL = 1, ...).
    pub fn leds(&self) -> u32 {
        self.leds
    }

    /// Eventi arrivati sulla coda di stato, dal più vecchio.
    pub fn take_status(&mut self) -> Vec<InputEvent> {
        core::mem::take(&mut self.status)
    }

    fn window(&self) -> [u8; 136] {
        let mut w = [0u8; 136];
        if let Some(e) = self.config.get(self.select, self.subsel) {
            w[0] = e.select;
            w[1] = e.subsel;
            w[2] = e.data.len() as u8;
            w[8..8 + e.data.len()].copy_from_slice(&e.data);
        }
        w
    }

    fn deliver(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        loop {
            let Some(end) = self.pending.iter().position(InputEvent::is_report) else { return Ok(()) };
            let n = end + 1;
            if usize::from(q.available(ram)?) < n {
                return Ok(());
            }
            for e in self.pending.drain(..n) {
                let c = q.pop(ram)?.expect("catene contate da available");
                let w = c.write(ram, 0, &e.to_bytes())?;
                q.push_used(ram, c.head, w as u32)?;
            }
        }
    }

    fn status_queue(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        while let Some(c) = q.pop(ram)? {
            let mut b = [0u8; EVENT_LEN];
            c.read(ram, 0, &mut b)?;
            let e = InputEvent::from_bytes(&b);
            if e.ty == EV_LED && e.code < 32 {
                if e.value != 0 {
                    self.leds |= 1 << e.code;
                } else {
                    self.leds &= !(1 << e.code);
                }
            }
            self.status.push(e);
            q.push_used(ram, c.head, 0)?;
        }
        Ok(())
    }
}

impl VirtioDevice for VirtioInput {
    fn device_id(&self) -> u32 {
        ID_INPUT
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.window(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset + i as u64 {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
    }

    fn reset(&mut self) {
        self.select = 0;
        self.subsel = 0;
        self.active = false;
        self.discarding = false;
        self.dropped += self.pending.len() as u64;
        self.pending.clear();
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        self.active = true;
        let (queues, ram) = (&mut *ctx.queues, &mut *ctx.ram);
        self.status_queue(&mut queues[STATUSQ], ram)?;
        self.deliver(&mut queues[EVENTQ], ram)
    }

    /// Finestra di configurazione scelta, eventi in attesa, LED, eventi di
    /// stato del guest, contatori. La configurazione (i bit evdev e gli assi
    /// dichiarati) si controlla con un hash.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.config_hash());
        w.u8(self.select);
        w.u8(self.subsel);
        w.bool(self.active);
        let ev = |w: &mut vetro_snapshot::Writer, e: &InputEvent| w.raw(&e.to_bytes());
        w.seq(&self.pending, ev);
        w.bool(self.discarding);
        w.u64(self.dropped);
        w.u32(self.leds);
        w.seq(&self.status, ev);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("configurazione di virtio-input", self.config_hash())?;
        self.select = r.u8()?;
        self.subsel = r.u8()?;
        self.active = r.bool()?;
        let ev = |r: &mut vetro_snapshot::Reader<'_>| {
            Ok(InputEvent::from_bytes(r.raw(EVENT_LEN)?.try_into().expect("8 byte")))
        };
        self.pending = r.seq(EVENT_LEN, ev)?.into();
        self.discarding = r.bool()?;
        self.dropped = r.u64()?;
        self.leds = r.u32()?;
        self.status = r.seq(EVENT_LEN, ev)?;
        Ok(())
    }
}

impl VirtioInput {
    fn config_hash(&self) -> u64 {
        let mut w = vetro_snapshot::Writer::new();
        w.seq(&self.config.entries, |w, e| {
            w.u8(e.select);
            w.u8(e.subsel);
            w.bytes(&e.data);
        });
        vetro_snapshot::hash64(w.as_bytes())
    }
}

#[cfg(test)]
mod tests;
