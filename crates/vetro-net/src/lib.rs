//! Stack di rete lato host di Vetro: gateway virtuale, TCP/UDP terminati
//! lato host (come slirp), sinkhole e interfaccia verso il relay.
//!
//! Il guest (Linux) scambia frame Ethernet con il dispositivo virtio-net;
//! [`Stack`] sta dall'altra parte del cavo. Risponde ad ARP, DHCP e ICMP come
//! la rete "user" di QEMU (guest 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3),
//! termina ogni connessione TCP e ogni flusso UDP del guest e ne consegna i
//! dati a un [`Upstream`]: il [`Sinkhole`] (tutto finto e registrato) o un
//! [`RelayUpstream`] (verso il relay WebSocket di M7). Nel verso opposto,
//! [`Stack::host_connect`] apre connessioni dall'host verso i servizi TCP del
//! guest (inoltro di porte come `hostfwd` di QEMU: la base per adb).
//!
//! Determinismo: nessun orologio dell'host e nessuna casualità non seminata.
//! Il tempo arriva come parametro ([`VirtualTime`]), i numeri di sequenza
//! iniziali derivano da `NetConfig::seed`, le tabelle sono `BTreeMap`.
//! Stesse chiamate con stessi argomenti producono gli stessi frame e lo stesso
//! registro degli eventi. Interfaccia e invarianti in `docs/specs/net.md`,
//! scelte in `docs/adr/0007-stack-di-rete-senza-smoltcp.md`.

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

/// Tempo virtuale in microsecondi, fornito da chi chiama (la piattaforma).
/// Deve essere non decrescente tra una chiamata e la successiva.
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

/// Identificativo di una connessione TCP o di un flusso UDP, unico per tutta
/// la vita dello stack e assegnato in ordine crescente da 1.
pub type ConnId = u64;

/// Quadrupla di una connessione vista dal guest: `guest` è l'estremo nel
/// guest, `remote` la destinazione che il guest crede di contattare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flow {
    pub guest: SocketAddrV4,
    pub remote: SocketAddrV4,
}
