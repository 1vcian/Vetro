//! Gateway virtuale: DHCP, ARP, ICMP, DNS e UDP, visti da un finto guest.

mod common;

use std::net::{Ipv4Addr, SocketAddrV4};

use common::*;
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, DnsPacket, DnsRcode, EthernetAddress};
use vetro_net::dns::{self, TYPE_A, TYPE_AAAA};
use vetro_net::wire::{self, Mac};
use vetro_net::{CloseReason, DhcpMessage, Direction, EventKind, NetConfig, Sinkhole, SinkholeConfig, Stack};

fn sinkhole_stack() -> Stack<Sinkhole> {
    Stack::new(config(), Sinkhole::new(SinkholeConfig::default()))
}

/// Messaggio DHCP del client costruito con smoltcp (codificatore indipendente).
fn dhcp_client(message_type: DhcpMessageType, requested: Option<Ipv4Addr>, broadcast: bool) -> Vec<u8> {
    let hostname = [smoltcp::wire::DhcpOption { kind: 12, data: b"vetro-guest" }];
    let repr = DhcpRepr {
        message_type,
        transaction_id: 0xdead_beef,
        secs: 0,
        client_hardware_address: EthernetAddress(GUEST_MAC.0),
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip: Ipv4Addr::UNSPECIFIED,
        server_ip: Ipv4Addr::UNSPECIFIED,
        router: None,
        subnet_mask: None,
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        broadcast,
        requested_ip: requested,
        client_identifier: None,
        server_identifier: requested.map(|_| GW_IP),
        parameter_request_list: Some(&[1, 3, 6, 51]),
        dns_servers: None,
        max_size: None,
        lease_duration: None,
        renew_duration: None,
        rebind_duration: None,
        additional_options: &hostname,
    };
    let mut buf = vec![0u8; repr.buffer_len()];
    repr.emit(&mut DhcpPacket::new_unchecked(&mut buf[..])).unwrap();
    // Dal client senza indirizzo: 0.0.0.0:68 → 255.255.255.255:67.
    let udp = wire::build_udp(Ipv4Addr::UNSPECIFIED, 68, Ipv4Addr::BROADCAST, 67, &buf);
    let ip = wire::build_ipv4(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, wire::PROTO_UDP, 1, &udp);
    wire::build_eth(Mac::BROADCAST, GUEST_MAC, wire::ETHERTYPE_IPV4, &ip)
}

fn expect_dhcp(out: &Out, want: DhcpMessageType, eth_dst: Mac) -> (Ipv4Addr, Vec<u8>) {
    let Out::Udp { eth_dst: d, src, dst, payload } = out else { panic!("atteso UDP: {out:?}") };
    assert_eq!(*d, eth_dst);
    assert_eq!(*src, SocketAddrV4::new(GW_IP, 67));
    assert_eq!(*dst, SocketAddrV4::new(Ipv4Addr::BROADCAST, 68));
    assert!(payload.len() >= 300, "risposta BOOTP di almeno 300 byte");
    let pkt = DhcpPacket::new_checked(&payload[..]).unwrap();
    let r = DhcpRepr::parse(&pkt).expect("DHCP analizzabile da smoltcp");
    assert_eq!(r.message_type, want);
    assert_eq!(r.transaction_id, 0xdead_beef);
    assert_eq!(r.client_hardware_address.0, GUEST_MAC.0);
    assert_eq!(r.server_identifier, Some(GW_IP));
    if want != DhcpMessageType::Nak {
        assert_eq!(r.subnet_mask, Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(r.router, Some(GW_IP));
        assert_eq!(r.dns_servers.as_ref().map(|d| d.to_vec()), Some(vec![DNS_IP]));
        assert_eq!(r.lease_duration, Some(86_400));
        assert_eq!(r.renew_duration, Some(43_200));
        assert_eq!(r.rebind_duration, Some(75_600));
    }
    (r.your_ip, payload.clone())
}

#[test]
fn dhcp_discover_offer_request_ack() {
    let mut s = sinkhole_stack();
    s.receive(t(0), &dhcp_client(DhcpMessageType::Discover, None, false));
    let out = drain(&mut s);
    assert_eq!(out.len(), 1);
    let (yiaddr, _) = expect_dhcp(&out[0], DhcpMessageType::Offer, GUEST_MAC);
    assert_eq!(yiaddr, GUEST_IP);

    s.receive(t(10), &dhcp_client(DhcpMessageType::Request, Some(GUEST_IP), true));
    let out = drain(&mut s);
    let (yiaddr, _) = expect_dhcp(&out[0], DhcpMessageType::Ack, Mac::BROADCAST);
    assert_eq!(yiaddr, GUEST_IP);

    // Indirizzo sbagliato: NAK in broadcast.
    s.receive(t(20), &dhcp_client(DhcpMessageType::Request, Some(Ipv4Addr::new(10, 0, 2, 99)), false));
    let out = drain(&mut s);
    expect_dhcp(&out[0], DhcpMessageType::Nak, Mac::BROADCAST);

    let host = Some("vetro-guest".to_owned());
    let dhcp: Vec<_> = s.events().iter().map(|e| (e.at, e.kind.clone())).collect();
    assert_eq!(
        dhcp,
        vec![
            (
                t(0),
                EventKind::Dhcp {
                    message: DhcpMessage::Offer,
                    mac: GUEST_MAC,
                    ip: GUEST_IP,
                    hostname: host.clone()
                }
            ),
            (
                t(10),
                EventKind::Dhcp {
                    message: DhcpMessage::Ack,
                    mac: GUEST_MAC,
                    ip: GUEST_IP,
                    hostname: host.clone()
                }
            ),
            (
                t(20),
                EventKind::Dhcp {
                    message: DhcpMessage::Nak,
                    mac: GUEST_MAC,
                    ip: Ipv4Addr::UNSPECIFIED,
                    hostname: host
                }
            ),
        ]
    );
    assert_eq!(s.guest_mac(), Some(GUEST_MAC));
}

#[test]
fn dhcp_request_for_another_server_is_ignored() {
    let mut s = sinkhole_stack();
    let mut frame = dhcp_client(DhcpMessageType::Request, Some(GUEST_IP), false);
    // Cambia il server id (opzione 54) con un altro server e ricalcola il
    // checksum UDP azzerandolo (0 = assente in IPv4).
    let pos = frame.windows(6).position(|w| w == [54, 4, 10, 0, 2, 2]).unwrap();
    frame[pos + 5] = 77;
    frame[14 + 20 + 6..14 + 20 + 8].copy_from_slice(&[0, 0]);
    s.receive(t(0), &frame);
    assert!(drain(&mut s).is_empty());
}

#[test]
fn arp_answers_only_for_gateway_addresses() {
    let mut s = sinkhole_stack();
    for target in [GW_IP, DNS_IP] {
        s.receive(t(0), &Guest::arp_request(target));
        let out = drain(&mut s);
        assert_eq!(
            out,
            vec![Out::Arp {
                op: 2,
                sender_mac: GW_MAC,
                sender_ip: target,
                target_mac: GUEST_MAC,
                target_ip: GUEST_IP
            }]
        );
    }
    // Il probe ARP del guest per il proprio indirizzo non deve avere risposta
    // (altrimenti il guest vedrebbe un conflitto), né un host inesistente.
    for target in [GUEST_IP, Ipv4Addr::new(10, 0, 2, 99)] {
        s.receive(t(0), &Guest::arp_request(target));
        assert!(drain(&mut s).is_empty());
    }
}

fn echo_request(id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut rest = [0u8; 4];
    rest[..2].copy_from_slice(&id.to_be_bytes());
    rest[2..].copy_from_slice(&seq.to_be_bytes());
    wire::build_icmp(wire::ICMP_ECHO_REQUEST, 0, rest, data)
}

#[test]
fn icmp_echo_gateway_and_external() {
    let mut s =
        Stack::new(config(), Sinkhole::new(SinkholeConfig { answer_ping: false, ..Default::default() }));
    let ext = Ipv4Addr::new(93, 184, 216, 34);
    for dst in [GW_IP, DNS_IP, ext] {
        s.receive(t(5), &Guest::ipv4(dst, wire::PROTO_ICMP, &echo_request(7, 3, b"ping-vetro")));
    }
    let out = drain(&mut s);
    assert_eq!(out.len(), 2, "il gateway e il DNS rispondono, l'esterno no (answer_ping falso)");
    for (o, src) in out.iter().zip([GW_IP, DNS_IP]) {
        assert_eq!(
            *o,
            Out::Icmp {
                src,
                dst: GUEST_IP,
                ty: 0,
                code: 0,
                rest: [0, 7, 0, 3],
                data: b"ping-vetro".to_vec()
            }
        );
    }
    let answered: Vec<_> = s
        .events()
        .iter()
        .map(|e| match &e.kind {
            EventKind::IcmpEcho { dst, answered } => (*dst, *answered),
            k => panic!("{k:?}"),
        })
        .collect();
    assert_eq!(answered, vec![(GW_IP, true), (DNS_IP, true), (ext, false)]);

    let mut s = sinkhole_stack();
    s.receive(t(0), &Guest::ipv4(ext, wire::PROTO_ICMP, &echo_request(1, 1, b"x")));
    assert!(matches!(drain(&mut s)[..], [Out::Icmp { src, ty: 0, .. }] if src == ext));
}

fn dns_ask(
    s: &mut Stack<Sinkhole>,
    now: u64,
    sport: u16,
    txid: u16,
    name: &str,
    qtype: u16,
) -> dns::Response {
    s.receive(
        t(now),
        &Guest::udp(sport, SocketAddrV4::new(DNS_IP, 53), &dns::build_query(txid, name, qtype)),
    );
    let out = drain(s);
    assert_eq!(out.len(), 1);
    let Out::Udp { src, dst, payload, .. } = &out[0] else { panic!("{out:?}") };
    assert_eq!(*src, SocketAddrV4::new(DNS_IP, 53));
    assert_eq!(*dst, SocketAddrV4::new(GUEST_IP, sport));
    // Validazione indipendente dell'intestazione DNS.
    let p = DnsPacket::new_checked(&payload[..]).unwrap();
    assert_eq!(p.transaction_id(), txid);
    assert_eq!(p.rcode(), DnsRcode::NoError);
    assert_eq!(p.question_count(), 1);
    dns::parse_response(payload).unwrap()
}

#[test]
fn dns_resolves_to_deterministic_fake_addresses() {
    let mut s = sinkhole_stack();
    let a = dns_ask(&mut s, 0, 40000, 1, "example.com", TYPE_A);
    assert_eq!(a.addrs, vec![Ipv4Addr::new(198, 18, 0, 1)]);
    let b = dns_ask(&mut s, 1, 40001, 2, "tracker.example.net", TYPE_A);
    assert_eq!(b.addrs, vec![Ipv4Addr::new(198, 18, 0, 2)]);
    // Stesso nome (anche con maiuscole): stesso indirizzo.
    let c = dns_ask(&mut s, 2, 40002, 3, "EXAMPLE.com", TYPE_A);
    assert_eq!(c.addrs, vec![Ipv4Addr::new(198, 18, 0, 1)]);
    // AAAA: nessun record, così il guest usa IPv4.
    let d = dns_ask(&mut s, 3, 40003, 4, "example.com", TYPE_AAAA);
    assert!(d.addrs.is_empty());

    let names: Vec<_> =
        s.upstream().dns_queries().iter().map(|q| (q.name.as_str(), q.qtype, q.answer)).collect();
    assert_eq!(
        names,
        vec![
            ("example.com", TYPE_A, Some(Ipv4Addr::new(198, 18, 0, 1))),
            ("tracker.example.net", TYPE_A, Some(Ipv4Addr::new(198, 18, 0, 2))),
            ("example.com", TYPE_A, Some(Ipv4Addr::new(198, 18, 0, 1))),
            ("example.com", TYPE_AAAA, None),
        ]
    );
    assert_eq!(s.upstream().hostname(Ipv4Addr::new(198, 18, 0, 2)), Some("tracker.example.net"));

    // Registro: per la prima domanda UdpOpen, UdpData, DnsQuery, UdpData, DnsAnswer.
    let first: Vec<_> = s.events().iter().take(5).map(|e| e.kind.clone()).collect();
    let flow =
        vetro_net::Flow { guest: SocketAddrV4::new(GUEST_IP, 40000), remote: SocketAddrV4::new(DNS_IP, 53) };
    let qlen = dns::build_query(1, "example.com", TYPE_A).len();
    assert_eq!(
        first,
        vec![
            EventKind::UdpOpen { id: 1, flow },
            EventKind::UdpData { id: 1, dir: Direction::ToRemote, len: qlen },
            EventKind::DnsQuery { id: 1, txid: 1, name: "example.com".into(), qtype: TYPE_A },
            EventKind::UdpData { id: 1, dir: Direction::ToGuest, len: qlen + 16 },
            EventKind::DnsAnswer {
                id: 1,
                txid: 1,
                name: "example.com".into(),
                qtype: TYPE_A,
                rcode: 0,
                addrs: vec![Ipv4Addr::new(198, 18, 0, 1)],
            },
        ]
    );
}

#[test]
fn udp_flow_reply_record_and_idle_close() {
    let cfg = SinkholeConfig { udp_reply: Some(b"pong".to_vec()), ..Default::default() };
    let mut s = Stack::new(config(), Sinkhole::new(cfg));
    let remote = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 5000);
    s.receive(t(0), &Guest::udp(33333, remote, b"hello"));
    s.receive(t(100), &Guest::udp(33333, remote, b"again!"));
    let out = drain(&mut s);
    assert_eq!(out.len(), 2);
    for o in &out {
        assert_eq!(
            *o,
            Out::Udp {
                eth_dst: GUEST_MAC,
                src: remote,
                dst: SocketAddrV4::new(GUEST_IP, 33333),
                payload: b"pong".to_vec()
            }
        );
    }
    let rec: Vec<_> = s.upstream().udp_flows().collect();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].datagrams, vec![(t(0), b"hello".to_vec()), (t(100), b"again!".to_vec())]);

    // Chiusura per inattività: la scadenza annunciata è 60 s dopo l'ultimo
    // datagramma (in entrambi i versi).
    assert_eq!(s.next_deadline(), Some(t(60_100)));
    s.poll(t(60_099));
    assert_eq!(s.udp_flows(), 1);
    s.poll(t(60_100));
    assert_eq!(s.udp_flows(), 0);
    assert_eq!(
        s.events().last().unwrap().kind,
        EventKind::UdpClosed { id: 1, reason: CloseReason::Idle, bytes_to_remote: 11, bytes_to_guest: 8 }
    );
    assert_eq!(s.upstream().udp_flows().next().unwrap().closed_at, Some(t(60_100)));
    assert_eq!(s.next_deadline(), None);
}

#[test]
fn udp_to_gateway_port_is_unreachable() {
    let mut s = sinkhole_stack();
    let frame = Guest::udp(1234, SocketAddrV4::new(GW_IP, 9999), b"??");
    s.receive(t(0), &frame);
    let out = drain(&mut s);
    let [Out::Icmp { src, dst, ty: 3, code: 3, data, .. }] = &out[..] else { panic!("{out:?}") };
    assert_eq!((*src, *dst), (GW_IP, GUEST_IP));
    // Intestazione IP originale + 8 byte di UDP.
    assert_eq!(data[..], frame[14..14 + 28]);
    assert!(s.events().is_empty());
}

#[test]
fn bad_checksums_and_unsupported_frames_are_dropped() {
    let mut s = sinkhole_stack();
    // Checksum UDP corrotto.
    let mut f = Guest::udp(1, SocketAddrV4::new(DNS_IP, 53), &dns::build_query(1, "a.b", TYPE_A));
    let last = f.len() - 1;
    f[last] ^= 0xff;
    s.receive(t(0), &f);
    // Checksum IP corrotto.
    let mut f = Guest::udp(1, SocketAddrV4::new(DNS_IP, 53), &dns::build_query(1, "a.b", TYPE_A));
    f[14 + 10] ^= 0xff;
    s.receive(t(0), &f);
    // IPv6 (non supportato), frame troncato, frame per un altro MAC.
    s.receive(t(0), &wire::build_eth(GW_MAC, GUEST_MAC, wire::ETHERTYPE_IPV6, &[0x60; 40]));
    s.receive(t(0), &[0u8; 10]);
    s.receive(t(0), &wire::build_eth(Mac([2, 0, 0, 0, 0, 1]), GUEST_MAC, wire::ETHERTYPE_ARP, &[0; 28]));
    // Frammento IPv4 (MF impostato).
    let mut f = Guest::udp(1, SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53), b"x");
    f[14 + 6] = 0x20;
    f[14 + 10..14 + 12].copy_from_slice(&[0, 0]);
    let c = wire::checksum(&f[14..34]);
    f[14 + 10..14 + 12].copy_from_slice(&c.to_be_bytes());
    s.receive(t(0), &f);

    assert!(drain(&mut s).is_empty());
    assert!(s.events().is_empty());
    let st = s.stats();
    assert_eq!((st.bad_checksum, st.ipv6, st.malformed, st.not_for_us, st.fragments), (2, 1, 1, 1, 1));

    // Con la verifica disattivata lo stesso pacchetto passa.
    let mut s = Stack::new(NetConfig { verify_checksums: false, ..config() }, Sinkhole::default());
    let mut f = Guest::udp(1, SocketAddrV4::new(DNS_IP, 53), &dns::build_query(1, "a.b", TYPE_A));
    f[14 + 20 + 6] ^= 0xff;
    s.receive(t(0), &f);
    assert_eq!(drain(&mut s).len(), 1);
}
