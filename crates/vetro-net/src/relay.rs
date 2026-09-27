//! Interface to the relay: the guest's connections really go out, but
//! from an external process (M7's WebSocket relay), because the browser can't
//! open TCP/UDP sockets.
//!
//! Here there are only the abstract protocol ([`RelayMessage`]), the transport
//! ([`Relay`]), the adapter [`RelayUpstream`] that turns the calls of
//! [`Upstream`] into messages, and an in-memory test relay
//! ([`MemoryRelay`]). The wire encoding and flow control between host
//! and relay come with M7.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime, dns};

/// Messages between the stack (host) and the relay. The `id`s are the stack's
/// [`ConnId`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMessage {
    /// Host → relay: open a TCP connection to `dst`.
    TcpConnect { id: ConnId, dst: SocketAddrV4 },
    /// Relay → host: connection open.
    TcpConnected { id: ConnId },
    /// Relay → host: connection refused or unreachable.
    TcpRefused { id: ConnId },
    /// In both directions: bytes of the connection.
    TcpData { id: ConnId, data: Vec<u8> },
    /// In both directions: the sender has closed its direction (FIN).
    TcpShutdown { id: ConnId },
    /// In both directions: connection finished; `reset` if aborted.
    TcpClose { id: ConnId, reset: bool },
    /// Host → relay: UDP datagram to `dst`.
    UdpSend { id: ConnId, dst: SocketAddrV4, data: Vec<u8> },
    /// Relay → host: response datagram on flow `id`.
    UdpRecv { id: ConnId, data: Vec<u8> },
    /// Host → relay: UDP flow expired.
    UdpClose { id: ConnId },
}

/// Transport of the messages to the relay. Non-blocking: `recv` returns
/// `None` if there is nothing to read right now.
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

/// [`Upstream`] that forwards everything to a [`Relay`].
#[derive(Debug)]
pub struct RelayUpstream<R: Relay> {
    relay: R,
    conns: BTreeMap<ConnId, RelayConn>,
    udp_inbox: VecDeque<(ConnId, Vec<u8>)>,
    /// For `ping`: the relay doesn't forward ICMP (M7 may change that).
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

    /// Reads all the available messages from the relay.
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
                // Messages that only go towards the relay: ignored.
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

/// In-memory test relay: plays the part of the relay process and of the network.
///
/// - TCP: "echo" service (sends the bytes back) to any
///   destination, except the ports in `refused_ports`; when the host closes its
///   direction, it closes its own too.
/// - UDP to port 53: resolves with the `hosts` table (NXDOMAIN for
///   missing names). Other UDP: echo.
///
/// Records every message received from the host in `sent`.
#[derive(Debug, Default)]
pub struct MemoryRelay {
    pub refused_ports: Vec<u16>,
    pub hosts: BTreeMap<String, Ipv4Addr>,
    /// Messages received from the host, in order.
    pub sent: Vec<RelayMessage>,
    /// Messages waiting to be read by the host.
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
