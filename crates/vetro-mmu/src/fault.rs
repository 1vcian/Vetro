//! MMU faults and their encoding in ESR_EL1, FAR_EL1 and PAR_EL1.

use vetro_cpu::{Access, MemFault};

use crate::walk::BusError;

/// Exception Class (ESR_ELx.EC) of the aborts.
pub mod ec {
    /// Instruction Abort from a lower level (EL0 → EL1).
    pub const INSN_ABORT_LOWER: u64 = 0x20;
    /// Instruction Abort without a change of level.
    pub const INSN_ABORT_SAME: u64 = 0x21;
    /// Data Abort from a lower level.
    pub const DATA_ABORT_LOWER: u64 = 0x24;
    /// Data Abort without a change of level.
    pub const DATA_ABORT_SAME: u64 = 0x25;
}

/// Fault type; the number is the lookup level (0-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// Physical address (base in TTBR, table or output) beyond the configured
    /// size, or VA beyond PARange with the MMU off.
    AddressSize(u8),
    /// Invalid descriptor, VA outside both halves, walk disabled
    /// (EPDx) or block at a level that does not allow it.
    Translation(u8),
    /// Descriptor with AF = 0 (no hardware update in ARMv8.0).
    AccessFlag(u8),
    /// Access denied by AP, UXN, PXN or WXN.
    Permission(u8),
    /// Unaligned data access on Device memory (or DC ZVA on Device),
    /// checked after the walk and before the permissions.
    Alignment,
    /// Synchronous external abort while reading a descriptor.
    ExternalWalk(u8, BusError),
    /// Synchronous external abort on the final access (physical address where
    /// nobody responds).
    External(BusError),
    /// Architecturally valid configuration that Vetro does not implement
    /// (64 KiB granule, big-endian descriptors). It is not an architectural
    /// fault: it must be reported as a limitation, not delivered to the guest.
    Unimplemented(&'static str),
}

impl FaultKind {
    /// DFSC/IFSC code (ESR_ELx.ISS[5:0], PAR_EL1.FST). `None` for
    /// [`FaultKind::Unimplemented`].
    pub fn fsc(self) -> Option<u8> {
        Some(match self {
            FaultKind::AddressSize(l) => l,
            FaultKind::Translation(l) => 0b00_0100 | l,
            FaultKind::AccessFlag(l) => 0b00_1000 | l,
            FaultKind::Permission(l) => 0b00_1100 | l,
            FaultKind::Alignment => 0b10_0001,
            FaultKind::External(_) => 0b01_0000,
            FaultKind::ExternalWalk(l, _) => 0b01_0100 | l,
            FaultKind::Unimplemented(_) => return None,
        })
    }

    /// Lookup level, if the fault has one.
    pub fn level(self) -> Option<u8> {
        match self {
            FaultKind::AddressSize(l)
            | FaultKind::Translation(l)
            | FaultKind::AccessFlag(l)
            | FaultKind::Permission(l)
            | FaultKind::ExternalWalk(l, _) => Some(l),
            FaultKind::External(_) | FaultKind::Alignment | FaultKind::Unimplemented(_) => None,
        }
    }

    /// EA bit (External abort type). Like QEMU: 1 for a slave error, 0 for
    /// a decode error and for faults that are not external aborts.
    pub fn ea(self) -> bool {
        matches!(self, FaultKind::External(BusError::Slave) | FaultKind::ExternalWalk(_, BusError::Slave))
    }
}

/// A failed access, with everything needed to build the syndrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub kind: FaultKind,
    /// Virtual address that caused the fault, tag included (goes into
    /// FAR_EL1). For an access straddling two pages it is the first byte
    /// of the page that fails, as in QEMU.
    pub va: u64,
    pub access: Access,
    /// Privilege used for the permission check (0 or 1).
    pub el: u8,
}

impl Fault {
    /// Value of FAR_EL1.
    pub fn far(&self) -> u64 {
        self.va
    }

    /// Value of ESR_EL1 for the exception taken to EL1 from `from_el`
    /// (PSTATE.EL at the time of the access: differs from `el` for LDTR/STTR).
    /// ISV = 0 like QEMU for stage 1 aborts; IL = 1 (RES1 for these
    /// aborts). CM (bit 8, cache maintenance) is added by whoever executes DC.
    pub fn esr(&self, from_el: u8) -> Option<u64> {
        let fsc = u64::from(self.kind.fsc()?);
        let lower = from_el == 0;
        let ea = u64::from(self.kind.ea()) << 9;
        let (class, iss) = match self.access {
            Access::Fetch => (if lower { ec::INSN_ABORT_LOWER } else { ec::INSN_ABORT_SAME }, ea | fsc),
            Access::Read => (if lower { ec::DATA_ABORT_LOWER } else { ec::DATA_ABORT_SAME }, ea | fsc),
            Access::Write => {
                (if lower { ec::DATA_ABORT_LOWER } else { ec::DATA_ABORT_SAME }, ea | 1 << 6 | fsc)
            }
        };
        Some(class << 26 | 1 << 25 | iss)
    }

    /// Value of PAR_EL1 after a failing AT: F = 1, FST, bit 11 (RES1)
    /// set to 1 like QEMU. PTW and S are 0 (no stage 2).
    pub fn par(&self) -> Option<u64> {
        Some(1 << 11 | u64::from(self.kind.fsc()?) << 1 | 1)
    }

    /// Reduced form for the `vetro_cpu::Memory` interface.
    pub fn mem_fault(&self) -> MemFault {
        MemFault { addr: self.va, access: self.access }
    }
}
