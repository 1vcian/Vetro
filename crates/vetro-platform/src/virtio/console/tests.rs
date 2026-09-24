use super::*;
use crate::virtio::testdrv::{Driver, Transport};

fn driver() -> Driver<VirtioMmio> {
    let mut d =
        Driver::new(VirtioMmio::new(Box::new(VirtioConsole::new(Box::new(BufferConsole::default())))));
    d.init(u64::MAX, 8);
    d
}

fn backend(d: &mut Driver<VirtioMmio>) -> &mut BufferConsole {
    d.t.device_as_mut::<VirtioConsole>().unwrap().backend_as_mut().unwrap()
}

#[test]
fn configurazione_e_feature() {
    let mut d = driver();
    assert_eq!(d.t.rd(DEVICE_ID), ID_CONSOLE);
    assert_eq!(d.features & 0xFF_FFFF, F_EMERG_WRITE);
    assert_eq!(d.t.cfg(4, 4), 1, "max_nr_ports");
    for q in 0..2 {
        d.t.wr(QUEUE_SEL, q);
        assert_eq!(d.t.rd(QUEUE_NUM_MAX), 128);
    }
    d.t.wr(QUEUE_SEL, 2);
    assert_eq!(d.t.rd(QUEUE_NUM_MAX), 0, "senza MULTIPORT solo due code");
}

#[test]
fn trasmissione_verso_il_backend() {
    let mut d = driver();
    let a = d.buf(b"adb ");
    let b = d.buf(b"shell\n");
    let head = d.add(TXQ, &[(a, 4, false), (b, 6, false)]);
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(TXQ), Some((head, 0)));
    assert_eq!(backend(&mut d).output, b"adb shell\n");

    // Più di un pezzo da 64 KiB.
    let big: Vec<u8> = (0..150_000u32).map(|i| (i % 253) as u8).collect();
    let p = d.buf(&big);
    d.add(TXQ, &[(p, big.len() as u32, false)]);
    d.service();
    assert!(d.pop_used(TXQ).is_some());
    assert_eq!(&backend(&mut d).output[10..], &big[..]);
}

#[test]
fn ricezione_dal_backend() {
    let mut d = driver();
    let bufs: Vec<u64> = (0..2)
        .map(|_| {
            let a = d.alloc(8, 8);
            d.add(RXQ, &[(a, 8, true)]);
            a
        })
        .collect();
    d.service();
    assert_eq!(d.pop_used(RXQ), None, "niente input: i buffer restano al driver");
    assert_eq!(d.irq(), 0);
    backend(&mut d).input.extend(b"0123456789AB");
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(8));
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(4));
    assert_eq!(d.mem(bufs[0], 8), b"01234567");
    assert_eq!(d.mem(bufs[1], 4), b"89AB");
    assert!(backend(&mut d).input.is_empty());
    // Byte in più senza buffer: restano nel backend.
    backend(&mut d).input.extend(b"z");
    d.service();
    assert_eq!(backend(&mut d).input.len(), 1);
}

#[test]
fn scrittura_di_emergenza() {
    let mut d = driver();
    d.t.cfg_wr(CFG_EMERG_WR, 4, u64::from(b'!'));
    d.t.cfg_wr(CFG_EMERG_WR, 1, u64::from(b'?'));
    assert_eq!(backend(&mut d).output, b"!");
}
