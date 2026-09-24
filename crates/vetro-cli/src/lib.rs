//! Esecuzione nativa di programmi Linux arm64 statici in user mode.
//!
//! M1: caricatore ELF e le poche syscall che servono ai test (write, exit).
//! Le syscall vere arrivano con M2.

pub mod elf;
pub mod user;
