//! Virtio (OASIS virtio v1.2 spec): virtio-mmio transport version 2,
//! split virtqueues and the blk, net, console, gpu, input and vsock devices.
//!
//! Structure:
//! - [`VirtioMmio`] (`mmio.rs`) is the transport: registers, feature
//!   negotiation, queue configuration, status, interrupts. A slot without a
//!   device behaves like QEMU's free slots (DeviceID 0).
//! - [`Virtqueue`] (`queue.rs`) is the split queue: descriptors, chains,
//!   indirect tables, available and used rings, EVENT_IDX.
//! - Devices implement [`VirtioDevice`]: [`VirtioBlk`],
//!   [`VirtioNet`], [`VirtioConsole`], [`VirtioGpu`], [`VirtioInput`],
//!   [`VirtioVsock`]. I/O to the outside goes through traits
//!   ([`BlockBackend`], [`NetBackend`], [`ConsoleBackend`],
//!   [`DisplayBackend`]) or through the device's host API (input events,
//!   vsock connections).
//!
//! Guest RAM does not go through the MMIO bus: a write to QueueNotify
//! only marks the queue. The real work is done in [`VirtioMmio::service`], which
//! receives RAM as [`GuestRam`] and which the engine calls after MMIO
//! accesses to the virtio slots and periodically (for incoming data from the backends).

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

// ---- Transport registers (§4.2.2) -----------------------------------------

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
/// Start of the device configuration space.
pub const CONFIG: u64 = 0x100;

/// "virt" in little endian.
pub const MAGIC: u32 = 0x7472_6976;
/// VendorID: the same as QEMU ("QEMU" in little endian), so the guest
/// sees the same values on both platforms.
pub const VENDOR: u32 = 0x554D_4551;

// ---- Status bits (§2.1) ----------------------------------------------------

pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const STATUS_FAILED: u32 = 128;

// ---- InterruptStatus (§4.2.2) ----------------------------------------------

/// Used buffers in a queue.
pub const INT_VRING: u32 = 1;
/// Configuration changed (or DEVICE_NEEDS_RESET).
pub const INT_CONFIG: u32 = 2;

// ---- Reserved features (§6) ------------------------------------------------

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

// ---- Guest memory ----------------------------------------------------------

/// Access outside guest RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamError {
    pub addr: u64,
    pub len: usize,
}

impl fmt::Display for RamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "access outside guest RAM: {:#x}+{:#x}", self.addr, self.len)
    }
}

/// Guest RAM as seen by the devices (DMA on physical addresses).
///
/// An access must lie entirely in RAM, otherwise it fails with no guarantee
/// about partial effects. The engine implements it on top of the CPU memory.
pub trait GuestRam {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError>;
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError>;
}

/// Little endian reads and writes on top of [`GuestRam`].
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

/// Contiguous RAM in a `Vec`, starting at `base`. Used by tests and by whoever
/// has no memory of its own.
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

// ---- Device ----------------------------------------------------------------

/// Context of [`VirtioDevice::service`]: the device's queues, RAM and
/// the negotiated features. The fields are public so that queue and RAM
/// can be borrowed together.
pub struct ServiceCtx<'a> {
    pub queues: &'a mut [Virtqueue],
    pub ram: &'a mut dyn GuestRam,
    pub features: u64,
    config_changed: bool,
}

impl ServiceCtx<'_> {
    /// Signals a configuration change to the driver (ConfigGeneration
    /// advances and the configuration interrupt fires).
    pub fn config_changed(&mut self) {
        self.config_changed = true;
    }
}

/// virtio device behind the transport. The queues belong to the
/// transport; the device uses them only inside `service`.
pub trait VirtioDevice: Any {
    fn device_id(&self) -> u32;
    /// Device-specific features (bits 0..23). The transport ones
    /// (VERSION_1, INDIRECT_DESC, EVENT_IDX) are added by
    /// [`VirtioMmio`].
    fn features(&self) -> u64;
    /// Maximum size of each queue; the length is the number of queues.
    fn queue_max_sizes(&self) -> &[u16];
    /// Read from the configuration space; past the end it reads 0.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// Write to the configuration space (normally ignored).
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// The driver accepted these features (FEATURES_OK). `false` rejects
    /// the combination and FEATURES_OK does not stay set.
    fn negotiate(&mut self, _features: u64) -> bool {
        true
    }
    /// Device reset (Status = 0): forgets the requests in progress.
    fn reset(&mut self) {}
    /// Consumes the queues and the backends' data. Called only with DRIVER_OK.
    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError>;
    /// Device state for snapshots (M6, ADR 0015): everything that
    /// is not configuration fixed at construction, in-flight requests and
    /// backend state included. The queues are saved by the transport.
    fn save_state(&self, w: &mut vetro_snapshot::Writer);
    /// Brings back to the saved state a device built with the same
    /// configuration (and with its external backends already connected).
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()>;
}

/// Reads `data.len()` bytes from the configuration structure `cfg`
/// starting at `offset`; bytes past the end read 0.
pub(crate) fn read_config_bytes(cfg: &[u8], offset: u64, data: &mut [u8]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = usize::try_from(offset).ok().and_then(|o| cfg.get(o + i)).copied().unwrap_or(0);
    }
}

// ---- Empty slot ------------------------------------------------------------

/// virtio-mmio slot without a device, like QEMU virt's free ones:
/// MagicValue and Version are valid and DeviceID is 0, so the Linux driver
/// recognises the transport and skips it. [`VirtioMmio::empty`] behaves
/// the same way and can also accept a device later.
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
