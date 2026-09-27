//! Introspection of the guest operating system from the outside (ADR 0027):
//! the common base of the TLS hooks (M7), the Binder decoder (M8) and of ART
//! and scripting (M9).
//!
//! Everything is read from the guest's physical memory, with no agents or
//! modules in the guest and without writing to it: the guest cannot notice.
//!
//! - [`btf`]: kernel types from BTF (structure offsets);
//! - [`kallsyms`]: kernel symbols from `System.map` or from the kallsyms
//!   table inside the `Image`;
//! - [`layout`]: the offsets we need, from BTF;
//! - [`mem`]: physical memory and translation through the page tables;
//! - [`linux`]: processes, threads, mappings, open files, page cache;
//! - [`elf`]: user-space symbols (file or memory);
//! - [`strace`]: traced and decoded syscalls;
//! - [`binder`]: commands and transactions of `BINDER_WRITE_READ`;
//! - [`parcel`], [`aidl`], [`ipc`], [`privacy`]: M8's Binder decoder
//!   (Parcel header, AIDL method names from the image, calls with sender
//!   and receiver, sensitive accesses).
//!
//! Interface: `docs/specs/introspection.md`.
//!
//! Interface: `docs/specs/introspection.md`.

pub mod aidl;
pub mod binder;
pub mod btf;
pub mod elf;
pub mod ipc;
pub mod kallsyms;
pub mod layout;
pub mod linux;
pub mod mem;
pub mod parcel;
pub mod privacy;
pub mod strace;

pub use btf::Btf;
pub use ipc::{BinderCall, BinderLog, Party};
pub use kallsyms::{KSym, Symbols};
pub use layout::Layout;
pub use linux::{CpuRegs, Kernel, Linux, OpenFile, Task, Vma};
pub use mem::{PhysMem, Space};
pub use strace::SyscallRecord;
