//! Walk delle tabelle stage 1 (granulo 4 KiB) e controllo dei permessi.
//!
//! Segue lo pseudocodice ARMv8.0 `AArch64.TranslationTableWalk`,
//! `AArch64.TranslateAddressS1Off` e `AArch64.CheckPermission` (Arm ARM,
//! D8 e J1), nello stesso ordine di controlli: così la priorità fra fault
//! diversi è quella architetturale.

use vetro_cpu::Access;

use crate::fault::{Fault, FaultKind};
use crate::regs::{Granule, MmuRegs, PAGE_SIZE, sctlr};

/// Risposta d'errore del bus fisico (abort esterno sincrono).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusError {
    /// Nessuno risponde a quell'indirizzo (decode error).
    Decode,
    /// Il dispositivo ha risposto con un errore (slave error).
    Slave,
}

/// Memoria fisica vista dalla MMU: RAM e dispositivi, little-endian.
pub trait PhysMemory {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError>;

    /// Lettura di un descrittore (8 byte allineati).
    fn read_u64(&mut self, pa: u64) -> Result<u64, BusError> {
        let mut b = [0u8; 8];
        self.read(pa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

/// Permessi finali di una traduzione: quelli della foglia combinati con
/// APTable, UXNTable e PXNTable delle tabelle attraversate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    /// AP[2:1]: bit 1 = AP[2] (sola lettura), bit 0 = AP[1] (accesso da EL0).
    pub ap: u8,
    /// UXN (XN): niente esecuzione a EL0.
    pub uxn: bool,
    /// PXN: niente esecuzione a EL1.
    pub pxn: bool,
}

impl Perms {
    /// Vero se l'accesso è permesso al privilegio `el` (0 o 1).
    /// Una pagina scrivibile da EL0 non è mai eseguibile a EL1.
    pub fn allows(self, access: Access, el: u8, wxn: bool) -> bool {
        let priv_w = self.ap & 0b10 == 0;
        let user_r = self.ap & 0b01 != 0;
        let user_w = self.ap == 0b01;
        if el == 0 {
            match access {
                Access::Read => user_r,
                Access::Write => user_w,
                Access::Fetch => !(self.uxn || (user_w && wxn)),
            }
        } else {
            match access {
                Access::Read => true,
                Access::Write => priv_w,
                Access::Fetch => !(self.pxn || (priv_w && wxn) || user_w),
            }
        }
    }
}

/// Esito di una traduzione riuscita.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    pub pa: u64,
    /// Livello del descrittore foglia (1, 2, 3); 0 a MMU spenta.
    pub level: u8,
    /// Dimensione della pagina o del blocco (4 KiB, 2 MiB, 1 GiB); a MMU
    /// spenta [`PAGE_SIZE`].
    pub block_size: u64,
    /// `None` a MMU spenta, dove ogni accesso è permesso.
    pub perms: Option<Perms>,
    /// AttrIndx del descrittore; `None` a MMU spenta.
    pub attr_index: Option<u8>,
    /// Attributi di memoria in formato MAIR: il byte scelto da AttrIndx, o
    /// quello fissato dall'architettura a MMU spenta (0x00 Device-nGnRnE per
    /// i dati; 0xaa Write-Through o 0x44 Non-cacheable per i fetch, secondo
    /// SCTLR.I).
    pub mair_attr: u8,
    /// Shareability SH[1:0]; 0b10 (Outer Shareable) a MMU spenta.
    pub sh: u8,
    /// Descrittore non globale: vale solo per `asid`.
    pub ng: bool,
    /// ASID corrente al momento del walk.
    pub asid: u16,
}

impl Translation {
    /// Valore di PAR_EL1 dopo un AT riuscito, come lo compone QEMU: ATTR,
    /// PA[47:12], bit 11 (RES1), NS = 1 (regime Non-secure), SH (0b10 per
    /// Device e Normal Non-cacheable).
    pub fn par(&self) -> u64 {
        let attr = u64::from(self.mair_attr);
        let sh = if attr & 0xf0 == 0 || attr == 0x44 || attr == 0x40 { 0b10 } else { u64::from(self.sh) };
        attr << 56 | (self.pa & bits(47, 12)) | 1 << 11 | 1 << 9 | sh << 7
    }
}

/// Maschera dei bit `hi..=lo`.
pub(crate) const fn bits(hi: u32, lo: u32) -> u64 {
    (u64::MAX >> (63 - hi)) & !((1u64 << lo) - 1)
}

/// Maschera dei `n` bit bassi (n < 64).
const fn low(n: u32) -> u64 {
    (1u64 << n) - 1
}

pub(crate) fn fault(kind: FaultKind, va: u64, access: Access, el: u8) -> Fault {
    Fault { kind, va, access, el }
}

/// Traduzione a MMU spenta: identità, con address size fault se la VA
/// (senza il tag, se c'è TBI) supera PARange.
pub(crate) fn mmu_off(
    regs: &MmuRegs,
    pa_bits: u32,
    va: u64,
    access: Access,
    el: u8,
) -> Result<Translation, Fault> {
    let top = regs.addr_top(va);
    if va & bits(top, pa_bits) != 0 {
        return Err(fault(FaultKind::AddressSize(0), va, access, el));
    }
    let mair_attr = match access {
        Access::Fetch if regs.sctlr & sctlr::I != 0 => 0xaa,
        Access::Fetch => 0x44,
        Access::Read | Access::Write => 0x00,
    };
    Ok(Translation {
        pa: va & low(pa_bits),
        level: 0,
        block_size: PAGE_SIZE,
        perms: None,
        attr_index: None,
        mair_attr,
        sh: 0b10,
        ng: false,
        asid: regs.asid(),
    })
}

/// Sceglie la metà dello spazio virtuale (vero = TTBR1) e verifica che i bit
/// fra AddrTop e la dimensione d'ingresso siano tutti uguali al selettore;
/// altrimenti translation fault di livello 0.
pub(crate) fn select(regs: &MmuRegs, va: u64) -> Result<bool, FaultKind> {
    let top = regs.addr_top(va);
    let hi = va >> top & 1 != 0;
    let field = va & bits(top, regs.input_size(hi));
    let ok = if hi { field == bits(top, regs.input_size(hi)) } else { field == 0 };
    if ok { Ok(hi) } else { Err(FaultKind::Translation(0)) }
}

/// Walk delle tabelle per una VA già validata da [`select`]. Non controlla i
/// permessi (lo fa [`check`]), così l'esito si può mettere nel TLB.
pub(crate) fn walk_tables<P: PhysMemory + ?Sized>(
    regs: &MmuRegs,
    pa_bits: u32,
    phys: &mut P,
    va: u64,
    hi: bool,
) -> Result<Translation, FaultKind> {
    const STRIDE: u32 = 9;
    const GRAIN: u32 = 12;
    if regs.walk_disabled(hi) {
        return Err(FaultKind::Translation(0));
    }
    if regs.granule(hi) == Granule::K64 {
        return Err(FaultKind::Unimplemented("granulo di traduzione da 64 KiB"));
    }
    if regs.sctlr & sctlr::EE != 0 {
        return Err(FaultKind::Unimplemented("descrittori big-endian (SCTLR_EL1.EE = 1)"));
    }
    let inputsize = regs.input_size(hi);
    let outputsize = regs.output_size(pa_bits);
    let oversize = |addr: u64| outputsize < 48 && addr & bits(47, outputsize) != 0;
    let ttbr = regs.ttbr(hi);
    if oversize(ttbr) {
        return Err(FaultKind::AddressSize(0));
    }
    // Livello iniziale e allineamento della prima tabella.
    let mut level = 4 - (inputsize - GRAIN).div_ceil(STRIDE);
    let baselowerbound = 3 + inputsize - ((3 - level) * STRIDE + GRAIN);
    let mut base = ttbr & bits(47, baselowerbound);
    let mut addrtop = inputsize - 1;
    // APTable[1:0], UXNTable, PXNTable accumulati lungo il walk.
    let (mut ap_table, mut xn_table, mut pxn_table) = (0u8, false, false);
    let desc = loop {
        let bottom = (3 - level) * STRIDE + GRAIN;
        let index = (va & bits(addrtop, bottom)) >> bottom << 3;
        let desc = phys.read_u64(base | index).map_err(|e| FaultKind::ExternalWalk(level as u8, e))?;
        // Non valido (x0), riservato, o "blocco" (01) al livello 3.
        if desc & 1 == 0 || (desc & 3 == 1 && level == 3) {
            return Err(FaultKind::Translation(level as u8));
        }
        if desc & 3 == 1 || level == 3 {
            break desc;
        }
        // Descrittore di tabella.
        if oversize(desc) {
            return Err(FaultKind::AddressSize(level as u8));
        }
        base = desc & bits(47, GRAIN);
        ap_table |= (desc >> 61 & 3) as u8;
        xn_table |= desc >> 60 & 1 != 0;
        pxn_table |= desc >> 59 & 1 != 0;
        level += 1;
        addrtop = bottom - 1;
    };
    // Con 4 KiB il primo livello con blocchi è l'1.
    if level < 1 {
        return Err(FaultKind::Translation(level as u8));
    }
    let bottom = (3 - level) * STRIDE + GRAIN;
    let pa = (desc & bits(47, bottom)) | (va & low(bottom));
    if oversize(pa) {
        return Err(FaultKind::AddressSize(level as u8));
    }
    if desc >> 10 & 1 == 0 {
        return Err(FaultKind::AccessFlag(level as u8));
    }
    let ap = (desc >> 6 & 3) as u8;
    let perms = Perms {
        // APTable[1] forza la sola lettura, APTable[0] toglie l'accesso a EL0.
        ap: (ap | (ap_table & 0b10)) & !(ap_table & 0b01),
        uxn: desc >> 54 & 1 != 0 || xn_table,
        pxn: desc >> 53 & 1 != 0 || pxn_table,
    };
    let attr_index = (desc >> 2 & 7) as u8;
    Ok(Translation {
        pa,
        level: level as u8,
        block_size: 1 << bottom,
        perms: Some(perms),
        attr_index: Some(attr_index),
        mair_attr: regs.mair_attr(attr_index),
        sh: (desc >> 8 & 3) as u8,
        ng: desc >> 11 & 1 != 0,
        asid: regs.asid(),
    })
}

/// Controllo dei permessi; il permission fault riporta il livello della
/// foglia.
pub(crate) fn check(regs: &MmuRegs, t: &Translation, access: Access, el: u8) -> Result<(), FaultKind> {
    match t.perms {
        Some(p) if !p.allows(access, el, regs.wxn()) => Err(FaultKind::Permission(t.level)),
        _ => Ok(()),
    }
}
