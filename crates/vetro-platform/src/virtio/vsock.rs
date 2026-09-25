//! virtio-vsock (virtio v1.2, §5.10): socket stream tra guest e host, per
//! adb nel guest Android (adbd in ascolto su vsock, l'host si collega).
//!
//! Code: 0 = rx (dispositivo → driver), 1 = tx (driver → dispositivo),
//! 2 = eventi. Configurazione: `guest_cid` (u64). Feature offerta:
//! VIRTIO_VSOCK_F_STREAM (solo stream, niente SEQPACKET).
//!
//! L'host sta dentro il dispositivo: CID 2, con un'API per ascoltare
//! ([`VirtioVsock::listen`], [`VirtioVsock::accept`]), collegarsi a una
//! porta del guest ([`VirtioVsock::connect`]), mandare e ricevere byte e
//! chiudere. Nessun I/O esterno e nessun tempo: ogni operazione dell'host
//! diventa pacchetti al prossimo `service`, in un ordine fisso (prima i
//! pacchetti di controllo nell'ordine in cui sono nati, poi i dati delle
//! connessioni in ordine di (porta host, porta guest)); le porte locali
//! dell'host si assegnano in sequenza da [`FIRST_HOST_PORT`]. Lo stesso
//! ingresso dà sempre la stessa sequenza di pacchetti.
//!
//! Protocollo (§5.10.6), come il trasporto dell'host di Linux
//! (net/vmw_vsock/virtio_transport_common.c):
//! - REQUEST verso una porta in ascolto → RESPONSE e la connessione va in
//!   coda di `accept`; verso una porta chiusa → RST;
//! - un pacchetto (non RST) senza connessione, o di tipo non stream → RST;
//! - pacchetti con CID sbagliati (sorgente diverso dal guest, destinazione
//!   diversa dall'host) si scartano, come vhost-vsock;
//! - credito: l'host non manda più di `buf_alloc - (tx_cnt - fwd_cnt)`
//!   byte del guest; annuncia il proprio buffer ([`HOST_BUF_ALLOC`]) e i
//!   byte consumati in ogni pacchetto, e manda CREDIT_UPDATE quando l'app
//!   dell'host consuma dati e il guest vede meno di [`CREDIT_THRESHOLD`]
//!   byte liberi, o su CREDIT_REQUEST;
//! - SHUTDOWN del guest con entrambi i bit → RST e connessione chiusa (i
//!   dati ricevuti restano da leggere); la chiusura dell'host manda
//!   SHUTDOWN dopo gli ultimi dati e aspetta l'RST del guest.
//!
//! Un evento TRANSPORT_RESET ([`VirtioVsock::transport_reset`]) chiude
//! tutte le connessioni: serve dopo il ripristino di uno snapshot (M6).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::*;

pub const F_STREAM: u64 = 1 << 0;
pub const F_SEQPACKET: u64 = 1 << 1;

/// CID dell'host (VMADDR_CID_HOST).
pub const HOST_CID: u64 = 2;
/// CID di default del guest (il primo libero, come `guest-cid=3`).
pub const DEFAULT_GUEST_CID: u64 = 3;
/// Prima porta locale assegnata dall'host a [`VirtioVsock::connect`].
pub const FIRST_HOST_PORT: u32 = 49152;
/// Buffer di ricezione dell'host annunciato al guest (come il default di
/// Linux, 256 KiB).
pub const HOST_BUF_ALLOC: u32 = 256 * 1024;
/// Sotto questo spazio libero visto dal guest l'host manda CREDIT_UPDATE.
pub const CREDIT_THRESHOLD: u32 = 64 * 1024;
/// Carico massimo di un pacchetto (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE).
pub const MAX_PKT: usize = 64 * 1024;

pub const TYPE_STREAM: u16 = 1;
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;
/// Bit di SHUTDOWN: niente più ricezione / trasmissione.
pub const SHUTDOWN_RCV: u32 = 1;
pub const SHUTDOWN_SEND: u32 = 2;
pub const EVENT_TRANSPORT_RESET: u32 = 0;

const RXQ: usize = 0;
const TXQ: usize = 1;
const EVTQ: usize = 2;
pub const HDR_LEN: usize = 44;

/// `struct virtio_vsock_hdr`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub ty: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl Hdr {
    pub fn parse(b: &[u8; HDR_LEN]) -> Self {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Self {
            src_cid: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            dst_cid: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            ty: u16::from_le_bytes([b[28], b[29]]),
            op: u16::from_le_bytes([b[30], b[31]]),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    pub fn to_bytes(self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.ty.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

/// Una connessione, vista dall'host: porta locale dell'host e porta del
/// guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VsockConn {
    pub host_port: u32,
    pub guest_port: u32,
}

/// Stato di una connessione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockState {
    /// L'host ha chiesto la connessione, il guest non ha ancora risposto.
    Connecting,
    Connected,
    /// L'host ha chiuso: SHUTDOWN mandato (o in attesa dei dati), si
    /// aspetta l'RST del guest.
    Closing,
    /// Chiusa: rifiutata dal guest, RST, chiusura completata o reset del
    /// trasporto. I dati già ricevuti restano leggibili.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockError {
    /// Connessione inesistente.
    NotFound,
    /// L'host ha già chiuso il lato di trasmissione, o la connessione è
    /// chiusa.
    Closed,
    /// La porta dell'host è già in ascolto o in uso.
    PortInUse,
}

#[derive(Debug)]
struct Conn {
    state: VsockState,
    /// Credito del guest (dall'ultimo pacchetto ricevuto).
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Byte mandati al guest.
    tx_cnt: u32,
    /// Byte ricevuti dal guest e consumati dall'app dell'host.
    fwd_cnt: u32,
    /// `fwd_cnt` annunciato nell'ultimo pacchetto mandato.
    last_fwd_sent: u32,
    rx_cnt: u32,
    tx_buf: VecDeque<u8>,
    rx_buf: VecDeque<u8>,
    /// Il guest non manderà più dati (SHUTDOWN con SEND).
    peer_eof: bool,
    /// Bit di SHUTDOWN da mandare quando `tx_buf` è vuoto (0 = nessuno).
    shutdown_pending: u32,
    /// L'host ha chiuso il suo lato di trasmissione.
    send_closed: bool,
    credit_update: bool,
}

impl Conn {
    fn new(state: VsockState) -> Self {
        Self {
            state,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            fwd_cnt: 0,
            last_fwd_sent: 0,
            rx_cnt: 0,
            tx_buf: VecDeque::new(),
            rx_buf: VecDeque::new(),
            peer_eof: false,
            shutdown_pending: 0,
            send_closed: false,
            credit_update: false,
        }
    }

    /// Byte che il guest può ancora ricevere.
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }
}

pub struct VirtioVsock {
    guest_cid: u64,
    listening: BTreeSet<u32>,
    conns: BTreeMap<VsockConn, Conn>,
    /// Connessioni aperte dal guest, per porta in ascolto, non ancora
    /// accettate dall'host.
    backlog: BTreeMap<u32, VecDeque<VsockConn>>,
    /// Pacchetti di controllo (senza dati) per il guest, in ordine.
    control: VecDeque<Hdr>,
    next_port: u32,
    reset_event: bool,
    /// Pacchetti del guest scartati (CID sbagliati, lunghezze invalide).
    dropped: u64,
    queue_sizes: [u16; 3],
}

impl VirtioVsock {
    /// Code da 128 come vhost-vsock.
    pub fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            listening: BTreeSet::new(),
            conns: BTreeMap::new(),
            backlog: BTreeMap::new(),
            control: VecDeque::new(),
            next_port: FIRST_HOST_PORT,
            reset_event: false,
            dropped: 0,
            queue_sizes: [128, 128, 128],
        }
    }

    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Pacchetti del guest scartati.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// L'host ascolta sulla porta `port`: le REQUEST del guest verso di
    /// essa vengono accettate.
    pub fn listen(&mut self, port: u32) -> Result<(), VsockError> {
        if !self.listening.insert(port) {
            return Err(VsockError::PortInUse);
        }
        Ok(())
    }

    /// Smette di ascoltare; le connessioni in attesa di `accept` si
    /// chiudono con RST.
    pub fn unlisten(&mut self, port: u32) {
        self.listening.remove(&port);
        for c in self.backlog.remove(&port).unwrap_or_default() {
            self.reset(c);
        }
    }

    /// Prossima connessione del guest verso la porta `port`, se c'è.
    pub fn accept(&mut self, port: u32) -> Option<VsockConn> {
        self.backlog.get_mut(&port)?.pop_front()
    }

    fn host_port_free(&self, p: u32) -> bool {
        !self.listening.contains(&p) && !self.conns.keys().any(|c| c.host_port == p)
    }

    /// Chiede una connessione alla porta `guest_port` del guest, da una
    /// porta locale nuova. Parte al prossimo `service`; lo stato dice se il
    /// guest l'ha accettata. I dati mandati prima partono dopo la RESPONSE.
    pub fn connect(&mut self, guest_port: u32) -> VsockConn {
        let mut p = self.next_port;
        while !self.host_port_free(p) {
            p = p.checked_add(1).unwrap_or(FIRST_HOST_PORT);
        }
        self.next_port = p.checked_add(1).unwrap_or(FIRST_HOST_PORT);
        let c = VsockConn { host_port: p, guest_port };
        self.conns.insert(c, Conn::new(VsockState::Connecting));
        let h = self.hdr(c, OP_REQUEST, 0);
        self.control.push_back(h);
        c
    }

    pub fn state(&self, c: VsockConn) -> Option<VsockState> {
        self.conns.get(&c).map(|k| k.state)
    }

    /// Connessioni note (anche chiuse, finché non si chiama `release`).
    pub fn connections(&self) -> Vec<VsockConn> {
        self.conns.keys().copied().collect()
    }

    /// Accoda `data` per il guest; parte quando il guest ha credito.
    pub fn send(&mut self, c: VsockConn, data: &[u8]) -> Result<(), VsockError> {
        let k = self.conns.get_mut(&c).ok_or(VsockError::NotFound)?;
        if k.send_closed || matches!(k.state, VsockState::Closing | VsockState::Closed) {
            return Err(VsockError::Closed);
        }
        k.tx_buf.extend(data);
        Ok(())
    }

    /// Byte dell'host non ancora mandati al guest.
    pub fn unsent(&self, c: VsockConn) -> usize {
        self.conns.get(&c).map_or(0, |k| k.tx_buf.len())
    }

    /// Byte ricevuti dal guest e non ancora letti.
    pub fn available(&self, c: VsockConn) -> usize {
        self.conns.get(&c).map_or(0, |k| k.rx_buf.len())
    }

    /// Legge fino a `max` byte ricevuti dal guest. Libera credito: se il
    /// guest ne vede poco, parte un CREDIT_UPDATE.
    pub fn recv(&mut self, c: VsockConn, max: usize) -> Vec<u8> {
        let Some(k) = self.conns.get_mut(&c) else { return Vec::new() };
        let n = max.min(k.rx_buf.len());
        let out: Vec<u8> = k.rx_buf.drain(..n).collect();
        k.fwd_cnt = k.fwd_cnt.wrapping_add(n as u32);
        let seen_free = HOST_BUF_ALLOC.saturating_sub(k.rx_cnt.wrapping_sub(k.last_fwd_sent));
        if n > 0 && seen_free < CREDIT_THRESHOLD && k.state == VsockState::Connected {
            k.credit_update = true;
        }
        out
    }

    /// Il guest ha chiuso il suo lato di trasmissione (o la connessione) e
    /// non ci sono più dati da leggere.
    pub fn eof(&self, c: VsockConn) -> bool {
        self.conns
            .get(&c)
            .is_none_or(|k| k.rx_buf.is_empty() && (k.peer_eof || k.state == VsockState::Closed))
    }

    /// Chiude il lato di trasmissione dell'host (`shutdown(SHUT_WR)`): dopo
    /// gli ultimi dati parte SHUTDOWN con SEND, il guest legge la fine del
    /// flusso e può ancora mandare dati.
    /// Si può chiamare anche prima che il guest accetti la connessione.
    pub fn shutdown_send(&mut self, c: VsockConn) {
        if let Some(k) = self.conns.get_mut(&c)
            && matches!(k.state, VsockState::Connecting | VsockState::Connected)
            && !k.send_closed
        {
            k.send_closed = true;
            k.shutdown_pending = SHUTDOWN_SEND;
        }
    }

    /// Chiusura ordinata dall'host: SHUTDOWN (ricezione e trasmissione)
    /// dopo gli ultimi dati, poi si aspetta l'RST del guest. Su una
    /// connessione non ancora accettata, la chiusura parte dopo la RESPONSE.
    pub fn close(&mut self, c: VsockConn) {
        if let Some(k) = self.conns.get_mut(&c)
            && matches!(k.state, VsockState::Connecting | VsockState::Connected)
        {
            k.send_closed = true;
            k.shutdown_pending = SHUTDOWN_RCV | SHUTDOWN_SEND;
            if k.state == VsockState::Connected {
                k.state = VsockState::Closing;
            }
        }
    }

    /// Chiusura immediata con RST.
    pub fn reset(&mut self, c: VsockConn) {
        if let Some(k) = self.conns.get_mut(&c)
            && k.state != VsockState::Closed
        {
            k.state = VsockState::Closed;
            k.tx_buf.clear();
            let h = self.hdr(c, OP_RST, 0);
            self.control.push_back(h);
        }
    }

    /// Dimentica una connessione chiusa (o la chiude con RST).
    pub fn release(&mut self, c: VsockConn) {
        self.reset(c);
        self.conns.remove(&c);
        for q in self.backlog.values_mut() {
            q.retain(|&b| b != c);
        }
    }

    /// Manda al guest VIRTIO_VSOCK_EVENT_TRANSPORT_RESET e chiude ogni
    /// connessione (il guest le considera perse, senza RST).
    pub fn transport_reset(&mut self) {
        self.reset_event = true;
        self.drop_all();
    }

    fn drop_all(&mut self) {
        for k in self.conns.values_mut() {
            k.state = VsockState::Closed;
            k.tx_buf.clear();
        }
        self.backlog.clear();
        self.control.clear();
    }

    /// Intestazione di un pacchetto dell'host per `c`, con il credito.
    fn hdr(&mut self, c: VsockConn, op: u16, len: u32) -> Hdr {
        let fwd_cnt = self.conns.get(&c).map_or(0, |k| k.fwd_cnt);
        Hdr {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: c.host_port,
            dst_port: c.guest_port,
            len,
            ty: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: HOST_BUF_ALLOC,
            fwd_cnt,
        }
    }

    /// RST in risposta a un pacchetto senza connessione (porte scambiate).
    fn rst_reply(&mut self, h: &Hdr) {
        self.control.push_back(Hdr {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: h.dst_port,
            dst_port: h.src_port,
            len: 0,
            ty: TYPE_STREAM,
            op: OP_RST,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        });
    }

    /// Un pacchetto dal guest.
    fn receive(&mut self, h: Hdr, payload: Vec<u8>) {
        if h.src_cid != self.guest_cid || h.dst_cid != HOST_CID {
            self.dropped += 1;
            return;
        }
        if h.ty != TYPE_STREAM {
            if h.op != OP_RST {
                self.rst_reply(&h);
            }
            return;
        }
        let c = VsockConn { host_port: h.dst_port, guest_port: h.src_port };
        let Some(k) = self.conns.get_mut(&c).filter(|k| k.state != VsockState::Closed) else {
            // Senza connessione aperta: solo una REQUEST verso una porta in
            // ascolto la crea.
            if h.op == OP_REQUEST && self.listening.contains(&h.dst_port) {
                let mut k = Conn::new(VsockState::Connected);
                k.peer_buf_alloc = h.buf_alloc;
                k.peer_fwd_cnt = h.fwd_cnt;
                self.conns.insert(c, k);
                self.backlog.entry(h.dst_port).or_default().push_back(c);
                let r = self.hdr(c, OP_RESPONSE, 0);
                self.control.push_back(r);
            } else if h.op != OP_RST {
                self.rst_reply(&h);
            }
            return;
        };
        k.peer_buf_alloc = h.buf_alloc;
        k.peer_fwd_cnt = h.fwd_cnt;
        match (k.state, h.op) {
            (_, OP_RST) => k.state = VsockState::Closed,
            (VsockState::Connecting, OP_RESPONSE) => {
                let closing = k.shutdown_pending == SHUTDOWN_RCV | SHUTDOWN_SEND;
                k.state = if closing { VsockState::Closing } else { VsockState::Connected };
            }
            (VsockState::Connected | VsockState::Closing, OP_RW) => {
                k.rx_cnt = k.rx_cnt.wrapping_add(payload.len() as u32);
                k.rx_buf.extend(payload);
            }
            (VsockState::Connected | VsockState::Closing, OP_CREDIT_UPDATE) => {}
            (VsockState::Connected | VsockState::Closing, OP_CREDIT_REQUEST) => k.credit_update = true,
            (VsockState::Connected | VsockState::Closing, OP_SHUTDOWN) => {
                if h.flags & SHUTDOWN_SEND != 0 {
                    k.peer_eof = true;
                }
                // Chiusura completa del guest, o risposta alla nostra: RST.
                if h.flags & (SHUTDOWN_RCV | SHUTDOWN_SEND) == SHUTDOWN_RCV | SHUTDOWN_SEND
                    || k.state == VsockState::Closing
                {
                    self.reset(c);
                }
            }
            // Qualunque altra cosa (REQUEST su una connessione aperta,
            // RESPONSE fuori posto, op sconosciuto): RST.
            _ => self.reset(c),
        }
    }

    /// Prossimo pacchetto con dati o chiusura per il guest, che entra in
    /// `room` byte di carico: (intestazione, dati).
    fn next_data(&mut self, room: usize) -> Option<(Hdr, Vec<u8>)> {
        let keys: Vec<VsockConn> = self.conns.keys().copied().collect();
        for c in keys {
            let k = self.conns.get_mut(&c).unwrap();
            let open = matches!(k.state, VsockState::Connected | VsockState::Closing);
            if open && !k.tx_buf.is_empty() && k.peer_credit() > 0 && room > 0 {
                let n = k.tx_buf.len().min(k.peer_credit() as usize).min(room).min(MAX_PKT);
                let data: Vec<u8> = k.tx_buf.drain(..n).collect();
                k.tx_cnt = k.tx_cnt.wrapping_add(n as u32);
                let h = self.hdr(c, OP_RW, n as u32);
                return Some((h, data));
            }
            if open && k.tx_buf.is_empty() && k.shutdown_pending != 0 {
                let flags = core::mem::take(&mut k.shutdown_pending);
                let mut h = self.hdr(c, OP_SHUTDOWN, 0);
                h.flags = flags;
                return Some((h, Vec::new()));
            }
            if k.credit_update && k.state == VsockState::Connected {
                k.credit_update = false;
                let h = self.hdr(c, OP_CREDIT_UPDATE, 0);
                return Some((h, Vec::new()));
            }
        }
        None
    }

    fn transmit_queue(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        while let Some(c) = q.pop(ram)? {
            let mut b = [0u8; HDR_LEN];
            let n = c.read(ram, 0, &mut b)?;
            let h = Hdr::parse(&b);
            let len = h.len as usize;
            if n < HDR_LEN || len > MAX_PKT || c.readable_len() < (HDR_LEN + len) as u64 {
                self.dropped += 1;
            } else {
                let mut payload = vec![0u8; len];
                c.read(ram, HDR_LEN as u64, &mut payload)?;
                self.receive(h, payload);
            }
            q.push_used(ram, c.head, 0)?;
        }
        Ok(())
    }

    fn receive_queue(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        while q.available(ram)? > 0 {
            let c = q.pop(ram)?.expect("contata da available");
            let room = c.writable_len();
            if room < HDR_LEN as u64 {
                return Err(QueueError::Malformed("buffer vsock più corto dell'intestazione"));
            }
            let room = (room - HDR_LEN as u64).min(MAX_PKT as u64) as usize;
            let pkt = match self.control.pop_front() {
                Some(h) => Some((h, Vec::new())),
                None => self.next_data(room),
            };
            let Some((h, data)) = pkt else {
                q.rewind(ram, 1)?;
                break;
            };
            if let Some(k) = self.conns.get_mut(&VsockConn { host_port: h.src_port, guest_port: h.dst_port })
            {
                k.last_fwd_sent = h.fwd_cnt;
            }
            let mut buf = h.to_bytes().to_vec();
            buf.extend_from_slice(&data);
            let n = c.write(ram, 0, &buf)?;
            q.push_used(ram, c.head, n as u32)?;
        }
        Ok(())
    }

    fn event_queue(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        if !self.reset_event {
            return Ok(());
        }
        if let Some(c) = q.pop(ram)? {
            let n = c.write(ram, 0, &EVENT_TRANSPORT_RESET.to_le_bytes())?;
            q.push_used(ram, c.head, n as u32)?;
            self.reset_event = false;
        }
        Ok(())
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_id(&self) -> u32 {
        ID_VSOCK
    }

    fn features(&self) -> u64 {
        F_STREAM
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.guest_cid.to_le_bytes(), offset, data);
    }

    fn reset(&mut self) {
        self.drop_all();
        self.reset_event = false;
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        let (queues, ram) = (&mut *ctx.queues, &mut *ctx.ram);
        self.event_queue(&mut queues[EVTQ], ram)?;
        self.transmit_queue(&mut queues[TXQ], ram)?;
        self.receive_queue(&mut queues[RXQ], ram)
    }

    /// Porte in ascolto, connessioni (stato, crediti, contatori, dati in
    /// transito nei due versi), backlog, pacchetti di controllo in attesa,
    /// prossima porta dell'host, evento di reset, contatore. Il CID è
    /// configurazione.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.guest_cid);
        w.seq(&self.listening, |w, &p| w.u32(p));
        w.seq(&self.conns, |w, (k, c)| {
            w.u32(k.host_port);
            w.u32(k.guest_port);
            w.u8(match c.state {
                VsockState::Connecting => 0,
                VsockState::Connected => 1,
                VsockState::Closing => 2,
                VsockState::Closed => 3,
            });
            for v in [c.peer_buf_alloc, c.peer_fwd_cnt, c.tx_cnt, c.fwd_cnt, c.last_fwd_sent, c.rx_cnt] {
                w.u32(v);
            }
            w.seq(&c.tx_buf, |w, &b| w.u8(b));
            w.seq(&c.rx_buf, |w, &b| w.u8(b));
            w.bool(c.peer_eof);
            w.u32(c.shutdown_pending);
            w.bool(c.send_closed);
            w.bool(c.credit_update);
        });
        w.seq(&self.backlog, |w, (&port, q)| {
            w.u32(port);
            w.seq(q, |w, k| {
                w.u32(k.host_port);
                w.u32(k.guest_port);
            });
        });
        w.seq(&self.control, |w, h| w.raw(&h.to_bytes()));
        w.u32(self.next_port);
        w.bool(self.reset_event);
        w.u64(self.dropped);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("CID del guest", self.guest_cid)?;
        let conn =
            |r: &mut vetro_snapshot::Reader<'_>| Ok(VsockConn { host_port: r.u32()?, guest_port: r.u32()? });
        self.listening = r.seq(4, |r| r.u32())?.into_iter().collect();
        let n = r.len_of(8)?;
        self.conns.clear();
        for _ in 0..n {
            let k = conn(r)?;
            let state = match r.u8()? {
                0 => VsockState::Connecting,
                1 => VsockState::Connected,
                2 => VsockState::Closing,
                3 => VsockState::Closed,
                v => return Err(vetro_snapshot::Error::invalid(format!("stato vsock {v}"))),
            };
            let mut c = Conn::new(state);
            for v in [
                &mut c.peer_buf_alloc,
                &mut c.peer_fwd_cnt,
                &mut c.tx_cnt,
                &mut c.fwd_cnt,
                &mut c.last_fwd_sent,
                &mut c.rx_cnt,
            ] {
                *v = r.u32()?;
            }
            c.tx_buf = r.seq(1, |r| r.u8())?.into();
            c.rx_buf = r.seq(1, |r| r.u8())?.into();
            c.peer_eof = r.bool()?;
            c.shutdown_pending = r.u32()?;
            c.send_closed = r.bool()?;
            c.credit_update = r.bool()?;
            self.conns.insert(k, c);
        }
        let n = r.len_of(12)?;
        self.backlog.clear();
        for _ in 0..n {
            let port = r.u32()?;
            let q = r.seq(8, conn)?;
            self.backlog.insert(port, q.into());
        }
        self.control =
            r.seq(HDR_LEN, |r| Ok(Hdr::parse(r.raw(HDR_LEN)?.try_into().expect("44 byte"))))?.into();
        self.next_port = r.u32()?;
        self.reset_event = r.bool()?;
        self.dropped = r.u64()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
