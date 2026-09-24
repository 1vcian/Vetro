//! La MMU di un core: registri, TLB e ultimo fault.

use vetro_cpu::Access;

use crate::fault::Fault;
use crate::regs::MmuRegs;
use crate::tlb::{Tlb, TlbEntry, TlbiOp, VA_MASK};
use crate::walk::{
    PhysMemory, Translation, check, check_device_alignment, fault, mmu_off, select, walk_tables,
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
}

impl Mmu {
    /// PARange della Cortex-A53 (ID_AA64MMFR0_EL1.PARange = 0b0010).
    pub const PA_BITS_CORTEX_A53: u32 = 40;

    /// `pa_bits` è PARange (32..=48): limita TCR.IPS e l'identità a MMU
    /// spenta.
    pub fn new(pa_bits: u32) -> Self {
        assert!((32..=48).contains(&pa_bits), "PARange non supportato: {pa_bits}");
        Mmu { regs: MmuRegs::default(), pa_bits, tlb: Tlb::new(), last_fault: None }
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
