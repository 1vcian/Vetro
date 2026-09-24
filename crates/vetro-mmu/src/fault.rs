//! Fault della MMU e loro codifica in ESR_EL1, FAR_EL1 e PAR_EL1.

use vetro_cpu::{Access, MemFault};

use crate::walk::BusError;

/// Exception Class (ESR_ELx.EC) degli abort.
pub mod ec {
    /// Instruction Abort da un livello inferiore (EL0 → EL1).
    pub const INSN_ABORT_LOWER: u64 = 0x20;
    /// Instruction Abort senza cambio di livello.
    pub const INSN_ABORT_SAME: u64 = 0x21;
    /// Data Abort da un livello inferiore.
    pub const DATA_ABORT_LOWER: u64 = 0x24;
    /// Data Abort senza cambio di livello.
    pub const DATA_ABORT_SAME: u64 = 0x25;
}

/// Tipo di fault; il numero è il livello di lookup (0-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// Indirizzo fisico (base in TTBR, tabella o uscita) oltre la dimensione
    /// configurata, oppure VA oltre PARange a MMU spenta.
    AddressSize(u8),
    /// Descrittore non valido, VA fuori dalle due metà, walk disabilitato
    /// (EPDx) o blocco a un livello che non lo ammette.
    Translation(u8),
    /// Descrittore con AF = 0 (niente aggiornamento hardware in ARMv8.0).
    AccessFlag(u8),
    /// Accesso negato da AP, UXN, PXN o WXN.
    Permission(u8),
    /// Accesso ai dati non allineato su memoria Device (o DC ZVA su Device),
    /// controllato dopo il walk e prima dei permessi.
    Alignment,
    /// Abort esterno sincrono leggendo un descrittore.
    ExternalWalk(u8, BusError),
    /// Abort esterno sincrono sull'accesso finale (indirizzo fisico dove non
    /// risponde nessuno).
    External(BusError),
    /// Configurazione architetturalmente valida che Vetro non implementa
    /// (granulo 64 KiB, descrittori big-endian). Non è un fault
    /// architetturale: va segnalato come limite, non consegnato al guest.
    Unimplemented(&'static str),
}

impl FaultKind {
    /// Codice DFSC/IFSC (ESR_ELx.ISS[5:0], PAR_EL1.FST). `None` per
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

    /// Livello di lookup, se il fault ne ha uno.
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

    /// Bit EA (External abort type). Come QEMU: 1 per uno slave error, 0 per
    /// un decode error e per i fault che non sono abort esterni.
    pub fn ea(self) -> bool {
        matches!(self, FaultKind::External(BusError::Slave) | FaultKind::ExternalWalk(_, BusError::Slave))
    }
}

/// Un accesso fallito, con tutto ciò che serve a costruire la sindrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub kind: FaultKind,
    /// Indirizzo virtuale che ha causato il fault, tag compreso (va in
    /// FAR_EL1). Per un accesso a cavallo di due pagine è il primo byte
    /// della pagina che fallisce, come in QEMU.
    pub va: u64,
    pub access: Access,
    /// Privilegio usato per il controllo dei permessi (0 o 1).
    pub el: u8,
}

impl Fault {
    /// Valore di FAR_EL1.
    pub fn far(&self) -> u64 {
        self.va
    }

    /// Valore di ESR_EL1 per l'eccezione presa a EL1 da `from_el`
    /// (PSTATE.EL al momento dell'accesso: differisce da `el` per LDTR/STTR).
    /// ISV = 0 come QEMU per gli abort stage 1; IL = 1 (RES1 per questi
    /// abort). CM (bit 8, manutenzione cache) lo aggiunge chi esegue DC.
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

    /// Valore di PAR_EL1 dopo un AT che fallisce: F = 1, FST, bit 11 (RES1)
    /// a 1 come QEMU. PTW e S sono 0 (niente stage 2).
    pub fn par(&self) -> Option<u64> {
        Some(1 << 11 | u64::from(self.kind.fsc()?) << 1 | 1)
    }

    /// Forma ridotta per l'interfaccia `vetro_cpu::Memory`.
    pub fn mem_fault(&self) -> MemFault {
        MemFault { addr: self.va, access: self.access }
    }
}
