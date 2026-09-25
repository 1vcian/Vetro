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
    /// Generazione di ogni slot: cresce a ogni modifica dello slot
    /// (inserimento, TLBI, svuotamento). La cache delle traduzioni recenti
    /// della [`Mmu`](crate::Mmu) è valida solo finché lo slot da cui viene
    /// non cambia.
    gens: Box<[u64; ENTRIES]>,
    /// Invalidazioni eseguite (TLBI e svuotamenti), anche a vuoto: chi
    /// tiene copie delle traduzioni fuori dal TLB (la TLB software del JIT)
    /// le scarta quando cambia.
    flushes: u64,
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlb {
    pub fn new() -> Self {
        Tlb { entries: vec![None; ENTRIES], gens: Box::new([0; ENTRIES]), flushes: 0 }
    }

    #[inline]
    pub(crate) fn slot(key: u64) -> usize {
        (key >> 12) as usize & (ENTRIES - 1)
    }

    /// Generazione dello slot `slot`.
    #[inline]
    pub(crate) fn generation(&self, slot: usize) -> u64 {
        self.gens[slot]
    }

    pub(crate) fn lookup(&self, key: u64, asid: u16) -> Option<&TlbEntry> {
        self.entries[Self::slot(key)].as_ref().filter(|e| e.contains(key) && (e.global || e.asid == asid))
    }

    pub(crate) fn insert(&mut self, key: u64, e: TlbEntry) {
        let s = Self::slot(key);
        self.entries[s] = Some(e);
        self.gens[s] += 1;
    }

    /// Numero di invalidazioni eseguite finora ([`tlbi`](Self::tlbi) e
    /// [`flush_all`](Self::flush_all)).
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Numero di voci valide.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn remove_if(&mut self, pred: impl Fn(&TlbEntry) -> bool) {
        for (slot, g) in self.entries.iter_mut().zip(self.gens.iter_mut()) {
            if slot.as_ref().is_some_and(&pred) {
                *slot = None;
                *g += 1;
            }
        }
    }

    /// Svuota tutto (VMALLE1).
    pub fn flush_all(&mut self) {
        self.flushes += 1;
        self.entries.fill(None);
        for g in self.gens.iter_mut() {
            *g += 1;
        }
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
        self.flushes += 1;
        match op {
            Vmalle1 | Vmalle1is => self.flush_all(),
            Vae1 | Vae1is | Vale1 | Vale1is => self.flush_va(tlbi_va(xt), tlbi_asid(xt)),
            Aside1 | Aside1is => self.flush_asid(tlbi_asid(xt)),
            Vaae1 | Vaae1is | Vaale1 | Vaale1is => self.flush_va_all_asids(tlbi_va(xt)),
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// Le voci del TLB entrano nello snapshot: sono stato osservabile (un guest
/// che cambia le tabelle senza TLBI vede ancora la traduzione vecchia, e
/// una voce assente dopo il ripristino cambierebbe il risultato). Le
/// generazioni degli slot no: servono solo alla cache delle traduzioni
/// recenti della MMU, che al ripristino riparte vuota. `flushes` nemmeno:
/// al ripristino cresce, così chi tiene copie delle traduzioni (la TLB
/// software del JIT) le scarta.
impl vetro_snapshot::Snapshot for Tlb {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        let valid = self.entries.iter().enumerate().filter_map(|(i, e)| e.map(|e| (i, e)));
        w.u32(self.len() as u32);
        for (slot, e) in valid {
            w.u32(slot as u32);
            w.u64(e.va_base);
            w.u64(e.size);
            w.u64(e.pa_base);
            w.u16(e.asid);
            w.bool(e.global);
            w.u8(e.level);
            w.u8(e.perms.ap);
            w.bool(e.perms.uxn);
            w.bool(e.perms.pxn);
            w.u8(e.attr_index);
            w.u8(e.sh);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        use vetro_snapshot::Error;
        let n = r.u32()? as usize;
        if n > ENTRIES {
            return Err(Error::invalid(format!("{n} voci nel TLB")));
        }
        self.entries.fill(None);
        let mut last = None;
        for _ in 0..n {
            let slot = r.u32()? as usize;
            if slot >= ENTRIES || last.is_some_and(|l| slot <= l) {
                return Err(Error::invalid(format!("slot del TLB {slot}")));
            }
            last = Some(slot);
            let e = TlbEntry {
                va_base: r.u64()?,
                size: r.u64()?,
                pa_base: r.u64()?,
                asid: r.u16()?,
                global: r.bool()?,
                level: r.u8()?,
                perms: Perms { ap: r.u8()?, uxn: r.bool()?, pxn: r.bool()? },
                attr_index: r.u8()?,
                sh: r.u8()?,
            };
            if !e.size.is_power_of_two() || e.va_base & (e.size - 1) != 0 || e.attr_index > 7 {
                return Err(Error::invalid(format!("voce del TLB {e:?}")));
            }
            self.entries[slot] = Some(e);
        }
        for g in self.gens.iter_mut() {
            *g += 1;
        }
        self.flushes += 1;
        Ok(())
    }
}
