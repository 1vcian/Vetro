//! Vetro's virt platform: MMIO bus, GICv3, generic timer, PL011 UART,
//! PL031 RTC, PL061 GPIO (power key), virtio-mmio (blk, net, console, gpu, input, vsock) and device
//! tree generator.
//!
//! M3 groundwork: the memory map follows QEMU's `virt` machine, so the
//! same kernel and the same device tree run on both.
//! Interfaces and invariants in `docs/specs/platform.md`.
//!
//! Determinism: no device reads the host clock. Time
//! (timer counter, RTC seconds) always comes in as an argument.

pub mod bus;
pub mod fdt;
pub mod gic;
pub mod map;
pub mod pl011;
pub mod pl031;
pub mod pl061;
pub mod timer;
pub mod virt;
pub mod virtio;

pub use bus::{Bus, BusError, DeviceId, MmioDevice};
pub use fdt::{FdtBuilder, FdtError, VirtDtbConfig, virt_dtb};
pub use gic::Gic;
pub use pl011::Pl011;
pub use pl031::Pl031;
pub use pl061::Pl061;
pub use timer::{GenericTimer, TimerChannel};
pub use virt::{Virt, VirtioSlotError};
pub use virtio::{GuestRam, VirtioDevice, VirtioMmio, VirtioMmioEmpty};
