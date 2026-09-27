//! TCP and UDP flows reconstructed from the frames.
//!
//! TCP: the client is whoever sends the SYN (with no SYN seen, whoever sends the first
//! segment). Each direction is a [`Stream`] reconstructed by sequence
//! number: retransmissions and overlaps are discarded, out-of-order
//! segments wait for the missing ones. Every piece added carries
//! the instant of the frame that completed it, so [`Stream::time_at`] gives
//! the instant at which a byte became readable. A SYN on an already closed
//! four-tuple (or with a different ISN) opens a new flow.
//!
//! UDP: every four-tuple is a flow of datagrams; the client is the first
//! sender.

use std::collections::BTreeMap;
use std::net::SocketAddrV4;

use super::capture::Frame;
use super::packet::{self, Packet, tcp_flags};

/// Bytes waiting beyond a hole, per direction (beyond that they are discarded and the flow
/// is marked with a hole).
const MAX_PENDING: usize = 16 << 20;

/// One direction of a TCP flow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    /// Reconstructed bytes, in order.
    pub bytes: Vec<u8>,
    /// (offset, instant): from `offset` on the bytes arrived at
    /// `instant` (up to the next mark).
    pub marks: Vec<(usize, u64)>,
    /// The FIN has arrived (after all the bytes).
    pub fin: bool,
    /// Lost bytes (holes never filled or data beyond `MAX_PENDING`).
    pub missing: u64,
    isn: Option<u32>,
    pending: BTreeMap<u64, Vec<u8>>,
    pending_len: usize,
    fin_at: Option<u64>,
}

impl Stream {
    /// Instant at which byte `offset` became readable (that of the
    /// last byte if `offset` is past the end; 0 if empty).
    pub fn time_at(&self, offset: usize) -> u64 {
        match self.marks.partition_point(|&(o, _)| o <= offset) {
            0 => self.marks.first().map_or(0, |m| m.1),
            i => self.marks[i - 1].1,
        }
    }

    fn start(&mut self, isn: u32) {
        if self.isn.is_none() {
            self.isn = Some(isn);
        }
    }

    /// Relative offset of sequence number `seq` (first data byte =
    /// 0), unwrapped around the current position.
    fn offset(&self, seq: u32) -> Option<u64> {
        let base = self.isn?.wrapping_add(1);
        let next = self.bytes.len() as u64;
        let rel = seq.wrapping_sub(base).wrapping_sub(next as u32) as i32;
        u64::try_from(next as i64 + i64::from(rel)).ok()
    }

    fn segment(&mut self, at: u64, seq: u32, data: &[u8], fin: bool) {
        let Some(off) = self.offset(seq) else { return };
        if fin {
            self.fin_at = Some(off + data.len() as u64);
        }
        if !data.is_empty() {
            let next = self.bytes.len() as u64;
            let end = off + data.len() as u64;
            if end > next {
                if off <= next {
                    self.append(at, &data[(next - off) as usize..]);
                    self.drain(at);
                } else if self.pending_len + data.len() <= MAX_PENDING {
                    let e = self.pending.entry(off).or_default();
                    if e.len() < data.len() {
                        self.pending_len += data.len() - e.len();
                        *e = data.to_vec();
                    }
                } else {
                    self.missing += data.len() as u64;
                }
            }
        }
        if self.fin_at == Some(self.bytes.len() as u64) {
            self.fin = true;
        }
    }

    fn append(&mut self, at: u64, data: &[u8]) {
        if self.marks.last().is_none_or(|m| m.1 != at) {
            self.marks.push((self.bytes.len(), at));
        }
        self.bytes.extend_from_slice(data);
    }

    fn drain(&mut self, at: u64) {
        while let Some((&off, _)) = self.pending.first_key_value() {
            let next = self.bytes.len() as u64;
            if off > next {
                break;
            }
            let (_, d) = self.pending.pop_first().expect("just seen");
            self.pending_len -= d.len();
            let end = off + d.len() as u64;
            if end > next {
                self.append(at, &d[(next - off) as usize..]);
            }
        }
    }

    /// Closes the direction at the end of the capture: data beyond a hole counts as
    /// lost.
    fn finish(&mut self) {
        let next = self.bytes.len() as u64;
        for (off, d) in std::mem::take(&mut self.pending) {
            self.missing += (off + d.len() as u64).saturating_sub(next.max(off)).min(d.len() as u64);
        }
        self.pending_len = 0;
    }
}

/// A TCP flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpFlow {
    /// Index in the list of flows (order of opening).
    pub index: usize,
    pub client: SocketAddrV4,
    pub server: SocketAddrV4,
    /// SYN of the client.
    pub syn_at: Option<u64>,
    /// SYN-ACK of the server.
    pub syn_ack_at: Option<u64>,
    /// First ACK of the client after the SYN-ACK: handshake completed.
    pub established_at: Option<u64>,
    pub first_at: u64,
    pub last_at: u64,
    /// Instant of the first FIN or RST (end of the connection, if any).
    pub closed_at: Option<u64>,
    pub reset: bool,
    pub packets: u64,
    /// From the client to the server.
    pub client_data: Stream,
    /// From the server to the client.
    pub server_data: Stream,
}

impl TcpFlow {
    fn new(index: usize, client: SocketAddrV4, server: SocketAddrV4, at: u64) -> Self {
        TcpFlow {
            index,
            client,
            server,
            syn_at: None,
            syn_ack_at: None,
            established_at: None,
            first_at: at,
            last_at: at,
            closed_at: None,
            reset: false,
            packets: 0,
            client_data: Stream::default(),
            server_data: Stream::default(),
        }
    }

    fn closed(&self) -> bool {
        self.reset || (self.client_data.fin && self.server_data.fin)
    }
}

/// Un datagramma UDP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Datagram {
    pub at: u64,
    pub from_client: bool,
    pub data: Vec<u8>,
}

/// A UDP flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdpFlow {
    pub index: usize,
    pub client: SocketAddrV4,
    pub server: SocketAddrV4,
    pub datagrams: Vec<Datagram>,
}

/// All the flows of a capture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flows {
    pub tcp: Vec<TcpFlow>,
    pub udp: Vec<UdpFlow>,
    /// Non-TCP/UDP frames (ARP, ICMP, IPv6, fragments, malformed).
    pub other_frames: u64,
}

type Key = (SocketAddrV4, SocketAddrV4);

fn key(a: SocketAddrV4, b: SocketAddrV4) -> Key {
    if (a.ip().octets(), a.port()) <= (b.ip().octets(), b.port()) { (a, b) } else { (b, a) }
}

impl Flows {
    /// Reconstructs the flows from the frames in time order.
    pub fn from_frames(frames: &[Frame]) -> Self {
        let mut f = Flows::default();
        let mut tcp_index: BTreeMap<Key, usize> = BTreeMap::new();
        let mut udp_index: BTreeMap<Key, usize> = BTreeMap::new();
        for fr in frames {
            match packet::parse(&fr.data) {
                Packet::Tcp { src, dst, tcp } => {
                    let k = key(src, dst);
                    let syn_only = tcp.has(tcp_flags::SYN) && !tcp.has(tcp_flags::ACK);
                    let reuse = tcp_index.get(&k).is_some_and(|&i| {
                        let fl = &f.tcp[i];
                        syn_only
                            && (fl.closed()
                                || fl.client != src
                                || fl.client_data.isn.is_some_and(|isn| isn != tcp.seq))
                    });
                    let i = match tcp_index.get(&k) {
                        Some(&i) if !reuse => i,
                        _ => {
                            let i = f.tcp.len();
                            f.tcp.push(TcpFlow::new(i, src, dst, fr.at_us));
                            tcp_index.insert(k, i);
                            i
                        }
                    };
                    let fl = &mut f.tcp[i];
                    fl.packets += 1;
                    fl.last_at = fr.at_us;
                    let from_client = src == fl.client;
                    if tcp.has(tcp_flags::SYN) {
                        if from_client {
                            fl.syn_at.get_or_insert(fr.at_us);
                            fl.client_data.start(tcp.seq);
                        } else {
                            fl.syn_ack_at.get_or_insert(fr.at_us);
                            fl.server_data.start(tcp.seq);
                        }
                    } else {
                        // With no SYN seen: the first byte is the start.
                        let s = if from_client { &mut fl.client_data } else { &mut fl.server_data };
                        s.start(tcp.seq.wrapping_sub(1));
                        if from_client && fl.syn_ack_at.is_some() && tcp.has(tcp_flags::ACK) {
                            fl.established_at.get_or_insert(fr.at_us);
                        }
                    }
                    if tcp.has(tcp_flags::RST) {
                        fl.reset = true;
                        fl.closed_at.get_or_insert(fr.at_us);
                        continue;
                    }
                    let fin = tcp.has(tcp_flags::FIN);
                    if fin {
                        fl.closed_at.get_or_insert(fr.at_us);
                    }
                    let seq = if tcp.has(tcp_flags::SYN) { tcp.seq.wrapping_add(1) } else { tcp.seq };
                    let s = if from_client { &mut fl.client_data } else { &mut fl.server_data };
                    s.segment(fr.at_us, seq, tcp.payload, fin);
                }
                Packet::Udp { src, dst, payload } => {
                    let k = key(src, dst);
                    let i = *udp_index.entry(k).or_insert_with(|| {
                        let i = f.udp.len();
                        f.udp.push(UdpFlow { index: i, client: src, server: dst, datagrams: Vec::new() });
                        i
                    });
                    let fl = &mut f.udp[i];
                    fl.datagrams.push(Datagram {
                        at: fr.at_us,
                        from_client: src == fl.client,
                        data: payload.to_vec(),
                    });
                }
                _ => f.other_frames += 1,
            }
        }
        for fl in &mut f.tcp {
            fl.client_data.finish();
            fl.server_data.finish();
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::capture::Direction;
    use crate::net::packet::build;
    use tcp_flags::*;

    fn sa(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }

    struct Conv {
        frames: Vec<Frame>,
        c: SocketAddrV4,
        s: SocketAddrV4,
    }

    impl Conv {
        fn new() -> Self {
            Conv { frames: Vec::new(), c: sa("10.0.2.15:40000"), s: sa("198.18.0.1:80") }
        }
        fn c(&mut self, at: u64, seq: u32, ack: u32, flags: u8, d: &[u8]) {
            let f = build::tcp(self.c, self.s, seq, ack, flags, d);
            self.frames.push(Frame { at_us: at, dir: Direction::FromGuest, data: f });
        }
        fn s(&mut self, at: u64, seq: u32, ack: u32, flags: u8, d: &[u8]) {
            let f = build::tcp(self.s, self.c, seq, ack, flags, d);
            self.frames.push(Frame { at_us: at, dir: Direction::ToGuest, data: f });
        }
    }

    #[test]
    fn handshake_dati_e_chiusura() {
        let mut v = Conv::new();
        v.c(10, 1000, 0, SYN, b"");
        v.s(20, 5000, 1001, SYN | ACK, b"");
        v.c(30, 1001, 5001, ACK, b"");
        v.c(40, 1001, 5001, ACK | PSH, b"GET / HTTP/1.1\r\n");
        v.c(41, 1017, 5001, ACK | PSH, b"\r\n");
        v.s(50, 5001, 1019, ACK, b"HTTP/1.1 200 OK\r\n\r\n");
        v.s(51, 5020, 1019, ACK | FIN, b"");
        v.c(60, 1019, 5021, ACK | FIN, b"");
        v.s(61, 5021, 1020, ACK, b"");
        let f = Flows::from_frames(&v.frames);
        assert_eq!(f.tcp.len(), 1);
        let t = &f.tcp[0];
        assert_eq!((t.client, t.server), (v.c, v.s));
        assert_eq!(
            (t.syn_at, t.syn_ack_at, t.established_at, t.closed_at),
            (Some(10), Some(20), Some(30), Some(51))
        );
        assert_eq!(t.client_data.bytes, b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(t.server_data.bytes, b"HTTP/1.1 200 OK\r\n\r\n");
        assert!(t.client_data.fin && t.server_data.fin && !t.reset);
        assert_eq!(t.client_data.time_at(0), 40);
        assert_eq!(t.client_data.time_at(16), 41);
        assert_eq!(t.client_data.time_at(100), 41);
        assert_eq!(t.packets, 9);
    }

    #[test]
    fn ritrasmissioni_fuori_ordine_e_sovrapposizioni() {
        let mut v = Conv::new();
        v.c(1, 0xffff_fff0, 0, SYN, b"");
        v.s(2, 7, 0, SYN | ACK, b"");
        // Sequences crossing 2^32.
        let data: Vec<u8> = (0..64u8).collect();
        let seq = |o: usize| 0xffff_fff1u32.wrapping_add(o as u32);
        v.c(3, seq(32), 0, ACK, &data[32..48]); // out of order
        v.c(4, seq(0), 0, ACK, &data[0..16]);
        v.c(5, seq(0), 0, ACK, &data[0..16]); // retransmission
        v.c(6, seq(8), 0, ACK, &data[8..40]); // overlapping: fills the hole
        v.c(7, seq(48), 0, ACK | FIN, &data[48..64]);
        let f = Flows::from_frames(&v.frames);
        let s = &f.tcp[0].client_data;
        assert_eq!(s.bytes, data);
        assert!(s.fin);
        assert_eq!(s.missing, 0);
        assert_eq!(s.marks, [(0, 4), (16, 6), (48, 7)]);
    }

    #[test]
    fn buco_mai_colmato_e_porta_riusata() {
        let mut v = Conv::new();
        v.c(1, 100, 0, SYN, b"");
        v.c(2, 101, 0, ACK, b"abc");
        v.c(3, 110, 0, ACK, b"zzz"); // 6-byte hole
        v.c(4, 113, 0, RST, b"");
        v.c(5, 900, 0, SYN, b""); // same four-tuple, new connection
        v.c(6, 901, 0, ACK, b"nuova");
        let f = Flows::from_frames(&v.frames);
        assert_eq!(f.tcp.len(), 2);
        assert_eq!(f.tcp[0].client_data.bytes, b"abc");
        assert_eq!(f.tcp[0].client_data.missing, 3);
        assert!(f.tcp[0].reset);
        assert_eq!(f.tcp[1].client_data.bytes, b"nuova");
        assert_eq!(f.tcp[1].index, 1);
    }

    #[test]
    fn senza_syn_e_udp() {
        let mut v = Conv::new();
        v.c(1, 5000, 0, ACK, b"a met");
        v.c(2, 5005, 0, ACK, b"a strada");
        let dns = build::udp(sa("10.0.2.15:3333"), sa("10.0.2.3:53"), b"q");
        let ans = build::udp(sa("10.0.2.3:53"), sa("10.0.2.15:3333"), b"r");
        v.frames.push(Frame { at_us: 3, dir: Direction::FromGuest, data: dns });
        v.frames.push(Frame { at_us: 4, dir: Direction::ToGuest, data: ans });
        v.frames.push(Frame { at_us: 5, dir: Direction::ToGuest, data: vec![0; 10] });
        let f = Flows::from_frames(&v.frames);
        assert_eq!(f.tcp[0].client_data.bytes, b"a meta strada");
        assert_eq!(f.udp.len(), 1);
        let u = &f.udp[0];
        assert_eq!((u.client, u.server), (sa("10.0.2.15:3333"), sa("10.0.2.3:53")));
        assert_eq!(
            u.datagrams.iter().map(|d| (d.at, d.from_client)).collect::<Vec<_>>(),
            [(3, true), (4, false)]
        );
        assert_eq!(f.other_frames, 1);
    }
}
