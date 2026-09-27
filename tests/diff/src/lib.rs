//! Differential harness against QEMU.
//!
//! - [`a64`]: encoding of a few AArch64 instructions, for prologues and epilogues.
//! - [`elf`]: construction of minimal static ELF64s, without a cross-compiler.
//! - [`qemu`]: finds and runs `qemu-aarch64` (the oracle).
//! - [`harness`]: programs with a known initial state and a dump of the final state,
//!   run on Vetro and on QEMU.
//! - [`random`]: random programs of integer instructions (ADR 0006).

pub mod a64;
pub mod elf;
pub mod harness;
pub mod qemu;
pub mod random;
pub mod rng;
