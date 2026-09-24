//! Harness differenziale contro QEMU.
//!
//! - [`a64`]: codifica di poche istruzioni AArch64, per prologhi ed epiloghi.
//! - [`elf`]: costruzione di ELF64 statici minimi, senza cross-compilatore.
//! - [`qemu`]: individua ed esegue `qemu-aarch64` (l'oracolo).
//! - [`harness`]: programmi con stato iniziale noto e dump dello stato finale,
//!   eseguiti su Vetro e su QEMU.
//! - [`random`]: programmi casuali di istruzioni intere (ADR 0006).

pub mod a64;
pub mod elf;
pub mod harness;
pub mod qemu;
pub mod random;
pub mod rng;
