//! Memory map and interrupt numbers of the virt platform.
//!
//! Same addresses and same lines as QEMU's `virt` machine
//! (hw/arm/virt.c), limited to what Vetro emulates.

/// GICv3 distributor (GICD), 64 KiB.
pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICD_SIZE: u64 = 0x1_0000;
/// GICv3 redistributor (GICR): two 64 KiB frames (RD and SGI) per CPU.
pub const GICR_BASE: u64 = 0x080A_0000;
pub const GICR_SIZE_PER_CPU: u64 = 0x2_0000;
/// PL011 UART.
pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x1000;
/// PL031 RTC.
pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SIZE: u64 = 0x1000;
/// PL061 GPIO (line 3: power key, `gpio-keys`).
pub const GPIO_BASE: u64 = 0x0903_0000;
pub const GPIO_SIZE: u64 = 0x1000;
/// virtio-mmio transports: 32 slots of 0x200 bytes.
pub const VIRTIO_BASE: u64 = 0x0A00_0000;
pub const VIRTIO_SLOT_SIZE: u64 = 0x200;
pub const VIRTIO_SLOTS: u64 = 32;
/// Start of guest RAM.
pub const RAM_BASE: u64 = 0x4000_0000;

/// First SPI INTID: SPI `n` has INTID `SPI_BASE + n`.
pub const SPI_BASE: u32 = 32;
/// UART SPI (INTID 33).
pub const UART_SPI: u32 = 1;
/// RTC SPI (INTID 34).
pub const RTC_SPI: u32 = 2;
/// GPIO SPI (INTID 39).
pub const GPIO_SPI: u32 = 7;
/// SPI of the first virtio-mmio slot (INTID 48); slot `k` uses `16 + k`.
pub const VIRTIO_SPI_BASE: u32 = 16;

/// Virtual timer PPI (INTID 27).
pub const PPI_VTIMER: u32 = 27;
/// Non-secure physical timer PPI (INTID 30).
pub const PPI_PTIMER: u32 = 30;
/// Secure physical timer PPI (INTID 29): device tree only.
pub const PPI_SEC_PTIMER: u32 = 29;
/// Hypervisor timer PPI (INTID 26): device tree only.
pub const PPI_HYP_TIMER: u32 = 26;

/// Default CNTFRQ_EL0 frequency (62.5 MHz, like QEMU with cortex-a53).
pub const CNTFRQ_HZ: u32 = 62_500_000;
/// Fixed PL011 clock declared in the device tree (24 MHz, like QEMU).
pub const UART_CLOCK_HZ: u32 = 24_000_000;
