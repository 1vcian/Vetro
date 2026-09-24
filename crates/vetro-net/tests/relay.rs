//! Lo stack con il relay di prova in memoria.

mod common;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};

use common::*;
use vetro_net::dns::{self, RCODE_NXDOMAIN, TYPE_A};
use vetro_net::{CloseReason, EventKind, MemoryRelay, Relay, RelayMessage, RelayUpstream, Stack};

fn relay_stack() -> Stack<RelayUpstream<MemoryRelay>> {
    let mut relay = MemoryRelay { refused_ports: vec![23], ..Default::default() };
    relay.hosts.insert("echo.vetro.test".into(), Ipv4Addr::new(192, 0, 2, 7));
    Stack::new(config(), RelayUpstream::new(relay))
}

#[test]
fn dns_through_relay() {
    let mut s = relay_stack();
    let dns_srv = SocketAddrV4::new(DNS_IP, 53);
    s.receive(t(0), &Guest::udp(5353, dns_srv, &dns::build_query(1, "echo.vetro.test", TYPE_A)));
    s.receive(t(0), &Guest::udp(5354, dns_srv, &dns::build_query(2, "missing.test", TYPE_A)));
    let out = drain(&mut s);
    let answers: Vec<_> = out
        .iter()
        .map(|o| match o {
            Out::Udp { payload, .. } => dns::parse_response(payload).unwrap(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(answers[0].addrs, vec![Ipv4Addr::new(192, 0, 2, 7)]);
    assert_eq!(answers[1].rcode, RCODE_NXDOMAIN);
    assert!(s.events().iter().any(|e| matches!(
        &e.kind,
        EventKind::DnsAnswer { name, rcode: 3, .. } if name == "missing.test"
    )));
}

#[test]
fn tcp_echo_through_relay_and_close() {
    let mut s = relay_stack();
    let dst = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 7);
    let mut c = Client::new(44000, dst);
    s.receive(t(0), &c.syn());
    let ack = c.on_syn_ack(&drain(&mut s).remove(0).tcp());
    s.receive(t(0), &ack);
    s.receive(t(1), &c.send(b"echo me"));
    let out = drain(&mut s);
    let seg = out.into_iter().map(Out::tcp).find(|s| !s.payload.is_empty()).unwrap();
    assert_eq!(seg.payload, b"echo me");
    c.take(&seg);
    // FIN del guest: il relay chiude il suo verso, lo stack manda FIN.
    s.receive(t(2), &c.fin());
    let fin = drain(&mut s).into_iter().map(Out::tcp).find(|s| s.fin).unwrap();
    c.take(&fin);
    s.receive(t(3), &c.ack_now());
    assert_eq!(s.tcp_connections(), 0);

    let sent = &s.upstream().relay().sent;
    assert_eq!(
        sent[..],
        [
            RelayMessage::TcpConnect { id: 1, dst },
            RelayMessage::TcpData { id: 1, data: b"echo me".to_vec() },
            RelayMessage::TcpShutdown { id: 1 },
            RelayMessage::TcpClose { id: 1, reset: false },
        ]
    );
    assert!(matches!(
        s.events().last().unwrap().kind,
        EventKind::TcpClosed { reason: CloseReason::Normal, .. }
    ));
}

/// Relay pilotato a mano dal test: niente risposte automatiche.
#[derive(Default)]
struct Manual {
    sent: Vec<RelayMessage>,
    inbox: VecDeque<RelayMessage>,
}

impl Relay for Manual {
    fn send(&mut self, msg: RelayMessage) {
        self.sent.push(msg);
    }
    fn recv(&mut self) -> Option<RelayMessage> {
        self.inbox.pop_front()
    }
}

#[test]
fn relay_refusal() {
    let mut s = relay_stack();
    let c = Client::new(44001, SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 23));
    s.receive(t(0), &c.segment(c.seq, F_SYN, b""));
    assert!(drain(&mut s).remove(0).tcp().rst);
    assert!(matches!(
        s.events().last().unwrap().kind,
        EventKind::TcpClosed { reason: CloseReason::Refused, .. }
    ));
}

#[test]
fn pending_connect_waits_for_relay_then_times_out() {
    let mut s = Stack::new(config(), RelayUpstream::new(Manual::default()));
    let dst = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 80);
    let mut a = Client::new(44002, dst);
    let b = Client::new(44003, dst);
    s.receive(t(0), &a.syn());
    s.receive(t(0), &b.syn());
    assert!(drain(&mut s).is_empty(), "in attesa del relay: nessun SYN-ACK");
    // SYN ritrasmesso: ancora niente.
    s.receive(t(1000), &a.syn());
    assert!(drain(&mut s).is_empty());

    // Il relay apre la prima connessione: SYN-ACK al giro di poll.
    s.upstream_mut().relay_mut().inbox.push_back(RelayMessage::TcpConnected { id: 1 });
    s.poll(t(1500));
    let synack = drain(&mut s).remove(0).tcp();
    assert!(synack.syn && synack.dst.port() == 44002);
    s.receive(t(1500), &a.on_syn_ack(&synack));

    // La seconda non riceve mai risposta: RST dopo 75 s.
    assert_eq!(s.next_deadline(), Some(t(75_000)));
    s.poll(t(75_000));
    let rst = drain(&mut s).remove(0).tcp();
    assert!(rst.rst && rst.dst.port() == 44003);
    assert!(s.events().iter().any(|e| e.kind
        == EventKind::TcpClosed {
            id: 2,
            reason: CloseReason::Timeout,
            bytes_to_remote: 0,
            bytes_to_guest: 0
        }));
    assert_eq!(s.upstream().relay().sent.last(), Some(&RelayMessage::TcpClose { id: 2, reset: true }));
}
