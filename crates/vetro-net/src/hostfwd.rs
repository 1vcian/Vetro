//! Inoltro di porte: connessioni TCP aperte dall'host verso un servizio del
//! guest (come `hostfwd` della rete user di QEMU).
//!
//! Lo stack fa da client TCP verso il guest: il SYN parte dal gateway
//! (10.0.2.2, come slirp traduce le connessioni da localhost) da una porta
//! effimera scelta in modo deterministico. Dal SYN-ACK in poi la connessione
//! è la stessa macchina a stati delle connessioni del guest (`tcp.rs`); al
//! posto dell'upstream c'è [`HostSide`], che tiene i byte in coda nei due
//! versi. L'host (la piattaforma: `vetro boot --hostfwd`, il browser) mette
//! byte in coda e li legge con i metodi `Stack::host_*`: nessun socket vero
//! nel core, tutto sincrono e deterministico. Le azioni dell'host hanno
//! effetto al successivo `Stack::poll`.

use std::collections::{BTreeMap, VecDeque};

use crate::events::CloseReason;
use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime};

/// Byte al massimo in coda in ciascun verso per connessione: oltre,
/// `Stack::host_send` accetta meno byte (contropressione verso l'host) e il
/// guest vede la finestra chiudersi finché l'host non legge.
pub const HOST_BUFFER: usize = 256 * 1024;

/// Prima porta effimera del gateway per le connessioni dell'host (quelle di
/// Linux partono da 32768; qui l'intervallo IANA 49152..=65535).
pub const FIRST_EPHEMERAL_PORT: u16 = 49152;

/// Stato di una connessione aperta dall'host verso il guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostConnState {
    /// SYN mandato (o da mandare al prossimo `poll`), nessuna risposta.
    Connecting,
    /// Handshake completato: i byte scorrono (anche durante la chiusura).
    Open,
    /// Finita: `Normal` dopo FIN nei due versi, `Refused` se il guest ha
    /// risposto RST al SYN (nessuno in ascolto), `GuestReset` per un RST del
    /// guest, `RemoteReset` dopo `Stack::host_abort`, `Timeout` se il guest
    /// non risponde.
    Closed(CloseReason),
}

/// Quel che l'host vede di una sua connessione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostConnInfo {
    pub state: HostConnState,
    pub flow: Flow,
    /// Byte del guest pronti per `Stack::host_recv`.
    pub readable: usize,
    /// Spazio per `Stack::host_send`.
    pub writable: usize,
    /// Il guest ha chiuso il suo verso (FIN) e l'host ha letto tutto.
    pub guest_eof: bool,
    /// Byte che l'host ha messo in coda e il guest non ha ancora preso
    /// (lo stack li prende quando la finestra del guest lo permette).
    pub unsent: usize,
}

/// Lato host di una connessione.
#[derive(Debug)]
pub(crate) struct HostEnd {
    pub flow: Flow,
    /// Aperta con `host_connect`, SYN non ancora mandato.
    pub pending_open: bool,
    pub to_guest: VecDeque<u8>,
    pub from_guest: VecDeque<u8>,
    /// L'host ha chiuso il suo verso: FIN dopo `to_guest`.
    pub shutdown: bool,
    /// L'host ha chiesto l'interruzione (RST al prossimo `poll`).
    pub abort: bool,
    /// FIN del guest arrivato (dopo tutti i suoi dati).
    pub guest_fin: bool,
    pub closed: Option<CloseReason>,
    /// L'host l'ha rilasciata prima della fine: sparisce appena chiusa.
    pub released: bool,
}

/// L'"upstream" delle connessioni dell'host: le code di byte.
#[derive(Debug, Default)]
pub(crate) struct HostSide {
    pub conns: BTreeMap<ConnId, HostEnd>,
    /// Prossima porta effimera da provare.
    pub next_port: u16,
}

impl HostSide {
    pub fn owns(&self, id: ConnId) -> bool {
        self.conns.contains_key(&id)
    }
}

impl Upstream for HostSide {
    fn tcp_open(&mut self, _now: VirtualTime, _id: ConnId, _flow: Flow) {}

    fn tcp_status(&mut self, _now: VirtualTime, _id: ConnId) -> TcpStatus {
        TcpStatus::Connected
    }

    fn tcp_write(&mut self, _now: VirtualTime, id: ConnId, data: &[u8]) -> usize {
        let Some(c) = self.conns.get_mut(&id) else { return data.len() };
        let n = data.len().min(HOST_BUFFER.saturating_sub(c.from_guest.len()));
        c.from_guest.extend(&data[..n]);
        n
    }

    fn tcp_read(&mut self, _now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead {
        let Some(c) = self.conns.get_mut(&id) else { return TcpRead::Reset };
        if c.to_guest.is_empty() {
            return if c.shutdown { TcpRead::Eof } else { TcpRead::WouldBlock };
        }
        let n = buf.len().min(c.to_guest.len());
        for (d, s) in buf.iter_mut().zip(c.to_guest.drain(..n)) {
            *d = s;
        }
        TcpRead::Data(n)
    }

    fn tcp_shutdown(&mut self, _now: VirtualTime, id: ConnId) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.guest_fin = true;
        }
    }

    fn tcp_close(&mut self, _now: VirtualTime, _id: ConnId, _reset: bool) {
        // Il motivo lo copia lo stack dalla connessione (`reap_tcp`).
    }

    fn udp_send(&mut self, _now: VirtualTime, _id: ConnId, _flow: Flow, _data: &[u8]) {}

    fn udp_recv(&mut self, _now: VirtualTime) -> Option<(ConnId, Vec<u8>)> {
        None
    }

    fn udp_close(&mut self, _now: VirtualTime, _id: ConnId) {}
}
