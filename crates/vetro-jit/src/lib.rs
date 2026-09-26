//! Traduzione a blocchi da AArch64 verso moduli WASM (M4, ADR 0012).
//!
//! - [`wasm`]: encoder minimo di moduli WebAssembly;
//! - [`state`]: [`JitState`], lo stato condiviso con i blocchi;
//! - [`translate`]: da regioni di istruzioni decodificate a funzioni WASM,
//!   il modulo di runtime e il dispatcher (ADR 0024);
//! - [`engine`]: i trait [`Engine`] e [`Host`] (ABI in docs/specs/jit.md);
//! - [`driver`]: [`JitCpu`], che in modalità utente alterna blocchi tradotti
//!   e passi dell'interprete, con cache e invalidazione;
//! - [`sys`]: [`SysJit`], i blocchi della modalità sistema (MMU, pagine
//!   fisiche, concatenamento, TLB software), che la macchina alterna
//!   all'interprete.
//!
//! Nessuna dipendenza esterna: compila anche per `wasm32-unknown-unknown`.

pub mod driver;
pub mod engine;
pub mod state;
pub mod sys;
pub mod translate;
pub mod wasm;

pub use driver::{JitConfig, JitCpu, JitStats};
pub use engine::{Engine, Host};
pub use state::JitState;
pub use sys::{Next, SysJit, SysJitConfig, SysJitDyn, SysJitStats, SysPhys, SysRun};

/// Codici d'uscita di un blocco (docs/specs/jit.md).
pub const NEXT: u32 = 0;
pub const FAULT: u32 = 1;
pub const STOP: u32 = 2;
pub const SVC: u32 = 3;
/// Il blocco ha smascherato interrupt (MSR DAIF/DAIFClr): `pc` è
/// l'istruzione successiva e l'host deve ricontrollare gli interrupt prima
/// di continuare (modalità sistema, ADR 0024).
pub const YIELD: u32 = 4;
