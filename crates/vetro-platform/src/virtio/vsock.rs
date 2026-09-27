//! virtio-vsock (virtio v1.2, §5.10): stream sockets between guest and host, for
//! adb in the Android guest (adbd listening on vsock, the host connects).
//!
//! Queues: 0 = rx (device → driver), 1 = tx (driver → device),
//! 2 = events. Configuration: `guest_cid` (u64). Feature offered:
//! VIRTIO_VSOCK_F_STREAM (stream only, no SEQPACKET).
//!
//! The host lives inside the device: CID 2, with an API to listen
//! ([`VirtioVsock::listen`], [`VirtioVsock::accept`]), connect to a
//! guest port ([`VirtioVsock::connect`]), send and receive bytes and
//! close. No external I/O and no time: every host operation
//! becomes packets at the next `service`, in a fixed order (first the
//! control packets in the order they were created, then the data of the
//! connections in order of (host port, guest port)); the host's local
//! ports are assigned in sequence from [`FIRST_HOST_PORT`]. The same
//! input always gives the same sequence of packets.
//!
//! Protocol (§5.10.6), like Linux's host transport
//! (net/vmw_vsock/virtio_transport_common.c):
//! - REQUEST to a listening port → RESPONSE and the connection goes into
//!   the `accept` queue; to a closed port → RST;
//! - a packet (not RST) without a connection, or of a non-stream type → RST;
//! - packets with wrong CIDs (source other than the guest, destination
//!   other than the host) are discarded, like vhost-vsock;
//! - credit: the host doesn't send more than `buf_alloc - (tx_cnt - fwd_cnt)`
//!   bytes to the guest; it announces its own buffer ([`HOST_BUF_ALLOC`]) and the
//!   bytes consumed in every packet, and sends CREDIT_UPDATE when the host
//!   app consumes data and the guest sees less than [`CREDIT_THRESHOLD`]
//!   free bytes, or on CREDIT_REQUEST;
//! - guest SHUTDOWN with both bits → RST and connection closed (the
//!   received data remains to be read); the host's close sends
//!   SHUTDOWN after the last data and waits for the guest's RST.
//!
//! A TRANSPORT_RESET event ([`VirtioVsock::transport_reset`]) closes
//! all connections: needed after restoring a snapshot (M6).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::*;

pub const F_STREAM: u64 = 1 << 0;
pub const F_SEQPACKET: u64 = 1 << 1;

/// Host CID (VMADDR_CID_HOST).
pub const HOST_CID: u64 = 2;
/// Default guest CID (the first free one, like `guest-cid=3`).
pub const DEFAULT_GUEST_CID: u64 = 3;
/// First local port assigned by the host to [`VirtioVsock::connect`].
pub const FIRST_HOST_PORT: u32 = 49152;
/// Host receive buffer announced to the guest (like Linux's
/// default, 256 KiB).
pub const HOST_BUF_ALLOC: u32 = 256 * 1024;
/// Below this free space seen by the guest the host sends CREDIT_UPDATE.
pub const CREDIT_THRESHOLD: u32 = 64 * 1024;
/// Maximum payload of a packet (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE).
pub const MAX_PKT: usize = 64 * 1024;

pub const TYPE_STREAM: u16 = 1;
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;
/// SHUTDOWN bits: no more receiving / transmitting.
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

/// A connection, as seen by the host: host local port and guest
/// port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VsockConn {
    pub host_port: u32,
    pub guest_port: u32,
}

/// State of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockState {
    /// The host asked for the connection, the guest hasn't answered yet.
    Connecting,
    Connected,
    /// The host has closed: SHUTDOWN sent (or waiting for the data), waiting
    /// for the guest's RST.
    Closing,
    /// Closed: refused by the guest, RST, close completed or transport
    /// reset. Data already received remains readable.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockError {
    /// Connessione inesistente.
    NotFound,
    /// The host has already closed the transmit side, or the connection is
    /// closed.
    Closed,
    /// The host port is already listening or in use.
    PortInUse,
}

#[derive(Debug)]
struct Conn {
    state: VsockState,
    /// Guest credit (from the last packet received).
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes sent to the guest.
    tx_cnt: u32,
    /// Bytes received from the guest and consumed by the host app.
    fwd_cnt: u32,
    /// `fwd_cnt` announced in the last packet sent.
    last_fwd_sent: u32,
    rx_cnt: u32,
    tx_buf: VecDeque<u8>,
    rx_buf: VecDeque<u8>,
    /// The guest will send no more data (SHUTDOWN with SEND).
    peer_eof: bool,
    /// SHUTDOWN bits to send when `tx_buf` is empty (0 = none).
    shutdown_pending: u32,
    /// The host has closed its transmit side.
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

    /// Bytes the guest can still receive.
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }
}

pub struct VirtioVsock {
    guest_cid: u64,
    listening: BTreeSet<u32>,
    conns: BTreeMap<VsockConn, Conn>,
    /// Connections opened by the guest, per listening port, not yet
    /// accepted by the host.
    backlog: BTreeMap<u32, VecDeque<VsockConn>>,
    /// Control packets (without data) for the guest, in order.
    control: VecDeque<Hdr>,
    next_port: u32,
    reset_event: bool,
    /// Guest packets discarded (wrong CIDs, invalid lengths).
    dropped: u64,
    queue_sizes: [u16; 3],
}

impl VirtioVsock {
    /// 128-entry queues like vhost-vsock.
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

    /// Guest packets discarded.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The host listens on port `port`: the guest's REQUESTs to
    /// it are accepted.
    pub fn listen(&mut self, port: u32) -> Result<(), VsockError> {
        if !self.listening.insert(port) {
            return Err(VsockError::PortInUse);
        }
        Ok(())
    }

    /// Stops listening; the connections waiting for `accept` are
    /// closed with RST.
    pub fn unlisten(&mut self, port: u32) {
        self.listening.remove(&port);
        for c in self.backlog.remove(&port).unwrap_or_default() {
            self.reset(c);
        }
    }

    /// Next guest connection to port `port`, if any.
    pub fn accept(&mut self, port: u32) -> Option<VsockConn> {
        self.backlog.get_mut(&port)?.pop_front()
    }

    fn host_port_free(&self, p: u32) -> bool {
        !self.listening.contains(&p) && !self.conns.keys().any(|c| c.host_port == p)
    }

    /// Asks for a connection to the guest's port `guest_port`, from a
    /// new local port. It leaves at the next `service`; the state says whether the
    /// guest accepted it. Data sent before leaves after the RESPONSE.
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

    /// Known connections (closed ones too, until `release` is called).
    pub fn connections(&self) -> Vec<VsockConn> {
        self.conns.keys().copied().collect()
    }

    /// Queues `data` for the guest; it leaves when the guest has credit.
    pub fn send(&mut self, c: VsockConn, data: &[u8]) -> Result<(), VsockError> {
        let k = self.conns.get_mut(&c).ok_or(VsockError::NotFound)?;
        if k.send_closed || matches!(k.state, VsockState::Closing | VsockState::Closed) {
            return Err(VsockError::Closed);
        }
        k.tx_buf.extend(data);
        Ok(())
    }

    /// Host bytes not yet sent to the guest.
    pub fn unsent(&self, c: VsockConn) -> usize {
        self.conns.get(&c).map_or(0, |k| k.tx_buf.len())
    }

    /// Bytes received from the guest and not read yet.
    pub fn available(&self, c: VsockConn) -> usize {
        self.conns.get(&c).map_or(0, |k| k.rx_buf.len())
    }

    /// Reads up to `max` bytes received from the guest. Frees credit: if the
    /// guest sees little of it, a CREDIT_UPDATE leaves.
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

    /// The guest has closed its transmit side (or the connection) and
    /// there is no more data to read.
    pub fn eof(&self, c: VsockConn) -> bool {
        self.conns
            .get(&c)
            .is_none_or(|k| k.rx_buf.is_empty() && (k.peer_eof || k.state == VsockState::Closed))
    }

    /// Closes the host's transmit side (`shutdown(SHUT_WR)`): after
    /// the last data SHUTDOWN with SEND leaves, the guest reads the end of the
    /// stream and can still send data.
    /// It can be called even before the guest accepts the connection.
    pub fn shutdown_send(&mut self, c: VsockConn) {
        if let Some(k) = self.conns.get_mut(&c)
            && matches!(k.state, VsockState::Connecting | VsockState::Connected)
            && !k.send_closed
        {
            k.send_closed = true;
            k.shutdown_pending = SHUTDOWN_SEND;
        }
    }

    /// Orderly close from the host: SHUTDOWN (receive and transmit)
    /// after the last data, then waits for the guest's RST. On a
    /// connection not yet accepted, the close leaves after the RESPONSE.
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

    /// Immediate close with RST.
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

    /// Forgets a closed connection (or closes it with RST).
    pub fn release(&mut self, c: VsockConn) {
        self.reset(c);
        self.conns.remove(&c);
        for q in self.backlog.values_mut() {
            q.retain(|&b| b != c);
        }
    }

    /// Sends VIRTIO_VSOCK_EVENT_TRANSPORT_RESET to the guest and closes every
    /// connection (the guest considers them lost, without RST).
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

    /// Header of a host packet for `c`, with the credit.
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

    /// RST in answer to a packet without a connection (ports swapped).
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

    /// A packet from the guest.
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
            // Without an open connection: only a REQUEST to a listening
            // port creates it.
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
                // Complete close from the guest, or answer to ours: RST.
                if h.flags & (SHUTDOWN_RCV | SHUTDOWN_SEND) == SHUTDOWN_RCV | SHUTDOWN_SEND
                    || k.state == VsockState::Closing
                {
                    self.reset(c);
                }
            }
            // Anything else (REQUEST on an open connection,
            // misplaced RESPONSE, unknown op): RST.
            _ => self.reset(c),
        }
    }

    /// Next packet with data or close for the guest, that fits in
    /// `room` bytes of payload: (header, data).
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
            let c = q.pop(ram)?.expect("counted from available");
            let room = c.writable_len();
            if room < HDR_LEN as u64 {
                return Err(QueueError::Malformed("vsock buffer shorter than the header"));
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

    /// Listening ports, connections (state, credits, counters, data in
    /// transit in both directions), backlog, pending control packets,
    /// next host port, reset event, counter. The CID is
    /// configuration.
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
        r.expect_u64("guest CID", self.guest_cid)?;
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
                v => return Err(vetro_snapshot::Error::invalid(format!("vsock state {v}"))),
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
