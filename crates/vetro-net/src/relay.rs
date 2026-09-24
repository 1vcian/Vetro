//! Interfaccia verso il relay: le connessioni del guest escono davvero, ma
//! da un processo esterno (il relay WebSocket di M7), perché il browser non
//! può aprire socket TCP/UDP.
//!
//! Qui ci sono solo il protocollo astratto ([`RelayMessage`]), il trasporto
//! ([`Relay`]), l'adattatore [`RelayUpstream`] che trasforma le chiamate di
//! [`Upstream`] in messaggi, e un relay di prova in memoria
//! ([`MemoryRelay`]). La codifica sul filo e il controllo di flusso tra host
//! e relay arrivano con M7.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime, dns};

/// Messaggi tra lo stack (host) e il relay. Gli `id` sono i [`ConnId`]
/// dello stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMessage {
    /// Host → relay: apri una connessione TCP verso `dst`.
    TcpConnect { id: ConnId, dst: SocketAddrV4 },
    /// Relay → host: connessione aperta.
    TcpConnected { id: ConnId },
    /// Relay → host: connessione rifiutata o irraggiungibile.
    TcpRefused { id: ConnId },
    /// In entrambi i versi: byte della connessione.
    TcpData { id: ConnId, data: Vec<u8> },
    /// In entrambi i versi: chi manda ha chiuso il suo verso (FIN).
    TcpShutdown { id: ConnId },
    /// In entrambi i versi: connessione finita; `reset` se interrotta.
    TcpClose { id: ConnId, reset: bool },
    /// Host → relay: datagramma UDP verso `dst`.
    UdpSend { id: ConnId, dst: SocketAddrV4, data: Vec<u8> },
    /// Relay → host: datagramma di risposta sul flusso `id`.
    UdpRecv { id: ConnId, data: Vec<u8> },
    /// Host → relay: flusso UDP scaduto.
    UdpClose { id: ConnId },
}

/// Trasporto dei messaggi verso il relay. Non bloccante: `recv` restituisce
/// `None` se non c'è niente da leggere adesso.
pub trait Relay {
    fn send(&mut self, msg: RelayMessage);
    fn recv(&mut self) -> Option<RelayMessage>;
}

#[derive(Debug, Default)]
struct RelayConn {
    status: Option<TcpStatus>,
    inbox: VecDeque<u8>,
    eof: bool,
    reset: bool,
}

/// [`Upstream`] che inoltra tutto a un [`Relay`].
#[derive(Debug)]
pub struct RelayUpstream<R: Relay> {
    relay: R,
    conns: BTreeMap<ConnId, RelayConn>,
    udp_inbox: VecDeque<(ConnId, Vec<u8>)>,
    /// Per `ping`: il relay non inoltra ICMP (M7 può cambiarlo).
    answer_ping: bool,
}

impl<R: Relay> RelayUpstream<R> {
    pub fn new(relay: R) -> Self {
        RelayUpstream { relay, conns: BTreeMap::new(), udp_inbox: VecDeque::new(), answer_ping: false }
    }

    pub fn relay(&self) -> &R {
        &self.relay
    }

    pub fn relay_mut(&mut self) -> &mut R {
        &mut self.relay
    }

    /// Legge tutti i messaggi disponibili dal relay.
    fn pump(&mut self) {
        while let Some(msg) = self.relay.recv() {
            match msg {
                RelayMessage::TcpConnected { id } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.status = Some(TcpStatus::Connected);
                    }
                }
                RelayMessage::TcpRefused { id } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.status = Some(TcpStatus::Refused);
                    }
                }
                RelayMessage::TcpData { id, data } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.inbox.extend(data);
                    }
                }
                RelayMessage::TcpShutdown { id } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.eof = true;
                    }
                }
                RelayMessage::TcpClose { id, reset } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.eof = true;
                        c.reset |= reset;
                    }
                }
                RelayMessage::UdpRecv { id, data } => self.udp_inbox.push_back((id, data)),
                // Messaggi che vanno solo verso il relay: ignorati.
                RelayMessage::TcpConnect { .. }
                | RelayMessage::UdpSend { .. }
                | RelayMessage::UdpClose { .. } => {}
            }
        }
    }
}

impl<R: Relay> Upstream for RelayUpstream<R> {
    fn tcp_open(&mut self, _now: VirtualTime, id: ConnId, flow: Flow) {
        self.conns.insert(id, RelayConn::default());
        self.relay.send(RelayMessage::TcpConnect { id, dst: flow.remote });
    }

    fn tcp_status(&mut self, _now: VirtualTime, id: ConnId) -> TcpStatus {
        self.pump();
        self.conns.get(&id).map_or(TcpStatus::Refused, |c| c.status.unwrap_or(TcpStatus::Pending))
    }

    fn tcp_write(&mut self, _now: VirtualTime, id: ConnId, data: &[u8]) -> usize {
        if !data.is_empty() {
            self.relay.send(RelayMessage::TcpData { id, data: data.to_vec() });
        }
        data.len()
    }

    fn tcp_read(&mut self, _now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead {
        self.pump();
        let Some(c) = self.conns.get_mut(&id) else { return TcpRead::Reset };
        if c.reset {
            return TcpRead::Reset;
        }
        if c.inbox.is_empty() {
            return if c.eof { TcpRead::Eof } else { TcpRead::WouldBlock };
        }
        let n = buf.len().min(c.inbox.len());
        for (dst, src) in buf.iter_mut().zip(c.inbox.drain(..n)) {
            *dst = src;
        }
        TcpRead::Data(n)
    }

    fn tcp_shutdown(&mut self, _now: VirtualTime, id: ConnId) {
        self.relay.send(RelayMessage::TcpShutdown { id });
    }

    fn tcp_close(&mut self, _now: VirtualTime, id: ConnId, reset: bool) {
        if self.conns.remove(&id).is_some() {
            self.relay.send(RelayMessage::TcpClose { id, reset });
        }
    }

    fn udp_send(&mut self, _now: VirtualTime, id: ConnId, flow: Flow, data: &[u8]) {
        self.relay.send(RelayMessage::UdpSend { id, dst: flow.remote, data: data.to_vec() });
    }

    fn udp_recv(&mut self, _now: VirtualTime) -> Option<(ConnId, Vec<u8>)> {
        self.pump();
        self.udp_inbox.pop_front()
    }

    fn udp_close(&mut self, _now: VirtualTime, id: ConnId) {
        self.relay.send(RelayMessage::UdpClose { id });
    }

    fn ping(&mut self, _now: VirtualTime, _dst: Ipv4Addr) -> bool {
        self.answer_ping
    }
}

/// Relay di prova in memoria: fa la parte del processo relay e della rete.
///
/// - TCP: servizio "echo" (rimanda indietro i byte) verso qualsiasi
///   destinazione, salvo le porte in `refused_ports`; quando l'host chiude il
///   suo verso, chiude anche il proprio.
/// - UDP verso la porta 53: risolve con la tabella `hosts` (NXDOMAIN per i
///   nomi assenti). Altri UDP: echo.
///
/// Registra ogni messaggio ricevuto dall'host in `sent`.
#[derive(Debug, Default)]
pub struct MemoryRelay {
    pub refused_ports: Vec<u16>,
    pub hosts: BTreeMap<String, Ipv4Addr>,
    /// Messaggi ricevuti dall'host, in ordine.
    pub sent: Vec<RelayMessage>,
    /// Messaggi in attesa di essere letti dall'host.
    pub inbox: VecDeque<RelayMessage>,
}

impl Relay for MemoryRelay {
    fn send(&mut self, msg: RelayMessage) {
        self.sent.push(msg.clone());
        match msg {
            RelayMessage::TcpConnect { id, dst } => {
                let reply = if self.refused_ports.contains(&dst.port()) {
                    RelayMessage::TcpRefused { id }
                } else {
                    RelayMessage::TcpConnected { id }
                };
                self.inbox.push_back(reply);
            }
            RelayMessage::TcpData { id, data } => self.inbox.push_back(RelayMessage::TcpData { id, data }),
            RelayMessage::TcpShutdown { id } => self.inbox.push_back(RelayMessage::TcpShutdown { id }),
            RelayMessage::UdpSend { id, dst, data } => {
                if dst.port() == 53 {
                    if let Some(q) = dns::parse_query(&data) {
                        let (rcode, addrs) = match self.hosts.get(&q.name) {
                            Some(a) if q.qtype == dns::TYPE_A => (dns::RCODE_NOERROR, vec![*a]),
                            Some(_) => (dns::RCODE_NOERROR, vec![]),
                            None => (dns::RCODE_NXDOMAIN, vec![]),
                        };
                        let data = dns::build_response(&q, rcode, &addrs, 60);
                        self.inbox.push_back(RelayMessage::UdpRecv { id, data });
                    }
                } else {
                    self.inbox.push_back(RelayMessage::UdpRecv { id, data });
                }
            }
            RelayMessage::TcpClose { .. } | RelayMessage::UdpClose { .. } => {}
            RelayMessage::TcpConnected { .. }
            | RelayMessage::TcpRefused { .. }
            | RelayMessage::UdpRecv { .. } => {}
        }
    }

    fn recv(&mut self) -> Option<RelayMessage> {
        self.inbox.pop_front()
    }
}
