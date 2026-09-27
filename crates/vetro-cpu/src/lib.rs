//! Vetro's AArch64 CPU: state, decoder and reference interpreter.
//!
//! ARMv8.0-A level (ADR 0005), AArch64 only. User mode (EL0, M1/M2)
//! and system mode (EL0/EL1 with MMU and exceptions, M3, ADR 0009).
//! Interface and invariants in `docs/specs/cpu.md`.

pub mod bits;
pub mod decode;
pub(crate) mod exec;
pub mod mem;
pub mod simd;
pub mod snapshot;
pub mod state;
pub mod sys;
pub mod sysreg;

pub use decode::{Insn, decode};
pub use exec::Exception;
pub use mem::{Access, MemFault, Memory, Perm, UserMemory};
pub use state::Cpu;
pub use sys::{CpuEnv, SysBus, SysConfig, SysEvent};
