//! Registro degli eventi di rete per il motore di analisi.
//!
//! Ogni evento porta il tempo virtuale del momento in cui è accaduto. Il
//! registro contiene metadati (chi, quando, quanti byte); il contenuto dei
//! byte lo conserva l'`Upstream` (per esempio il `Sinkhole`).

use core::fmt;
use std::net::Ipv4Addr;

use crate::wire::Mac;
use crate::{ConnId, Flow, VirtualTime};

/// Verso dei dati.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Dal guest verso la destinazione remota.
    ToRemote,
    /// Dalla destinazione remota verso il guest.
    ToGuest,
}

/// Perché una connessione è finita.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// FIN in entrambi i versi, tutto riscontrato.
    Normal,
    /// RST inviato dal guest.
    GuestReset,
    /// L'upstream (o l'host, per le connessioni aperte dall'host) ha
    /// interrotto la connessione (RST verso il guest).
    RemoteReset,
    /// L'upstream ha rifiutato la connessione (RST al SYN del guest); per le
    /// connessioni aperte dall'host, il guest ha risposto RST al SYN.
    Refused,
    /// Il guest non risponde più: ritrasmissioni esaurite o connessione
    /// rimasta in attesa dell'upstream troppo a lungo.
    Timeout,
    /// Flusso UDP inattivo oltre il limite.
    Idle,
}

/// Messaggi DHCP registrati.
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
    /// Scambio DHCP: per Offer/Ack/Nak lo invia il gateway, per
    /// Release/Decline il guest. `hostname` è l'opzione 12 del client.
    Dhcp {
        message: DhcpMessage,
        mac: Mac,
        ip: Ipv4Addr,
        hostname: Option<String>,
    },
    /// Echo ICMP del guest verso `dst`; `answered` se è partita la risposta.
    IcmpEcho {
        dst: Ipv4Addr,
        answered: bool,
    },
    /// SYN del guest ricevuto: la connessione esiste da qui.
    TcpOpen {
        id: ConnId,
        flow: Flow,
    },
    /// L'host apre una connessione verso un servizio del guest (inoltro di
    /// porte, `Stack::host_connect`): SYN dal gateway verso `flow.guest`,
    /// da `flow.remote`. Da qui la connessione è come le altre: `TcpData`
    /// `ToRemote` sono i byte del guest verso l'host.
    TcpConnect {
        id: ConnId,
        flow: Flow,
    },
    /// Handshake completato.
    TcpEstablished {
        id: ConnId,
    },
    /// Byte nuovi (mai contati prima) in un verso.
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
    /// Primo datagramma di un flusso UDP.
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
    /// Domanda DNS del guest verso il server DNS virtuale.
    DnsQuery {
        id: ConnId,
        txid: u16,
        name: String,
        qtype: u16,
    },
    /// Risposta DNS consegnata al guest (solo i record A sono estratti).
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

/// Registro in memoria, in ordine di accadimento.
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

/// Una riga leggibile per evento (per `vetro boot --net-events` e i log):
/// tempo virtuale in secondi, poi il fatto.
impl fmt::Display for NetEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let us = self.at.as_micros();
        write!(f, "[{:6}.{:06}] ", us / 1_000_000, us % 1_000_000)?;
        let dir = |d: &Direction| match d {
            Direction::ToRemote => "guest->remoto",
            Direction::ToGuest => "remoto->guest",
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
                write!(f, "icmp echo {dst} {}", if *answered { "risposto" } else { "senza risposta" })
            }
            EventKind::TcpOpen { id, flow } => write!(f, "tcp {id} syn {flow}"),
            EventKind::TcpConnect { id, flow } => {
                write!(f, "tcp {id} dall'host {} -> {}", flow.remote, flow.guest)
            }
            EventKind::TcpEstablished { id } => write!(f, "tcp {id} stabilita"),
            EventKind::TcpData { id, dir: d, len } => write!(f, "tcp {id} {} {len} byte", dir(d)),
            EventKind::TcpClosed { id, reason, bytes_to_remote, bytes_to_guest } => write!(
                f,
                "tcp {id} chiusa {reason:?} (guest->remoto {bytes_to_remote} byte, remoto->guest {bytes_to_guest} byte)"
            ),
            EventKind::UdpOpen { id, flow } => write!(f, "udp {id} nuovo {flow}"),
            EventKind::UdpData { id, dir: d, len } => write!(f, "udp {id} {} {len} byte", dir(d)),
            EventKind::UdpClosed { id, reason, bytes_to_remote, bytes_to_guest } => write!(
                f,
                "udp {id} chiuso {reason:?} (guest->remoto {bytes_to_remote} byte, remoto->guest {bytes_to_guest} byte)"
            ),
            EventKind::DnsQuery { id, txid, name, qtype } => {
                write!(f, "dns {id} domanda {name} tipo {qtype} (id {txid:#06x})")
            }
            EventKind::DnsAnswer { id, txid, name, qtype, rcode, addrs } => {
                write!(f, "dns {id} risposta {name} tipo {qtype} rcode {rcode} (id {txid:#06x})")?;
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
            "[     0.000000] dns 2 risposta vetro.example tipo 1 rcode 0 (id 0x1234) 198.18.0.1"
        );
    }
}
