use super::*;
use crate::virtio::testdrv::{Driver, Transport};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

fn driver(mrg: bool, wanted: u64) -> Driver<VirtioMmio> {
    let dev = VirtioNet::new(Box::new(QueueNet::default()), MAC).with_mrg_rxbuf(mrg);
    let mut d = Driver::new(VirtioMmio::new(Box::new(dev)));
    d.init(wanted, 16);
    d
}

fn backend(d: &mut Driver<VirtioMmio>) -> &mut QueueNet {
    d.t.device_as_mut::<VirtioNet>().unwrap().backend_as_mut().unwrap()
}

fn frame(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(3).wrapping_add(seed)).collect()
}

/// Pubblica `n` buffer di ricezione da `len` byte; restituisce gli indirizzi.
fn post_rx(d: &mut Driver<VirtioMmio>, n: usize, len: u32) -> Vec<u64> {
    (0..n)
        .map(|_| {
            let a = d.alloc(u64::from(len), 8);
            d.add(RXQ, &[(a, len, true)]);
            a
        })
        .collect()
}

#[test]
fn configurazione_e_feature() {
    let mut d = driver(true, u64::MAX);
    assert_eq!(d.t.rd(DEVICE_ID), ID_NET);
    let mac: Vec<u8> = (0..6).map(|i| d.t.cfg(i, 1) as u8).collect();
    assert_eq!(mac, MAC);
    assert_eq!(d.t.cfg(6, 2), u64::from(S_LINK_UP));
    assert_eq!(d.features & (F_MAC | F_STATUS | F_MRG_RXBUF), F_MAC | F_STATUS | F_MRG_RXBUF);
    let d = driver(false, u64::MAX);
    assert_eq!(d.features & F_MRG_RXBUF, 0);
    assert_eq!(d.features & 0x3F_FFFF & !(F_MAC | F_STATUS), 0, "niente offload");
}

#[test]
fn trasmissione() {
    let mut d = driver(true, u64::MAX);
    let f = frame(60, 1);
    let h = d.buf(&[0; NET_HDR_LEN]);
    let p = d.buf(&f);
    let head = d.add(TXQ, &[(h, NET_HDR_LEN as u32, false), (p, 60, false)]);
    // Intestazione e dati nello stesso buffer, via tabella indiretta.
    let g = frame(1514, 9);
    let hp = d.buf(&[vec![0; NET_HDR_LEN], g.clone()].concat());
    let head2 = d.add_indirect(TXQ, &[(hp, 1526, false)]);
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(TXQ), Some((head, 0)));
    assert_eq!(d.pop_used(TXQ), Some((head2, 0)));
    assert_eq!(backend(&mut d).tx, vec![f, g]);
}

#[test]
fn trasmissione_senza_intestazione_e_un_errore() {
    let mut d = driver(true, u64::MAX);
    let p = d.buf(&[0; 8]);
    d.add(TXQ, &[(p, 8, false)]);
    d.service();
    assert!(matches!(d.t.last_error(), Some(QueueError::Malformed(_))));
    assert_ne!(d.t.rd(STATUS) & STATUS_DEVICE_NEEDS_RESET, 0);

    // Descrittore enorme: rifiutato prima di leggere (e allocare) nulla.
    let mut d = driver(true, u64::MAX);
    let p = d.buf(&[0; 16]);
    d.add(TXQ, &[(p, u32::MAX, false)]);
    d.service();
    assert!(matches!(d.t.last_error(), Some(QueueError::Malformed(_))));
    assert!(backend(&mut d).tx.is_empty());
}

#[test]
fn ricezione_senza_mrg_rxbuf() {
    let mut d = driver(false, u64::MAX);
    let bufs = post_rx(&mut d, 2, 1526);
    let f = frame(100, 5);
    backend(&mut d).rx.push_back(f.clone());
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    let (_, len) = d.pop_used(RXQ).unwrap();
    assert_eq!(len, 112);
    let got = d.mem(bufs[0], 112);
    let mut hdr = [0u8; NET_HDR_LEN];
    hdr[10] = 1; // num_buffers = 1
    assert_eq!(&got[..NET_HDR_LEN], &hdr);
    assert_eq!(&got[NET_HDR_LEN..], &f[..]);
    assert_eq!(d.pop_used(RXQ), None);
}

#[test]
fn ricezione_senza_buffer_lascia_i_frame_nel_backend() {
    let mut d = driver(false, u64::MAX);
    backend(&mut d).rx.push_back(frame(60, 0));
    d.service();
    assert_eq!(backend(&mut d).rx.len(), 1, "nessun buffer: il frame resta");
    post_rx(&mut d, 1, 1526);
    d.service();
    assert_eq!(backend(&mut d).rx.len(), 0);
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(72));
}

#[test]
fn frame_troppo_grande_senza_mrg_si_scarta() {
    let mut d = driver(false, u64::MAX);
    let bufs = post_rx(&mut d, 1, 100);
    backend(&mut d).rx.extend([frame(200, 0), frame(50, 7)]);
    d.service();
    assert_eq!(d.t.device_as::<VirtioNet>().unwrap().rx_dropped(), 1);
    // Il buffer è rimasto al driver e ha ricevuto il frame successivo.
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(62));
    assert_eq!(d.mem(bufs[0] + NET_HDR_LEN as u64, 50), frame(50, 7));
}

#[test]
fn ricezione_con_mrg_rxbuf_su_piu_buffer() {
    let mut d = driver(true, u64::MAX);
    assert_ne!(d.features & F_MRG_RXBUF, 0);
    let bufs = post_rx(&mut d, 4, 64);
    let f = frame(150, 3);
    backend(&mut d).rx.push_back(f.clone());
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    let lens: Vec<u32> = (0..3).map(|_| d.pop_used(RXQ).unwrap().1).collect();
    assert_eq!(lens, [64, 64, 34]);
    assert_eq!(d.pop_used(RXQ), None, "il quarto buffer resta libero");
    let all = [d.mem(bufs[0], 64), d.mem(bufs[1], 64), d.mem(bufs[2], 34)].concat();
    assert_eq!(u16::from_le_bytes([all[10], all[11]]), 3, "num_buffers");
    assert_eq!(&all[NET_HDR_LEN..], &f[..]);
}

#[test]
fn mrg_rxbuf_aspetta_buffer_sufficienti() {
    let mut d = driver(true, u64::MAX);
    post_rx(&mut d, 1, 64);
    let f = frame(100, 1);
    backend(&mut d).rx.push_back(f.clone());
    d.service();
    assert_eq!(d.pop_used(RXQ), None);
    assert_eq!(d.irq(), 0);
    let bufs = post_rx(&mut d, 1, 64);
    d.service();
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(64));
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(48));
    assert_eq!(d.mem(bufs[0], 48), f[52..].to_vec());
}

#[test]
fn link_giu_e_su() {
    let mut d = driver(true, u64::MAX);
    post_rx(&mut d, 2, 1526);
    let gen0 = d.t.rd(CONFIG_GENERATION);
    d.t.device_as_mut::<VirtioNet>().unwrap().set_link_up(false);
    backend(&mut d).rx.push_back(frame(60, 0));
    d.service();
    assert_eq!(d.irq(), INT_CONFIG);
    assert_eq!(d.t.cfg(6, 2), 0);
    assert_eq!(d.t.rd(CONFIG_GENERATION), gen0.wrapping_add(1));
    assert_eq!(backend(&mut d).rx.len(), 1, "link giù: niente ricezione");
    // Con il link giù la trasmissione si scarta ma il buffer torna.
    let p = d.buf(&[0; 72]);
    d.add(TXQ, &[(p, 72, false)]);
    d.service();
    assert_eq!(d.pop_used(TXQ).map(|u| u.1), Some(0));
    assert!(backend(&mut d).tx.is_empty());
    d.t.device_as_mut::<VirtioNet>().unwrap().set_link_up(true);
    d.service();
    assert_eq!(d.irq(), INT_CONFIG | INT_VRING);
    assert_eq!(d.t.cfg(6, 2), u64::from(S_LINK_UP));
    assert_eq!(d.pop_used(RXQ).map(|u| u.1), Some(72));
}
