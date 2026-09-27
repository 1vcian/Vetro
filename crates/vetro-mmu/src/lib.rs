//! Vetro's MMU: AArch64 stage 1 translation of the EL1&0 regime, software TLB
//! and adapter to [`vetro_cpu::Memory`].
//!
//! Reference: Arm ARM, chapter D8 (VMSAv8-64), ARMv8.0 level with the
//! Cortex-A53 parameters (ADR 0005). 4 KiB granule, no stage 2,
//! no EL2/EL3. Interface and limits in `docs/specs/mmu.md`.
//!
//! Pieces:
//! - [`MmuRegs`]: SCTLR_EL1, TCR_EL1, TTBR0_EL1, TTBR1_EL1, MAIR_EL1.
//! - [`PhysMemory`]: the physical memory the descriptors are read from.
//! - [`Mmu`]: translation with TLB ([`Mmu::translate`]) or without
//!   ([`Mmu::walk`]), TLBI invalidations ([`Mmu::tlbi`]).
//! - [`Fault`]: faults with DFSC/IFSC encodings, ESR, FAR and PAR.
//! - [`VirtMemory`]: implements [`vetro_cpu::Memory`] on top of the MMU and physical
//!   memory (a single privilege, faults returned to the caller).
//! - [`MmuBus`]: implements [`vetro_cpu::SysBus`] for the CPU's system
//!   mode (ADR 0009).

mod adapter;
mod bus;
mod fault;
mod mmu;
mod regs;
mod tlb;
mod walk;

#[cfg(test)]
mod tests;

pub use adapter::VirtMemory;
pub use bus::MmuBus;
pub use fault::{Fault, FaultKind, ec};
pub use mmu::Mmu;
pub use regs::{MmuRegs, PAGE_SIZE, sctlr, tcr};
pub use tlb::{Inval, Tlb, TlbiOp};
pub use walk::{BusError, Perms, PhysMemory, Translation};
