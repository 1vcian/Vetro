//! Log of network events for the analysis engine.
//!
//! Every event carries the virtual time of the moment it happened. The
//! log contains metadata (who, when, how many bytes); the contents of the
//! bytes are kept by the `Upstream` (for example the `Sinkhole`).

use core::fmt;
use std::net::Ipv4Addr;

use crate::wire::Mac;
use crate::{ConnId, Flow, VirtualTime};

/// Direction of the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the guest to the remote destination.
    ToRemote,
    /// From the remote destination to the guest.
    ToGuest,
}

/// Why a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// FIN in both directions, everything acknowledged.
    Normal,
    /// RST sent by the guest.
    GuestReset,
    /// The upstream (or the host, for connections opened by the host)
    /// aborted the connection (RST to the guest).
    RemoteReset,
    /// The upstream refused the connection (RST to the guest's SYN); for
    /// connections opened by the host, the guest answered RST to the SYN.
    Refused,
    /// The guest no longer answers: retransmissions exhausted or connection
    /// left waiting for the upstream too long.
    Timeout,
    /// UDP flow inactive beyond the limit.
    Idle,
}

/// DHCP messages recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpMessage {
    Offer,
    Ack,
    Nak,
    Release,
    Decline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// DHCP exchange: for Offer/Ack/Nak it is sent by the gateway, for
    /// Release/Decline by the guest. `hostname` is the client's option 12.
    Dhcp {
        message: DhcpMessage,
        mac: Mac,
        ip: Ipv4Addr,
        hostname: Option<String>,
    },
    /// ICMP echo from the guest to `dst`; `answered` if the reply was sent.
    IcmpEcho {
        dst: Ipv4Addr,
        answered: bool,
    },
    /// Guest SYN received: the connection exists from here.
    TcpOpen {
        id: ConnId,
        flow: Flow,
    },
    /// The host opens a connection to a guest service (port
    /// forwarding, `Stack::host_connect`): SYN from the gateway to `flow.guest`,
    /// from `flow.remote`. From here the connection is like the others: `TcpData`
    /// `ToRemote` are the guest's bytes to the host.
    TcpConnect {
        id: ConnId,
        flow: Flow,
    },
    /// Handshake completed.
    TcpEstablished {
        id: ConnId,
    },
    /// New bytes (never counted before) in one direction.
    TcpData {
        id: ConnId,
        dir: Direction,
        len: usize,
    },
    TcpClosed {
        id: ConnId,
        reason: CloseReason,
        bytes_to_remote: u64,
        bytes_to_guest: u64,
    },
    /// First datagram of a UDP flow.
    UdpOpen {
        id: ConnId,
        flow: Flow,
    },
    UdpData {
        id: ConnId,
        dir: Direction,
        len: usize,
    },
    UdpClosed {
        id: ConnId,
        reason: CloseReason,
        bytes_to_remote: u64,
        bytes_to_guest: u64,
    },
    /// DNS query from the guest to the virtual DNS server.
    DnsQuery {
        id: ConnId,
        txid: u16,
        name: String,
        qtype: u16,
    },
    /// DNS answer delivered to the guest (only A records are extracted).
    DnsAnswer {
        id: ConnId,
        txid: u16,
        name: String,
        qtype: u16,
        rcode: u8,
        addrs: Vec<Ipv4Addr>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetEvent {
    pub at: VirtualTime,
    pub kind: EventKind,
}

/// In-memory log, in order of occurrence.
#[derive(Debug, Default)]
pub(crate) struct EventLog {
    pub(crate) events: Vec<NetEvent>,
}

impl EventLog {
    pub(crate) fn push(&mut self, at: VirtualTime, kind: EventKind) {
        self.events.push(NetEvent { at, kind });
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

impl fmt::Display for Flow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -> {}", self.guest, self.remote)
    }
}

/// One readable line per event (for `vetro boot --net-events` and the logs):
/// virtual time in seconds, then the fact.
impl fmt::Display for NetEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let us = self.at.as_micros();
        write!(f, "[{:6}.{:06}] ", us / 1_000_000, us % 1_000_000)?;
        let dir = |d: &Direction| match d {
            Direction::ToRemote => "guest->remote",
            Direction::ToGuest => "remote->guest",
        };
        match &self.kind {
            EventKind::Dhcp { message, mac, ip, hostname } => {
                write!(f, "dhcp {message:?} {ip} {mac}")?;
                if let Some(h) = hostname {
                    write!(f, " ({h})")?;
                }
                Ok(())
            }
            EventKind::IcmpEcho { dst, answered } => {
                write!(f, "icmp echo {dst} {}", if *answered { "answered" } else { "unanswered" })
            }
            EventKind::TcpOpen { id, flow } => write!(f, "tcp {id} syn {flow}"),
            EventKind::TcpConnect { id, flow } => {
                write!(f, "tcp {id} from host {} -> {}", flow.remote, flow.guest)
            }
            EventKind::TcpEstablished { id } => write!(f, "tcp {id} established"),
            EventKind::TcpData { id, dir: d, len } => write!(f, "tcp {id} {} {len} bytes", dir(d)),
            EventKind::TcpClosed { id, reason, bytes_to_remote, bytes_to_guest } => write!(
                f,
                "tcp {id} closed {reason:?} (guest->remote {bytes_to_remote} bytes, remote->guest {bytes_to_guest} bytes)"
            ),
            EventKind::UdpOpen { id, flow } => write!(f, "udp {id} new {flow}"),
            EventKind::UdpData { id, dir: d, len } => write!(f, "udp {id} {} {len} bytes", dir(d)),
            EventKind::UdpClosed { id, reason, bytes_to_remote, bytes_to_guest } => write!(
                f,
                "udp {id} closed {reason:?} (guest->remote {bytes_to_remote} bytes, remote->guest {bytes_to_guest} bytes)"
            ),
            EventKind::DnsQuery { id, txid, name, qtype } => {
                write!(f, "dns {id} query {name} type {qtype} (id {txid:#06x})")
            }
            EventKind::DnsAnswer { id, txid, name, qtype, rcode, addrs } => {
                write!(f, "dns {id} answer {name} type {qtype} rcode {rcode} (id {txid:#06x})")?;
                for a in addrs {
                    write!(f, " {a}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    #[test]
    fn righe_leggibili() {
        let flow = Flow {
            guest: SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 40000),
            remote: SocketAddrV4::new(Ipv4Addr::new(198, 18, 0, 1), 80),
        };
        let e = |at, kind| NetEvent { at: VirtualTime(at), kind }.to_string();
        assert_eq!(
            e(1_500_000, EventKind::TcpOpen { id: 3, flow }),
            "[     1.500000] tcp 3 syn 10.0.2.15:40000 -> 198.18.0.1:80"
        );
        let mac = Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let ip = Ipv4Addr::new(10, 0, 2, 15);
        assert_eq!(
            e(7, EventKind::Dhcp { message: DhcpMessage::Ack, mac, ip, hostname: None }),
            "[     0.000007] dhcp Ack 10.0.2.15 52:54:00:12:34:56"
        );
        let answer = EventKind::DnsAnswer {
            id: 2,
            txid: 0x1234,
            name: "vetro.example".into(),
            qtype: 1,
            rcode: 0,
            addrs: vec![Ipv4Addr::new(198, 18, 0, 1)],
        };
        assert_eq!(
            e(0, answer),
            "[     0.000000] dns 2 answer vetro.example type 1 rcode 0 (id 0x1234) 198.18.0.1"
        );
    }
}
