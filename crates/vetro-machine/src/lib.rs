//! The complete virt machine (M3): an AArch64 CPU in system mode
//! (`vetro-cpu`), the stage 1 MMU (`vetro-mmu`), the virt platform
//! (`vetro-platform`) and RAM, tied together by an execution loop with
//! deterministic time (ADR 0011).
//!
//! - [`boot`]: where the kernel, initramfs and device tree go, and how the
//!   CPU starts (like QEMU's `hw/arm/boot.c`).
//! - [`android`]: the Android bootloader's work (`boot.img`,
//!   `vendor_boot.img`, `init_boot.img`, bootconfig) in front of [`boot`]
//!   ([`Machine::load_android`]).
//! - [`Machine`]: construction, loading a Linux kernel, execution in
//!   quanta ([`Machine::run`]), PL011 console, the M5 virtio devices
//!   ([`Devices`]: GPU, keyboard, tablet or touchscreen, network, vsock) with
//!   host access ([`Machine::gpu`], [`Machine::keyboard`],
//!   [`Machine::pointer`], [`Machine::net`], [`Machine::vsock`]).
//! - [`net`]: virtio-net connected to the `vetro-net` stack (gateway like
//!   QEMU's user network, sinkhole), in the machine's virtual time.
//!
//! - [`record`]: record & replay (M10, ADR 0019): every host input goes
//!   through [`Machine::input`] and is recorded with the instruction number;
//!   replay reapplies it at the same instruction, and [`Machine::goto`]
//!   brings the machine back to any instruction of the recording.
//!
//! - [`hooks`] and [`introspect`]: introspection of the guest from outside
//!   (ADR 0027): EL0 syscalls and invisible breakpoints observed by the
//!   machine loop without changing execution, and reading the Linux
//!   kernel (processes, maps, files) with `vetro-analysis`.
//!
//! With the JIT ([`Machine::set_jit`], ADR 0012 and 0013) translated blocks
//! alternate with the interpreter between one platform event and the next:
//! same instruction count, interrupts at the same points.
//!
//! Guest time is the number of executed instructions: CNTPCT advances by 5
//! every 8 instructions, i.e. 62.5 MHz with a nominal 100 MHz CPU (the same
//! step as the user mode layer, ADR 0010). A WFI with no pending interrupts
//! jumps straight to the next timer deadline.

pub mod analysis;
pub mod android;
mod board;
pub mod boot;
pub mod files;
pub mod hooks;
pub mod introspect;
mod machine;
pub mod net;
pub mod profile;
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
