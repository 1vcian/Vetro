//! Il trait `Upstream`: dove finiscono le connessioni del guest.
//!
//! Interfaccia "sans-I/O" a interrogazione: lo stack chiama i metodi quando
//! riceve frame dal guest e durante `Stack::poll`; l'upstream non chiama mai
//! lo stack. Così un upstream asincrono (il relay WebSocket nel browser) può
//! rispondere più tardi: basta restituire `Pending`/`WouldBlock` e lasciare
//! che la piattaforma chiami di nuovo `poll`.

use std::net::Ipv4Addr;

use crate::{ConnId, Flow, VirtualTime};

/// Esito dell'apertura verso la destinazione remota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpStatus {
    /// Ancora in corso: il guest resta in attesa del SYN-ACK.
    Pending,
    /// Aperta: lo stack risponde al guest con SYN-ACK.
    Connected,
    /// Rifiutata: lo stack risponde al guest con RST (connection refused).
    Refused,
}

/// Esito di una lettura dall'upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpRead {
    /// `n` byte copiati nel buffer (`n > 0`).
    Data(usize),
    /// Niente per ora.
    WouldBlock,
    /// Il remoto ha chiuso il suo verso: lo stack manda FIN dopo i dati.
    Eof,
    /// Il remoto ha interrotto la connessione: lo stack manda RST.
    Reset,
}

pub trait Upstream {
    /// Il guest ha mandato un SYN verso `flow.remote`. L'esito arriva da
    /// [`Upstream::tcp_status`].
    fn tcp_open(&mut self, now: VirtualTime, id: ConnId, flow: Flow);

    /// Stato dell'apertura di `id`. Interrogato finché non è più `Pending`.
    fn tcp_status(&mut self, now: VirtualTime, id: ConnId) -> TcpStatus;

    /// Byte dal guest verso il remoto, in ordine. Restituisce quanti ne ha
    /// accettati: i rimanenti restano nel buffer di ricezione dello stack e
    /// riducono la finestra annunciata al guest (controllo di flusso).
    fn tcp_write(&mut self, now: VirtualTime, id: ConnId, data: &[u8]) -> usize;

    /// Byte dal remoto verso il guest. Lo stack legge solo quando ha spazio
    /// nel buffer di trasmissione.
    fn tcp_read(&mut self, now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead;

    /// Il guest ha chiuso il suo verso (FIN) dopo tutti i dati già scritti.
    fn tcp_shutdown(&mut self, now: VirtualTime, id: ConnId);

    /// La connessione è finita: `reset` se è stata interrotta (RST dal guest,
    /// timeout), falso dopo una chiusura ordinata. Ultima chiamata per `id`.
    fn tcp_close(&mut self, now: VirtualTime, id: ConnId, reset: bool);

    /// Datagramma UDP del guest sul flusso `id` (il primo datagramma di un
    /// flusso nuovo arriva con un `id` mai visto).
    fn udp_send(&mut self, now: VirtualTime, id: ConnId, flow: Flow, data: &[u8]);

    /// Prossima risposta UDP da consegnare al guest, sul flusso indicato:
    /// parte da `flow.remote` verso `flow.guest`.
    fn udp_recv(&mut self, now: VirtualTime) -> Option<(ConnId, Vec<u8>)>;

    /// Il flusso UDP `id` è scaduto per inattività.
    fn udp_close(&mut self, now: VirtualTime, id: ConnId);

    /// Echo ICMP verso un indirizzo esterno: vero se va risposto. Il gateway
    /// e il DNS virtuale rispondono sempre, senza chiedere all'upstream.
    fn ping(&mut self, _now: VirtualTime, _dst: Ipv4Addr) -> bool {
        false
    }
}
