//! CPU AArch64 di Vetro: stato, decoder e interprete di riferimento.
//!
//! Livello ARMv8.0-A (ADR 0005), solo AArch64. Modalità utente (EL0, M1/M2)
//! e modalità sistema (EL0/EL1 con MMU ed eccezioni, M3, ADR 0009).
//! Interfaccia e invarianti in `docs/specs/cpu.md`.

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
