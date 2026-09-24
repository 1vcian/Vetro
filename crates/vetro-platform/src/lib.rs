//! Piattaforma virt di Vetro: bus MMIO, GICv3, timer generico, UART PL011,
//! RTC PL031, slot virtio-mmio vuoti e generatore del device tree.
//!
//! Preparazione di M3: la mappa della memoria ricalca la macchina `virt` di
//! QEMU, così lo stesso kernel e lo stesso device tree girano su entrambi.
//! Interfacce e invarianti in `docs/specs/platform.md`.
//!
//! Determinismo: nessun dispositivo legge l'orologio dell'host. Il tempo
//! (contatore del timer, secondi dell'RTC) entra sempre come argomento.

pub mod bus;
pub mod fdt;
pub mod gic;
pub mod map;
pub mod pl011;
pub mod pl031;
pub mod timer;
pub mod virt;
pub mod virtio;

pub use bus::{Bus, BusError, DeviceId, MmioDevice};
pub use fdt::{FdtBuilder, FdtError, VirtDtbConfig, virt_dtb};
pub use gic::Gic;
pub use pl011::Pl011;
pub use pl031::Pl031;
pub use timer::{GenericTimer, TimerChannel};
pub use virt::Virt;
pub use virtio::VirtioMmioEmpty;
