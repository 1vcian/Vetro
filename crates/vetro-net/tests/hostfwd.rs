//! Port forwarding: connections opened by the host to a TCP service of the
//! fake guest (`Stack::host_connect`), like QEMU's `hostfwd`. The fake
//! guest is the server: it answers the gateway's SYN, acknowledges, echoes.

mod common;

use std::net::SocketAddrV4;

use common::*;
use vetro_net::hostfwd::HOST_BUFFER;
use vetro_net::{
    CloseReason, ConnId, Direction, EventKind, Flow, HostConnState, Sinkhole, SinkholeConfig, Stack,
};

const PORT: u16 = 5555;

/// Stack that already knows the guest's MAC (from its first frame, as after
/// DHCP): segments to the guest go to its MAC.
fn stack() -> Stack<Sinkhole> {
    let mut s = Stack::new(config(), Sinkhole::new(SinkholeConfig::default()));
    s.receive(t(0), &Guest::arp_request(GW_IP));
    assert_eq!(drain(&mut s).len(), 1);
    s
}

fn segs(s: &mut Stack<Sinkhole>) -> Vec<TcpSeg> {
    drain(s).into_iter().map(Out::tcp).collect()
}

fn one(s: &mut Stack<Sinkhole>) -> TcpSeg {
    let mut v = segs(s);
    assert_eq!(v.len(), 1, "atteso un segmento: {v:?}");
    v.remove(0)
}

fn state(s: &Stack<Sinkhole>, id: ConnId) -> HostConnState {
    s.host_conn(id).expect("host connection").state
}

/// The guest's service: a `Client` of the fake guest with the service port
/// as source and the gateway as destination.
fn server_for(syn: &TcpSeg) -> Client {
    assert!(syn.syn && syn.ack.is_none() && !syn.rst, "atteso SYN: {syn:?}");
    let mut c = Client::new(syn.dst.port(), syn.src);
    c.seq = 7_000_000;
    c.ack = syn.seq.wrapping_add(1);
    c
}

/// Full handshake to the guest port; returns id and server.
fn open(s: &mut Stack<Sinkhole>, now: u64) -> (ConnId, Client) {
    let id = s.host_connect(PORT).unwrap();
    assert_eq!(state(s, id), HostConnState::Connecting);
    assert!(drain(s).is_empty(), "the SYN leaves at poll");
    s.poll(t(now));
    let syn = one(s);
    let mut g = server_for(&syn);
    s.receive(t(now), &g.segment(g.seq, F_SYN | F_ACK, b""));
    g.seq = g.seq.wrapping_add(1);
    let ack = one(s);
    assert_eq!((ack.seq, ack.ack, ack.syn), (syn.seq.wrapping_add(1), Some(g.seq), false));
    assert_eq!(state(s, id), HostConnState::Open);
    (id, g)
}

/// The guest takes and acknowledges everything the stack sends it, until
/// the stack is silent. Returns the bytes received and whether the FIN arrived.
fn guest_takes(s: &mut Stack<Sinkhole>, g: &mut Client, now: u64) -> (Vec<u8>, bool) {
    let (mut data, mut fin) = (Vec::new(), false);
    loop {
        let out = segs(s);
        if out.is_empty() {
            return (data, fin);
        }
        for seg in out {
            if seg.payload.is_empty() && !seg.fin {
                continue;
            }
            g.take(&seg);
            data.extend_from_slice(&seg.payload);
            fin |= seg.fin;
        }
        s.receive(t(now), &g.ack_now());
    }
}

#[test]
fn handshake_dal_gateway_come_qemu() {
    let mut s = stack();
    let id = s.host_connect(PORT).unwrap();
    s.poll(t(5));
    let syn = one(&mut s);
    assert_eq!(syn.src, SocketAddrV4::new(GW_IP, 49152), "from the gateway, first ephemeral port");
    assert_eq!(syn.dst, SocketAddrV4::new(GUEST_IP, PORT));
    assert_eq!((syn.mss, syn.window), (Some(1460), 65_535));
    let g = server_for(&syn);
    s.receive(t(6), &g.segment(g.seq, F_SYN | F_ACK, b""));
    let ack = one(&mut s);
    assert!(!ack.syn && !ack.fin && ack.payload.is_empty());
    assert_eq!(ack.ack, Some(g.seq + 1), "ACK of the guest's SYN");
    let info = s.host_conn(id).unwrap();
    assert_eq!(info.state, HostConnState::Open);
    assert_eq!((info.readable, info.writable, info.guest_eof), (0, HOST_BUFFER, false));
    let flow = Flow { guest: syn.dst, remote: syn.src };
    assert_eq!(
        s.events().iter().map(|e| (e.at, e.kind.clone())).collect::<Vec<_>>(),
        [(t(5), EventKind::TcpConnect { id, flow }), (t(6), EventKind::TcpEstablished { id })]
    );
    assert_eq!(s.events()[0].to_string(), "[     0.005000] tcp 1 from host 10.0.2.2:49152 -> 10.0.2.15:5555");
    // La seconda connessione prende la porta effimera successiva.
    s.host_connect(PORT).unwrap();
    s.poll(t(7));
    assert_eq!(one(&mut s).src.port(), 49153);
}

#[test]
fn eco_di_200_kb_oltre_la_finestra() {
    let mut s = stack();
    let (id, mut g) = open(&mut s, 0);
    let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let (mut sent, mut echoed, mut now) = (0, Vec::new(), 1);
    let mut buf = vec![0u8; 70_000];
    while echoed.len() < data.len() {
        now += 1;
        assert!(now < 10_000, "eco ferma a {} byte", echoed.len());
        sent += s.host_send(id, &data[sent..]);
        s.poll(t(now));
        // The guest receives (at most one MSS-sized segment at a time, within
        // its window) and sends the same bytes back.
        let (got, fin) = guest_takes(&mut s, &mut g, now);
        assert!(!fin);
        for chunk in got.chunks(1460) {
            s.receive(t(now), &g.send(chunk));
            let acks = segs(&mut s);
            assert!(acks.iter().any(|a| a.ack == Some(g.seq)), "{acks:?}");
        }
        let n = s.host_recv(id, &mut buf);
        echoed.extend_from_slice(&buf[..n]);
    }
    assert_eq!(echoed, data, "eco byte per byte");
    let to_guest: usize = s
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::TcpData { dir: Direction::ToGuest, len, .. } => Some(len),
            _ => None,
        })
        .sum();
    assert_eq!(to_guest, data.len());
    assert_eq!(s.stats().bad_checksum, 0);
}

#[test]
fn contropressione_verso_l_host() {
    let mut s = stack();
    let (id, mut g) = open(&mut s, 0);
    // The guest sends more than the host keeps queued: the window
    // closes when the connection buffer and the host queue are full.
    let chunk = vec![0x5a; 1460];
    let start = g.seq;
    let mut last_window = u16::MAX;
    for _ in 0..400 {
        s.receive(t(1), &g.send(&chunk));
        let ack = one(&mut s);
        last_window = ack.window;
        // Whatever was not taken the guest will resend.
        g.seq = ack.ack.unwrap();
        if ack.window == 0 {
            break;
        }
    }
    let accepted = g.seq.wrapping_sub(start) as usize;
    assert_eq!(last_window, 0, "finestra chiusa");
    let info = s.host_conn(id).unwrap();
    assert_eq!(info.readable, HOST_BUFFER);
    assert_eq!(accepted, HOST_BUFFER + 65_535, "host queue plus maximum window");
    // The host reads: at poll the window reopens with an update.
    let mut buf = vec![0u8; 100_000];
    assert_eq!(s.host_recv(id, &mut buf), 100_000);
    s.poll(t(2));
    let upd = one(&mut s);
    assert_eq!((upd.ack, upd.window), (Some(g.seq), 65_535));
    assert_eq!(s.host_conn(id).unwrap().readable, HOST_BUFFER - 100_000 + 65_535);
}

#[test]
fn chiusura_dall_host_con_time_wait() {
    let mut s = stack();
    let (id, mut g) = open(&mut s, 0);
    assert_eq!(s.host_send(id, b"ciao"), 4);
    s.host_shutdown(id);
    assert_eq!(s.host_send(id, b"x"), 0, "direction closed");
    s.poll(t(1));
    let (got, fin) = guest_takes(&mut s, &mut g, 1);
    assert_eq!((got.as_slice(), fin), (&b"ciao"[..], true));
    // The guest answers and closes.
    s.receive(t(2), &g.send(b"ciao"));
    assert_eq!(one(&mut s).ack, Some(g.seq));
    s.receive(t(2), &g.fin());
    assert_eq!(one(&mut s).ack, Some(g.seq), "ACK of the guest's FIN");
    let mut buf = [0u8; 16];
    assert_eq!(s.host_recv(id, &mut buf), 4);
    let info = s.host_conn(id).unwrap();
    assert!(info.guest_eof);
    assert_eq!(info.state, HostConnState::Open, "TIME-WAIT: still alive");
    s.poll(t(4002));
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::Normal));
    assert!(s.events().iter().any(|e| e.kind
        == EventKind::TcpClosed { id, reason: CloseReason::Normal, bytes_to_remote: 4, bytes_to_guest: 4 }));
    s.host_release(id);
    assert!(s.host_conn(id).is_none());
}

#[test]
fn chiusura_dal_guest() {
    let mut s = stack();
    let (id, mut g) = open(&mut s, 0);
    s.receive(t(1), &g.send(b"addio"));
    let _ = segs(&mut s);
    s.receive(t(1), &g.fin());
    let _ = segs(&mut s);
    let info = s.host_conn(id).unwrap();
    assert!(!info.guest_eof, "data first");
    let mut buf = [0u8; 16];
    assert_eq!(s.host_recv(id, &mut buf), 5);
    assert!(s.host_conn(id).unwrap().guest_eof);
    s.host_shutdown(id);
    s.poll(t(2));
    let fin = one(&mut s);
    assert!(fin.fin);
    g.take(&fin);
    s.receive(t(3), &g.ack_now());
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::Normal));
    assert_eq!(s.tcp_connections(), 0);
}

#[test]
fn nessuno_in_ascolto_rifiutata() {
    let mut s = stack();
    let id = s.host_connect(PORT).unwrap();
    s.poll(t(1));
    let syn = one(&mut s);
    // Linux answers RST|ACK to a SYN to a closed port.
    let mut g = server_for(&syn);
    s.receive(t(1), &g.segment(0, F_RST | F_ACK, b""));
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::Refused));
    assert!(drain(&mut s).is_empty());
    assert_eq!(s.tcp_connections(), 0);
    assert_eq!(s.host_send(id, b"x"), 0);
    // An RST with a wrong ACK is ignored.
    let id2 = s.host_connect(PORT).unwrap();
    s.poll(t(2));
    let syn2 = one(&mut s);
    g = server_for(&syn2);
    g.ack = g.ack.wrapping_add(10);
    s.receive(t(2), &g.segment(0, F_RST | F_ACK, b""));
    assert_eq!(state(&s, id2), HostConnState::Connecting);
}

#[test]
fn syn_ritrasmesso_poi_timeout() {
    let mut s = stack();
    let id = s.host_connect(PORT).unwrap();
    s.poll(t(0));
    let first = one(&mut s);
    assert_eq!(s.next_deadline(), Some(t(1000)), "RTO iniziale 1 s");
    s.poll(t(1000));
    let again = one(&mut s);
    assert_eq!((again.seq, again.syn), (first.seq, true));
    assert_eq!(s.next_deadline(), Some(t(3000)), "RTO raddoppiato");
    // No answer for 75 s: give up without RST.
    let mut now = 3000;
    while state(&s, id) == HostConnState::Connecting {
        now = s.next_deadline().unwrap().as_micros() / 1000;
        assert!(now <= 75_000);
        s.poll(t(now));
        assert!(segs(&mut s).iter().all(|x| x.syn && !x.rst));
    }
    assert_eq!(now, 75_000);
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::Timeout));
}

#[test]
fn reset_dall_host_e_dal_guest() {
    let mut s = stack();
    let (id, g) = open(&mut s, 0);
    s.host_send(id, b"dati");
    s.host_abort(id);
    s.poll(t(1));
    let rst = one(&mut s);
    assert!(rst.rst && rst.payload.is_empty(), "{rst:?}");
    assert_eq!(rst.seq, g.ack, "RST with the sequence expected by the guest");
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::RemoteReset));

    let (id, g) = open(&mut s, 2);
    s.receive(t(3), &g.segment(g.seq, F_RST, b""));
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::GuestReset));
    assert!(drain(&mut s).is_empty());

    // Release of a live connection: RST and it disappears.
    let (id, _) = open(&mut s, 4);
    s.host_release(id);
    assert!(s.host_conn(id).is_some(), "until the poll");
    s.poll(t(5));
    assert!(one(&mut s).rst);
    assert!(s.host_conn(id).is_none());
    // Interrupted before the SYN: no packet.
    let id = s.host_connect(PORT).unwrap();
    s.host_abort(id);
    s.poll(t(6));
    assert!(drain(&mut s).is_empty());
    assert_eq!(state(&s, id), HostConnState::Closed(CloseReason::RemoteReset));
}

#[test]
fn deterministico() {
    let run = || {
        let mut s = stack();
        let (id, mut g) = open(&mut s, 0);
        s.host_send(id, &[7u8; 5000]);
        s.poll(t(1));
        let frames: Vec<TcpSeg> = segs(&mut s);
        for f in &frames {
            g.take(f);
        }
        s.receive(t(2), &g.ack_now());
        (frames, format!("{:?}", s.events()))
    };
    assert_eq!(run(), run());
}

/// Snapshot halfway: SYN requested and not sent, then connection established with
/// bytes queued in both directions. The restored stack saves the same bytes again
/// and continues like the original (frames, host state, log).
#[test]
fn snapshot_a_meta_connessione() {
    use vetro_snapshot::{Reader, Snapshot, Writer};
    fn copy(s: &Stack<Sinkhole>) -> Stack<Sinkhole> {
        let mut w = Writer::new();
        s.save(&mut w);
        let bytes = w.into_bytes();
        let mut n = Stack::new(config(), Sinkhole::new(SinkholeConfig::default()));
        let mut r = Reader::new(&bytes);
        n.restore(&mut r).unwrap();
        r.finish().unwrap();
        let mut w2 = Writer::new();
        n.save(&mut w2);
        assert_eq!(w2.into_bytes(), bytes, "risalvataggio");
        n
    }
    let mut a = stack();
    let pending = a.host_connect(PORT).unwrap();
    let mut b = copy(&a);
    assert_eq!(b.host_conn(pending), a.host_conn(pending));
    a.poll(t(1));
    b.poll(t(1));
    assert_eq!(segs(&mut a), segs(&mut b), "the SYN leaves from the copy too");
    a.host_abort(pending);
    a.poll(t(1));
    let _ = segs(&mut a);

    let (id, mut g) = open(&mut a, 2);
    a.host_send(id, &[3u8; 100_000]);
    a.poll(t(3));
    for f in &segs(&mut a) {
        g.take(f);
    }
    a.receive(t(3), &g.send(b"dal guest"));
    let _ = segs(&mut a);
    let mut b = copy(&a);
    assert_eq!(b.host_conn(id), a.host_conn(id));
    assert_eq!(b.next_deadline(), a.next_deadline());
    let ack = g.ack_now();
    for s in [&mut a, &mut b] {
        s.receive(t(4), &ack);
        s.host_shutdown(id);
        s.poll(t(5));
    }
    assert_eq!(segs(&mut a), segs(&mut b));
    let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
    assert_eq!(a.host_recv(id, &mut x), 9);
    assert_eq!(b.host_recv(id, &mut y), 9);
    assert_eq!(x, y);
    assert_eq!(a.host_conn(id), b.host_conn(id));
    assert_eq!(a.events(), b.events());
}
