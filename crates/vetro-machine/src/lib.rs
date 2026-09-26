//! La macchina virt completa (M3): una CPU AArch64 in modalità sistema
//! (`vetro-cpu`), la MMU stage 1 (`vetro-mmu`), la piattaforma virt
//! (`vetro-platform`) e la RAM, legate da un ciclo di esecuzione con tempo
//! deterministico (ADR 0011).
//!
//! - [`boot`]: dove vanno kernel, initramfs e device tree, e come parte la
//!   CPU (come `hw/arm/boot.c` di QEMU).
//! - [`android`]: il lavoro del bootloader Android (`boot.img`,
//!   `vendor_boot.img`, `init_boot.img`, bootconfig) davanti a [`boot`]
//!   ([`Machine::load_android`]).
//! - [`Machine`]: costruzione, caricamento di un kernel Linux, esecuzione a
//!   quanti ([`Machine::run`]), console PL011, dispositivi virtio di M5
//!   ([`Devices`]: GPU, tastiera, tablet o touchscreen, rete, vsock) con
//!   l'accesso dell'host ([`Machine::gpu`], [`Machine::keyboard`],
//!   [`Machine::pointer`], [`Machine::net`], [`Machine::vsock`]).
//! - [`net`]: virtio-net collegato allo stack di `vetro-net` (gateway come la
//!   rete user di QEMU, sinkhole), nel tempo virtuale della macchina.
//!
//! - [`record`]: record & replay (M10, ADR 0019): ogni ingresso dell'host
//!   passa da [`Machine::input`] e si registra con il numero d'istruzione;
//!   il replay lo riapplica alla stessa istruzione, e [`Machine::goto`]
//!   riporta la macchina a un'istruzione qualsiasi della registrazione.
//!
//! - [`hooks`] e [`introspect`]: introspezione del guest dall'esterno
//!   (ADR 0027): syscall di EL0 e punti d'arresto invisibili osservati dal
//!   ciclo della macchina senza cambiare l'esecuzione, e la lettura del
//!   kernel Linux (processi, mappe, file) con `vetro-analysis`.
//!
//! Col JIT ([`Machine::set_jit`], ADR 0012 e 0013) i blocchi tradotti si
//! alternano all'interprete fra un evento della piattaforma e l'altro: stesso
//! numero di istruzioni, interrupt negli stessi punti.
//!
//! Il tempo del guest è il numero di istruzioni eseguite: CNTPCT avanza di 5
//! ogni 8 istruzioni, cioè 62,5 MHz con una CPU nominale da 100 MHz (lo stesso
//! passo del livello user mode, ADR 0010). Una WFI senza interrupt pronti
//! salta direttamente alla prossima scadenza del timer.

pub mod analysis;
pub mod android;
mod board;
pub mod boot;
pub mod files;
pub mod hooks;
pub mod introspect;
mod machine;
pub mod net;
mod psci;
pub mod record;
pub mod tls;

pub use board::Board;
pub use files::FilesClient;
pub use hooks::{Breakpoint, Event, GuestView, SyscallEntry, Tracer};
pub use machine::{Devices, Machine, MachineConfig, Pointer, RecordOptions, Slots, Stop};
pub use net::{FrameDir, NetLink, NetSetup, TappedFrame};
pub use record::{Digest, Divergence, HostNetOp, Input, Log, ReplayStatus, Reply, VsockOp};
pub use vetro_analysis;
pub use vetro_jit::{SysJitDyn, SysJitStats};
pub use vetro_net;
pub use vetro_snapshot;
