//! Virtio (spec OASIS virtio v1.2): trasporto virtio-mmio versione 2,
//! virtqueue split e i dispositivi blk, net, console, gpu, input e vsock.
//!
//! Struttura:
//! - [`VirtioMmio`] (`mmio.rs`) è il trasporto: registri, negoziazione delle
//!   feature, configurazione delle code, stato, interrupt. Uno slot senza
//!   dispositivo si comporta come gli slot liberi di QEMU (DeviceID 0).
//! - [`Virtqueue`] (`queue.rs`) è la coda split: descrittori, catene,
//!   tabelle indirette, ring available e used, EVENT_IDX.
//! - I dispositivi implementano [`VirtioDevice`]: [`VirtioBlk`],
//!   [`VirtioNet`], [`VirtioConsole`], [`VirtioGpu`], [`VirtioInput`],
//!   [`VirtioVsock`]. L'I/O verso l'esterno passa da trait
//!   ([`BlockBackend`], [`NetBackend`], [`ConsoleBackend`],
//!   [`DisplayBackend`]) o dall'API host del dispositivo (eventi di input,
//!   connessioni vsock).
//!
//! La RAM del guest non passa dal bus MMIO: una scrittura in QueueNotify
//! segna solo la coda. Il lavoro vero si fa in [`VirtioMmio::service`], che
//! riceve la RAM come [`GuestRam`] e che il motore chiama dopo gli accessi
//! MMIO agli slot virtio e periodicamente (per i dati in arrivo dai backend).

pub mod blk;
pub mod console;
pub mod edid;
pub mod gpu;
pub mod input;
pub mod mmio;
pub mod net;
pub mod queue;
pub mod vsock;

#[cfg(test)]
pub(crate) mod testdrv;

pub use blk::{
    BLK_SECTOR_SIZE, BlockBackend, BlockError, CowBackend, MemBackend, VirtioBlk, VirtioBlkConfig,
};
pub use console::{BufferConsole, ConsoleBackend, VirtioConsole};
pub use gpu::{DisplayBackend, Frame, GpuConfig, MemDisplay, PixelFormat, Rect, VirtioGpu};
pub use input::{AbsInfo, InputConfig, InputEvent, VirtioInput};
pub use mmio::VirtioMmio;
pub use net::{NetBackend, QueueNet, VirtioNet};
pub use queue::{Buf, DescChain, QueueError, Virtqueue};
pub use vsock::{VirtioVsock, VsockConn, VsockError, VsockState};

use core::any::Any;
use core::fmt;

use crate::bus::{MmioDevice, sub_word};

// ---- Registri del trasporto (§4.2.2) ---------------------------------------

pub const MAGIC_VALUE: u64 = 0x000;
pub const VERSION: u64 = 0x004;
pub const DEVICE_ID: u64 = 0x008;
pub const VENDOR_ID: u64 = 0x00C;
pub const DEVICE_FEATURES: u64 = 0x010;
pub const DEVICE_FEATURES_SEL: u64 = 0x014;
pub const DRIVER_FEATURES: u64 = 0x020;
pub const DRIVER_FEATURES_SEL: u64 = 0x024;
pub const QUEUE_SEL: u64 = 0x030;
pub const QUEUE_NUM_MAX: u64 = 0x034;
pub const QUEUE_NUM: u64 = 0x038;
pub const QUEUE_READY: u64 = 0x044;
pub const QUEUE_NOTIFY: u64 = 0x050;
pub const INTERRUPT_STATUS: u64 = 0x060;
pub const INTERRUPT_ACK: u64 = 0x064;
pub const STATUS: u64 = 0x070;
pub const QUEUE_DESC_LOW: u64 = 0x080;
pub const QUEUE_DESC_HIGH: u64 = 0x084;
pub const QUEUE_DRIVER_LOW: u64 = 0x090;
pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
pub const QUEUE_DEVICE_LOW: u64 = 0x0A0;
pub const QUEUE_DEVICE_HIGH: u64 = 0x0A4;
pub const SHM_SEL: u64 = 0x0AC;
pub const SHM_LEN_LOW: u64 = 0x0B0;
pub const SHM_LEN_HIGH: u64 = 0x0B4;
pub const SHM_BASE_LOW: u64 = 0x0B8;
pub const SHM_BASE_HIGH: u64 = 0x0BC;
pub const CONFIG_GENERATION: u64 = 0x0FC;
/// Inizio dello spazio di configurazione del dispositivo.
pub const CONFIG: u64 = 0x100;

/// "virt" in little endian.
pub const MAGIC: u32 = 0x7472_6976;
/// VendorID: lo stesso di QEMU ("QEMU" in little endian), così il guest
/// vede gli stessi valori sulle due piattaforme.
pub const VENDOR: u32 = 0x554D_4551;

// ---- Bit di Status (§2.1) --------------------------------------------------

pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const STATUS_FAILED: u32 = 128;

// ---- InterruptStatus (§4.2.2) ----------------------------------------------

/// Buffer usati in una coda.
pub const INT_VRING: u32 = 1;
/// Configurazione cambiata (o DEVICE_NEEDS_RESET).
pub const INT_CONFIG: u32 = 2;

// ---- Feature riservate (§6) ------------------------------------------------

pub const F_INDIRECT_DESC: u64 = 1 << 28;
pub const F_EVENT_IDX: u64 = 1 << 29;
pub const F_VERSION_1: u64 = 1 << 32;

// ---- DeviceID (§5) ---------------------------------------------------------

pub const ID_NET: u32 = 1;
pub const ID_BLOCK: u32 = 2;
pub const ID_CONSOLE: u32 = 3;
pub const ID_GPU: u32 = 16;
pub const ID_INPUT: u32 = 18;
pub const ID_VSOCK: u32 = 19;

// ---- Memoria del guest -----------------------------------------------------

/// Accesso fuori dalla RAM del guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamError {
    pub addr: u64,
    pub len: usize,
}

impl fmt::Display for RamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "accesso fuori dalla RAM del guest: {:#x}+{:#x}", self.addr, self.len)
    }
}

/// RAM del guest vista dai dispositivi (DMA su indirizzi fisici).
///
/// Un accesso deve stare tutto in RAM, altrimenti fallisce senza effetti
/// parziali garantiti. Il motore la implementa sopra la memoria della CPU.
pub trait GuestRam {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError>;
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError>;
}

/// Letture e scritture little endian sopra [`GuestRam`].
pub trait GuestRamExt: GuestRam {
    fn read_u16(&self, addr: u64) -> Result<u16, RamError> {
        let mut b = [0; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }
    fn read_u32(&self, addr: u64) -> Result<u32, RamError> {
        let mut b = [0; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn read_u64(&self, addr: u64) -> Result<u64, RamError> {
        let mut b = [0; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn write_u16(&mut self, addr: u64, v: u16) -> Result<(), RamError> {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_u32(&mut self, addr: u64, v: u32) -> Result<(), RamError> {
        self.write(addr, &v.to_le_bytes())
    }
}

impl<T: GuestRam + ?Sized> GuestRamExt for T {}

/// RAM contigua in un `Vec`, a partire da `base`. Serve ai test e a chi
/// non ha una memoria propria.
#[derive(Clone, Debug)]
pub struct VecRam {
    pub base: u64,
    pub bytes: Vec<u8>,
}

impl VecRam {
    pub fn new(base: u64, size: usize) -> Self {
        Self { base, bytes: vec![0; size] }
    }

    fn range(&self, addr: u64, len: usize) -> Result<core::ops::Range<usize>, RamError> {
        let err = RamError { addr, len };
        let start = addr.checked_sub(self.base).ok_or(err)?;
        let end = start.checked_add(len as u64).ok_or(err)?;
        if end > self.bytes.len() as u64 {
            return Err(err);
        }
        Ok(start as usize..end as usize)
    }
}

impl GuestRam for VecRam {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError> {
        let r = self.range(addr, buf.len())?;
        buf.copy_from_slice(&self.bytes[r]);
        Ok(())
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError> {
        let r = self.range(addr, data.len())?;
        self.bytes[r].copy_from_slice(data);
        Ok(())
    }
}

// ---- Dispositivo -----------------------------------------------------------

/// Contesto di [`VirtioDevice::service`]: le code del dispositivo, la RAM e
/// le feature negoziate. I campi sono pubblici per poter prendere in
/// prestito coda e RAM insieme.
pub struct ServiceCtx<'a> {
    pub queues: &'a mut [Virtqueue],
    pub ram: &'a mut dyn GuestRam,
    pub features: u64,
    config_changed: bool,
}

impl ServiceCtx<'_> {
    /// Segnala al driver un cambio della configurazione (ConfigGeneration
    /// avanza e parte l'interrupt di configurazione).
    pub fn config_changed(&mut self) {
        self.config_changed = true;
    }
}

/// Dispositivo virtio dietro al trasporto. Le code appartengono al
/// trasporto; il dispositivo le usa solo dentro `service`.
pub trait VirtioDevice: Any {
    fn device_id(&self) -> u32;
    /// Feature specifiche del dispositivo (bit 0..23). Quelle del
    /// trasporto (VERSION_1, INDIRECT_DESC, EVENT_IDX) le aggiunge
    /// [`VirtioMmio`].
    fn features(&self) -> u64;
    /// Dimensione massima di ogni coda; la lunghezza è il numero di code.
    fn queue_max_sizes(&self) -> &[u16];
    /// Lettura dallo spazio di configurazione; oltre la fine si legge 0.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// Scrittura nello spazio di configurazione (di norma ignorata).
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// Il driver ha accettato queste feature (FEATURES_OK). `false` rifiuta
    /// la combinazione e FEATURES_OK non resta impostato.
    fn negotiate(&mut self, _features: u64) -> bool {
        true
    }
    /// Reset del dispositivo (Status = 0): dimentica le richieste in corso.
    fn reset(&mut self) {}
    /// Consuma le code e i dati dei backend. Chiamato solo con DRIVER_OK.
    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError>;
}

/// Legge `data.len()` byte dalla struttura di configurazione `cfg` a
/// partire da `offset`; i byte oltre la fine valgono 0.
pub(crate) fn read_config_bytes(cfg: &[u8], offset: u64, data: &mut [u8]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = usize::try_from(offset).ok().and_then(|o| cfg.get(o + i)).copied().unwrap_or(0);
    }
}

// ---- Slot vuoto ------------------------------------------------------------

/// Slot virtio-mmio senza dispositivo, come quelli liberi di QEMU virt:
/// MagicValue e Version sono validi e DeviceID vale 0, così il driver Linux
/// riconosce il trasporto e lo salta. [`VirtioMmio::empty`] si comporta
/// allo stesso modo e in più accetta un dispositivo in seguito.
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioMmioEmpty;

impl MmioDevice for VirtioMmioEmpty {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        let word = match offset & !3 {
            MAGIC_VALUE => MAGIC,
            VERSION => 2,
            _ => 0,
        };
        if size > 4 { 0 } else { sub_word(word, offset, size) }
    }

    fn write(&mut self, _offset: u64, _size: u8, _value: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_vuoto_riconoscibile() {
        let mut v = VirtioMmioEmpty;
        assert_eq!(v.read(MAGIC_VALUE, 4), 0x7472_6976);
        assert_eq!(v.read(VERSION, 4), 2);
        assert_eq!(v.read(DEVICE_ID, 4), 0);
        v.write(0x070, 4, 0xF);
        assert_eq!(v.read(0x070, 4), 0);
    }

    #[test]
    fn vec_ram_rifiuta_accessi_fuori_limite() {
        let mut r = VecRam::new(0x1000, 16);
        assert!(r.write(0x1008, &[1; 8]).is_ok());
        assert_eq!(r.write(0x1009, &[1; 8]), Err(RamError { addr: 0x1009, len: 8 }));
        assert!(r.read(0xFFF, &mut [0; 1]).is_err());
        assert!(r.read(u64::MAX, &mut [0; 2]).is_err());
        assert_eq!(r.read_u64(0x1008), Ok(0x0101_0101_0101_0101));
    }

    #[test]
    fn config_oltre_la_fine_vale_zero() {
        let mut d = [0xFF; 4];
        read_config_bytes(&[1, 2, 3], 1, &mut d);
        assert_eq!(d, [2, 3, 0, 0]);
    }
}
