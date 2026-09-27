//! Sinkhole: an upstream that never leaves the machine.
//!
//! It accepts (or refuses, per port) every connection, records the bytes the
//! guest sends and answers with configurable data. The DNS resolves every A name
//! to deterministic fake addresses (in order of first request, starting
//! from 198.18.0.1, a block reserved for network tests by RFC 2544) and
//! remembers the names, so every connection to a fake address is
//! attributed to the name the guest had asked for.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime, dns};

mod snapshot;

/// Configured response for TCP connections to a port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TcpReply {
    /// Bytes sent right after opening (a banner, like SMTP or SSH).
    pub on_connect: Vec<u8>,
    /// Bytes sent once, after the guest's first block of data.
    pub on_data: Vec<u8>,
    /// Closes the direction towards the guest (FIN) after the configured responses:
    /// after `on_data` if not empty, otherwise right after `on_connect`.
    pub close_after_reply: bool,
}

impl TcpReply {
    /// A minimal, empty HTTP/1.1 response, then close.
    pub fn http_empty() -> Self {
        TcpReply {
            on_connect: Vec::new(),
            on_data: b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            close_after_reply: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkholeConfig {
    /// Response for the ports not listed in `tcp_by_port`.
    pub tcp_default: TcpReply,
    pub tcp_by_port: BTreeMap<u16, TcpReply>,
    /// Refused TCP ports (the guest gets RST, "connection refused").
    pub refused_ports: BTreeSet<u16>,
    /// Response to every non-DNS UDP datagram (`None`: no response).
    pub udp_reply: Option<Vec<u8>>,
    /// The virtual DNS server (must match `NetConfig::dns_ip`).
    pub dns_server: SocketAddrV4,
    /// Primo indirizzo finto assegnato ai nomi.
    pub fake_base: Ipv4Addr,
    pub dns_ttl: u32,
    /// Answers ICMP echoes to any external address.
    pub answer_ping: bool,
}

impl Default for SinkholeConfig {
    fn default() -> Self {
        let mut tcp_by_port = BTreeMap::new();
        tcp_by_port.insert(80, TcpReply::http_empty());
        SinkholeConfig {
            tcp_default: TcpReply::default(),
            tcp_by_port,
            refused_ports: BTreeSet::new(),
            udp_reply: None,
            dns_server: SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 3), 53),
            fake_base: Ipv4Addr::new(198, 18, 0, 0),
            dns_ttl: 300,
            answer_ping: true,
        }
    }
}

/// A TCP connection as seen by the sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpRecord {
    pub id: ConnId,
    pub flow: Flow,
    /// DNS name that had resolved to `flow.remote`, if it is a fake address.
    pub hostname: Option<String>,
    pub refused: bool,
    pub opened_at: VirtualTime,
    pub closed_at: Option<VirtualTime>,
    /// Closed with RST or timeout instead of FIN.
    pub reset: bool,
    /// The guest has closed its direction.
    pub guest_shutdown: bool,
    /// All the bytes sent by the guest, in order.
    pub from_guest: Vec<u8>,
    /// Bytes delivered to the stack towards the guest.
    pub to_guest: u64,
}

/// A UDP flow as seen by the sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdpRecord {
    pub id: ConnId,
    pub flow: Flow,
    pub hostname: Option<String>,
    /// Guest datagrams with their instant.
    pub datagrams: Vec<(VirtualTime, Vec<u8>)>,
    pub closed_at: Option<VirtualTime>,
}

/// A DNS query resolved by the sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsRecord {
    pub at: VirtualTime,
    pub name: String,
    pub qtype: u16,
    /// Fake address given in the answer (only for A queries).
    pub answer: Option<Ipv4Addr>,
}

#[derive(Debug, Default)]
struct TcpRuntime {
    reply: TcpReply,
    out: VecDeque<u8>,
    replied_to_data: bool,
    eof: bool,
}

#[derive(Debug, Default)]
pub struct Sinkhole {
    config: SinkholeConfig,
    tcp: BTreeMap<ConnId, TcpRecord>,
    tcp_runtime: BTreeMap<ConnId, TcpRuntime>,
    udp: BTreeMap<ConnId, UdpRecord>,
    udp_out: VecDeque<(ConnId, Vec<u8>)>,
    dns: Vec<DnsRecord>,
    names: BTreeMap<String, Ipv4Addr>,
    addrs: BTreeMap<Ipv4Addr, String>,
}

impl Sinkhole {
    pub fn new(config: SinkholeConfig) -> Self {
        Sinkhole { config, ..Default::default() }
    }

    pub fn config(&self) -> &SinkholeConfig {
        &self.config
    }

    /// TCP connections in order of opening.
    pub fn tcp_connections(&self) -> impl Iterator<Item = &TcpRecord> {
        self.tcp.values()
    }

    pub fn tcp_connection(&self, id: ConnId) -> Option<&TcpRecord> {
        self.tcp.get(&id)
    }

    pub fn udp_flows(&self) -> impl Iterator<Item = &UdpRecord> {
        self.udp.values()
    }

    pub fn dns_queries(&self) -> &[DnsRecord] {
        &self.dns
    }

    /// Name resolved to a fake address.
    pub fn hostname(&self, addr: Ipv4Addr) -> Option<&str> {
        self.addrs.get(&addr).map(String::as_str)
    }

    /// Indirizzo finto dato a un nome (in minuscolo).
    pub fn fake_addr(&self, name: &str) -> Option<Ipv4Addr> {
        self.names.get(name).copied()
    }

    fn resolve(&mut self, name: &str) -> Ipv4Addr {
        if let Some(a) = self.names.get(name) {
            return *a;
        }
        let n = self.names.len() as u32 + 1;
        let addr = Ipv4Addr::from(u32::from(self.config.fake_base).wrapping_add(n));
        self.names.insert(name.to_owned(), addr);
        self.addrs.insert(addr, name.to_owned());
        addr
    }

    fn answer_dns(&mut self, now: VirtualTime, id: ConnId, data: &[u8]) {
        let Some(q) = dns::parse_query(data) else {
            return; // Not a query: no answer, like a real server.
        };
        let (rcode, answer) = if q.opcode != 0 {
            (dns::RCODE_NOTIMP, None)
        } else if q.qtype == dns::TYPE_A && q.qclass == dns::CLASS_IN && !q.name.is_empty() {
            (dns::RCODE_NOERROR, Some(self.resolve(&q.name)))
        } else {
            // AAAA and other types: existing name but without records, so the guest
            // falls back to IPv4.
            (dns::RCODE_NOERROR, None)
        };
        self.dns.push(DnsRecord { at: now, name: q.name.clone(), qtype: q.qtype, answer });
        let addrs: Vec<Ipv4Addr> = answer.into_iter().collect();
        self.udp_out.push_back((id, dns::build_response(&q, rcode, &addrs, self.config.dns_ttl)));
    }
}

impl TcpRuntime {
    fn maybe_finish(&mut self) {
        let waiting_for_data = !self.reply.on_data.is_empty() && !self.replied_to_data;
        if self.reply.close_after_reply && !waiting_for_data {
            self.eof = true;
        }
    }
}

impl Upstream for Sinkhole {
    fn tcp_open(&mut self, now: VirtualTime, id: ConnId, flow: Flow) {
        let refused = self.config.refused_ports.contains(&flow.remote.port());
        let hostname = self.addrs.get(flow.remote.ip()).cloned();
        self.tcp.insert(
            id,
            TcpRecord {
                id,
                flow,
                hostname,
                refused,
                opened_at: now,
                closed_at: None,
                reset: false,
                guest_shutdown: false,
                from_guest: Vec::new(),
                to_guest: 0,
            },
        );
        if !refused {
            let reply =
                self.config.tcp_by_port.get(&flow.remote.port()).unwrap_or(&self.config.tcp_default).clone();
            let mut rt =
                TcpRuntime { out: reply.on_connect.iter().copied().collect(), reply, ..Default::default() };
            rt.maybe_finish();
            self.tcp_runtime.insert(id, rt);
        }
    }

    fn tcp_status(&mut self, _now: VirtualTime, id: ConnId) -> TcpStatus {
        match self.tcp.get(&id) {
            Some(r) if !r.refused => TcpStatus::Connected,
            _ => TcpStatus::Refused,
        }
    }

    fn tcp_write(&mut self, _now: VirtualTime, id: ConnId, data: &[u8]) -> usize {
        if let Some(r) = self.tcp.get_mut(&id) {
            r.from_guest.extend_from_slice(data);
        }
        if let Some(rt) = self.tcp_runtime.get_mut(&id)
            && !data.is_empty()
            && !rt.replied_to_data
            && !rt.eof
        {
            rt.replied_to_data = true;
            let reply = rt.reply.on_data.clone();
            rt.out.extend(reply);
            rt.maybe_finish();
        }
        data.len()
    }

    fn tcp_read(&mut self, _now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead {
        let Some(rt) = self.tcp_runtime.get_mut(&id) else { return TcpRead::Reset };
        if rt.out.is_empty() {
            return if rt.eof { TcpRead::Eof } else { TcpRead::WouldBlock };
        }
        let n = buf.len().min(rt.out.len());
        for (dst, src) in buf.iter_mut().zip(rt.out.drain(..n)) {
            *dst = src;
        }
        if let Some(r) = self.tcp.get_mut(&id) {
            r.to_guest += n as u64;
        }
        TcpRead::Data(n)
    }

    fn tcp_shutdown(&mut self, _now: VirtualTime, id: ConnId) {
        if let Some(r) = self.tcp.get_mut(&id) {
            r.guest_shutdown = true;
        }
        // Like a server that closes when the client has finished.
        if let Some(rt) = self.tcp_runtime.get_mut(&id) {
            rt.eof = true;
        }
    }

    fn tcp_close(&mut self, now: VirtualTime, id: ConnId, reset: bool) {
        if let Some(r) = self.tcp.get_mut(&id) {
            r.closed_at = Some(now);
            r.reset = reset && !r.refused;
        }
        self.tcp_runtime.remove(&id);
    }

    fn udp_send(&mut self, now: VirtualTime, id: ConnId, flow: Flow, data: &[u8]) {
        let hostname = self.addrs.get(flow.remote.ip()).cloned();
        let rec = self.udp.entry(id).or_insert_with(|| UdpRecord {
            id,
            flow,
            hostname,
            datagrams: Vec::new(),
            closed_at: None,
        });
        rec.datagrams.push((now, data.to_vec()));
        if flow.remote == self.config.dns_server {
            self.answer_dns(now, id, data);
        } else if let Some(reply) = &self.config.udp_reply {
            self.udp_out.push_back((id, reply.clone()));
        }
    }

    fn udp_recv(&mut self, _now: VirtualTime) -> Option<(ConnId, Vec<u8>)> {
        self.udp_out.pop_front()
    }

    fn udp_close(&mut self, now: VirtualTime, id: ConnId) {
        if let Some(r) = self.udp.get_mut(&id) {
            r.closed_at = Some(now);
        }
    }

    fn ping(&mut self, _now: VirtualTime, _dst: Ipv4Addr) -> bool {
        self.config.answer_ping
    }
}
