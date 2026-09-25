use super::*;
use crate::virtio::testdrv::{Driver, Transport};

fn driver(cfg: InputConfig) -> Driver<VirtioMmio> {
    let mut d = Driver::new(VirtioMmio::new(Box::new(VirtioInput::new(cfg))));
    d.init(u64::MAX, 64);
    d
}

fn dev(d: &mut Driver<VirtioMmio>) -> &mut VirtioInput {
    d.t.device_as_mut::<VirtioInput>().unwrap()
}

/// Legge una voce come `virtinput_cfg_select` di Linux: scrive select e
/// subsel, legge size e poi i byte.
fn cfg(d: &mut Driver<VirtioMmio>, select: u8, subsel: u8) -> Vec<u8> {
    d.t.cfg_wr(0, 1, u64::from(select));
    d.t.cfg_wr(1, 1, u64::from(subsel));
    let size = d.t.cfg(2, 1) as usize;
    (0..size).map(|i| d.t.cfg(8 + i as u64, 1) as u8).collect()
}

/// Offre `n` buffer da 8 byte sulla coda eventi: (testa, indirizzo).
fn offer(d: &mut Driver<VirtioMmio>, n: usize) -> Vec<(u16, u64)> {
    (0..n)
        .map(|_| {
            let a = d.alloc(8, 8);
            (d.add(EVENTQ, &[(a, 8, true)]), a)
        })
        .collect()
}

/// Eventi restituiti nello used ring, in ordine.
fn received(d: &mut Driver<VirtioMmio>, bufs: &[(u16, u64)]) -> Vec<InputEvent> {
    let mut out = Vec::new();
    while let Some((head, len)) = d.pop_used(EVENTQ) {
        assert_eq!(len, 8);
        let &(_, a) = bufs.iter().find(|b| b.0 == head).expect("testa sconosciuta");
        out.push(InputEvent::from_bytes(&d.mem(a, 8).try_into().unwrap()));
    }
    out
}

#[test]
fn tastiera_come_qemu() {
    let mut d = driver(InputConfig::keyboard());
    assert_eq!(d.t.rd(DEVICE_ID), ID_INPUT);
    assert_eq!(d.features & 0xFF_FFFF, 0);
    for q in 0..2 {
        d.t.wr(QUEUE_SEL, q);
        assert_eq!(d.t.rd(QUEUE_NUM_MAX), 64);
    }
    assert_eq!(cfg(&mut d, CFG_ID_NAME, 0), b"QEMU Virtio Keyboard\0");
    assert_eq!(cfg(&mut d, CFG_ID_SERIAL, 0), b"", "senza seriale, come QEMU");
    assert_eq!(cfg(&mut d, CFG_ID_DEVIDS, 0), [6, 0, 0x27, 0x06, 1, 0, 1, 0]);
    assert_eq!(cfg(&mut d, CFG_EV_BITS, EV_REP as u8), [0], "EV_REP senza codici");
    assert_eq!(cfg(&mut d, CFG_EV_BITS, EV_LED as u8), [7]);
    let keys = cfg(&mut d, CFG_EV_BITS, EV_KEY as u8);
    // Lo stesso che Linux mostra in /proc/bus/input/devices sotto QEMU:
    // KEY=400000007 ff803078f800dfff febeffff7bcfffff fffffffffffffffe
    let words: Vec<u64> =
        keys.chunks(8).map(|c| c.iter().rev().fold(0u64, |a, &b| (a << 8) | u64::from(b))).collect();
    assert_eq!(words, [0xfffffffffffffffe, 0xfebeffff7bcfffff, 0xff803078f800dfff, 0x0000000400000007]);
    let n: u32 = keys.iter().map(|b| b.count_ones()).sum();
    assert_eq!(n, 159);
    assert_eq!(cfg(&mut d, CFG_EV_BITS, EV_ABS as u8), b"");
    assert_eq!(cfg(&mut d, CFG_PROP_BITS, 0), b"");
    // Voce assente: anche select e subsel si leggono 0 (come QEMU).
    d.t.cfg_wr(0, 1, u64::from(CFG_ABS_INFO));
    assert_eq!(d.t.cfg(0, 4), 0);
}

#[test]
fn tablet_e_touchscreen_come_qemu() {
    let mut d = driver(InputConfig::tablet());
    assert_eq!(cfg(&mut d, CFG_ID_NAME, 0), b"QEMU Virtio Tablet\0");
    assert_eq!(cfg(&mut d, CFG_ID_DEVIDS, 0), [6, 0, 0x27, 0x06, 3, 0, 2, 0]);
    // /proc/bus/input/devices sotto QEMU: EV=f KEY=30400 1f0000 0 0 0 0 REL=100 ABS=3.
    assert_eq!(cfg(&mut d, CFG_EV_BITS, EV_ABS as u8), [3]);
    assert_eq!(cfg(&mut d, CFG_EV_BITS, EV_REL as u8), [0, 1]);
    let keys = cfg(&mut d, CFG_EV_BITS, EV_KEY as u8);
    assert_eq!(keys.len(), 0x151 / 8 + 1);
    assert_eq!(&keys[0x110 / 8..], [0x1f, 0, 0, 0, 0, 0, 0, 0x04, 0x03]);
    let abs = cfg(&mut d, CFG_ABS_INFO, ABS_Y as u8);
    assert_eq!(abs, [0, 0, 0, 0, 0xff, 0x7f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    let mut d = driver(InputConfig::multitouch());
    assert_eq!(cfg(&mut d, CFG_ID_NAME, 0), b"QEMU Virtio MultiTouch\0");
    assert_eq!(cfg(&mut d, CFG_PROP_BITS, 0), [2], "INPUT_PROP_DIRECT");
    // ABS=260800000000000: bit 0x2f, 0x35, 0x36, 0x39.
    let abs = cfg(&mut d, CFG_EV_BITS, EV_ABS as u8);
    let v = abs.iter().rev().fold(0u64, |a, &b| (a << 8) | u64::from(b));
    assert_eq!(v, 0x260800000000000);
    assert_eq!(cfg(&mut d, CFG_ABS_INFO, ABS_MT_SLOT as u8)[4], 10);
}

#[test]
fn eventi_a_rapporti_interi() {
    let mut d = driver(InputConfig::keyboard());
    let bufs = offer(&mut d, 3);
    d.service();
    assert_eq!(d.irq(), 0, "nessun evento");
    dev(&mut d).key(30, true); // KEY_A: 2 eventi
    dev(&mut d).key(30, false); // altri 2: il secondo rapporto non entra
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    let got = received(&mut d, &bufs);
    assert_eq!(got, [InputEvent::new(EV_KEY, 30, 1), InputEvent::syn()]);
    assert_eq!(dev(&mut d).pending(), 2, "il rilascio aspetta i buffer");
    // Rapporto incompleto (senza SYN): non parte.
    dev(&mut d).inject(&[InputEvent::new(EV_KEY, 31, 1)]);
    let mut bufs = bufs;
    bufs.extend(offer(&mut d, 4));
    d.service();
    let got = received(&mut d, &bufs);
    assert_eq!(got, [InputEvent::new(EV_KEY, 30, 0), InputEvent::syn()]);
    dev(&mut d).inject(&[InputEvent::syn()]);
    d.service();
    // Con il SYN il rapporto è completo e parte.
    assert_eq!(dev(&mut d).pending(), 0);
    assert_eq!(dev(&mut d).dropped(), 0);
}

#[test]
fn senza_driver_attivo_si_scarta() {
    let mut d = Driver::new(VirtioMmio::new(Box::new(VirtioInput::new(InputConfig::tablet()))));
    dev(&mut d).move_abs(100, 200);
    assert_eq!(dev(&mut d).dropped(), 3);
    d.init(u64::MAX, 64);
    let bufs = offer(&mut d, 8);
    d.service();
    dev(&mut d).move_abs(100, 200);
    d.service();
    let got = received(&mut d, &bufs);
    assert_eq!(
        got,
        [InputEvent::new(EV_ABS, ABS_X, 100), InputEvent::new(EV_ABS, ABS_Y, 200), InputEvent::syn()]
    );
    // Reset: il driver non è più attivo.
    d.t.wr(STATUS, 0);
    dev(&mut d).move_abs(1, 2);
    assert_eq!(dev(&mut d).pending(), 0);
}

#[test]
fn coda_piena_scarta_rapporti_interi() {
    let mut d = driver(InputConfig::keyboard());
    d.service();
    for _ in 0..MAX_PENDING / 2 {
        dev(&mut d).key(30, true);
    }
    assert_eq!(dev(&mut d).pending(), MAX_PENDING);
    dev(&mut d).key(31, true);
    dev(&mut d).inject(&[InputEvent::new(EV_KEY, 32, 1)]);
    assert_eq!(dev(&mut d).pending(), MAX_PENDING);
    assert_eq!(dev(&mut d).dropped(), 3);
    // Senza la fine del rapporto troncato si scarta anche il SYN.
    dev(&mut d).inject(&[InputEvent::syn()]);
    assert_eq!(dev(&mut d).dropped(), 4);
}

#[test]
fn touch_protocollo_b() {
    let mut d = driver(InputConfig::multitouch());
    let bufs = offer(&mut d, 16);
    d.service();
    dev(&mut d).touch(1, Some((10, 20)));
    dev(&mut d).touch(1, None);
    d.service();
    let got = received(&mut d, &bufs);
    let e = InputEvent::new;
    assert_eq!(
        got,
        [
            e(EV_ABS, ABS_MT_SLOT, 1),
            e(EV_ABS, ABS_MT_TRACKING_ID, 1),
            e(EV_ABS, ABS_MT_POSITION_X, 10),
            e(EV_ABS, ABS_MT_POSITION_Y, 20),
            e(EV_KEY, BTN_TOUCH, 1),
            InputEvent::syn(),
            e(EV_ABS, ABS_MT_SLOT, 1),
            e(EV_ABS, ABS_MT_TRACKING_ID, -1),
            e(EV_KEY, BTN_TOUCH, 0),
            InputEvent::syn(),
        ]
    );
}

#[test]
fn led_dalla_coda_di_stato() {
    let mut d = driver(InputConfig::keyboard());
    let on = d.buf(&InputEvent::new(EV_LED, LED_CAPSL, 1).to_bytes());
    let head = d.add(STATUSQ, &[(on, 8, false)]);
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(STATUSQ), Some((head, 0)));
    assert_eq!(dev(&mut d).leds(), 1 << LED_CAPSL);
    let off = d.buf(&InputEvent::new(EV_LED, LED_CAPSL, 0).to_bytes());
    d.add(STATUSQ, &[(off, 8, false)]);
    d.service();
    assert_eq!(dev(&mut d).leds(), 0);
    assert_eq!(dev(&mut d).take_status().len(), 2);
}
