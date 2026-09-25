//! Intestazioni Ethernet II, IPv4, TCP e UDP. Nessun panic su byte
//! arbitrari: ciò che non si capisce resta `None`/`Other`.

use std::net::{Ipv4Addr, SocketAddrV4};

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

/// Bit dei flag TCP.
pub mod tcp_flags {
    pub const FIN: u8 = 0x01;
    pub const SYN: u8 = 0x02;
    pub const RST: u8 = 0x04;
    pub const PSH: u8 = 0x08;
    pub const ACK: u8 = 0x10;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ethernet<'a> {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn ethernet(frame: &[u8]) -> Option<Ethernet<'_>> {
    if frame.len() < 14 {
        return None;
    }
    Some(Ethernet {
        dst: frame[0..6].try_into().ok()?,
        src: frame[6..12].try_into().ok()?,
        ethertype: u16::from_be_bytes([frame[12], frame[13]]),
        payload: &frame[14..],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub protocol: u8,
    pub ttl: u8,
    /// Frammento (MF o offset diverso da zero): il carico non è completo.
    pub fragment: bool,
    /// Carico secondo la lunghezza totale (il padding Ethernet è tolto).
    pub payload: &'a [u8],
}

pub fn ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(p[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([p[2], p[3]]));
    if ihl < 20 || total < ihl || total > p.len() {
        return None;
    }
    let frag = u16::from_be_bytes([p[6], p[7]]);
    Some(Ipv4 {
        src: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        dst: Ipv4Addr::new(p[16], p[17], p[18], p[19]),
        protocol: p[9],
        ttl: p[8],
        fragment: frag & 0x3fff != 0,
        payload: &p[ihl..total],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tcp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

impl Tcp<'_> {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

pub fn tcp(p: &[u8]) -> Option<Tcp<'_>> {
    if p.len() < 20 {
        return None;
    }
    let off = usize::from(p[12] >> 4) * 4;
    if off < 20 || off > p.len() {
        return None;
    }
    Some(Tcp {
        src_port: u16::from_be_bytes([p[0], p[1]]),
        dst_port: u16::from_be_bytes([p[2], p[3]]),
        seq: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        ack: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
        flags: p[13],
        window: u16::from_be_bytes([p[14], p[15]]),
        payload: &p[off..],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

pub fn udp(p: &[u8]) -> Option<Udp<'_>> {
    if p.len() < 8 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([p[4], p[5]]));
    if len < 8 || len > p.len() {
        return None;
    }
    Some(Udp {
        src_port: u16::from_be_bytes([p[0], p[1]]),
        dst_port: u16::from_be_bytes([p[2], p[3]]),
        payload: &p[8..len],
    })
}

/// Un frame decodificato fino al livello di trasporto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Tcp {
        src: SocketAddrV4,
        dst: SocketAddrV4,
        tcp: Tcp<'a>,
    },
    Udp {
        src: SocketAddrV4,
        dst: SocketAddrV4,
        payload: &'a [u8],
    },
    /// IPv4 con un altro protocollo (ICMP...) o frammentato.
    OtherIpv4 {
        src: Ipv4Addr,
        dst: Ipv4Addr,
        protocol: u8,
    },
    Arp,
    /// Ethertype sconosciuto (IPv6 compreso) o frame malformato.
    Other,
}

pub fn parse(frame: &[u8]) -> Packet<'_> {
    let Some(eth) = ethernet(frame) else { return Packet::Other };
    match eth.ethertype {
        ETHERTYPE_ARP => Packet::Arp,
        ETHERTYPE_IPV4 => {
            let Some(ip) = ipv4(eth.payload) else { return Packet::Other };
            let other = Packet::OtherIpv4 { src: ip.src, dst: ip.dst, protocol: ip.protocol };
            if ip.fragment {
                return other;
            }
            match ip.protocol {
                PROTO_TCP => match tcp(ip.payload) {
                    Some(t) => Packet::Tcp {
                        src: SocketAddrV4::new(ip.src, t.src_port),
                        dst: SocketAddrV4::new(ip.dst, t.dst_port),
                        tcp: t,
                    },
                    None => Packet::Other,
                },
                PROTO_UDP => match udp(ip.payload) {
                    Some(u) => Packet::Udp {
                        src: SocketAddrV4::new(ip.src, u.src_port),
                        dst: SocketAddrV4::new(ip.dst, u.dst_port),
                        payload: u.payload,
                    },
                    None => Packet::Other,
                },
                _ => other,
            }
        }
        _ => Packet::Other,
    }
}

/// Costruttori di frame per i test (checksum a zero: l'analisi non li
/// controlla).
#[cfg(test)]
pub mod build {
    use std::net::SocketAddrV4;

    fn ip(src: SocketAddrV4, dst: SocketAddrV4, proto: u8, l4: &[u8]) -> Vec<u8> {
        let mut f = vec![0x52, 0x55, 10, 0, 2, 2, 0x52, 0x54, 0, 0x12, 0x34, 0x56, 0x08, 0x00];
        let total = (20 + l4.len()) as u16;
        f.extend([0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0x40, 0, 64, proto, 0, 0]);
        f.extend(src.ip().octets());
        f.extend(dst.ip().octets());
        f.extend(l4);
        f
    }

    pub fn tcp(src: SocketAddrV4, dst: SocketAddrV4, seq: u32, ack: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(src.port().to_be_bytes());
        t.extend(dst.port().to_be_bytes());
        t.extend(seq.to_be_bytes());
        t.extend(ack.to_be_bytes());
        t.extend([0x50, flags, 0xff, 0xff, 0, 0, 0, 0]);
        t.extend(data);
        ip(src, dst, super::PROTO_TCP, &t)
    }

    pub fn udp(src: SocketAddrV4, dst: SocketAddrV4, data: &[u8]) -> Vec<u8> {
        let mut u = Vec::new();
        u.extend(src.port().to_be_bytes());
        u.extend(dst.port().to_be_bytes());
        u.extend(((8 + data.len()) as u16).to_be_bytes());
        u.extend([0, 0]);
        u.extend(data);
        ip(src, dst, super::PROTO_UDP, &u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }

    #[test]
    fn tcp_e_udp() {
        let f = build::tcp(sa("10.0.2.15:40000"), sa("198.18.0.1:80"), 7, 9, tcp_flags::SYN, b"xy");
        match parse(&f) {
            Packet::Tcp { src, dst, tcp } => {
                assert_eq!((src, dst), (sa("10.0.2.15:40000"), sa("198.18.0.1:80")));
                assert_eq!((tcp.seq, tcp.ack, tcp.payload), (7, 9, &b"xy"[..]));
                assert!(tcp.has(tcp_flags::SYN) && !tcp.has(tcp_flags::ACK));
            }
            p => panic!("{p:?}"),
        }
        let mut f = build::udp(sa("10.0.2.15:1000"), sa("10.0.2.3:53"), b"dns");
        f.extend([0; 10]); // padding Ethernet
        assert_eq!(
            parse(&f),
            Packet::Udp { src: sa("10.0.2.15:1000"), dst: sa("10.0.2.3:53"), payload: b"dns" }
        );
    }

    #[test]
    fn nessun_panic_su_byte_arbitrari() {
        let mut x = 0x1234_5678u32;
        for len in 0..200 {
            let v: Vec<u8> = (0..len)
                .map(|i| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    if i == 12 {
                        8
                    } else if i == 13 {
                        0
                    } else {
                        x as u8
                    }
                })
                .collect();
            let _ = parse(&v);
        }
    }
}
