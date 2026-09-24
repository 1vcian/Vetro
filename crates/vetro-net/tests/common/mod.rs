//! Finto guest e validazione indipendente dei frame prodotti dallo stack.
//!
//! Ogni frame che esce dallo stack passa da [`validate`]: lo analizza
//! `smoltcp::wire` (un'implementazione indipendente) e i checksum vengono
//! ricalcolati anche qui con una funzione separata da quella del crate.

#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddrV4};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol, Icmpv4Packet,
    IpAddress, IpProtocol, Ipv4Packet, TcpPacket, TcpRepr, UdpPacket,
};
use vetro_net::wire::{self, Mac, TcpOut};
use vetro_net::{NetConfig, Stack, Upstream, VirtualTime};

pub const GUEST_MAC: Mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
pub const GW_MAC: Mac = Mac([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]);
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
pub const GW_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

pub fn t(ms: u64) -> VirtualTime {
    VirtualTime::from_millis(ms)
}

/// Checksum di Internet scritto di nuovo, senza usare il crate.
pub fn inet_sum(parts: &[&[u8]]) -> u16 {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(p);
    }
    if bytes.len() % 2 == 1 {
        bytes.push(0);
    }
    let mut sum: u64 = bytes.chunks(2).map(|c| u64::from(c[0]) << 8 | u64::from(c[1])).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn pseudo(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    p.push(0);
    p.push(proto);
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p
}

/// Frame prodotto dallo stack, già validato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Arp { op: u16, sender_mac: Mac, sender_ip: Ipv4Addr, target_mac: Mac, target_ip: Ipv4Addr },
    Tcp(TcpSeg),
    Udp { eth_dst: Mac, src: SocketAddrV4, dst: SocketAddrV4, payload: Vec<u8> },
    Icmp { src: Ipv4Addr, dst: Ipv4Addr, ty: u8, code: u8, rest: [u8; 4], data: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpSeg {
    pub src: SocketAddrV4,
    pub dst: SocketAddrV4,
    pub seq: u32,
    pub ack: Option<u32>,
    pub syn: bool,
    pub fin: bool,
    pub rst: bool,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: Vec<u8>,
}

impl Out {
    pub fn tcp(self) -> TcpSeg {
        match self {
            Out::Tcp(s) => s,
            other => panic!("atteso TCP, arrivato {other:?}"),
        }
    }
}

fn mac(a: EthernetAddress) -> Mac {
    Mac(a.0)
}

/// Analizza e valida un frame dello stack con smoltcp e con i checksum
/// ricalcolati qui. Va in panic su qualsiasi incoerenza.
pub fn validate(frame: &[u8]) -> Out {
    let caps = ChecksumCapabilities::default();
    let eth = EthernetFrame::new_checked(frame).expect("Ethernet valido");
    assert_eq!(mac(eth.src_addr()), GW_MAC, "sorgente Ethernet del gateway");
    match eth.ethertype() {
        EthernetProtocol::Arp => {
            let p = ArpPacket::new_checked(eth.payload()).expect("ARP valido");
            let ArpRepr::EthernetIpv4 {
                operation,
                source_hardware_addr,
                source_protocol_addr,
                target_hardware_addr,
                target_protocol_addr,
            } = ArpRepr::parse(&p).expect("ARP analizzabile")
            else {
                panic!("ARP non Ethernet/IPv4")
            };
            let op = match operation {
                ArpOperation::Request => 1,
                ArpOperation::Reply => 2,
                ArpOperation::Unknown(x) => x,
            };
            assert_eq!(mac(eth.dst_addr()), mac(target_hardware_addr));
            Out::Arp {
                op,
                sender_mac: mac(source_hardware_addr),
                sender_ip: source_protocol_addr,
                target_mac: mac(target_hardware_addr),
                target_ip: target_protocol_addr,
            }
        }
        EthernetProtocol::Ipv4 => {
            let ip = Ipv4Packet::new_checked(eth.payload()).expect("IPv4 valido");
            assert!(ip.verify_checksum(), "checksum IPv4 (smoltcp)");
            let hl = usize::from(ip.header_len());
            assert_eq!(inet_sum(&[&eth.payload()[..hl]]), 0, "checksum IPv4 (ricalcolato)");
            assert_eq!(usize::from(ip.total_len()), eth.payload().len(), "niente padding né troncamenti");
            assert!(ip.dont_frag());
            assert_eq!(ip.hop_limit(), 64);
            let (src, dst) = (ip.src_addr(), ip.dst_addr());
            let (isrc, idst) = (IpAddress::Ipv4(src), IpAddress::Ipv4(dst));
            let payload = ip.payload();
            match ip.next_header() {
                IpProtocol::Tcp => {
                    let p = TcpPacket::new_checked(payload).expect("TCP valido");
                    assert!(p.verify_checksum(&isrc, &idst), "checksum TCP (smoltcp)");
                    assert_eq!(
                        inet_sum(&[&pseudo(src, dst, 6, payload.len()), payload]),
                        0,
                        "checksum TCP (ricalcolato)"
                    );
                    let r = TcpRepr::parse(&p, &isrc, &idst, &caps).expect("TCP analizzabile");
                    assert_eq!(mac(eth.dst_addr()), GUEST_MAC);
                    Out::Tcp(TcpSeg {
                        src: SocketAddrV4::new(src, p.src_port()),
                        dst: SocketAddrV4::new(dst, p.dst_port()),
                        seq: p.seq_number().0 as u32,
                        ack: p.ack().then(|| p.ack_number().0 as u32),
                        syn: p.syn(),
                        fin: p.fin(),
                        rst: p.rst(),
                        window: p.window_len(),
                        mss: r.max_seg_size,
                        payload: p.payload().to_vec(),
                    })
                }
                IpProtocol::Udp => {
                    let p = UdpPacket::new_checked(payload).expect("UDP valido");
                    assert!(p.verify_checksum(&isrc, &idst), "checksum UDP (smoltcp)");
                    assert_ne!(p.checksum(), 0, "checksum UDP presente");
                    assert_eq!(
                        inet_sum(&[&pseudo(src, dst, 17, payload.len()), payload]),
                        0,
                        "checksum UDP (ricalcolato)"
                    );
                    Out::Udp {
                        eth_dst: mac(eth.dst_addr()),
                        src: SocketAddrV4::new(src, p.src_port()),
                        dst: SocketAddrV4::new(dst, p.dst_port()),
                        payload: p.payload().to_vec(),
                    }
                }
                IpProtocol::Icmp => {
                    let p = Icmpv4Packet::new_checked(payload).expect("ICMP valido");
                    assert!(p.verify_checksum(), "checksum ICMP (smoltcp)");
                    assert_eq!(inet_sum(&[payload]), 0, "checksum ICMP (ricalcolato)");
                    Out::Icmp {
                        src,
                        dst,
                        ty: payload[0],
                        code: payload[1],
                        rest: [payload[4], payload[5], payload[6], payload[7]],
                        data: payload[8..].to_vec(),
                    }
                }
                other => panic!("protocollo IP inatteso {other:?}"),
            }
        }
        other => panic!("ethertype inatteso {other:?}"),
    }
}

/// Tutti i frame in attesa, validati.
pub fn drain<U: Upstream>(stack: &mut Stack<U>) -> Vec<Out> {
    std::iter::from_fn(|| stack.pop_frame()).map(|f| validate(&f)).collect()
}

pub fn config() -> NetConfig {
    NetConfig { seed: 42, ..NetConfig::default() }
}

/// Il finto guest: costruisce i frame come li manderebbe Linux.
pub struct Guest;

impl Guest {
    pub fn ipv4(dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
        wire::build_eth(
            GW_MAC,
            GUEST_MAC,
            wire::ETHERTYPE_IPV4,
            &wire::build_ipv4(GUEST_IP, dst, proto, 1, payload),
        )
    }

    pub fn udp(sport: u16, dst: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
        let seg = wire::build_udp(GUEST_IP, sport, *dst.ip(), dst.port(), payload);
        Self::ipv4(*dst.ip(), wire::PROTO_UDP, &seg)
    }

    pub fn arp_request(target: Ipv4Addr) -> Vec<u8> {
        let arp = wire::build_arp(&wire::Arp {
            op: wire::ARP_REQUEST,
            sender_mac: GUEST_MAC,
            sender_ip: GUEST_IP,
            target_mac: Mac::ZERO,
            target_ip: target,
        });
        wire::build_eth(Mac::BROADCAST, GUEST_MAC, wire::ETHERTYPE_ARP, &arp)
    }
}

/// Estremo TCP del finto guest, pilotato a mano dai test.
pub struct Client {
    pub sport: u16,
    pub dst: SocketAddrV4,
    /// Prossimo numero di sequenza da mandare.
    pub seq: u32,
    /// Prossimo byte atteso dallo stack.
    pub ack: u32,
    pub window: u16,
    pub mss: u16,
}

pub const F_FIN: u8 = wire::TCP_FIN;
pub const F_SYN: u8 = wire::TCP_SYN;
pub const F_RST: u8 = wire::TCP_RST;
pub const F_ACK: u8 = wire::TCP_ACK;
pub const F_PSH: u8 = wire::TCP_PSH;

impl Client {
    pub fn new(sport: u16, dst: SocketAddrV4) -> Self {
        Client { sport, dst, seq: 1_000_000, ack: 0, window: 64_240, mss: 1460 }
    }

    pub fn segment(&self, seq: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
        let seg = wire::build_tcp(&TcpOut {
            src: GUEST_IP,
            src_port: self.sport,
            dst: *self.dst.ip(),
            dst_port: self.dst.port(),
            seq,
            ack: if flags & F_ACK != 0 { self.ack } else { 0 },
            flags,
            window: self.window,
            mss: (flags & F_SYN != 0).then_some(self.mss),
            payload,
        });
        Guest::ipv4(*self.dst.ip(), wire::PROTO_TCP, &seg)
    }

    pub fn syn(&self) -> Vec<u8> {
        self.segment(self.seq, F_SYN, b"")
    }

    /// Accetta il SYN-ACK e restituisce l'ACK finale dell'handshake.
    pub fn on_syn_ack(&mut self, s: &TcpSeg) -> Vec<u8> {
        assert!(s.syn && !s.rst, "atteso SYN-ACK: {s:?}");
        assert_eq!(s.ack, Some(self.seq.wrapping_add(1)));
        self.seq = self.seq.wrapping_add(1);
        self.ack = s.seq.wrapping_add(1);
        self.segment(self.seq, F_ACK, b"")
    }

    pub fn send(&mut self, data: &[u8]) -> Vec<u8> {
        let f = self.segment(self.seq, F_ACK | F_PSH, data);
        self.seq = self.seq.wrapping_add(data.len() as u32);
        f
    }

    pub fn fin(&mut self) -> Vec<u8> {
        let f = self.segment(self.seq, F_ACK | F_FIN, b"");
        self.seq = self.seq.wrapping_add(1);
        f
    }

    /// Prende i dati (e l'eventuale FIN) di un segmento in ordine.
    pub fn take(&mut self, s: &TcpSeg) {
        assert_eq!(s.seq, self.ack, "segmento fuori ordine: {s:?}");
        self.ack = self.ack.wrapping_add(s.payload.len() as u32 + u32::from(s.fin));
    }

    pub fn ack_now(&self) -> Vec<u8> {
        self.segment(self.seq, F_ACK, b"")
    }
}
