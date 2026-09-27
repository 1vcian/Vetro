//! virtio-net (virtio v1.2, §5.1), without offloads.
//!
//! Queues: 0 = receive, 1 = transmit (no control queue, no
//! multiqueue). Features: MAC, STATUS and, if enabled, MRG_RXBUF. No
//! checksum and no GSO: the `virtio_net_hdr` header (12 bytes with
//! VERSION_1) is always zero except `num_buffers`.
//!
//! Choices:
//! - TX: a buffer shorter than the header, or longer than
//!   header + [`MAX_FRAME`], is a queue error (like QEMU's
//!   `virtio_error`); with the link down frames are discarded;
//! - RX: the backend is polled only if there are free buffers and
//!   the link is up, so frames stay in the backend until the guest can
//!   receive them. With MRG_RXBUF a frame is spread over several chains; if
//!   they are not enough they go back into the available ring and the frame waits. Without
//!   MRG_RXBUF a frame that doesn't fit in the chain is discarded (and the chain
//!   stays with the driver), counted in [`VirtioNet::rx_dropped`].

use core::any::Any;
use std::collections::VecDeque;

use super::*;

pub const F_MAC: u64 = 1 << 5;
pub const F_MRG_RXBUF: u64 = 1 << 15;
pub const F_STATUS: u64 = 1 << 16;

/// `virtio_net_hdr` with VERSION_1 (including `num_buffers`).
pub const NET_HDR_LEN: usize = 12;
/// Bits of `status` in the configuration.
pub const S_LINK_UP: u16 = 1;
/// Longest frame accepted for transmission (without GSO an ethernet frame
/// is well below; the limit avoids allocations decided by the guest).
pub const MAX_FRAME: usize = 65535;

const RXQ: usize = 0;
const TXQ: usize = 1;

/// Network as seen by the device: ethernet frames without the virtio header.
pub trait NetBackend: Any {
    /// Frame transmitted by the guest.
    fn send(&mut self, frame: &[u8]);
    /// Next frame for the guest, if any. Called only when the guest
    /// has free receive buffers.
    fn recv(&mut self) -> Option<Vec<u8>>;
    /// Backend state in snapshots (M6, ADR 0015): usually none (a
    /// link to the outside that the host recreates). The machine's network
    /// stack and the in-memory queues save theirs.
    fn save_state(&self, _w: &mut vetro_snapshot::Writer) {}
    fn restore_state(&mut self, _r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        Ok(())
    }
}

/// In-memory backend: `rx` towards the guest, `tx` from the guest.
#[derive(Clone, Debug, Default)]
pub struct QueueNet {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
}

impl NetBackend for QueueNet {
    fn send(&mut self, frame: &[u8]) {
        self.tx.push(frame.to_vec());
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.pop_front()
    }
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.seq(&self.rx, |w, f| w.bytes(f));
        w.seq(&self.tx, |w, f| w.bytes(f));
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.rx = r.seq(8, |r| r.vec())?.into();
        self.tx = r.seq(8, |r| r.vec())?;
        Ok(())
    }
}

pub struct VirtioNet {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
    link_up: bool,
    link_changed: bool,
    offer_mrg: bool,
    mrg: bool,
    pending_rx: Option<Vec<u8>>,
    rx_dropped: u64,
    queue_sizes: [u16; 2],
}

impl VirtioNet {
    /// Device with the link up, 256-entry queues (like QEMU) and MRG_RXBUF offered.
    pub fn new(backend: Box<dyn NetBackend>, mac: [u8; 6]) -> Self {
        Self {
            backend,
            mac,
            link_up: true,
            link_changed: false,
            offer_mrg: true,
            mrg: false,
            pending_rx: None,
            rx_dropped: 0,
            queue_sizes: [256, 256],
        }
    }

    /// Offers VIRTIO_NET_F_MRG_RXBUF or not.
    pub fn with_mrg_rxbuf(mut self, offer: bool) -> Self {
        self.offer_mrg = offer;
        self
    }

    pub fn backend_mut(&mut self) -> &mut dyn NetBackend {
        self.backend.as_mut()
    }

    /// Typed access to the backend, read-only.
    pub fn backend_as<T: NetBackend>(&self) -> Option<&T> {
        let b: &dyn Any = self.backend.as_ref();
        b.downcast_ref()
    }

    /// Typed access to the backend.
    pub fn backend_as_mut<T: NetBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn link_up(&self) -> bool {
        self.link_up
    }

    /// Changes the link state; the driver will learn it at the next `service`
    /// with a configuration interrupt.
    pub fn set_link_up(&mut self, up: bool) {
        if up != self.link_up {
            self.link_up = up;
            self.link_changed = true;
        }
    }

    /// Frames discarded on receive because they didn't fit in the buffers.
    pub fn rx_dropped(&self) -> u64 {
        self.rx_dropped
    }

    fn config_bytes(&self) -> [u8; 12] {
        let mut c = [0u8; 12];
        c[0..6].copy_from_slice(&self.mac);
        let status = if self.link_up { S_LINK_UP } else { 0 };
        c[6..8].copy_from_slice(&status.to_le_bytes());
        c[8..10].copy_from_slice(&1u16.to_le_bytes()); // max_virtqueue_pairs
        c
    }

    fn transmit(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        while let Some(c) = q.pop(ram)? {
            if c.readable_len() < NET_HDR_LEN as u64 {
                return Err(QueueError::Malformed("incomplete virtio-net header"));
            }
            if c.readable_len() > (NET_HDR_LEN + MAX_FRAME) as u64 {
                return Err(QueueError::Malformed("virtio-net frame over 64 KiB"));
            }
            let frame = c.read_to_vec(ram, NET_HDR_LEN as u64)?;
            if self.link_up {
                self.backend.send(&frame);
            }
            q.push_used(ram, c.head, 0)?;
        }
        Ok(())
    }

    fn receive(&mut self, q: &mut Virtqueue, ram: &mut dyn GuestRam) -> Result<(), QueueError> {
        while self.link_up {
            let frame = match self.pending_rx.take() {
                Some(f) => f,
                None if q.available(ram)? > 0 => match self.backend.recv() {
                    Some(f) => f,
                    None => return Ok(()),
                },
                None => return Ok(()),
            };
            let need = (NET_HDR_LEN + frame.len()) as u64;
            // Chains needed: just one without MRG_RXBUF.
            let mut chains = Vec::new();
            let mut room = 0u64;
            while room < need && (self.mrg || chains.is_empty()) {
                let Some(c) = q.pop(ram)? else { break };
                room += c.writable_len();
                chains.push(c);
            }
            if room < need {
                q.rewind(ram, chains.len() as u16)?;
                if self.mrg || chains.is_empty() {
                    self.pending_rx = Some(frame);
                    return Ok(());
                }
                self.rx_dropped += 1;
                continue;
            }
            let mut pkt = vec![0u8; NET_HDR_LEN];
            pkt[10..12].copy_from_slice(&(chains.len() as u16).to_le_bytes());
            pkt.extend_from_slice(&frame);
            let mut off = 0usize;
            for c in &chains {
                let n = c.write(ram, 0, &pkt[off..])?;
                off += n;
                q.push_used(ram, c.head, n as u32)?;
            }
        }
        Ok(())
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        ID_NET
    }

    fn features(&self) -> u64 {
        F_MAC | F_STATUS | if self.offer_mrg { F_MRG_RXBUF } else { 0 }
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_bytes(), offset, data);
    }

    fn negotiate(&mut self, features: u64) -> bool {
        self.mrg = features & F_MRG_RXBUF != 0;
        true
    }

    fn reset(&mut self) {
        self.mrg = false;
        self.pending_rx = None;
        self.link_changed = false;
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        if std::mem::take(&mut self.link_changed) {
            ctx.config_changed();
        }
        let (queues, ram) = (&mut *ctx.queues, &mut *ctx.ram);
        self.transmit(&mut queues[TXQ], ram)?;
        self.receive(&mut queues[RXQ], ram)
    }

    /// Link, negotiated MRG_RXBUF, frame waiting for buffers, counter and
    /// backend state. MAC and the MRG_RXBUF offer are configuration.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.raw(&self.mac);
        w.u64(u64::from(self.offer_mrg));
        w.bool(self.link_up);
        w.bool(self.link_changed);
        w.bool(self.mrg);
        w.opt(self.pending_rx.as_deref(), vetro_snapshot::Writer::bytes);
        w.u64(self.rx_dropped);
        self.backend.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        if r.raw(6)? != self.mac {
            return Err(vetro_snapshot::Error::invalid("different virtio-net MAC"));
        }
        r.expect_u64("MRG_RXBUF offer", u64::from(self.offer_mrg))?;
        self.link_up = r.bool()?;
        self.link_changed = r.bool()?;
        self.mrg = r.bool()?;
        self.pending_rx = r.opt(|r| r.vec())?;
        self.rx_dropped = r.u64()?;
        self.backend.restore_state(r)
    }
}

#[cfg(test)]
mod tests;
