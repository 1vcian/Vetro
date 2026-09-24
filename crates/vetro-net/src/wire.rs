//! Formati sul filo: Ethernet II, ARP, IPv4, ICMP, UDP, TCP e checksum.
//!
//! Solo quello che serve al gateway virtuale. I parser non allocano e non
//! vanno mai in panic su input arbitrari: un frame malformato diventa `None`.

use std::net::Ipv4Addr;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86dd;

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const ETH_HEADER_LEN: usize = 14;
pub const IPV4_HEADER_LEN: usize = 20;
pub const UDP_HEADER_LEN: usize = 8;
pub const TCP_HEADER_LEN: usize = 20;

/// Indirizzo MAC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const BROADCAST: Mac = Mac([0xff; 6]);
    pub const ZERO: Mac = Mac([0; 6]);

    pub fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn ip_at(b: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(b[at], b[at + 1], b[at + 2], b[at + 3])
}

fn mac_at(b: &[u8], at: usize) -> Mac {
    let mut m = [0; 6];
    m.copy_from_slice(&b[at..at + 6]);
    Mac(m)
}

// ---------------------------------------------------------------------------
// Checksum di Internet (RFC 1071)
// ---------------------------------------------------------------------------

/// Somma a complemento a uno non ancora ripiegata.
fn sum_words(data: &[u8], mut acc: u32) -> u32 {
    // Pacchetti fino a 64 KiB: la somma sta in u32 senza traboccare.
    let (words, rest) = data.as_chunks::<2>();
    for w in words {
        acc += u32::from(u16::from_be_bytes(*w));
    }
    if let [last] = rest {
        acc += u32::from(*last) << 8;
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

/// Checksum di Internet di `data`. Su un'intestazione che contiene già il
/// campo corretto il risultato è 0.
pub fn checksum(data: &[u8]) -> u16 {
    fold(sum_words(data, 0))
}

/// Checksum di TCP o UDP con la pseudo-intestazione IPv4.
pub fn transport_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, segment: &[u8]) -> u16 {
    let mut acc = sum_words(&src.octets(), 0);
    acc = sum_words(&dst.octets(), acc);
    acc += u32::from(proto);
    acc += segment.len() as u32;
    fold(sum_words(segment, acc))
}

// ---------------------------------------------------------------------------
// Ethernet
// ---------------------------------------------------------------------------

/// Intestazione Ethernet II (senza VLAN: il guest non ne usa).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
}

pub fn parse_eth(frame: &[u8]) -> Option<(EthHeader, &[u8])> {
    if frame.len() < ETH_HEADER_LEN {
        return None;
    }
    let h = EthHeader { dst: mac_at(frame, 0), src: mac_at(frame, 6), ethertype: be16(frame, 12) };
    Some((h, &frame[ETH_HEADER_LEN..]))
}

pub fn build_eth(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HEADER_LEN + payload.len());
    f.extend_from_slice(&dst.0);
    f.extend_from_slice(&src.0);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

// ---------------------------------------------------------------------------
// ARP (Ethernet/IPv4)
// ---------------------------------------------------------------------------

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4Addr,
    pub target_mac: Mac,
    pub target_ip: Ipv4Addr,
}

pub fn parse_arp(p: &[u8]) -> Option<Arp> {
    // htype 1, ptype IPv4, hlen 6, plen 4.
    if p.len() < 28 || be16(p, 0) != 1 || be16(p, 2) != ETHERTYPE_IPV4 || p[4] != 6 || p[5] != 4 {
        return None;
    }
    Some(Arp {
        op: be16(p, 6),
        sender_mac: mac_at(p, 8),
        sender_ip: ip_at(p, 14),
        target_mac: mac_at(p, 18),
        target_ip: ip_at(p, 24),
    })
}

pub fn build_arp(a: &Arp) -> Vec<u8> {
    let mut p = Vec::with_capacity(28);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    p.extend_from_slice(&[6, 4]);
    p.extend_from_slice(&a.op.to_be_bytes());
    p.extend_from_slice(&a.sender_mac.0);
    p.extend_from_slice(&a.sender_ip.octets());
    p.extend_from_slice(&a.target_mac.0);
    p.extend_from_slice(&a.target_ip.octets());
    p
}

// ---------------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Header {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub ttl: u8,
    pub ident: u16,
    /// Bit MF o offset diverso da zero: è un frammento.
    pub fragment: bool,
    pub checksum_ok: bool,
}

/// Analizza un pacchetto IPv4 e restituisce intestazione e payload (tagliato
/// alla lunghezza totale dichiarata, ignorando il padding Ethernet).
pub fn parse_ipv4(p: &[u8]) -> Option<(Ipv4Header, &[u8])> {
    if p.len() < IPV4_HEADER_LEN || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(p[0] & 0x0f) * 4;
    let total = usize::from(be16(p, 2));
    if ihl < IPV4_HEADER_LEN || total < ihl || total > p.len() {
        return None;
    }
    let flags_frag = be16(p, 6);
    let h = Ipv4Header {
        src: ip_at(p, 12),
        dst: ip_at(p, 16),
        proto: p[9],
        ttl: p[8],
        ident: be16(p, 4),
        fragment: flags_frag & 0x3fff != 0,
        checksum_ok: checksum(&p[..ihl]) == 0,
    };
    Some((h, &p[ihl..total]))
}

/// Costruisce un pacchetto IPv4 senza opzioni, con DF impostato.
pub fn build_ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, ident: u16, payload: &[u8]) -> Vec<u8> {
    let total = IPV4_HEADER_LEN + payload.len();
    let mut p = Vec::with_capacity(total);
    p.push(0x45);
    p.push(0);
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&ident.to_be_bytes());
    p.extend_from_slice(&0x4000u16.to_be_bytes()); // DF
    p.push(64);
    p.push(proto);
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let c = checksum(&p);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

// ---------------------------------------------------------------------------
// ICMP
// ---------------------------------------------------------------------------

pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_DEST_UNREACHABLE: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;
pub const ICMP_PORT_UNREACHABLE: u8 = 3;

/// Messaggio ICMP con checksum calcolato.
pub fn build_icmp(ty: u8, code: u8, rest_of_header: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + data.len());
    m.extend_from_slice(&[ty, code, 0, 0]);
    m.extend_from_slice(&rest_of_header);
    m.extend_from_slice(data);
    let c = checksum(&m);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    m
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub checksum_ok: bool,
}

pub fn parse_udp(src: Ipv4Addr, dst: Ipv4Addr, s: &[u8]) -> Option<(UdpHeader, &[u8])> {
    if s.len() < UDP_HEADER_LEN {
        return None;
    }
    let len = usize::from(be16(s, 4));
    if len < UDP_HEADER_LEN || len > s.len() {
        return None;
    }
    let s = &s[..len];
    // Checksum 0 in IPv4 significa "non calcolato".
    let checksum_ok = be16(s, 6) == 0 || transport_checksum(src, dst, PROTO_UDP, s) == 0;
    let h = UdpHeader { src_port: be16(s, 0), dst_port: be16(s, 2), checksum_ok };
    Some((h, &s[UDP_HEADER_LEN..]))
}

pub fn build_udp(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = UDP_HEADER_LEN + payload.len();
    let mut s = Vec::with_capacity(len);
    s.extend_from_slice(&sport.to_be_bytes());
    s.extend_from_slice(&dport.to_be_bytes());
    s.extend_from_slice(&(len as u16).to_be_bytes());
    s.extend_from_slice(&[0, 0]);
    s.extend_from_slice(payload);
    let mut c = transport_checksum(src, dst, PROTO_UDP, &s);
    if c == 0 {
        c = 0xffff;
    }
    s[6..8].copy_from_slice(&c.to_be_bytes());
    s
}

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

/// Segmento TCP analizzato (intestazione e opzioni che ci interessano).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// Opzione MSS, presente di solito solo nel SYN.
    pub mss: Option<u16>,
    pub checksum_ok: bool,
}

impl TcpHeader {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

pub fn parse_tcp(src: Ipv4Addr, dst: Ipv4Addr, s: &[u8]) -> Option<(TcpHeader, &[u8])> {
    if s.len() < TCP_HEADER_LEN {
        return None;
    }
    let off = usize::from(s[12] >> 4) * 4;
    if off < TCP_HEADER_LEN || off > s.len() {
        return None;
    }
    let mut mss = None;
    let mut opts = &s[TCP_HEADER_LEN..off];
    while let [kind, rest @ ..] = opts {
        match kind {
            0 => break,
            1 => opts = rest,
            _ => {
                let Some(&len) = rest.first() else { break };
                let len = usize::from(len);
                if len < 2 || len > opts.len() {
                    break;
                }
                if *kind == 2 && len == 4 {
                    mss = Some(be16(opts, 2));
                }
                opts = &opts[len..];
            }
        }
    }
    let h = TcpHeader {
        src_port: be16(s, 0),
        dst_port: be16(s, 2),
        seq: be32(s, 4),
        ack: be32(s, 8),
        flags: s[13],
        window: be16(s, 14),
        mss,
        checksum_ok: transport_checksum(src, dst, PROTO_TCP, s) == 0,
    };
    Some((h, &s[off..]))
}

/// Segmento TCP da costruire.
#[derive(Clone, Copy, Debug)]
pub struct TcpOut<'a> {
    pub src: Ipv4Addr,
    pub src_port: u16,
    pub dst: Ipv4Addr,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

pub fn build_tcp(t: &TcpOut<'_>) -> Vec<u8> {
    let opt_len = if t.mss.is_some() { 4 } else { 0 };
    let hdr = TCP_HEADER_LEN + opt_len;
    let mut s = Vec::with_capacity(hdr + t.payload.len());
    s.extend_from_slice(&t.src_port.to_be_bytes());
    s.extend_from_slice(&t.dst_port.to_be_bytes());
    s.extend_from_slice(&t.seq.to_be_bytes());
    s.extend_from_slice(&t.ack.to_be_bytes());
    s.push(((hdr / 4) as u8) << 4);
    s.push(t.flags);
    s.extend_from_slice(&t.window.to_be_bytes());
    s.extend_from_slice(&[0, 0, 0, 0]); // checksum, urgent pointer
    if let Some(mss) = t.mss {
        s.extend_from_slice(&[2, 4]);
        s.extend_from_slice(&mss.to_be_bytes());
    }
    s.extend_from_slice(t.payload);
    let c = transport_checksum(t.src, t.dst, PROTO_TCP, &s);
    s[16..18].copy_from_slice(&c.to_be_bytes());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_rfc1071_example() {
        // Esempio della RFC 1071, sezione 3: somma 0xddf2, checksum 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), 0x220d);
    }

    #[test]
    fn ipv4_roundtrip_and_checksum() {
        let a = Ipv4Addr::new(10, 0, 2, 2);
        let b = Ipv4Addr::new(10, 0, 2, 15);
        let p = build_ipv4(a, b, PROTO_UDP, 7, b"xyz");
        let (h, payload) = parse_ipv4(&p).unwrap();
        assert!(h.checksum_ok);
        assert_eq!((h.src, h.dst, h.proto, h.ident, h.fragment), (a, b, PROTO_UDP, 7, false));
        assert_eq!(payload, b"xyz");
    }

    #[test]
    fn tcp_options_parse_and_truncated_inputs() {
        let a = Ipv4Addr::new(1, 2, 3, 4);
        let b = Ipv4Addr::new(5, 6, 7, 8);
        let s = build_tcp(&TcpOut {
            src: a,
            src_port: 1,
            dst: b,
            dst_port: 2,
            seq: 3,
            ack: 4,
            flags: TCP_SYN,
            window: 5,
            mss: Some(1400),
            payload: b"",
        });
        let (h, _) = parse_tcp(a, b, &s).unwrap();
        assert_eq!(h.mss, Some(1400));
        assert!(h.checksum_ok);
        for n in 0..s.len() {
            // Nessun panic su input troncati.
            let _ = parse_tcp(a, b, &s[..n]);
            let _ = parse_ipv4(&s[..n]);
            let _ = parse_udp(a, b, &s[..n]);
        }
    }
}
