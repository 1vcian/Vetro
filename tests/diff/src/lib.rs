//! Harness differenziale contro QEMU.
//!
//! - [`a64`]: codifica di poche istruzioni AArch64, quanto basta per i test.
//! - [`elf`]: costruzione di ELF64 statici minimi, senza cross-compilatore.
//! - [`qemu`]: individua ed esegue `qemu-aarch64` (l'oracolo).

pub mod a64;
pub mod elf;
pub mod qemu;
