//! MMU di Vetro: traduzione stage 1 AArch64 del regime EL1&0, TLB software
//! e adattatore verso [`vetro_cpu::Memory`].
//!
//! Riferimento: Arm ARM, capitolo D8 (VMSAv8-64), livello ARMv8.0 con i
//! parametri della Cortex-A53 (ADR 0005). Granulo 4 KiB, niente stage 2,
//! niente EL2/EL3. Interfaccia e limiti in `docs/specs/mmu.md`.
//!
//! Pezzi:
//! - [`MmuRegs`]: SCTLR_EL1, TCR_EL1, TTBR0_EL1, TTBR1_EL1, MAIR_EL1.
//! - [`PhysMemory`]: la memoria fisica su cui si leggono i descrittori.
//! - [`Mmu`]: traduzione con TLB ([`Mmu::translate`]) o senza
//!   ([`Mmu::walk`]), invalidazioni TLBI ([`Mmu::tlbi`]).
//! - [`Fault`]: fault con codifiche DFSC/IFSC, ESR, FAR e PAR.
//! - [`VirtMemory`]: implementa [`vetro_cpu::Memory`] sopra MMU e memoria
//!   fisica (un solo privilegio, fault restituiti al chiamante).
//! - [`MmuBus`]: implementa [`vetro_cpu::SysBus`] per la modalità sistema
//!   della CPU (ADR 0007).

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
pub use tlb::{Tlb, TlbiOp};
pub use walk::{BusError, Perms, PhysMemory, Translation};
