//! CPU AArch64 di Vetro: stato, decoder e interprete di riferimento.
//!
//! Livello ARMv8.0-A (ADR 0005), solo AArch64. In M1: istruzioni intere a
//! EL0. Interfaccia e invarianti in `docs/specs/cpu.md`.

pub mod bits;
pub mod decode;
mod exec;
pub mod mem;
pub mod state;

pub use decode::{Insn, decode};
pub use exec::Exception;
pub use mem::{Access, MemFault, Memory, Perm, UserMemory};
pub use state::Cpu;
