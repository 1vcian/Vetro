//! The guest network: virtio-net connected to the `vetro-net` stack
//! (virtual gateway like QEMU's user network, sinkhole).
//!
//! The stack's time is the machine's: CNTPCT converted to
//! microseconds, never the host clock. The machine updates the instant
//! before serving the devices and calls `poll` at the stack's deadline
//! (see `Machine::sync_irqs`), so retransmissions and timers always land
//! on the same instruction.
//!
//! Connections from the host to guest services (port forwarding,
//! `Stack::host_connect`: `vetro boot --hostfwd`, `vetro_net_*` in the browser)
//! go through `Machine::input` with `Input::HostNet` (ADR 0019): they are
//! host inputs, recorded for replay, and the `poll` forced before
//! the next instruction delivers them to the guest.
//!
//! The host can also deliver its own Ethernet frames to the guest
//! (`Input::NetFrame`): they go ahead of the stack's and are also
//! a recorded input.

use std::collections::VecDeque;

use vetro_net::{NetConfig, Sinkhole, SinkholeConfig, Stack, VirtualTime};
use vetro_platform::map;
use vetro_platform::virtio::NetBackend;

/// Default guest MAC: the one QEMU assigns to the first
/// `virtio-net-device` (`52:54:00:12:34:56`).
pub const DEFAULT_GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// The machine's network card and what sits at the other end of the cable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetSetup {
    /// Guest MAC (virtio-net configuration).
    pub mac: [u8; 6],
    /// Virtual network (addresses, MTU, lease, ISN seed).
    pub config: NetConfig,
    /// The sinkhole: per-port replies, fake DNS, ping.
    pub sinkhole: SinkholeConfig,
}

impl Default for NetSetup {
    fn default() -> Self {
        NetSetup { mac: DEFAULT_GUEST_MAC, config: NetConfig::default(), sinkhole: SinkholeConfig::default() }
    }
}

/// Direction of a frame seen at the virtio-net boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDir {
    /// Transmitted by the guest (goes to the stack).
    FromGuest,
    /// Delivered to the guest (comes from the stack).
    ToGuest,
}

/// An Ethernet frame observed at the capture point of [`NetLink`] (M7,
/// ADR 0016): instant in virtual time, direction and bytes as they pass
/// through virtio-net (without the virtio header).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TappedFrame {
    pub at: VirtualTime,
    pub dir: FrameDir,
    pub data: Vec<u8>,
}

/// virtio-net backend on top of the stack: guest frames go to
/// [`Stack::receive`], the stack's go to the guest with [`Stack::pop_frame`].
pub struct NetLink {
    pub stack: Stack<Sinkhole>,
    /// Current instant, set by the machine before each service.
    pub(crate) now: VirtualTime,
    /// Capture point: if active, a copy of every frame in both directions.
    /// Observation only: it does not change execution and does not go into
    /// snapshots.
    pub(crate) tap: Option<Vec<TappedFrame>>,
    /// Host frames for the guest (`Input::NetFrame`), delivered before
    /// the stack's.
    pub(crate) host_rx: VecDeque<Vec<u8>>,
}

impl NetLink {
    pub fn new(setup: &NetSetup) -> Self {
        NetLink {
            stack: Stack::new(setup.config.clone(), Sinkhole::new(setup.sinkhole.clone())),
            now: VirtualTime(0),
            tap: None,
            host_rx: VecDeque::new(),
        }
    }

    /// Turns frame capture on or off (turning it off loses the frames
    /// not yet taken).
    pub fn set_tap(&mut self, on: bool) {
        if on != self.tap.is_some() {
            self.tap = on.then(Vec::new);
        }
    }

    /// The frames captured so far, in order; capture stays as it is.
    pub fn take_tapped(&mut self) -> Vec<TappedFrame> {
        self.tap.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

impl NetBackend for NetLink {
    fn send(&mut self, frame: &[u8]) {
        if let Some(tap) = &mut self.tap {
            tap.push(TappedFrame { at: self.now, dir: FrameDir::FromGuest, data: frame.to_vec() });
        }
        self.stack.receive(self.now, frame);
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        let frame = self.host_rx.pop_front().or_else(|| self.stack.pop_frame())?;
        if let Some(tap) = &mut self.tap {
            tap.push(TappedFrame { at: self.now, dir: FrameDir::ToGuest, data: frame.clone() });
        }
        Some(frame)
    }
    /// The current instant, the queued host frames and the whole stack
    /// (connections, timers, sinkhole, event log): the network is
    /// inside the machine, nothing to reconnect.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.now.0);
        w.seq(&self.host_rx, |w, f| w.bytes(f));
        w.section(b"NETS", |w| w.put(&self.stack));
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.now = VirtualTime(r.u64()?);
        self.host_rx = r.seq(8, |r| r.vec())?.into();
        let mut s = r.section(b"NETS")?;
        s.get(&mut self.stack)?;
        s.finish()
    }
}

/// Microseconds of virtual time at CNTPCT = `cnt` (rounded down).
pub(crate) fn micros(cnt: u64) -> VirtualTime {
    VirtualTime((u128::from(cnt) * 1_000_000 / u128::from(map::CNTFRQ_HZ)) as u64)
}

/// First CNTPCT at which virtual time is at least `t`.
pub(crate) fn counter_at(t: VirtualTime) -> u64 {
    let c = (u128::from(t.0) * u128::from(map::CNTFRQ_HZ)).div_ceil(1_000_000);
    c.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_conversion() {
        assert_eq!(map::CNTFRQ_HZ, 62_500_000);
        assert_eq!(micros(62_500_000), VirtualTime::from_secs(1));
        assert_eq!(micros(62), VirtualTime(0));
        assert_eq!(micros(63), VirtualTime(1));
        for us in [0u64, 1, 2, 3, 999, 1_000_001, 75_000_000] {
            let c = counter_at(VirtualTime(us));
            assert!(micros(c) >= VirtualTime(us) && (c == 0 || micros(c - 1) < VirtualTime(us)), "{us}");
        }
    }

    /// Guest ARP request for the gateway 10.0.2.2.
    fn arp_request() -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend(DEFAULT_GUEST_MAC);
        f.extend([0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
        f.extend(DEFAULT_GUEST_MAC);
        f.extend([10, 0, 2, 15]);
        f.extend([0; 6]);
        f.extend([10, 0, 2, 2]);
        f
    }

    #[test]
    fn cattura_dei_frame_nei_due_versi() {
        let mut link = NetLink::new(&NetSetup::default());
        link.now = VirtualTime(5);
        link.send(&arp_request());
        assert!(link.take_tapped().is_empty(), "capture off: nothing");
        link.recv().expect("ARP reply");

        link.set_tap(true);
        link.now = VirtualTime(1_000);
        link.send(&arp_request());
        link.now = VirtualTime(1_250);
        let reply = link.recv().expect("ARP reply");
        assert_eq!(link.recv(), None);
        let t = link.take_tapped();
        assert_eq!(t.len(), 2);
        assert_eq!(
            (t[0].at, t[0].dir, &t[0].data),
            (VirtualTime(1_000), FrameDir::FromGuest, &arp_request())
        );
        assert_eq!((t[1].at, t[1].dir, &t[1].data), (VirtualTime(1_250), FrameDir::ToGuest, &reply));
        assert!(link.take_tapped().is_empty(), "already taken");
        link.set_tap(false);
        link.send(&arp_request());
        assert!(link.take_tapped().is_empty());
    }
}
