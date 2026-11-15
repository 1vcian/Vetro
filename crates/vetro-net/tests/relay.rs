//! The machine's `Uplink` (ADR 0048): the stack with the gateway protocol,
//! the gateway played by the test (it drains the outbox and answers with
//! frames, as the page's Worker does with `Machine::input`).

mod common;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};

use common::*;
use vetro_net::dns::{self, RCODE_NOERROR, TYPE_A, TYPE_AAAA};
use vetro_net::relay::{MAX_DATA, WINDOW};
use vetro_net::{CloseReason, EventKind, Frame, Refusal, Side, Sinkhole, SinkholeConfig, Stack, Uplink};
use vetro_snapshot::{Reader, Snapshot, Writer};

fn offline() -> Stack<Uplink> {
    Stack::new(config(), Uplink::new(Sinkhole::new(SinkholeConfig::default())))
}

fn online() -> Stack<Uplink> {
    let mut s = offline();
    assert!(s.upstream_mut().input(Frame::Online(true)));
    s
}

fn outbox(s: &mut Stack<Uplink>) -> Vec<Frame> {
    s.upstream_mut().take_outbox()
}

fn give(s: &mut Stack<Uplink>, now: u64, f: Frame) {
    // As the machine does: decode the recorded bytes, apply, poll.
    let bytes = f.encode_from(Side::Gateway);
    let (side, f) = Frame::decode(&bytes).expect("valid frame");
    assert_eq!(side, Side::Gateway);
    s.upstream_mut().input(f);
    s.poll(t(now));
}

const SERVER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(93, 184, 215, 14), 443);

/// Opens a guest connection through the gateway and returns it established.
fn open(s: &mut Stack<Uplink>, sport: u16, dst: SocketAddrV4) -> Client {
    let mut c = Client::new(sport, dst);
    s.receive(t(0), &c.syn());
    assert!(drain(s).is_empty(), "no SYN-ACK before the gateway answers");
    let id = match outbox(s).as_slice() {
        [Frame::Connect { id, dst: d }] if *d == dst => *id,
        o => panic!("{o:?}"),
    };
    give(s, 1, Frame::Connected { id });
    let ack = c.on_syn_ack(&drain(s).remove(0).tcp());
    s.receive(t(1), &ack);
    c
}

#[test]
fn offline_is_the_sinkhole() {
    let mut s = offline();
    assert!(!s.upstream().online());
    let dns_srv = SocketAddrV4::new(DNS_IP, 53);
    s.receive(t(0), &Guest::udp(5353, dns_srv, &dns::build_query(1, "example.com", TYPE_A)));
    let Out::Udp { payload, .. } = drain(&mut s).remove(0) else { panic!() };
    assert_eq!(dns::parse_response(&payload).unwrap().addrs, vec![Ipv4Addr::new(198, 18, 0, 1)]);
    assert!(outbox(&mut s).is_empty(), "nothing goes to the gateway");
}

#[test]
fn dns_through_the_gateway_and_aaaa_locally() {
    let mut s = online();
    let dns_srv = SocketAddrV4::new(DNS_IP, 53);
    let q = dns::build_query(7, "example.com", TYPE_A);
    s.receive(t(0), &Guest::udp(5353, dns_srv, &q));
    s.receive(t(0), &Guest::udp(5354, dns_srv, &dns::build_query(8, "example.com", TYPE_AAAA)));
    // AAAA: answered here with no records.
    let Out::Udp { payload, dst, .. } = drain(&mut s).remove(0) else { panic!() };
    assert_eq!(dst.port(), 5354);
    let r = dns::parse_response(&payload).unwrap();
    assert_eq!((r.rcode, r.addrs.len()), (RCODE_NOERROR, 0));
    // A: the raw query goes to the gateway.
    let id = match outbox(&mut s).as_slice() {
        [Frame::Dns { id, msg, answer: false }] if *msg == q => *id,
        o => panic!("{o:?}"),
    };
    let real = Ipv4Addr::new(93, 184, 215, 14);
    let answer = dns::build_response(&dns::parse_query(&q).unwrap(), RCODE_NOERROR, &[real], 60);
    give(&mut s, 2, Frame::Dns { id, msg: answer, answer: true });
    let Out::Udp { payload, dst, .. } = drain(&mut s).remove(0) else { panic!() };
    assert_eq!(dst.port(), 5353);
    assert_eq!(dns::parse_response(&payload).unwrap().addrs, vec![real]);
    assert!(
        s.events()
            .iter()
            .any(|e| matches!(&e.kind, EventKind::DnsAnswer { addrs, .. } if *addrs == vec![real]))
    );
    // Other UDP is dropped online; ICMP to the outside is not answered.
    s.receive(t(3), &Guest::udp(5000, SocketAddrV4::new(real, 443), b"quic"));
    assert!(drain(&mut s).is_empty() && outbox(&mut s).is_empty());
    assert_eq!(s.upstream().stats().udp_dropped, 1);
    // A DNS answer for a flow that is not the gateway's does not apply.
    assert!(!s.upstream_mut().input(Frame::Dns { id: 99, msg: vec![0; 12], answer: true }));
}

/// The fake gateway end of one connection: echoes the guest's bytes back,
/// within the window, acknowledging what it "wrote to the socket".
#[derive(Default)]
struct Echo {
    got: Vec<u8>,
    pending: VecDeque<u8>,
    /// Sent to the machine and not yet acknowledged by it.
    in_flight: u32,
    shut: bool,
    closed: Option<bool>,
}

impl Echo {
    fn serve(&mut self, s: &mut Stack<Uplink>, id: u32, now: u64) {
        for f in outbox(s) {
            match f {
                Frame::Data { id: i, data } => {
                    assert_eq!(i, id);
                    assert!(!data.is_empty() && data.len() <= MAX_DATA);
                    self.got.extend(&data);
                    self.pending.extend(&data);
                    give(s, now, Frame::Ack { id, n: data.len() as u32 });
                }
                Frame::Ack { id: i, n } => {
                    assert_eq!(i, id);
                    assert!(n <= self.in_flight, "machine acked more than sent");
                    self.in_flight -= n;
                }
                Frame::Shutdown { id: i } if i == id => self.shut = true,
                Frame::Close { id: i, reset } if i == id => self.closed = Some(reset),
                f => panic!("{f:?}"),
            }
        }
        while !self.pending.is_empty() && self.in_flight < WINDOW {
            let n = self.pending.len().min(MAX_DATA).min((WINDOW - self.in_flight) as usize);
            let data: Vec<u8> = self.pending.drain(..n).collect();
            self.in_flight += n as u32;
            give(s, now, Frame::Data { id, data });
        }
        if self.shut && self.pending.is_empty() {
            self.shut = false;
            give(s, now, Frame::Shutdown { id });
        }
    }
}

#[test]
fn echo_beyond_the_window_both_ways() {
    let mut s = online();
    let mut c = open(&mut s, 44000, SERVER);
    let data: Vec<u8> = (0..700_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
    let mut sent = 0;
    let mut received: Vec<u8> = Vec::new();
    let mut stack_acked = c.seq;
    let mut stack_window = 65_535u32;
    let mut echo = Echo::default();
    let mut now = 2;
    let mut fin_sent = false;
    while echo.closed.is_none() && now < 200_000 {
        now += 1;
        // The guest sends within the stack's window.
        let mut burst = 0;
        while sent < data.len() && burst < 8 {
            let in_flight = c.seq.wrapping_sub(stack_acked);
            let n = (data.len() - sent).min(1460);
            if in_flight + n as u32 > stack_window {
                break;
            }
            s.receive(t(now), &c.send(&data[sent..sent + n]));
            sent += n;
            burst += 1;
        }
        if sent == data.len() && !fin_sent && c.seq == stack_acked {
            s.receive(t(now), &c.fin());
            fin_sent = true;
        }
        echo.serve(&mut s, 1, now);
        s.poll(t(now));
        for seg in drain(&mut s).into_iter().map(Out::tcp) {
            if let Some(a) = seg.ack {
                stack_acked = a;
                stack_window = u32::from(seg.window);
            }
            if seg.seq == c.ack && (!seg.payload.is_empty() || seg.fin) {
                received.extend(&seg.payload);
                c.take(&seg);
                s.receive(t(now), &c.ack_now());
            }
        }
    }
    assert_eq!(echo.got, data, "the gateway got the guest's bytes in order");
    assert_eq!(received, data, "the guest got the gateway's bytes in order");
    assert_eq!(echo.closed, Some(false), "orderly close");
    assert_eq!(s.tcp_connections(), 0);
    assert!(matches!(
        s.events().last().unwrap().kind,
        EventKind::TcpClosed {
            reason: CloseReason::Normal,
            bytes_to_remote: 700_000,
            bytes_to_guest: 700_000,
            ..
        }
    ));
}

#[test]
fn credit_bounds_what_the_guest_can_send() {
    let mut s = online();
    let mut c = open(&mut s, 44001, SERVER);
    let mut total = 0usize;
    // Nobody acknowledges: the uplink takes WINDOW bytes, then the stack's
    // receive buffer fills and the advertised window closes.
    for i in 0..400 {
        s.receive(t(2 + i), &c.send(&[0x55; 1000]));
        total += outbox(&mut s)
            .iter()
            .map(|f| match f {
                Frame::Data { data, .. } => data.len(),
                f => panic!("{f:?}"),
            })
            .sum::<usize>();
    }
    assert_eq!(total, WINDOW as usize);
    // An ACK frees credit; an ACK beyond the window is a violation.
    give(&mut s, 500, Frame::Ack { id: 1, n: 1000 });
    let more: usize =
        outbox(&mut s).iter().map(|f| if let Frame::Data { data, .. } = f { data.len() } else { 0 }).sum();
    assert_eq!(more, 1000);
    assert!(!s.upstream_mut().input(Frame::Ack { id: 1, n: WINDOW + 1 }));
}

#[test]
fn gateway_over_its_window_resets_the_connection() {
    let mut s = online();
    let _c = open(&mut s, 44002, SERVER);
    let mut sent = 0;
    while sent <= WINDOW as usize {
        s.upstream_mut().input(Frame::Data { id: 1, data: vec![1; MAX_DATA] });
        sent += MAX_DATA;
    }
    s.poll(t(3));
    assert!(drain(&mut s).into_iter().map(Out::tcp).any(|seg| seg.rst));
    assert!(
        s.events()
            .iter()
            .any(|e| matches!(e.kind, EventKind::TcpClosed { reason: CloseReason::RemoteReset, .. }))
    );
}

#[test]
fn refusal_and_timeout_and_down() {
    let mut s = online();
    let dst80 = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 80);
    let a = Client::new(45000, SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 22));
    s.receive(t(0), &a.syn());
    give(&mut s, 1, Frame::Refused { id: 1, reason: Refusal::Port });
    assert!(drain(&mut s).remove(0).tcp().rst);
    assert!(matches!(
        s.events().last().unwrap().kind,
        EventKind::TcpClosed { reason: CloseReason::Refused, .. }
    ));
    // Refused: already gone at the gateway, no CLOSE.
    assert_eq!(outbox(&mut s), vec![Frame::Connect { id: 1, dst: a.dst }]);

    // No answer: RST after 75 s, CLOSE with reset to the gateway.
    let b = Client::new(45001, dst80);
    s.receive(t(2), &b.syn());
    outbox(&mut s);
    s.poll(t(75_002));
    assert!(drain(&mut s).remove(0).tcp().rst);
    assert_eq!(outbox(&mut s), vec![Frame::Close { id: 2, reset: true }]);

    // DOWN: a pending connection is refused, an open one is reset.
    let _open = open(&mut s, 45002, dst80);
    let p = Client::new(45003, dst80);
    s.receive(t(75_010), &p.syn());
    outbox(&mut s);
    give(&mut s, 75_011, Frame::Down);
    let segs: Vec<_> = drain(&mut s).into_iter().map(Out::tcp).collect();
    assert_eq!(segs.iter().filter(|g| g.rst).count(), 2, "{segs:?}");
    assert_eq!(s.tcp_connections(), 0);
    assert_eq!(s.upstream().gateway_connections(), 0);
    assert_eq!(s.upstream().stats().refused, 2);
}

#[test]
fn fake_addresses_go_out_by_name_and_offline_connections_stay() {
    let mut s = offline();
    let dns_srv = SocketAddrV4::new(DNS_IP, 53);
    s.receive(t(0), &Guest::udp(5353, dns_srv, &dns::build_query(1, "vetro.example", TYPE_A)));
    drain(&mut s);
    let fake = SocketAddrV4::new(Ipv4Addr::new(198, 18, 0, 1), 80);
    // An offline connection: the sinkhole answers it.
    let mut old = Client::new(46000, fake);
    s.receive(t(1), &old.syn());
    let ack = old.on_syn_ack(&drain(&mut s).remove(0).tcp());
    s.receive(t(1), &ack);

    give(&mut s, 2, Frame::Online(true));
    let c = Client::new(46001, SocketAddrV4::new(*fake.ip(), 443));
    s.receive(t(3), &c.syn());
    assert_eq!(outbox(&mut s), vec![Frame::ConnectName { id: 3, name: "vetro.example".into(), port: 443 }]);
    // The old connection still talks to the sinkhole (HTTP 200).
    s.receive(t(4), &old.send(b"GET / HTTP/1.0\r\n\r\n"));
    let reply: Vec<u8> = drain(&mut s).into_iter().map(Out::tcp).flat_map(|g| g.payload).collect();
    assert!(reply.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&reply));
    assert!(outbox(&mut s).is_empty());
}

fn save(u: &Uplink) -> Vec<u8> {
    let mut w = Writer::new();
    u.save(&mut w);
    w.into_bytes()
}

#[test]
fn snapshot_with_and_without_the_gateway_section() {
    let mut s = online();
    let _c = open(&mut s, 47000, SERVER);
    give(&mut s, 2, Frame::Data { id: 1, data: b"pending".to_vec() });
    let bytes = save(s.upstream());
    let mut copy = Uplink::new(Sinkhole::new(SinkholeConfig::default()));
    let mut r = Reader::new(&bytes);
    copy.restore(&mut r).unwrap();
    r.finish().unwrap();
    assert_eq!(save(&copy), bytes);
    assert!(copy.online() && copy.gateway_connections() == 1);

    // A snapshot from before ADR 0048: only the sinkhole. Offline, empty.
    let mut w = Writer::new();
    Sinkhole::new(SinkholeConfig::default()).save(&mut w);
    let old = w.into_bytes();
    let mut r = Reader::new(&old);
    copy.restore(&mut r).unwrap();
    r.finish().unwrap();
    assert!(!copy.online() && copy.gateway_connections() == 0);
}

#[test]
fn same_inputs_same_frames() {
    let run = || {
        let mut s = online();
        let mut c = open(&mut s, 48000, SERVER);
        s.receive(t(2), &c.send(b"hello"));
        give(&mut s, 3, Frame::Data { id: 1, data: b"world".to_vec() });
        let mut frames = Vec::new();
        while let Some(f) = s.pop_frame() {
            frames.push(f);
        }
        (frames, outbox(&mut s), format!("{:?}", s.events()))
    };
    assert_eq!(run(), run());
}

/// The frames of the shared vectors (`relay/test/vectors.txt`), which the JS
/// implementation (`relay/src/protocol.mjs`) builds and checks too.
fn vector_frames() -> Vec<(&'static str, Side, Frame)> {
    let dns_q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    let dns_a = vec![0x12, 0x34, 0x81, 0x80, 0, 0, 0, 0, 0, 0, 0, 0];
    vec![
        ("connect", Side::Machine, Frame::Connect { id: 7, dst: SERVER }),
        ("connect_name", Side::Machine, Frame::ConnectName { id: 8, name: "example.com".into(), port: 80 }),
        ("data_machine", Side::Machine, Frame::Data { id: 7, data: b"hello".to_vec() }),
        ("data_gateway", Side::Gateway, Frame::Data { id: 7, data: b"world".to_vec() }),
        ("shutdown_machine", Side::Machine, Frame::Shutdown { id: 7 }),
        ("shutdown_gateway", Side::Gateway, Frame::Shutdown { id: 7 }),
        ("close_machine", Side::Machine, Frame::Close { id: 7, reset: false }),
        ("close_gateway", Side::Gateway, Frame::Close { id: 9, reset: true }),
        ("ack_machine", Side::Machine, Frame::Ack { id: 7, n: 1000 }),
        ("ack_gateway", Side::Gateway, Frame::Ack { id: 7, n: WINDOW }),
        ("dns", Side::Machine, Frame::Dns { id: 3, msg: dns_q, answer: false }),
        ("dns_answer", Side::Gateway, Frame::Dns { id: 3, msg: dns_a, answer: true }),
        ("connected", Side::Gateway, Frame::Connected { id: 7 }),
        ("refused", Side::Gateway, Frame::Refused { id: 9, reason: Refusal::Platform }),
        ("online", Side::Gateway, Frame::Online(true)),
        ("down", Side::Gateway, Frame::Down),
        ("big_id", Side::Machine, Frame::Data { id: 0xffff_fffe, data: vec![0xff] }),
    ]
}

#[test]
fn shared_vectors() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../relay/test/vectors.txt");
    let mut text = String::from("# Gateway protocol vectors (ADR 0048): name, side, frame in hex.\n");
    text.push_str(
        "# Written by `VETRO_WRITE_VECTORS=1 cargo test -p vetro-net --test relay shared_vectors`,\n",
    );
    text.push_str("# checked by that test and by relay/test/unit.mjs.\n");
    for (name, side, f) in vector_frames() {
        let bytes = f.encode_from(side);
        assert_eq!(Frame::decode(&bytes), Some((side, f.clone())));
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let side = if side == Side::Machine { "machine" } else { "gateway" };
        text.push_str(&format!("{name} {side} {hex}\n"));
    }
    if std::env::var_os("VETRO_WRITE_VECTORS").is_some() {
        std::fs::write(path, &text).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).expect("relay/test/vectors.txt"), text);
}
