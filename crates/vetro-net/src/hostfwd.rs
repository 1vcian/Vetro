//! Port forwarding: TCP connections opened by the host to a service of the
//! guest (like the `hostfwd` of QEMU's user network).
//!
//! The stack acts as a TCP client towards the guest: the SYN leaves from the gateway
//! (10.0.2.2, as slirp translates connections from localhost) from an
//! ephemeral port chosen deterministically. From the SYN-ACK on the connection
//! is the same state machine as the guest's connections (`tcp.rs`); in
//! place of the upstream there is [`HostSide`], which keeps the bytes queued in both
//! directions. The host (the platform: `vetro boot --hostfwd`, the browser) queues
//! bytes and reads them with the `Stack::host_*` methods: no real socket
//! in the core, everything synchronous and deterministic. The host's actions take
//! effect at the next `Stack::poll`.

use std::collections::{BTreeMap, VecDeque};

use crate::events::CloseReason;
use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, VirtualTime};

/// Maximum bytes queued in each direction per connection: beyond that,
/// `Stack::host_send` accepts fewer bytes (backpressure towards the host) and the
/// guest sees the window close until the host reads.
pub const HOST_BUFFER: usize = 256 * 1024;

/// First ephemeral port of the gateway for the host's connections (Linux's
/// start from 32768; here the IANA range 49152..=65535).
pub const FIRST_EPHEMERAL_PORT: u16 = 49152;

/// State of a connection opened by the host to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostConnState {
    /// SYN sent (or to be sent at the next `poll`), no answer.
    Connecting,
    /// Handshake completed: the bytes flow (also during the close).
    Open,
    /// Finished: `Normal` after FIN in both directions, `Refused` if the guest
    /// answered RST to the SYN (nobody listening), `GuestReset` for an RST from the
    /// guest, `RemoteReset` after `Stack::host_abort`, `Timeout` if the guest
    /// doesn't answer.
    Closed(CloseReason),
}

/// What the host sees of one of its connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostConnInfo {
    pub state: HostConnState,
    pub flow: Flow,
    /// Guest bytes ready for `Stack::host_recv`.
    pub readable: usize,
    /// Spazio per `Stack::host_send`.
    pub writable: usize,
    /// The guest has closed its direction (FIN) and the host has read everything.
    pub guest_eof: bool,
    /// Bytes the host has queued and the guest hasn't taken yet
    /// (the stack takes them when the guest's window allows it).
    pub unsent: usize,
}

/// Host side of a connection.
#[derive(Debug)]
pub(crate) struct HostEnd {
    pub flow: Flow,
    /// Opened with `host_connect`, SYN not sent yet.
    pub pending_open: bool,
    pub to_guest: VecDeque<u8>,
    pub from_guest: VecDeque<u8>,
    /// The host has closed its direction: FIN after `to_guest`.
    pub shutdown: bool,
    /// The host has requested the abort (RST at the next `poll`).
    pub abort: bool,
    /// Guest FIN arrived (after all its data).
    pub guest_fin: bool,
    pub closed: Option<CloseReason>,
    /// The host released it before the end: it disappears as soon as it is closed.
    pub released: bool,
}

/// The "upstream" of the host's connections: the byte queues.
#[derive(Debug, Default)]
pub(crate) struct HostSide {
    pub conns: BTreeMap<ConnId, HostEnd>,
    /// Next ephemeral port to try.
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
        // The stack copies the reason from the connection (`reap_tcp`).
    }

    fn udp_send(&mut self, _now: VirtualTime, _id: ConnId, _flow: Flow, _data: &[u8]) {}

    fn udp_recv(&mut self, _now: VirtualTime) -> Option<(ConnId, Vec<u8>)> {
        None
    }

    fn udp_close(&mut self, _now: VirtualTime, _id: ConnId) {}
}
