//! virtio-console (virtio v1.2, §5.3) with a single port.
//!
//! Without VIRTIO_CONSOLE_F_MULTIPORT: queues 0 = receive and 1 = transmit
//! of port 0, no control messages. In the guest it becomes
//! `/dev/hvc0`; in M5 adb will go through it. Feature offered: EMERG_WRITE (a
//! write of `emerg_wr` in the configuration emits a byte even before
//! the queues are ready).
//!
//! RX: a chain is popped only if there are free buffers, and the backend
//! fills at most the writable space; if it has nothing the chain goes back
//! into the available ring.

use core::any::Any;
use std::collections::VecDeque;

use super::*;

pub const F_SIZE: u64 = 1 << 0;
pub const F_MULTIPORT: u64 = 1 << 1;
pub const F_EMERG_WRITE: u64 = 1 << 2;

/// Offset of `emerg_wr` in the configuration.
pub const CFG_EMERG_WR: u64 = 8;

const RXQ: usize = 0;
const TXQ: usize = 1;
/// Byte massimi per catena in ricezione e per pezzo in trasmissione.
const CHUNK: u64 = 64 * 1024;

/// Byte stream of the port, as seen by the device.
pub trait ConsoleBackend: Any {
    /// Bytes written by the guest.
    fn write(&mut self, data: &[u8]);
    /// Fills `buf` with the bytes arriving for the guest; 0 if there are none.
    fn read(&mut self, buf: &mut [u8]) -> usize;
    /// Backend state in snapshots (M6, ADR 0015): usually none (a
    /// link that the host recreates).
    fn save_state(&self, _w: &mut vetro_snapshot::Writer) {}
    fn restore_state(&mut self, _r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        Ok(())
    }
}

/// In-memory backend: `input` towards the guest, `output` from the guest.
#[derive(Clone, Debug, Default)]
pub struct BufferConsole {
    pub input: VecDeque<u8>,
    pub output: Vec<u8>,
}

impl ConsoleBackend for BufferConsole {
    fn write(&mut self, data: &[u8]) {
        self.output.extend_from_slice(data);
    }
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.input.len());
        for (b, c) in buf.iter_mut().zip(self.input.drain(..n)) {
            *b = c;
        }
        n
    }
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.seq(&self.input, |w, &b| w.u8(b));
        w.bytes(&self.output);
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.input = r.seq(1, |r| r.u8())?.into();
        self.output = r.vec()?;
        Ok(())
    }
}

pub struct VirtioConsole {
    backend: Box<dyn ConsoleBackend>,
    queue_sizes: [u16; 2],
}

impl VirtioConsole {
    pub fn new(backend: Box<dyn ConsoleBackend>) -> Self {
        Self { backend, queue_sizes: [128, 128] }
    }

    pub fn backend_mut(&mut self) -> &mut dyn ConsoleBackend {
        self.backend.as_mut()
    }

    /// Typed access to the backend.
    pub fn backend_as_mut<T: ConsoleBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    fn config_bytes(&self) -> [u8; 12] {
        let mut c = [0u8; 12];
        // cols and rows at 0 (no F_SIZE); max_nr_ports = 1.
        c[4..8].copy_from_slice(&1u32.to_le_bytes());
        c
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_id(&self) -> u32 {
        ID_CONSOLE
    }

    fn features(&self) -> u64 {
        F_EMERG_WRITE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_bytes(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if offset == CFG_EMERG_WR && data.len() == 4 {
            self.backend.write(&data[..1]);
        }
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        let (queues, ram) = (&mut *ctx.queues, &mut *ctx.ram);
        let tx = &mut queues[TXQ];
        while let Some(c) = tx.pop(ram)? {
            // In pieces: the length of the descriptors is decided by the guest.
            let mut buf = vec![0u8; c.readable_len().min(CHUNK) as usize];
            let mut off = 0u64;
            loop {
                let n = c.read(ram, off, &mut buf)?;
                if n == 0 {
                    break;
                }
                self.backend.write(&buf[..n]);
                off += n as u64;
            }
            tx.push_used(ram, c.head, 0)?;
        }
        let rx = &mut queues[RXQ];
        while rx.available(ram)? > 0 {
            let Some(c) = rx.pop(ram)? else { break };
            let mut buf = vec![0u8; c.writable_len().min(CHUNK) as usize];
            let n = self.backend.read(&mut buf);
            if n == 0 {
                rx.rewind(ram, 1)?;
                break;
            }
            let n = c.write(ram, 0, &buf[..n])?;
            rx.push_used(ram, c.head, n as u32)?;
        }
        Ok(())
    }

    /// The device has no state of its own: only the backend's.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        self.backend.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.backend.restore_state(r)
    }
}

#[cfg(test)]
mod tests;
