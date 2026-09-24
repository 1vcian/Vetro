//! TCP terminato lato host: handshake, dati, finestre, ritrasmissione,
//! FIN e RST, con un finto guest e il sinkhole.

mod common;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddrV4};

use common::*;
use vetro_net::dns::{self, TYPE_A};
use vetro_net::{
    CloseReason, Direction, EventKind, Flow, NetConfig, Sinkhole, SinkholeConfig, Stack, TcpReply,
    VirtualTime,
};

const REMOTE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 5), 443);

fn stack_with(cfg: SinkholeConfig) -> Stack<Sinkhole> {
    Stack::new(config(), Sinkhole::new(cfg))
}

fn one(out: Vec<Out>) -> TcpSeg {
    assert_eq!(out.len(), 1, "atteso un segmento: {out:?}");
    out.into_iter().next().unwrap().tcp()
}

/// Handshake completo; restituisce il client pronto.
fn connect(s: &mut Stack<Sinkhole>, now: u64, sport: u16, dst: SocketAddrV4) -> Client {
    let mut c = Client::new(sport, dst);
    s.receive(t(now), &c.syn());
    let synack = one(drain(s));
    assert_eq!(synack.src, dst);
    assert_eq!(synack.dst, SocketAddrV4::new(GUEST_IP, sport));
    assert_eq!(synack.mss, Some(1460), "MSS = MTU - 40");
    assert_eq!(synack.window, 65_535);
    let ack = c.on_syn_ack(&synack);
    s.receive(t(now), &ack);
    c
}

fn kinds(s: &Stack<Sinkhole>) -> Vec<EventKind> {
    s.events().iter().map(|e| e.kind.clone()).collect()
}

#[test]
fn http_session_with_dns_attribution_and_server_close() {
    let mut s = stack_with(SinkholeConfig::default());
    // DNS prima: la connessione verso l'indirizzo finto porta il nome.
    s.receive(
        t(0),
        &Guest::udp(50000, SocketAddrV4::new(DNS_IP, 53), &dns::build_query(9, "api.example.org", TYPE_A)),
    );
    let Out::Udp { payload, .. } = drain(&mut s).remove(0) else { panic!() };
    let addr = dns::parse_response(&payload).unwrap().addrs[0];
    let dst = SocketAddrV4::new(addr, 80);
    s.take_events();

    let mut c = connect(&mut s, 10, 41000, dst);
    assert!(drain(&mut s).is_empty(), "nessun dato prima della richiesta");

    let req = b"GET /v1/ping HTTP/1.1\r\nHost: api.example.org\r\n\r\n";
    s.receive(t(20), &c.send(req));
    let out = drain(&mut s);
    // ACK dei dati insieme alla risposta, poi FIN (close_after_reply).
    assert_eq!(out.len(), 2, "{out:?}");
    let data = out[0].clone().tcp();
    let fin = out[1].clone().tcp();
    assert_eq!(data.ack, Some(c.seq));
    assert_eq!(data.payload, TcpReply::http_empty().on_data);
    c.take(&data);
    assert!(fin.fin && fin.payload.is_empty());
    c.take(&fin);

    // Il guest riscontra tutto e chiude a sua volta.
    s.receive(t(30), &c.ack_now());
    assert!(drain(&mut s).is_empty());
    s.receive(t(31), &c.fin());
    let last = one(drain(&mut s));
    assert_eq!(last.ack, Some(c.seq), "ACK del FIN del guest");
    assert!(!last.fin && !last.rst);
    // TIME-WAIT: la connessione resta per 4 s virtuali.
    assert_eq!(s.tcp_connections(), 1);
    assert_eq!(s.next_deadline(), Some(t(4031)));
    s.poll(t(4030));
    assert_eq!(s.tcp_connections(), 1);
    s.poll(t(4031));
    assert_eq!(s.tcp_connections(), 0);

    let resp_len = TcpReply::http_empty().on_data.len();
    let flow = Flow { guest: SocketAddrV4::new(GUEST_IP, 41000), remote: dst };
    let at: Vec<_> = s.events().iter().map(|e| (e.at, e.kind.clone())).collect();
    assert_eq!(
        at,
        vec![
            (t(10), EventKind::TcpOpen { id: 2, flow }),
            (t(10), EventKind::TcpEstablished { id: 2 }),
            (t(20), EventKind::TcpData { id: 2, dir: Direction::ToRemote, len: req.len() }),
            (t(20), EventKind::TcpData { id: 2, dir: Direction::ToGuest, len: resp_len }),
            (
                t(4031),
                EventKind::TcpClosed {
                    id: 2,
                    reason: CloseReason::Normal,
                    bytes_to_remote: req.len() as u64,
                    bytes_to_guest: resp_len as u64
                }
            ),
        ]
    );
    let rec = s.upstream().tcp_connection(2).unwrap();
    assert_eq!(rec.hostname.as_deref(), Some("api.example.org"));
    assert_eq!(rec.from_guest, req);
    assert_eq!(rec.to_guest, resp_len as u64);
    assert!(rec.guest_shutdown && !rec.reset);
    assert_eq!(rec.closed_at, Some(t(4031)));
}

#[test]
fn guest_closes_first_and_server_follows() {
    let mut s = stack_with(SinkholeConfig::default());
    let mut c = connect(&mut s, 0, 41001, REMOTE);
    s.receive(t(1), &c.send(b"hello"));
    let ack = one(drain(&mut s));
    assert_eq!(ack.ack, Some(c.seq));
    assert!(ack.payload.is_empty());

    // FIN del guest: lo stack riscontra e, siccome il sinkhole chiude quando
    // il client ha finito, manda subito anche il suo FIN (LAST-ACK).
    s.receive(t(2), &c.fin());
    let fin = one(drain(&mut s));
    assert!(fin.fin);
    assert_eq!(fin.ack, Some(c.seq));
    c.take(&fin);
    s.receive(t(3), &c.ack_now());
    assert!(drain(&mut s).is_empty());
    assert_eq!(s.tcp_connections(), 0, "chiusa senza TIME-WAIT lato host");
    assert_eq!(
        kinds(&s).last().unwrap(),
        &EventKind::TcpClosed { id: 1, reason: CloseReason::Normal, bytes_to_remote: 5, bytes_to_guest: 0 }
    );
}

#[test]
fn banner_retransmitted_until_acknowledged() {
    let mut cfg = SinkholeConfig::default();
    cfg.tcp_by_port.insert(
        25,
        TcpReply {
            on_connect: b"220 vetro ESMTP\r\n".to_vec(),
            on_data: b"250 ok\r\n".to_vec(),
            close_after_reply: false,
        },
    );
    let mut s = stack_with(cfg);
    let dst = SocketAddrV4::new(REMOTE.ip().to_owned(), 25);
    let mut c = Client::new(41002, dst);
    s.receive(t(0), &c.syn());
    let synack = one(drain(&mut s));
    let ack = c.on_syn_ack(&synack);
    s.receive(t(0), &ack);
    let banner = one(drain(&mut s));
    assert_eq!(banner.payload, b"220 vetro ESMTP\r\n");

    // Il guest "perde" il banner: dopo l'RTO iniziale (1 s) torna uguale.
    assert_eq!(s.next_deadline(), Some(t(1000)));
    s.poll(t(999));
    assert!(drain(&mut s).is_empty());
    s.poll(t(1000));
    let again = one(drain(&mut s));
    assert_eq!((again.seq, &again.payload), (banner.seq, &banner.payload));
    // Raddoppio: il prossimo tentativo è a +2 s.
    assert_eq!(s.next_deadline(), Some(t(3000)));
    c.take(&again);
    s.receive(t(1100), &c.ack_now());
    assert_eq!(s.next_deadline(), None, "tutto riscontrato: nessun timer");
    s.poll(t(1150));
    assert!(drain(&mut s).is_empty());

    // L'ACK di dati nuovi ha annullato il raddoppio: la risposta successiva
    // riparte con l'RTO base (1 s: nessun campione valido, per Karn).
    s.receive(t(1200), &c.send(b"EHLO guest\r\n"));
    let reply = one(drain(&mut s));
    assert_eq!(reply.payload, b"250 ok\r\n");
    assert_eq!(s.next_deadline(), Some(t(2200)));
    c.take(&reply);
}

#[test]
fn syn_ack_retransmitted_and_duplicate_syn_answered() {
    let mut s = stack_with(SinkholeConfig::default());
    let mut c = Client::new(41003, REMOTE);
    s.receive(t(0), &c.syn());
    let first = one(drain(&mut s));
    // SYN ritrasmesso dal guest: stesso SYN-ACK.
    s.receive(t(500), &c.syn());
    let dup = one(drain(&mut s));
    assert_eq!((dup.seq, dup.ack, dup.syn), (first.seq, first.ack, true));
    // Nessun ACK: il timer ritrasmette il SYN-ACK.
    s.poll(t(1500));
    let rtx = one(drain(&mut s));
    assert!(rtx.syn && rtx.seq == first.seq);
    let ack = c.on_syn_ack(&rtx);
    s.receive(t(1600), &ack);
    assert!(matches!(kinds(&s)[..], [EventKind::TcpOpen { .. }, EventKind::TcpEstablished { id: 1 }]));
}

#[test]
fn large_response_respects_mss_window_and_ack_clocking() {
    let body: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let cfg = SinkholeConfig {
        tcp_default: TcpReply { on_data: body.clone(), close_after_reply: true, ..Default::default() },
        ..Default::default()
    };
    let mut s = stack_with(cfg);
    let mut c = Client::new(41004, REMOTE);
    c.mss = 536;
    c.window = 3000;
    s.receive(t(0), &c.syn());
    let ack = c.on_syn_ack(&one(drain(&mut s)));
    s.receive(t(0), &ack);
    s.receive(t(1), &c.send(b"GET"));

    let mut got = Vec::new();
    let mut now = 1;
    let mut fin_seen = false;
    for _ in 0..200 {
        let out = drain(&mut s);
        let mut in_flight = 0usize;
        for o in out {
            let seg = o.tcp();
            assert!(seg.payload.len() <= 536, "segmento oltre l'MSS del guest");
            in_flight += seg.payload.len();
            c.take(&seg);
            got.extend_from_slice(&seg.payload);
            fin_seen |= seg.fin;
        }
        assert!(in_flight <= 3000, "oltre la finestra del guest: {in_flight}");
        if fin_seen {
            break;
        }
        now += 1;
        s.receive(t(now), &c.ack_now());
    }
    assert!(fin_seen);
    assert_eq!(got, body);
}

#[test]
fn zero_window_probe_then_window_opens() {
    let cfg = SinkholeConfig {
        tcp_default: TcpReply { on_connect: vec![7u8; 100], ..Default::default() },
        ..Default::default()
    };
    let mut s = stack_with(cfg);
    let mut c = Client::new(41005, REMOTE);
    s.receive(t(0), &c.syn());
    let synack = one(drain(&mut s));
    c.window = 0;
    let ack = c.on_syn_ack(&synack);
    s.receive(t(0), &ack);
    assert!(drain(&mut s).is_empty(), "finestra zero: niente dati");

    // Sonda allo scadere del timer; il guest risponde con finestra ancora
    // zero, quindi la connessione non viene abbandonata anche oltre il
    // numero massimo di ritrasmissioni.
    let mut now = 0;
    for _ in 0..(vetro_net_max_retries() + 3) {
        now = s.next_deadline().unwrap().as_micros().div_ceil(1000);
        s.poll(t(now));
        let probe = one(drain(&mut s));
        assert!(probe.payload.is_empty());
        assert_eq!(probe.seq, c.ack.wrapping_sub(1), "sequenza già riscontrata");
        s.receive(t(now), &c.ack_now());
        assert!(drain(&mut s).is_empty());
    }
    assert_eq!(s.tcp_connections(), 1);

    c.window = 65_535;
    s.receive(t(now + 1), &c.ack_now());
    let seg = one(drain(&mut s));
    assert_eq!(seg.payload, vec![7u8; 100]);
}

/// Stesso valore di `tcp::MAX_RETRIES` (privato nel crate).
fn vetro_net_max_retries() -> usize {
    15
}

#[test]
fn retransmissions_exhausted_reset_and_timeout() {
    let cfg = SinkholeConfig {
        tcp_default: TcpReply { on_connect: b"banner".to_vec(), ..Default::default() },
        ..Default::default()
    };
    let mut s = stack_with(cfg);
    let _c = connect(&mut s, 0, 41006, REMOTE);
    let banner = one(drain(&mut s));
    let mut sent = 0;
    while let Some(d) = s.next_deadline() {
        s.poll(d);
        let out = drain(&mut s);
        let seg = out.last().unwrap().clone().tcp();
        if seg.rst {
            assert_eq!(seg.seq, banner.seq.wrapping_add(6));
            break;
        }
        assert_eq!(seg.payload, b"banner");
        sent += 1;
    }
    assert_eq!(sent, vetro_net_max_retries());
    assert_eq!(s.tcp_connections(), 0);
    assert!(matches!(
        kinds(&s).last().unwrap(),
        EventKind::TcpClosed { reason: CloseReason::Timeout, bytes_to_guest: 6, .. }
    ));
    assert!(s.upstream().tcp_connection(1).unwrap().reset);
}

#[test]
fn guest_reset_closes_connection() {
    let mut s = stack_with(SinkholeConfig::default());
    let mut c = connect(&mut s, 0, 41007, REMOTE);
    s.receive(t(1), &c.send(b"abc"));
    drain(&mut s);
    // RST fuori finestra: ignorato.
    s.receive(t(2), &c.segment(c.seq.wrapping_add(100_000), F_RST, b""));
    assert_eq!(s.tcp_connections(), 1);
    s.receive(t(3), &c.segment(c.seq, F_RST, b""));
    assert_eq!(s.tcp_connections(), 0);
    assert!(drain(&mut s).is_empty(), "nessuna risposta a un RST");
    assert_eq!(
        kinds(&s).last().unwrap(),
        &EventKind::TcpClosed {
            id: 1,
            reason: CloseReason::GuestReset,
            bytes_to_remote: 3,
            bytes_to_guest: 0
        }
    );
    let rec = s.upstream().tcp_connection(1).unwrap();
    assert!(rec.reset);
    assert_eq!(rec.from_guest, b"abc");
}

#[test]
fn refused_port_gets_rst_and_unknown_segments_get_rst() {
    let mut cfg = SinkholeConfig::default();
    cfg.refused_ports.insert(22);
    let mut s = stack_with(cfg);
    let dst = SocketAddrV4::new(*REMOTE.ip(), 22);
    let mut c = Client::new(41008, dst);
    s.receive(t(0), &c.syn());
    let rst = one(drain(&mut s));
    assert!(rst.rst);
    assert_eq!(rst.ack, Some(c.seq.wrapping_add(1)));
    assert_eq!(rst.seq, 0);
    assert_eq!(s.tcp_connections(), 0);
    assert!(matches!(
        kinds(&s)[..],
        [EventKind::TcpOpen { .. }, EventKind::TcpClosed { reason: CloseReason::Refused, .. }]
    ));
    assert!(!s.upstream().tcp_connection(1).unwrap().reset);

    // Segmento con ACK per una connessione che non esiste: RST con seq = ACK.
    c.ack = 777;
    s.receive(t(1), &c.segment(5, F_ACK, b"zz"));
    let r = one(drain(&mut s));
    assert!(r.rst && r.seq == 777 && r.ack.is_none());
    // Senza ACK: RST|ACK che riscontra il segmento.
    s.receive(t(1), &c.segment(5, F_FIN, b"zz"));
    let r = one(drain(&mut s));
    assert!(r.rst && r.seq == 0 && r.ack == Some(8));
    // A un RST non si risponde mai.
    s.receive(t(1), &c.segment(5, F_RST, b""));
    assert!(drain(&mut s).is_empty());
}

#[test]
fn out_of_order_segment_is_dropped_with_duplicate_ack() {
    let mut s = stack_with(SinkholeConfig::default());
    let c = connect(&mut s, 0, 41009, REMOTE);
    let base = c.seq;
    // Arriva prima il secondo pezzo.
    s.receive(t(1), &c.segment(base.wrapping_add(3), F_ACK, b"def"));
    let dup = one(drain(&mut s));
    assert_eq!(dup.ack, Some(base));
    s.receive(t(2), &c.segment(base, F_ACK, b"abc"));
    assert_eq!(one(drain(&mut s)).ack, Some(base.wrapping_add(3)));
    // Ritrasmissione che si sovrappone: si prende solo la parte nuova.
    s.receive(t(3), &c.segment(base.wrapping_add(1), F_ACK, b"bcdef"));
    assert_eq!(one(drain(&mut s)).ack, Some(base.wrapping_add(6)));
    // Duplicato completo: solo un ACK.
    s.receive(t(4), &c.segment(base, F_ACK, b"abc"));
    assert_eq!(one(drain(&mut s)).ack, Some(base.wrapping_add(6)));
    assert_eq!(s.upstream().tcp_connection(1).unwrap().from_guest, b"abcdef");
    let to_remote: usize = s
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::TcpData { dir: Direction::ToRemote, len, .. } => Some(len),
            _ => None,
        })
        .sum();
    assert_eq!(to_remote, 6, "i byte ripetuti non si contano due volte");
}

#[test]
fn syn_in_established_gets_challenge_ack() {
    let mut s = stack_with(SinkholeConfig::default());
    let c = connect(&mut s, 0, 41010, REMOTE);
    s.receive(t(1), &c.segment(c.seq, F_SYN, b""));
    let ch = one(drain(&mut s));
    assert!(!ch.syn && !ch.rst && ch.ack == Some(c.seq));
    assert_eq!(s.tcp_connections(), 1);
}

/// Upstream lento: accetta pochi byte alla volta, così la finestra annunciata
/// al guest si chiude e poi si riapre.
#[derive(Default)]
struct Slow {
    budget: usize,
    got: Vec<u8>,
}

impl vetro_net::Upstream for Slow {
    fn tcp_open(&mut self, _: VirtualTime, _: u64, _: Flow) {}
    fn tcp_status(&mut self, _: VirtualTime, _: u64) -> vetro_net::TcpStatus {
        vetro_net::TcpStatus::Connected
    }
    fn tcp_write(&mut self, _: VirtualTime, _: u64, data: &[u8]) -> usize {
        let n = data.len().min(self.budget);
        self.budget -= n;
        self.got.extend_from_slice(&data[..n]);
        n
    }
    fn tcp_read(&mut self, _: VirtualTime, _: u64, _: &mut [u8]) -> vetro_net::TcpRead {
        vetro_net::TcpRead::WouldBlock
    }
    fn tcp_shutdown(&mut self, _: VirtualTime, _: u64) {}
    fn tcp_close(&mut self, _: VirtualTime, _: u64, _: bool) {}
    fn udp_send(&mut self, _: VirtualTime, _: u64, _: Flow, _: &[u8]) {}
    fn udp_recv(&mut self, _: VirtualTime) -> Option<(u64, Vec<u8>)> {
        None
    }
    fn udp_close(&mut self, _: VirtualTime, _: u64) {}
}

#[test]
fn receive_window_tracks_upstream_backpressure() {
    let mut s = Stack::new(config(), Slow::default());
    let mut c = Client::new(41011, REMOTE);
    s.receive(t(0), &c.syn());
    let ack = c.on_syn_ack(&one(drain(&mut s)));
    s.receive(t(0), &ack);

    let chunk = vec![1u8; 1460];
    let mut sent = 0usize;
    let mut window = 65_535usize;
    // Riempie la finestra: l'upstream non prende niente.
    while window > 0 {
        let n = chunk.len().min(window);
        s.receive(t(1), &c.send(&chunk[..n]));
        sent += n;
        let a = one(drain(&mut s));
        assert_eq!(a.ack, Some(c.seq));
        window = usize::from(a.window);
        assert_eq!(window, 65_535 - sent);
    }
    // Un byte oltre la finestra zero non viene accettato.
    s.receive(t(2), &c.segment(c.seq, F_ACK, b"x"));
    assert_eq!(one(drain(&mut s)).ack, Some(c.seq));

    // L'upstream si libera: aggiornamento di finestra spontaneo.
    s.upstream_mut().budget = 10_000;
    s.poll(t(3));
    let upd = one(drain(&mut s));
    assert_eq!(usize::from(upd.window), 10_000);
    assert_eq!(s.upstream().got.len(), 10_000);
    assert_eq!(sent, 65_535);
}

#[test]
fn same_scenario_is_deterministic_and_seed_changes_isn() {
    fn run(seed: u64) -> (Vec<Vec<u8>>, Vec<vetro_net::NetEvent>) {
        let mut s = Stack::new(NetConfig { seed, ..NetConfig::default() }, Sinkhole::default());
        let mut frames = Vec::new();
        let mut c = Client::new(42000, REMOTE);
        s.receive(t(0), &c.syn());
        let f = s.pop_frame().unwrap();
        let synack = validate(&f).tcp();
        frames.push(f);
        s.receive(t(1), &c.on_syn_ack(&synack));
        s.receive(t(2), &c.send(b"payload"));
        s.receive(t(3), &c.fin());
        s.poll(t(5000));
        frames.extend(std::iter::from_fn(|| s.pop_frame()));
        (frames, s.take_events())
    }
    let a = run(7);
    let b = run(7);
    assert_eq!(a, b);
    let c = run(8);
    assert_ne!(a.0[0], c.0[0], "seme diverso, ISN diverso");
    assert_eq!(a.1.len(), c.1.len());
}

#[test]
fn many_parallel_connections_to_same_port() {
    let mut s = stack_with(SinkholeConfig::default());
    let mut clients = BTreeMap::new();
    for i in 0..20u16 {
        let c = connect(&mut s, 0, 43000 + i, REMOTE);
        clients.insert(i, c);
    }
    for (i, c) in clients.iter_mut() {
        s.receive(t(1), &c.send(format!("conn {i}").as_bytes()));
    }
    drain(&mut s);
    for (i, rec) in s.upstream().tcp_connections().enumerate() {
        assert_eq!(rec.from_guest, format!("conn {i}").as_bytes());
    }
    assert_eq!(s.tcp_connections(), 20);
}

/// Generatore xorshift64 seminato, per perdere frame in modo riproducibile.
const LOSS: u64 = 20;
struct Rng(u64);

impl Rng {
    fn chance(&mut self, percent: u64) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % 100 < percent
    }
}

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// Trasferimento in entrambi i versi, contemporaneo, su un cavo che perde
/// il 20% dei frame in ciascun verso. Il finto guest è un TCP minimo con
/// go-back-N; lo stack deve consegnare tutto, in ordine, e chiudere pulito.
fn lossy_transfer(seed: u64) {
    let download: Vec<u8> = (0..120_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let upload: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
    let cfg = SinkholeConfig {
        tcp_default: TcpReply { on_data: download.clone(), close_after_reply: true, ..Default::default() },
        ..Default::default()
    };
    let mut s = stack_with(cfg);
    let mut c = connect(&mut s, 0, 45000, REMOTE);
    let mut rng = Rng(seed);

    let base = c.seq;
    let end = base.wrapping_add(upload.len() as u32);
    let fin_end = end.wrapping_add(1);
    let (mut acked, mut next) = (base, base);
    let mut peer_window = 65_535u32;
    let mut received = Vec::new();
    let mut got_fin = false;
    let mut last_progress = 0u64;
    let mut now = 1u64;
    let mut done_at = None;

    for _ in 0..50_000 {
        // Frame dallo stack al guest.
        let mut need_ack = false;
        while let Some(f) = s.pop_frame() {
            let seg = validate(&f).tcp();
            if rng.chance(LOSS) {
                continue;
            }
            assert!(!seg.rst, "RST inatteso: {seg:?}");
            if let Some(a) = seg.ack
                && seq_lt(acked, a)
            {
                acked = a;
                last_progress = now;
            }
            peer_window = u32::from(seg.window);
            if !seg.payload.is_empty() || seg.fin {
                need_ack = true;
                if seg.seq == c.ack && !got_fin {
                    received.extend_from_slice(&seg.payload);
                    c.ack = c.ack.wrapping_add(seg.payload.len() as u32);
                    if seg.fin {
                        c.ack = c.ack.wrapping_add(1);
                        got_fin = true;
                    }
                }
            }
        }
        if need_ack && !rng.chance(LOSS) {
            s.receive(t(now), &c.segment(next, F_ACK, b""));
        }
        if got_fin && acked == fin_end {
            done_at = Some(now);
            break;
        }
        // Timeout del guest: go-back-N.
        if acked != next && now - last_progress >= 300 {
            next = acked;
            last_progress = now;
        }
        // Dati (e FIN) del guest nella finestra dello stack.
        let limit = peer_window.min(8_000);
        while next.wrapping_sub(acked) < limit && seq_lt(next, fin_end) {
            let frame = if next == end {
                let f = c.segment(next, F_ACK | F_FIN, b"");
                next = fin_end;
                f
            } else {
                let off = next.wrapping_sub(base) as usize;
                let room = (limit - next.wrapping_sub(acked)) as usize;
                let n = (upload.len() - off).min(1_000).min(room);
                let f = c.segment(next, F_ACK | F_PSH, &upload[off..off + n]);
                next = next.wrapping_add(n as u32);
                f
            };
            if !rng.chance(LOSS) {
                s.receive(t(now), &frame);
            }
        }
        now += 20;
        s.poll(t(now));
    }
    let done_at = done_at.unwrap_or_else(|| {
        panic!(
            "trasferimento non concluso (seme {seed}, scadenza {:?}, ora {now}): acked={} next={} end={end} ricevuti={} fin={got_fin} conn={} ultimi eventi {:?}",
            s.next_deadline(),
            acked.wrapping_sub(base),
            next.wrapping_sub(base),
            received.len(),
            s.tcp_connections(),
            &s.events()[s.events().len().saturating_sub(4)..]
        )
    });
    assert_eq!(received.len(), download.len());
    assert!(received == download, "download corrotto");
    let rec = s.upstream().tcp_connection(1).unwrap();
    assert!(rec.from_guest == upload, "upload corrotto");
    assert!(rec.guest_shutdown);
    // Gli ultimi ACK del guest possono essersi persi: il guest resta in
    // ascolto (senza perdite) e riscontra le ritrasmissioni dello stack.
    // Lo stack ha chiuso per primo: TIME-WAIT, poi si chiude da solo.
    let mut now = done_at;
    while s.tcp_connections() > 0 && now < done_at + 200_000 {
        now = s.next_deadline().map_or(now + 1_000, |d| d.as_micros().div_ceil(1_000));
        s.poll(t(now));
        let resend = drain(&mut s).into_iter().map(Out::tcp).any(|seg| seg.fin || !seg.payload.is_empty());
        if resend {
            s.receive(t(now), &c.segment(fin_end, F_ACK, b""));
        }
    }
    assert_eq!(s.tcp_connections(), 0);
    assert_eq!(
        kinds(&s).last().unwrap(),
        &EventKind::TcpClosed {
            id: 1,
            reason: CloseReason::Normal,
            bytes_to_remote: upload.len() as u64,
            bytes_to_guest: download.len() as u64
        },
        "seme {seed}"
    );
}

#[test]
fn lossy_link_bulk_transfer_both_directions() {
    for seed in 1..=12u64 {
        lossy_transfer(seed);
    }
}
