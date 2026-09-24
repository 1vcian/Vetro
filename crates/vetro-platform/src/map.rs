//! Mappa della memoria e numeri di interrupt della piattaforma virt.
//!
//! Stessi indirizzi e stesse linee della macchina `virt` di QEMU
//! (hw/arm/virt.c), limitati a ciò che Vetro emula.

/// Distributore GICv3 (GICD), 64 KiB.
pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICD_SIZE: u64 = 0x1_0000;
/// Redistributore GICv3 (GICR): due frame da 64 KiB (RD e SGI) per CPU.
pub const GICR_BASE: u64 = 0x080A_0000;
pub const GICR_SIZE_PER_CPU: u64 = 0x2_0000;
/// UART PL011.
pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x1000;
/// RTC PL031.
pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SIZE: u64 = 0x1000;
/// Trasporti virtio-mmio: 32 slot da 0x200 byte.
pub const VIRTIO_BASE: u64 = 0x0A00_0000;
pub const VIRTIO_SLOT_SIZE: u64 = 0x200;
pub const VIRTIO_SLOTS: u64 = 32;
/// Inizio della RAM del guest.
pub const RAM_BASE: u64 = 0x4000_0000;

/// Primo INTID degli SPI: lo SPI `n` ha INTID `SPI_BASE + n`.
pub const SPI_BASE: u32 = 32;
/// SPI della UART (INTID 33).
pub const UART_SPI: u32 = 1;
/// SPI dell'RTC (INTID 34).
pub const RTC_SPI: u32 = 2;
/// SPI del primo slot virtio-mmio (INTID 48); lo slot `k` usa `16 + k`.
pub const VIRTIO_SPI_BASE: u32 = 16;

/// PPI del timer virtuale (INTID 27).
pub const PPI_VTIMER: u32 = 27;
/// PPI del timer fisico non sicuro (INTID 30).
pub const PPI_PTIMER: u32 = 30;
/// PPI del timer fisico sicuro (INTID 29): solo per il device tree.
pub const PPI_SEC_PTIMER: u32 = 29;
/// PPI del timer dell'hypervisor (INTID 26): solo per il device tree.
pub const PPI_HYP_TIMER: u32 = 26;

/// Frequenza di default di CNTFRQ_EL0 (62,5 MHz, come QEMU con cortex-a53).
pub const CNTFRQ_HZ: u32 = 62_500_000;
/// Clock fisso della PL011 dichiarato nel device tree (24 MHz, come QEMU).
pub const UART_CLOCK_HZ: u32 = 24_000_000;
