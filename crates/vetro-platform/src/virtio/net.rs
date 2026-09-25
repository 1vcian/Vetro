//! virtio-net (virtio v1.2, §5.1), senza offload.
//!
//! Code: 0 = ricezione, 1 = trasmissione (niente coda di controllo, niente
//! multiqueue). Feature: MAC, STATUS e, se abilitato, MRG_RXBUF. Nessun
//! checksum né GSO: l'intestazione `virtio_net_hdr` (12 byte con
//! VERSION_1) è sempre a zero tranne `num_buffers`.
//!
//! Scelte:
//! - TX: un buffer più corto dell'intestazione, o più lungo di
//!   intestazione + [`MAX_FRAME`], è un errore della coda (come
//!   `virtio_error` di QEMU); con il link giù i frame si scartano;
//! - RX: il backend viene interrogato solo se ci sono buffer liberi e con
//!   il link su, così i frame restano nel backend finché il guest non può
//!   riceverli. Con MRG_RXBUF un frame si distribuisce su più catene; se
//!   non bastano si rimettono nell'available ring e il frame aspetta. Senza
//!   MRG_RXBUF un frame che non entra nella catena si scarta (e la catena
//!   resta al driver), contato in [`VirtioNet::rx_dropped`].

use core::any::Any;
use std::collections::VecDeque;

use super::*;

pub const F_MAC: u64 = 1 << 5;
pub const F_MRG_RXBUF: u64 = 1 << 15;
pub const F_STATUS: u64 = 1 << 16;

/// `virtio_net_hdr` con VERSION_1 (compreso `num_buffers`).
pub const NET_HDR_LEN: usize = 12;
/// Bit di `status` nella configurazione.
pub const S_LINK_UP: u16 = 1;
/// Frame più lungo accettato in trasmissione (senza GSO un frame ethernet
/// sta ben sotto; il limite evita allocazioni decise dal guest).
pub const MAX_FRAME: usize = 65535;

const RXQ: usize = 0;
const TXQ: usize = 1;

/// Rete vista dal dispositivo: frame ethernet senza intestazione virtio.
pub trait NetBackend: Any {
    /// Frame trasmesso dal guest.
    fn send(&mut self, frame: &[u8]);
    /// Prossimo frame per il guest, se c'è. Chiamato solo quando il guest
    /// ha buffer di ricezione liberi.
    fn recv(&mut self) -> Option<Vec<u8>>;
    /// Stato del backend negli snapshot (M6, ADR 0015): di norma nessuno (un
    /// collegamento verso l'esterno che l'host ricrea). Lo stack di rete
    /// della macchina e le code in memoria salvano il loro.
    fn save_state(&self, _w: &mut vetro_snapshot::Writer) {}
    fn restore_state(&mut self, _r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        Ok(())
    }
}

/// Backend in memoria: `rx` verso il guest, `tx` dal guest.
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
    /// Dispositivo con link su, code da 256 (come QEMU) e MRG_RXBUF offerto.
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

    /// Offre o no VIRTIO_NET_F_MRG_RXBUF.
    pub fn with_mrg_rxbuf(mut self, offer: bool) -> Self {
        self.offer_mrg = offer;
        self
    }

    pub fn backend_mut(&mut self) -> &mut dyn NetBackend {
        self.backend.as_mut()
    }

    /// Accesso tipizzato al backend, in sola lettura.
    pub fn backend_as<T: NetBackend>(&self) -> Option<&T> {
        let b: &dyn Any = self.backend.as_ref();
        b.downcast_ref()
    }

    /// Accesso tipizzato al backend.
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

    /// Cambia lo stato del link; il driver lo saprà al prossimo `service`
    /// con un interrupt di configurazione.
    pub fn set_link_up(&mut self, up: bool) {
        if up != self.link_up {
            self.link_up = up;
            self.link_changed = true;
        }
    }

    /// Frame scartati in ricezione perché non entravano nei buffer.
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
                return Err(QueueError::Malformed("intestazione virtio-net incompleta"));
            }
            if c.readable_len() > (NET_HDR_LEN + MAX_FRAME) as u64 {
                return Err(QueueError::Malformed("frame virtio-net oltre 64 KiB"));
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
            // Catene necessarie: una sola senza MRG_RXBUF.
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

    /// Link, MRG_RXBUF negoziato, frame in attesa di buffer, contatore e
    /// stato del backend. MAC e offerta di MRG_RXBUF sono configurazione.
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
            return Err(vetro_snapshot::Error::invalid("MAC di virtio-net diverso"));
        }
        r.expect_u64("offerta di MRG_RXBUF", u64::from(self.offer_mrg))?;
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
