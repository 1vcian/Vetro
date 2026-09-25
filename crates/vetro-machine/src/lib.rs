//! La macchina virt completa (M3): una CPU AArch64 in modalità sistema
//! (`vetro-cpu`), la MMU stage 1 (`vetro-mmu`), la piattaforma virt
//! (`vetro-platform`) e la RAM, legate da un ciclo di esecuzione con tempo
//! deterministico (ADR 0011).
//!
//! - [`boot`]: dove vanno kernel, initramfs e device tree, e come parte la
//!   CPU (come `hw/arm/boot.c` di QEMU).
//! - [`Machine`]: costruzione, caricamento di un kernel Linux, esecuzione a
//!   quanti ([`Machine::run`]), console PL011, dispositivi virtio di M5
//!   ([`Devices`]: GPU, tastiera, tablet o touchscreen, vsock) con l'accesso
//!   dell'host ([`Machine::gpu`], [`Machine::keyboard`], [`Machine::pointer`],
//!   [`Machine::vsock`]).
//!
//! Il tempo del guest è il numero di istruzioni eseguite: CNTPCT avanza di 5
//! ogni 8 istruzioni, cioè 62,5 MHz con una CPU nominale da 100 MHz (lo stesso
//! passo del livello user mode, ADR 0010). Una WFI senza interrupt pronti
//! salta direttamente alla prossima scadenza del timer.

mod board;
pub mod boot;
mod machine;
mod psci;

pub use board::Board;
pub use machine::{Devices, Machine, MachineConfig, Pointer, Slots, Stop};
