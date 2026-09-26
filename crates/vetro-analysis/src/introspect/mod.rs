//! Introspezione del sistema operativo guest dall'esterno (ADR 0027): la
//! base comune degli hook TLS (M7), del decoder Binder (M8) e di ART e
//! dello scripting (M9).
//!
//! Tutto si legge dalla memoria fisica del guest, senza agenti né moduli
//! nel guest e senza scriverci: il guest non può accorgersene.
//!
//! - [`btf`]: tipi del kernel dal BTF (offset delle strutture);
//! - [`kallsyms`]: simboli del kernel da `System.map` o dalla tabella
//!   kallsyms dentro l'`Image`;
//! - [`layout`]: gli offset che servono, dal BTF;
//! - [`mem`]: memoria fisica e traduzione con le tabelle delle pagine;
//! - [`linux`]: processi, thread, mappe, file aperti, page cache;
//! - [`elf`]: simboli dello spazio utente (file o memoria);
//! - [`strace`]: syscall tracciate e decodificate;
//! - [`binder`]: comandi e transazioni di `BINDER_WRITE_READ`.
//!
//! Interfaccia: `docs/specs/introspection.md`.

pub mod binder;
pub mod btf;
pub mod elf;
pub mod kallsyms;
pub mod layout;
pub mod linux;
pub mod mem;
pub mod strace;

pub use btf::Btf;
pub use kallsyms::{KSym, Symbols};
pub use layout::Layout;
pub use linux::{CpuRegs, Kernel, Linux, OpenFile, Task, Vma};
pub use mem::{PhysMem, Space};
pub use strace::SyscallRecord;
