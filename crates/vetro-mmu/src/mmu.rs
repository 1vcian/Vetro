//! La MMU di un core: registri, TLB e ultimo fault.

use vetro_cpu::Access;

use crate::fault::Fault;
use crate::regs::{MmuRegs, PAGE_SIZE};
use crate::tlb::{Tlb, TlbEntry, TlbiOp, VA_MASK};
use crate::walk::{
    Perms, PhysMemory, Translation, check, check_device_alignment, fault, mmu_off, select, walk_tables,
};

/// Voci della cache delle traduzioni recenti: una per slot del TLB.
const RECENT: usize = 512;

/// Traduzione recente riuscita a MMU accesa, già pronta per il controllo
/// dei permessi. È solo una scorciatoia: vale finché i registri di
/// traduzione non cambiano (`epoch`) e finché lo slot del TLB da cui viene
/// resta com'era (`tlb_gen`). In quelle condizioni il percorso completo
/// ([`Mmu::translate_checked`]) troverebbe nel TLB la stessa voce e darebbe
/// lo stesso risultato: la cache non cambia nulla di osservabile, nemmeno
/// quando il guest modifica le tabelle senza TLBI.
#[derive(Clone, Copy, Debug)]
struct Recent {
    /// VA[63:12] intera (tag compreso: `select` dipende dai bit alti).
    vpage: u64,
    /// Indirizzo fisico della pagina da 4 KiB.
    pa_page: u64,
    perms: Perms,
    /// Memoria Device (byte MAIR 0b0000xxxx): i dati non allineati fanno
    /// fault.
    device: bool,
    epoch: u64,
    tlb_gen: u64,
}

const EMPTY: Recent = Recent {
    vpage: u64::MAX,
    pa_page: 0,
    perms: Perms { ap: 0, uxn: true, pxn: true },
    device: true,
    epoch: 0,
    tlb_gen: 0,
};

/// MMU stage 1 del regime EL1&0 per un core.
#[derive(Clone, Debug)]
pub struct Mmu {
    /// Registri di traduzione. Dopo aver cambiato SCTLR o TCR il sistema
    /// chiama [`Tlb::flush_all`] come QEMU (l'architettura permette di tenere
    /// quei campi nel TLB, ma il guest deve comunque fare una TLBI).
    pub regs: MmuRegs,
    pa_bits: u32,
    tlb: Tlb,
    last_fault: Option<Fault>,
    /// Cache delle traduzioni recenti (vedi [`Recent`]), indicizzata come
    /// il TLB.
    recent: Box<[Recent; RECENT]>,
    /// Registri con cui sono state riempite le voci dell'epoca corrente.
    recent_regs: MmuRegs,
    /// Epoca corrente: cresce quando cambiano i registri di traduzione.
    recent_epoch: u64,
    /// Colpi della cache recente (solo per i test).
    #[cfg(test)]
    pub(crate) recent_hits: u64,
}

impl Mmu {
    /// PARange della Cortex-A53 (ID_AA64MMFR0_EL1.PARange = 0b0010).
    pub const PA_BITS_CORTEX_A53: u32 = 40;

    /// `pa_bits` è PARange (32..=48): limita TCR.IPS e l'identità a MMU
    /// spenta.
    pub fn new(pa_bits: u32) -> Self {
        assert!((32..=48).contains(&pa_bits), "PARange non supportato: {pa_bits}");
        Mmu {
            regs: MmuRegs::default(),
            pa_bits,
            tlb: Tlb::new(),
            last_fault: None,
            recent: Box::new([EMPTY; RECENT]),
            recent_regs: MmuRegs::default(),
            recent_epoch: 1,
            #[cfg(test)]
            recent_hits: 0,
        }
    }

    pub fn pa_bits(&self) -> u32 {
        self.pa_bits
    }

    /// Traduce `va` per un accesso con privilegio `el` (0 o 1), passando dal
    /// TLB. Ordine: identità se SCTLR.M = 0; controllo della metà e dei bit
    /// alti (translation fault di livello 0); lookup nel TLB; walk (che su un
    /// miss rispetta EPDx); permessi.
    pub fn translate<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
    ) -> Result<Translation, Fault> {
        self.translate_checked(phys, va, access, el, true)
    }

    /// Come [`translate`](Self::translate), ma con `aligned = false` un
    /// accesso ai dati su memoria Device (anche a MMU spenta, dove i dati
    /// sono Device-nGnRnE) dà [`FaultKind::Alignment`]: dopo i fault del
    /// walk e prima dei permessi, come `AArch64.FirstStageTranslate`.
    pub fn translate_checked<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<Translation, Fault> {
        let regs = self.regs;
        if !regs.enabled() {
            let t = mmu_off(&regs, self.pa_bits, va, access, el)?;
            check_device_alignment(&t, access, aligned).map_err(|k| fault(k, va, access, el))?;
            return Ok(t);
        }
        let hi = select(&regs, va).map_err(|k| fault(k, va, access, el))?;
        let key = va & VA_MASK;
        let t = match self.tlb.lookup(key, regs.asid()) {
            Some(e) => e.translation(key, &regs),
            None => {
                let t =
                    walk_tables(&regs, self.pa_bits, phys, va, hi).map_err(|k| fault(k, va, access, el))?;
                self.tlb.insert(key, TlbEntry::new(key, &t));
                t
            }
        };
        check_device_alignment(&t, access, aligned).map_err(|k| fault(k, va, access, el))?;
        check(&regs, &t, access, el).map_err(|k| fault(k, va, access, el))?;
        Ok(t)
    }

    /// Come `translate_pa_with`, con i registri già in `self.regs` (per i
    /// test, che li cambiano direttamente).
    #[cfg(test)]
    #[inline]
    pub(crate) fn translate_pa<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        let regs = self.regs;
        self.sync_recent(&regs);
        self.translate_recent(phys, va, access, el, aligned)
    }

    /// Come [`translate_checked`](Self::translate_checked) con i registri
    /// `regs`, che diventano quelli della MMU, ma restituisce solo
    /// l'indirizzo fisico e passa prima dalla cache delle traduzioni recenti.
    /// Risultato, fault e stato del TLB dopo la chiamata sono identici a
    /// quelli del percorso completo. La usa [`MmuBus`](crate::MmuBus), a cui
    /// la CPU passa i registri a ogni accesso.
    #[inline]
    pub(crate) fn translate_pa_with<P: PhysMemory + ?Sized>(
        &mut self,
        regs: &MmuRegs,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        self.regs = *regs;
        self.sync_recent(regs);
        self.translate_recent(phys, va, access, el, aligned)
    }

    /// Nuova epoca della cache recente se `regs` (i registri correnti) non
    /// sono quelli con cui è stata riempita.
    #[inline]
    fn sync_recent(&mut self, regs: &MmuRegs) {
        if !regs.same(&self.recent_regs) {
            self.recent_regs = *regs;
            self.recent_epoch += 1;
        }
    }

    /// Cache recente, poi percorso completo. Richiede `self.regs ==
    /// self.recent_regs` (lo garantisce `sync_recent`).
    #[inline]
    fn translate_recent<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
    ) -> Result<u64, Fault> {
        let enabled = self.recent_regs.enabled();
        let vpage = va / PAGE_SIZE;
        let slot = Tlb::slot(va & VA_MASK);
        if enabled {
            let r = &self.recent[slot];
            if r.vpage == vpage
                && r.epoch == self.recent_epoch
                && r.tlb_gen == self.tlb.generation(slot)
                && (aligned || access == Access::Fetch || !r.device)
                && r.perms.allows(access, el, self.recent_regs.wxn())
            {
                #[cfg(test)]
                {
                    self.recent_hits += 1;
                }
                return Ok(r.pa_page | (va & (PAGE_SIZE - 1)));
            }
        }
        self.translate_fill(phys, va, access, el, aligned, slot)
    }

    /// Percorso completo e riempimento della voce recente (fuori linea: il
    /// percorso veloce resta piccolo e si inlinea nel fetch).
    #[inline(never)]
    fn translate_fill<P: PhysMemory + ?Sized>(
        &mut self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
        aligned: bool,
        slot: usize,
    ) -> Result<u64, Fault> {
        let t = self.translate_checked(phys, va, access, el, aligned)?;
        if self.regs.enabled()
            && let Some(perms) = t.perms
        {
            self.recent[slot] = Recent {
                vpage: va / PAGE_SIZE,
                pa_page: t.pa & !(PAGE_SIZE - 1),
                perms,
                device: t.mair_attr & 0xf0 == 0,
                epoch: self.recent_epoch,
                tlb_gen: self.tlb.generation(slot),
            };
        }
        Ok(t.pa)
    }

    /// Come [`translate`](Self::translate) ma senza leggere né riempire il
    /// TLB: per il debugger e per verificare le tabelle.
    pub fn walk<P: PhysMemory + ?Sized>(
        &self,
        phys: &mut P,
        va: u64,
        access: Access,
        el: u8,
    ) -> Result<Translation, Fault> {
        let regs = self.regs;
        if !regs.enabled() {
            return mmu_off(&regs, self.pa_bits, va, access, el);
        }
        let t = select(&regs, va)
            .and_then(|hi| walk_tables(&regs, self.pa_bits, phys, va, hi))
            .map_err(|k| fault(k, va, access, el))?;
        check(&regs, &t, access, el).map_err(|k| fault(k, va, access, el))?;
        Ok(t)
    }

    pub fn tlb(&self) -> &Tlb {
        &self.tlb
    }

    pub fn tlb_mut(&mut self) -> &mut Tlb {
        &mut self.tlb
    }

    /// Esegue una TLBI su questo core (vedi [`Tlb::tlbi`]).
    pub fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        self.tlb.tlbi(op, xt);
    }

    /// Fault dettagliato dell'ultimo accesso fallito tramite
    /// [`VirtMemory`](crate::VirtMemory).
    pub fn last_fault(&self) -> Option<Fault> {
        self.last_fault
    }

    pub(crate) fn set_last_fault(&mut self, f: Fault) {
        self.last_fault = Some(f);
    }
}
