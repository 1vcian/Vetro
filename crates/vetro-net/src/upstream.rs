//! The `Upstream` trait: where the guest's connections end up.
//!
//! Polling "sans-I/O" interface: the stack calls the methods when it
//! receives frames from the guest and during `Stack::poll`; the upstream never calls
//! the stack. So an asynchronous upstream (the WebSocket relay in the browser) can
//! answer later: it just returns `Pending`/`WouldBlock` and lets
//! the platform call `poll` again.

use std::net::Ipv4Addr;

use crate::{ConnId, Flow, VirtualTime};

/// Outcome of opening towards the remote destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpStatus {
    /// Still in progress: the guest keeps waiting for the SYN-ACK.
    Pending,
    /// Open: the stack answers the guest with SYN-ACK.
    Connected,
    /// Refused: the stack answers the guest with RST (connection refused).
    Refused,
}

/// Outcome of a read from the upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpRead {
    /// `n` bytes copied into the buffer (`n > 0`).
    Data(usize),
    /// Nothing for now.
    WouldBlock,
    /// The remote has closed its direction: the stack sends FIN after the data.
    Eof,
    /// The remote has aborted the connection: the stack sends RST.
    Reset,
}

pub trait Upstream {
    /// The guest sent a SYN to `flow.remote`. The outcome comes from
    /// [`Upstream::tcp_status`].
    fn tcp_open(&mut self, now: VirtualTime, id: ConnId, flow: Flow);

    /// State of the opening of `id`. Polled until it is no longer `Pending`.
    fn tcp_status(&mut self, now: VirtualTime, id: ConnId) -> TcpStatus;

    /// Bytes from the guest to the remote, in order. Returns how many it
    /// accepted: the remaining ones stay in the stack's receive buffer and
    /// reduce the window announced to the guest (flow control).
    fn tcp_write(&mut self, now: VirtualTime, id: ConnId, data: &[u8]) -> usize;

    /// Bytes from the remote to the guest. The stack reads only when it has room
    /// in the transmit buffer.
    fn tcp_read(&mut self, now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead;

    /// The guest has closed its direction (FIN) after all the data already written.
    fn tcp_shutdown(&mut self, now: VirtualTime, id: ConnId);

    /// The connection is over: `reset` if it was aborted (RST from the guest,
    /// timeout), false after an orderly close. Last call for `id`.
    fn tcp_close(&mut self, now: VirtualTime, id: ConnId, reset: bool);

    /// UDP datagram from the guest on flow `id` (the first datagram of a
    /// new flow arrives with an `id` never seen).
    fn udp_send(&mut self, now: VirtualTime, id: ConnId, flow: Flow, data: &[u8]);

    /// Next UDP response to deliver to the guest, on the indicated flow:
    /// it leaves from `flow.remote` to `flow.guest`.
    fn udp_recv(&mut self, now: VirtualTime) -> Option<(ConnId, Vec<u8>)>;

    /// UDP flow `id` has expired for inactivity.
    fn udp_close(&mut self, now: VirtualTime, id: ConnId);

    /// ICMP echo to an external address: true if it must be answered. The gateway
    /// and the virtual DNS always answer, without asking the upstream.
    fn ping(&mut self, _now: VirtualTime, _dst: Ipv4Addr) -> bool {
        false
    }
}
