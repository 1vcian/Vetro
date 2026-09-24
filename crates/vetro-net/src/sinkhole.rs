//! Sinkhole: un upstream che non esce mai dalla macchina.
//!
//! Accetta (o rifiuta, per porta) ogni connessione, registra i byte che il
//! guest manda e risponde con dati configurabili. Il DNS risolve ogni nome A
//! verso indirizzi finti deterministici (in ordine di prima richiesta, a
//! partire da 198.18.0.1, blocco riservato ai test di rete dalla RFC 2544) e
//! ricorda i nomi, così ogni connessione verso un indirizzo finto è
//! attribuita al nome che il guest aveva chiesto.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime, dns};

/// Risposta configurata per le connessioni TCP verso una porta.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TcpReply {
    /// Byte mandati subito dopo l'apertura (un banner, come SMTP o SSH).
    pub on_connect: Vec<u8>,
    /// Byte mandati una volta, dopo il primo blocco di dati del guest.
    pub on_data: Vec<u8>,
    /// Chiude il verso verso il guest (FIN) dopo le risposte configurate:
    /// dopo `on_data` se non è vuoto, altrimenti subito dopo `on_connect`.
    pub close_after_reply: bool,
}

impl TcpReply {
    /// Una risposta HTTP/1.1 minima e vuota, poi chiusura.
    pub fn http_empty() -> Self {
        TcpReply {
            on_connect: Vec::new(),
            on_data: b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            close_after_reply: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SinkholeConfig {
    /// Risposta per le porte non elencate in `tcp_by_port`.
    pub tcp_default: TcpReply,
    pub tcp_by_port: BTreeMap<u16, TcpReply>,
    /// Porte TCP rifiutate (il guest riceve RST, "connection refused").
    pub refused_ports: BTreeSet<u16>,
    /// Risposta a ogni datagramma UDP non DNS (`None`: nessuna risposta).
    pub udp_reply: Option<Vec<u8>>,
    /// Il server DNS virtuale (deve coincidere con `NetConfig::dns_ip`).
    pub dns_server: SocketAddrV4,
    /// Primo indirizzo finto assegnato ai nomi.
    pub fake_base: Ipv4Addr,
    pub dns_ttl: u32,
    /// Risponde agli echo ICMP verso qualsiasi indirizzo esterno.
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

/// Una connessione TCP vista dal sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpRecord {
    pub id: ConnId,
    pub flow: Flow,
    /// Nome DNS che aveva risolto verso `flow.remote`, se è un indirizzo finto.
    pub hostname: Option<String>,
    pub refused: bool,
    pub opened_at: VirtualTime,
    pub closed_at: Option<VirtualTime>,
    /// Chiusa con RST o timeout invece che con FIN.
    pub reset: bool,
    /// Il guest ha chiuso il suo verso.
    pub guest_shutdown: bool,
    /// Tutti i byte mandati dal guest, in ordine.
    pub from_guest: Vec<u8>,
    /// Byte consegnati allo stack verso il guest.
    pub to_guest: u64,
}

/// Un flusso UDP visto dal sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdpRecord {
    pub id: ConnId,
    pub flow: Flow,
    pub hostname: Option<String>,
    /// Datagrammi del guest con il loro istante.
    pub datagrams: Vec<(VirtualTime, Vec<u8>)>,
    pub closed_at: Option<VirtualTime>,
}

/// Una domanda DNS risolta dal sinkhole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsRecord {
    pub at: VirtualTime,
    pub name: String,
    pub qtype: u16,
    /// Indirizzo finto dato in risposta (solo per le domande A).
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

    /// Connessioni TCP in ordine di apertura.
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

    /// Nome risolto verso un indirizzo finto.
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
            return; // Non è una domanda: nessuna risposta, come un server vero.
        };
        let (rcode, answer) = if q.opcode != 0 {
            (dns::RCODE_NOTIMP, None)
        } else if q.qtype == dns::TYPE_A && q.qclass == dns::CLASS_IN && !q.name.is_empty() {
            (dns::RCODE_NOERROR, Some(self.resolve(&q.name)))
        } else {
            // AAAA e altri tipi: nome esistente ma senza record, così il guest
            // ripiega su IPv4.
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
        // Come un server che chiude quando il client ha finito.
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
