//! Registro degli eventi di rete per il motore di analisi.
//!
//! Ogni evento porta il tempo virtuale del momento in cui è accaduto. Il
//! registro contiene metadati (chi, quando, quanti byte); il contenuto dei
//! byte lo conserva l'`Upstream` (per esempio il `Sinkhole`).

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
    /// L'upstream ha interrotto la connessione (RST verso il guest).
    RemoteReset,
    /// L'upstream ha rifiutato la connessione (RST al SYN del guest).
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
