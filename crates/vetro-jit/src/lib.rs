//! Block translation from AArch64 to WASM modules (M4, ADR 0012).
//!
//! - [`wasm`]: minimal encoder of WebAssembly modules;
//! - [`state`]: [`JitState`], the state shared with the blocks;
//! - [`translate`]: from regions of decoded instructions to WASM functions,
//!   the runtime module and the dispatcher (ADR 0024);
//! - [`engine`]: the [`Engine`] and [`Host`] traits (ABI in docs/specs/jit.md);
//! - [`helper`]: `env.simd`, the SIMD/FP instructions executed
//!   by the interpreter inside the regions (ADR 0026);
//! - [`profile`]: interpreter instructions by class, for measurements;
//! - [`driver`]: [`JitCpu`], which in user mode alternates translated blocks
//!   and interpreter steps, with cache and invalidation;
//! - [`sys`]: [`SysJit`], the system-mode blocks (MMU, physical
//!   pages, chaining, software TLB), which the machine alternates
//!   with the interpreter.
//!
//! No external dependencies: it also compiles for `wasm32-unknown-unknown`.

pub mod driver;
pub mod engine;
pub mod helper;
pub mod profile;
pub mod state;
pub mod sys;
pub mod translate;
pub mod wasm;

pub use driver::{JitConfig, JitCpu, JitStats};
pub use engine::{Engine, Host};
pub use profile::Profile;
pub use state::JitState;
pub use sys::{Clock, Next, SysJit, SysJitConfig, SysJitDyn, SysJitStats, SysPhys, SysRun};

/// Exit codes of a block (docs/specs/jit.md).
pub const NEXT: u32 = 0;
pub const FAULT: u32 = 1;
pub const STOP: u32 = 2;
pub const SVC: u32 = 3;
/// The block unmasked interrupts (MSR DAIF/DAIFClr): `pc` is the
/// next instruction and the host must check interrupts again before
/// continuing (system mode, ADR 0024).
pub const YIELD: u32 = 4;
