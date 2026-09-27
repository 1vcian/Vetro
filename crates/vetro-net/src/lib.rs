//! Vetro's host-side network stack: virtual gateway, TCP/UDP terminated
//! on the host side (like slirp), sinkhole and interface to the relay.
//!
//! The guest (Linux) exchanges Ethernet frames with the virtio-net device;
//! [`Stack`] sits at the other end of the cable. It answers ARP, DHCP and ICMP like
//! QEMU's "user" network (guest 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3),
//! terminates every TCP connection and every UDP flow of the guest and delivers the
//! data to an [`Upstream`]: the [`Sinkhole`] (all fake and recorded) or a
//! [`RelayUpstream`] (to M7's WebSocket relay). In the opposite direction,
//! [`Stack::host_connect`] opens connections from the host to the guest's TCP
//! services (port forwarding like QEMU's `hostfwd`: the basis for adb).
//!
//! Determinism: no host clock and no unseeded randomness.
//! Time arrives as a parameter ([`VirtualTime`]), the initial sequence
//! numbers derive from `NetConfig::seed`, the tables are `BTreeMap`s.
//! The same calls with the same arguments produce the same frames and the same
//! event log. Interface and invariants in `docs/specs/net.md`,
//! choices in `docs/adr/0007-network-stack-without-smoltcp.md`.

pub mod dhcp;
pub mod dns;
pub mod events;
pub mod hostfwd;
pub mod relay;
pub mod sinkhole;
mod stack;
mod tcp;
pub mod upstream;
pub mod wire;

use std::net::SocketAddrV4;

pub use events::{CloseReason, DhcpMessage, Direction, EventKind, NetEvent};
pub use hostfwd::{HostConnInfo, HostConnState};
pub use relay::{MemoryRelay, Relay, RelayMessage, RelayUpstream};
pub use sinkhole::{Sinkhole, SinkholeConfig, TcpReply};
pub use stack::{NetConfig, Stack, Stats};
pub use upstream::{TcpRead, TcpStatus, Upstream};
pub use wire::Mac;

/// Virtual time in microseconds, provided by the caller (the platform).
/// It must be non-decreasing between one call and the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtualTime(pub u64);

impl VirtualTime {
    pub const fn from_micros(us: u64) -> Self {
        Self(us)
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self(ms * 1_000)
    }

    pub const fn from_secs(s: u64) -> Self {
        Self(s * 1_000_000)
    }

    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// `self + us`, saturato.
    pub const fn after(self, us: u64) -> Self {
        Self(self.0.saturating_add(us))
    }
}

/// Identifier of a TCP connection or UDP flow, unique for the whole
/// life of the stack and assigned in increasing order from 1.
pub type ConnId = u64;

/// Four-tuple of a connection as seen by the guest: `guest` is the endpoint in the
/// guest, `remote` the destination the guest believes it is contacting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flow {
    pub guest: SocketAddrV4,
    pub remote: SocketAddrV4,
}
