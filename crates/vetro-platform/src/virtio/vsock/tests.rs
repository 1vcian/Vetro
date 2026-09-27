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

/// Header of a packet from the guest to the host.
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

/// The guest sends a packet: header and data in two descriptors,
/// like Linux.
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

/// Receive buffers like Linux's: one descriptor for
/// header plus `payload` bytes.
fn offer_rx(d: &mut Driver<VirtioMmio>, n: usize, payload: u32) -> Vec<(u16, u64)> {
    (0..n)
        .map(|_| {
            let a = d.alloc(u64::from(payload) + HDR_LEN as u64, 8);
            (d.add(RXQ, &[(a, payload + HDR_LEN as u32, true)]), a)
        })
        .collect()
}

/// Packets arrived at the guest.
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
    assert_eq!(d.t.cfg(0, 4), CID, "32-bit reads as Linux does");
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

    // Data from the guest, then SHUTDOWN for writing: the host reads up to EOF.
    send(&mut d, g(1025, 1234, OP_RW), b"ciao ");
    send(&mut d, g(1025, 1234, OP_RW), b"host");
    let mut sh = g(1025, 1234, OP_SHUTDOWN);
    sh.flags = SHUTDOWN_SEND;
    send(&mut d, sh, &[]);
    assert_eq!(vs(&mut d).available(c), 9);
    assert!(!vs(&mut d).eof(c));
    assert_eq!(vs(&mut d).recv(c, 100), b"ciao host");
    assert!(vs(&mut d).eof(c));

    // Host response and close: data, then SHUTDOWN, then the guest's RST.
    vs(&mut d).send(c, b"CIAO").unwrap();
    vs(&mut d).close(c);
    assert_eq!(vs(&mut d).send(c, b"x"), Err(VsockError::Closed));
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RW, OP_SHUTDOWN]);
    assert_eq!(p[0].1, b"CIAO");
    assert_eq!(p[0].0.fwd_cnt, 9, "the host announces the bytes consumed");
    assert_eq!(p[1].0.flags, SHUTDOWN_RCV | SHUTDOWN_SEND);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closing));
    send(&mut d, g(1025, 1234, OP_RST), &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Closed));
    assert!(rx(&mut d, &mut bufs).is_empty(), "no answer to an RST");
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
    assert_eq!(vs(&mut d).recv(c, 100), b"ultimi", "the data remains readable");
    assert!(vs(&mut d).eof(c));
    // The same pair of ports can be reopened.
    send(&mut d, g(40, 5, OP_REQUEST), &[]);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_RESPONSE]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));
}

#[test]
fn rifiuti_e_pacchetti_scartati() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    // Port not listening; data without a connection: RST with the ports swapped.
    send(&mut d, g(1, 99, OP_REQUEST), &[]);
    send(&mut d, g(2, 98, OP_RW), b"x");
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RST, OP_RST]);
    assert_eq!((p[0].0.src_port, p[0].0.dst_port), (99, 1));
    // An RST without a connection gets no answer.
    send(&mut d, g(3, 97, OP_RST), &[]);
    // Wrong CIDs: discarded without an answer.
    let mut h = g(1, 99, OP_REQUEST);
    h.src_cid = 3;
    send(&mut d, h, &[]);
    let mut h = g(1, 99, OP_REQUEST);
    h.dst_cid = 1;
    send(&mut d, h, &[]);
    // Length beyond the data present: discarded.
    let mut h = g(1, 99, OP_RW);
    h.len = 10;
    let a = d.buf(&h.to_bytes());
    d.add(TXQ, &[(a, HDR_LEN as u32, false)]);
    d.service();
    assert!(rx(&mut d, &mut bufs).is_empty());
    assert_eq!(vs(&mut d).dropped(), 3);
    // SEQPACKET type (not negotiated): RST.
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
    assert_eq!(ops(&p), [OP_REQUEST], "the data waits for the RESPONSE");
    assert_eq!((p[0].0.src_port, p[0].0.dst_port), (FIRST_HOST_PORT, 5555));
    // The guest accepts with a 100-byte buffer.
    let mut r = g(5555, FIRST_HOST_PORT, OP_RESPONSE);
    r.buf_alloc = 100;
    send(&mut d, r, &[]);
    assert_eq!(vs(&mut d).state(c), Some(VsockState::Connected));
    let p = rx(&mut d, &mut bufs);
    // Buffers with 64 bytes of payload: 64 + 36 = the 100 of credit.
    assert_eq!(ops(&p), [OP_RW, OP_RW]);
    assert_eq!(p.iter().map(|x| x.1.len()).collect::<Vec<_>>(), [64, 36]);
    assert_eq!(vs(&mut d).unsent(c), 200);
    // The guest consumes 80 bytes and says so: another 80 leave.
    let mut cu = g(5555, FIRST_HOST_PORT, OP_CREDIT_UPDATE);
    cu.buf_alloc = 100;
    cu.fwd_cnt = 80;
    send(&mut d, cu, &[]);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(p.iter().map(|x| x.1.len()).sum::<usize>(), 80);
    assert_eq!(vs(&mut d).unsent(c), 120);
    // A second connection takes the next port.
    assert_eq!(vs(&mut d).connect(1).host_port, FIRST_HOST_PORT + 1);
}

#[test]
fn chiusura_della_sola_trasmissione_dell_host() {
    let mut d = driver();
    let mut bufs = offer_rx(&mut d, 8, 4096);
    // Data and closing of transmission even before the RESPONSE.
    let c = vs(&mut d).connect(5000);
    vs(&mut d).send(c, b"dati").unwrap();
    vs(&mut d).shutdown_send(c);
    assert_eq!(ops(&rx(&mut d, &mut bufs)), [OP_REQUEST]);
    send(&mut d, g(5000, FIRST_HOST_PORT, OP_RESPONSE), &[]);
    assert_eq!(vs(&mut d).send(c, b"x"), Err(VsockError::Closed));
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_RW, OP_SHUTDOWN]);
    assert_eq!(p[1].0.flags, SHUTDOWN_SEND);
    // The guest answers and closes completely: the host reads and answers RST.
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
    // The guest fills almost all of the host's buffer.
    let chunk = vec![1u8; 60 * 1024];
    for _ in 0..4 {
        send(&mut d, g(2, 1, OP_RW), &chunk);
    }
    // Small consumption with plenty of space seen: no update... but here
    // the guest sees 256 - 240 = 16 KiB free: CREDIT_UPDATE leaves.
    assert_eq!(vs(&mut d).recv(c, 1000).len(), 1000);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_CREDIT_UPDATE]);
    assert_eq!(p[0].0.fwd_cnt, 1000);
    // Right after, the guest sees 16 KiB + 1000: still below the threshold, but
    // without new consumption nothing is sent.
    assert!(rx(&mut d, &mut bufs).is_empty());
    // CREDIT_REQUEST: answer with the current credit.
    send(&mut d, g(2, 1, OP_CREDIT_REQUEST), &[]);
    let p = rx(&mut d, &mut bufs);
    assert_eq!(ops(&p), [OP_CREDIT_UPDATE]);
    // With the buffer almost empty a consumption sends no updates.
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
    assert!(rx(&mut d, &mut bufs).is_empty(), "no RST after the transport reset");
    // Device reset: connections closed, listening remains.
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
    // The same sequence of operations gives the same packets.
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
    assert_eq!(x.len(), 3 + 2 * 2, "REQUEST x2, RESPONSE, then 200 bytes in chunks of 128 per connection");
}
