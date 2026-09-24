//! TLB software e istruzioni TLBI del regime EL1&0.
//!
//! Cache a corrispondenza diretta, deterministica (nessun hash casuale):
//! uno slot per pagina da 4 KiB, scelto dai bit bassi del numero di pagina.
//! Ogni voce ricorda però la pagina o il blocco interi da cui viene, così una
//! TLBI per VA dentro un blocco da 2 MiB o 1 GiB toglie tutte le voci di quel
//! blocco. Si mettono in cache solo i walk riusciti (i fault di traduzione,
//! AF e address size non entrano mai, come richiede l'architettura); i
//! permessi si controllano a ogni accesso dai bit AP/XN salvati.

pub use vetro_cpu::sys::TlbiOp;

use crate::regs::MmuRegs;
use crate::walk::{Perms, Translation};

/// VA[55:0]: la parte dell'indirizzo che identifica una voce. I bit 63:56
/// sono il tag (con TBI) o copie del bit 55 (verificate prima del lookup).
pub(crate) const VA_MASK: u64 = (1 << 56) - 1;

const ENTRIES: usize = 512;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TlbEntry {
    /// Base della pagina o del blocco, in VA[55:0].
    va_base: u64,
    size: u64,
    pa_base: u64,
    asid: u16,
    global: bool,
    level: u8,
    perms: Perms,
    attr_index: u8,
    sh: u8,
}

impl TlbEntry {
    pub(crate) fn new(key: u64, t: &Translation) -> Self {
        let mask = !(t.block_size - 1);
        TlbEntry {
            va_base: key & mask,
            size: t.block_size,
            pa_base: t.pa & mask,
            asid: t.asid,
            global: !t.ng,
            level: t.level,
            perms: t.perms.expect("solo walk a MMU accesa"),
            attr_index: t.attr_index.expect("solo walk a MMU accesa"),
            sh: t.sh,
        }
    }

    fn contains(&self, key: u64) -> bool {
        key.wrapping_sub(self.va_base) < self.size
    }

    /// Ricostruisce la traduzione per `key`. MAIR si rilegge adesso
    /// (l'architettura permette sia questo sia di tenerlo in cache).
    pub(crate) fn translation(&self, key: u64, regs: &MmuRegs) -> Translation {
        Translation {
            pa: self.pa_base + (key - self.va_base),
            level: self.level,
            block_size: self.size,
            perms: Some(self.perms),
            attr_index: Some(self.attr_index),
            mair_attr: regs.mair_attr(self.attr_index),
            sh: self.sh,
            ng: !self.global,
            asid: self.asid,
        }
    }
}

/// VA[55:12] dal registro di una TLBI (Xt[43:0]).
fn tlbi_va(xt: u64) -> u64 {
    (xt & ((1 << 44) - 1)) << 12
}

/// ASID dal registro di una TLBI (Xt[63:48]).
fn tlbi_asid(xt: u64) -> u16 {
    (xt >> 48) as u16
}

/// TLB di un core.
#[derive(Clone, Debug)]
pub struct Tlb {
    entries: Vec<Option<TlbEntry>>,
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlb {
    pub fn new() -> Self {
        Tlb { entries: vec![None; ENTRIES] }
    }

    fn slot(key: u64) -> usize {
        (key >> 12) as usize & (ENTRIES - 1)
    }

    pub(crate) fn lookup(&self, key: u64, asid: u16) -> Option<&TlbEntry> {
        self.entries[Self::slot(key)].as_ref().filter(|e| e.contains(key) && (e.global || e.asid == asid))
    }

    pub(crate) fn insert(&mut self, key: u64, e: TlbEntry) {
        self.entries[Self::slot(key)] = Some(e);
    }

    /// Numero di voci valide.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn remove_if(&mut self, pred: impl Fn(&TlbEntry) -> bool) {
        for slot in &mut self.entries {
            if slot.as_ref().is_some_and(&pred) {
                *slot = None;
            }
        }
    }

    /// Svuota tutto (VMALLE1).
    pub fn flush_all(&mut self) {
        self.entries.fill(None);
    }

    /// Voci che contengono `va` e sono globali o di `asid` (VAE1, VALE1).
    pub fn flush_va(&mut self, va: u64, asid: u16) {
        let key = va & VA_MASK;
        self.remove_if(|e| e.contains(key) && (e.global || e.asid == asid));
    }

    /// Voci non globali di `asid` (ASIDE1).
    pub fn flush_asid(&mut self, asid: u16) {
        self.remove_if(|e| !e.global && e.asid == asid);
    }

    /// Voci che contengono `va`, di qualunque ASID (VAAE1, VAALE1).
    pub fn flush_va_all_asids(&mut self, va: u64) {
        let key = va & VA_MASK;
        self.remove_if(|e| e.contains(key));
    }

    /// Esegue una TLBI con il valore di Xt (ignorato da VMALLE1). Le
    /// varianti "last level" coincidono con le altre perché il TLB contiene
    /// solo foglie (nessuna cache dei livelli intermedi); le varianti IS
    /// agiscono qui come quelle locali.
    pub fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        use TlbiOp::*;
        match op {
            Vmalle1 | Vmalle1is => self.flush_all(),
            Vae1 | Vae1is | Vale1 | Vale1is => self.flush_va(tlbi_va(xt), tlbi_asid(xt)),
            Aside1 | Aside1is => self.flush_asid(tlbi_asid(xt)),
            Vaae1 | Vaae1is | Vaale1 | Vaale1is => self.flush_va_all_asids(tlbi_va(xt)),
        }
    }
}
