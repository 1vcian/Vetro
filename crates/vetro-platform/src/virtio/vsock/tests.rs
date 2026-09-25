use super::*;
use crate::virtio::testdrv::{Driver, Transport};

const CID: u64 = 7;

fn driver() -> Driver<VirtioMmio> {
    let mut d = Driver::new(VirtioMmio::new(Box::new(VirtioVsock::new(CID))));
    d.init(u64::MAX, 32);
    d
}

fn vs(d: &mut Driver<VirtioMmio>) -> &mut VirtioVsock {
    d.t.device_as_mut::<VirtioVsock>().unwrap()
}

/// Intestazione di un pacchetto del guest verso l'host.
fn g(src_port: u32, dst_port: u32, op: u16) -> Hdr {
    Hdr {
        src_cid: CID,
        dst_cid: HOST_CID,
        src_port,
        dst_port,
        ty: TYPE_STREAM,
        op,
        buf_alloc: 1 << 20,
        ..Hdr::default()
    }
}

/// Il guest manda un pacchetto: intestazione e dati in due descrittori,
/// come Linux.
fn send(d: &mut Driver<VirtioMmio>, mut h: Hdr, data: &[u8]) {
    h.len = data.len() as u32;
    let a = d.buf(&h.to_bytes());
    if data.is_empty() {
        d.add(TXQ, &[(a, HDR_LEN as u32, false)]);
    } else {
        let b = d.buf(data);
        d.add(TXQ, &[(a, HDR_LEN as u32, false), (b, data.len() as u32, false)]);
    }
    d.service();
    while d.pop_used(TXQ).is_some() {}
}

/// Buffer di ricezione come quelli di Linux: un descrittore per
/// intestazione più `payload` byte.
fn offer_rx(d: &mut Driver<VirtioMmio>, n: usize, payload: u32) -> Vec<(u16, u64)> {
    (0..n)
        .map(|_| {
            let a = d.alloc(u64::from(payload) + HDR_LEN as u64, 8);
            (d.add(RXQ, &[(a, payload + HDR_LEN as u32, true)]), a)
        })
        .collect()
}

/// Pacchetti arrivati al guest.
fn rx(d: &mut Driver<VirtioMmio>, bufs: &mut Vec<(u16, u64)>) -> Vec<(Hdr, Vec<u8>)> {
    d.service();
    let mut out = Vec::new();
    while let Some((head, len)) = d.pop_used(RXQ) {
        let i = bufs.iter().position(|b| b.0 == head).unwrap();
        let (_, a) = bufs.remove(i);
        let bytes = d.mem(a, len as usize);
        let h = Hdr::parse(&bytes[..HDR_LEN].try_into().unwrap());
        assert_eq!(h.len as usize, len as usize - HDR_LEN);
        out.push((h, bytes[HDR_LEN..].to_vec()));
    }
    out
}

fn ops(p: &[(Hdr, Vec<u8>)]) -> Vec<u16> {
    p.iter().map(|(h, _)| h.op).collect()
}

#[test]
fn configurazione() {
    let mut d = driver();
    assert_eq!(d.t.rd(DEVICE_ID), ID_VSOCK);
    assert_eq!(d.t.cfg(0, 8), CID);
    assert_eq!(d.t.cfg(0, 4), CID, "letture a 32 bit come fa Linux");
    assert_eq!(d.features & 0xFF_FFFF, F_STREAM);
    for q in 0..3 {
        d.t.wr(QUEUE_SEL, q);
        assert_eq!(d.t.rd(QUEUE_NUM_MAX), 128);
    }
    d.t.wr(QUEUE_SEL, 3);
    assert_eq!(d.t.rd(QUEUE_NUM_MAX), 0);
}

#[test]
fn il_guest_si_collega_all_host() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    vs(&mut d).listen(1234).unwrap();
    assert_eq!(vs(&mut d).listen(1234), Err(VsockError::PortInUse));
    send(&mut d, g(1025, 1234, OP_REQUEST), &[]);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RESPONSE]);
    let h = p[0].0;
    assert_eq!((h.src_cid, h.dst_cid, h.src_port, h.dst_port), (HOST_CID, CID, 1234, 1025));
    assert_eq!((h.buf_alloc, h.fwd_cnt, h.ty), (HOST_BUF_ALLOC, 0, TYPE_STREAM));
    let c = vs(&mut d).accept(1234).unwrap();
    assert_eq!(c, VsockConn { host_port: 1234, guest_port: 1025 });
    assert_eq!(vs(&mut d).accept(1234), None);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));

    // Dati dal guest, poi SHUTDOWN in scrittura: l'host legge fino all'EOF.
    send(&mut d, g(1025, 1234, OP_RW), b"ciao ");
    send(&mut d, g(1025, 1234, OP_RW), b"host");
    let mut sh = g(1025, 1234, OP_SHUTDOWN);
    sh.flags = SHUTDOWN_SEND;
    send(&mut d, sh, &[]);
    assert_eq!(vs(&mut d).available(c), 9);
    assert!(!vs(&mut d).eof(c));
    assert_eq!(vs(&mut d).recv(c, 100), b"ciao host");
    assert!(vs(&mut d).eof(c));

    // Risposta dell'host e chiusura: dati, poi SHUTDOWN, poi l'RST del guest.
    vs(&mut d).send(c, b"CIAO").unwrap();
    vs(&mut d).close(c);
    assert_eq!(vs(&mut d).send(c, b"x"), Err(VsockError::Closed));
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RW, OP_SHUTDOWN]);
    assert_eq!(p[0].1, b"CIAO");
    assert_eq!(p[0].0.fwd_cnt, 9, "l'host annuncia i byte consumati");
    assert_eq!(p[1].0.flags, SHUTDOWN_RCV | SHUTDOWN_SEND);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closing));
    send(&mut d, g(1025, 1234, OP_RST), &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
    assert!(rx(&mut d, &mut bufs).is_empty(), "niente risposta a un RST");
    vs(&mut d).release(c);
    assert_eq!(vs(&mut d).state(c), None);
}

#[test]
fn chiusura_completa_del_guest_riceve_rst() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    vs(&mut d).listen(5).unwrap();
    send(&mut d, g(40, 5, OP_REQUEST), &[]);
    rx(&mut d, &mut bufs);
    let c = vs(&mut d).accept(5).unwrap();
    send(&mut d, g(40, 5, OP_RW), b"ultimi");
    let mut sh = g(40, 5, OP_SHUTDOWN);
    sh.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
    send(&mut d, sh, &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RST]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
    assert_eq!(vs(&mut d).recv(c, 100), b"ultimi", "i dati restano leggibili");
    assert!(vs(&mut d).eof(c));
    // La stessa coppia di porte si può riaprire.
    send(&mut d, g(40, 5, OP_REQUEST), &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RESPONSE]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));
}

#[test]
fn rifiuti_e_pacchetti_scartati() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    // Porta non in ascolto; dati senza connessione: RST con le porte scambiate.
    send(&mut d, g(1, 99, OP_REQUEST), &[]);
    send(&mut d, g(2, 98, OP_RW), b"x");
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RST, OP_RST]);
    assert_eq!((p[0].0.src_port, p[0].0.dst_port), (99, 1));
    // Un RST senza connessione non riceve risposta.
    send(&mut d, g(3, 97, OP_RST), &[]);
    // CID sbagliati: scartati senza risposta.
    let mut h = g(1, 99, OP_REQUEST);
    h.src_cid = 3;
    send(&mut d, h, &[]);
    let mut h = g(1, 99, OP_REQUEST);
    h.dst_cid = 1;
    send(&mut d, h, &[]);
    // Lunghezza oltre i dati presenti: scartato.
    let mut h = g(1, 99, OP_RW);
    h.len = 10;
    let a = d.buf(&h.to_bytes());
    d.add(TXQ, &[(a, HDR_LEN as u32, false)]);
    d.service();
    assert!(rx(&mut d, &mut bufs).is_empty());
    assert_eq!(vs(&mut d).dropped(), 3);
    // Tipo SEQPACKET (non negoziato): RST.
    vs(&mut d).listen(99).unwrap();
    let mut h = g(1, 99, OP_REQUEST);
    h.ty = 2;
    send(&mut d, h, &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RST]);
    assert_eq!(vs(&mut d).accept(99), None);
    assert!(d.t.last_error().is_none());
}

#[test]
fn l_host_si_collega_e_rispetta_il_credito() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 16, 64);
    let c = vs(&mut d).connect(5555);
    assert_eq!(c, VsockConn { host_port: FIRST_HOST_PORT, guest_port: 5555 });
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connecting));
    vs(&mut d).send(c, &[7u8; 300]).unwrap();
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_REQUEST], "i dati aspettano la RESPONSE");
    assert_eq!((p[0].0.src_port, p[0].0.dst_port), (FIRST_HOST_PORT, 5555));
    // Il guest accetta con un buffer di 100 byte.
    let mut r = g(5555, FIRST_HOST_PORT, OP_RESPONSE);
    r.buf_alloc = 100;
    send(&mut d, r, &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));
    let p = rx(&mut d, &mut bufs);
    // Buffer da 64 byte di carico: 64 + 36 = i 100 di credito.
    assert_eq!(ops(&p), [OP_RW, OP_RW]);
    assert_eq!(p.iter().map(|x| x.1.len()).collect::<Vec<_>>(), [64, 36]);
    assert_eq!(vs(&mut d).unsent(c), 200);
    // Il guest consuma 80 byte e lo dice: altri 80 partono.
    let mut cu = g(5555, FIRST_HOST_PORT, OP_CREDIT_UPDATE);
    cu.buf_alloc = 100;
    cu.fwd_cnt = 80;
    send(&mut d, cu, &[]);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(p.iter().map(|x| x.1.len()).sum::<usize>(), 80);
    assert_eq!(vs(&mut d).unsent(c), 120);
    // Una seconda connessione prende la porta dopo.
    assert_eq!(vs(&mut d).connect(1).host_port, FIRST_HOST_PORT + 1);
}

#[test]
fn chiusura_della_sola_trasmissione_dell_host() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    // Dati e chiusura della trasmissione prima ancora della RESPONSE.
    let c = vs(&mut d).connect(5000);
    vs(&mut d).send(c, b"dati").unwrap();
    vs(&mut d).shutdown_send(c);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_REQUEST]);
    send(&mut d, g(5000, FIRST_HOST_PORT, OP_RESPONSE), &[]);
    assert_eq!(vs(&mut d).send(c, b"x"), Err(VsockError::Closed));
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RW, OP_SHUTDOWN]);
    assert_eq!(p[1].0.flags, SHUTDOWN_SEND);
    // Il guest risponde e chiude del tutto: l'host legge e risponde RST.
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));
    send(&mut d, g(5000, FIRST_HOST_PORT, OP_RW), b"DATI");
    let mut sh = g(5000, FIRST_HOST_PORT, OP_SHUTDOWN);
    sh.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
    send(&mut d, sh, &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RST]);
    assert_eq!(vs(&mut d).recv(c, 10), b"DATI");
    assert!(vs(&mut d).eof(c));
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
}

#[test]
fn chiusura_prima_della_risposta() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    let c = vs(&mut d).connect(6000);
    vs(&mut d).send(c, b"addio").unwrap();
    vs(&mut d).close(c);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connecting));
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_REQUEST]);
    send(&mut d, g(6000, FIRST_HOST_PORT, OP_RESPONSE), &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closing));
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RW, OP_SHUTDOWN]);
    assert_eq!(p[1].0.flags, SHUTDOWN_RCV | SHUTDOWN_SEND);
}

#[test]
fn rifiuto_del_guest() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 4, 64);
    let c = vs(&mut d).connect(9);
    rx(&mut d, &mut bufs);
    send(&mut d, g(9, FIRST_HOST_PORT, OP_RST), &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
    assert!(vs(&mut d).eof(c));
    assert_eq!(vs(&mut d).send(c, b"x"), Err(VsockError::Closed));
}

#[test]
fn aggiornamenti_di_credito_dell_host() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    vs(&mut d).listen(1).unwrap();
    send(&mut d, g(2, 1, OP_REQUEST), &[]);
    rx(&mut d, &mut bufs);
    let c = vs(&mut d).accept(1).unwrap();
    // Il guest riempie quasi tutto il buffer dell'host.
    let chunk = vec![1u8; 60 * 1024];
    for _ in 0..4 {
        send(&mut d, g(2, 1, OP_RW), &chunk);
    }
    // Consumo piccolo con tanto spazio visto: nessun aggiornamento... ma qui
    // il guest vede 256 - 240 = 16 KiB liberi: parte CREDIT_UPDATE.
    assert_eq!(vs(&mut d).recv(c, 1000).len(), 1000);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_CREDIT_UPDATE]);
    assert_eq!(p[0].0.fwd_cnt, 1000);
    // Subito dopo il guest vede 16 KiB + 1000: ancora sotto soglia, ma
    // senza nuovi consumi non si manda nulla.
    assert!(rx(&mut d, &mut bufs).is_empty());
    // CREDIT_REQUEST: risposta con il credito attuale.
    send(&mut d, g(2, 1, OP_CREDIT_REQUEST), &[]);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_CREDIT_UPDATE]);
    // Con il buffer quasi vuoto un consumo non manda aggiornamenti.
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    vs(&mut d).listen(1).unwrap();
    send(&mut d, g(2, 1, OP_REQUEST), &[]);
    rx(&mut d, &mut bufs);
    let c = vs(&mut d).accept(1).unwrap();
    send(&mut d, g(2, 1, OP_RW), b"poco");
    vs(&mut d).recv(c, 10);
    assert!(rx(&mut d, &mut bufs).is_empty());
}

#[test]
fn reset_del_trasporto_e_del_dispositivo() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 4, 64);
    vs(&mut d).listen(1).unwrap();
    send(&mut d, g(2, 1, OP_REQUEST), &[]);
    rx(&mut d, &mut bufs);
    let c = vs(&mut d).accept(1).unwrap();
    let ev = d.alloc(4, 4);
    let head = d.add(EVTQ, &[(ev, 4, true)]);
    d.ram.write(ev, &[0xff; 4]).unwrap();
    vs(&mut d).transport_reset();
    d.service();
    assert_eq!(d.pop_used(EVTQ), Some((head, 4)));
    assert_eq!(d.mem(ev, 4), EVENT_TRANSPORT_RESET.to_le_bytes());
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
    assert!(rx(&mut d, &mut bufs).is_empty(), "niente RST dopo il reset del trasporto");
    // Reset del dispositivo: connessioni chiuse, l'ascolto resta.
    send(&mut d, g(3, 1, OP_REQUEST), &[]);
    let c2 = vs(&mut d).accept(1).unwrap();
    d.init(u64::MAX, 32);
    assert_eq!(vs(&mut d).state(c2), Some(VsockState::Closed));
    let mut bufs = offer_rx(&mut d, 4, 64);
    send(&mut d, g(4, 1, OP_REQUEST), &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RESPONSE]);
}

#[test]
fn deterministico() {
    // La stessa sequenza di operazioni dà gli stessi pacchetti.
    let run = || {
        let mut d = driver();
        let mut bufs = offer_rx(&mut d, 32, 128);
        vs(&mut d).listen(10).unwrap();
        let a = vs(&mut d).connect(20);
        let b = vs(&mut d).connect(21);
        send(&mut d, g(30, 10, OP_REQUEST), &[]);
        for (c, gp) in [(a, 20), (b, 21)] {
            let mut r = g(gp, c.host_port, OP_RESPONSE);
            r.buf_alloc = 1000;
            send(&mut d, r, &[]);
            vs(&mut d).send(c, &[gp as u8; 200]).unwrap();
        }
        rx(&mut d, &mut bufs).into_iter().map(|(h, data)| (h.to_bytes().to_vec(), data)).collect::<Vec<_>>()
    };
    let (x, y) = (run(), run());
    assert_eq!(x, y);
    assert_eq!(x.len(), 3 + 2 * 2, "REQUEST x2, RESPONSE, poi 200 byte a pezzi da 128 per connessione");
}
