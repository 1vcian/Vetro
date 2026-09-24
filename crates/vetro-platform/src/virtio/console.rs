//! virtio-console (virtio v1.2, §5.3) con una sola porta.
//!
//! Senza VIRTIO_CONSOLE_F_MULTIPORT: code 0 = ricezione e 1 = trasmissione
//! della porta 0, niente messaggi di controllo. Nel guest diventa
//! `/dev/hvc0`; in M5 ci passerà adb. Feature offerta: EMERG_WRITE (una
//! scrittura di `emerg_wr` nella configurazione emette un byte anche prima
//! che le code siano pronte).
//!
//! RX: si estrae una catena solo se ci sono buffer liberi, e il backend
//! riempie al massimo lo spazio scrivibile; se non ha nulla la catena torna
//! nell'available ring.

use core::any::Any;
use std::collections::VecDeque;

use super::*;

pub const F_SIZE: u64 = 1 << 0;
pub const F_MULTIPORT: u64 = 1 << 1;
pub const F_EMERG_WRITE: u64 = 1 << 2;

/// Offset di `emerg_wr` nella configurazione.
pub const CFG_EMERG_WR: u64 = 8;

const RXQ: usize = 0;
const TXQ: usize = 1;
/// Byte massimi per catena in ricezione e per pezzo in trasmissione.
const CHUNK: u64 = 64 * 1024;

/// Flusso di byte della porta, visto dal dispositivo.
pub trait ConsoleBackend: Any {
    /// Byte scritti dal guest.
    fn write(&mut self, data: &[u8]);
    /// Riempie `buf` con i byte in arrivo per il guest; 0 se non ce ne sono.
    fn read(&mut self, buf: &mut [u8]) -> usize;
}

/// Backend in memoria: `input` verso il guest, `output` dal guest.
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

    /// Accesso tipizzato al backend.
    pub fn backend_as_mut<T: ConsoleBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    fn config_bytes(&self) -> [u8; 12] {
        let mut c = [0u8; 12];
        // cols e rows a 0 (niente F_SIZE); max_nr_ports = 1.
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
            // A pezzi: la lunghezza dei descrittori la decide il guest.
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
}

#[cfg(test)]
mod tests;
