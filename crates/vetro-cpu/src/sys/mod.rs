//! Modalità sistema: EL0 ed EL1, eccezioni, registri di sistema, MMU e
//! interrupt (ADR 0009, `docs/specs/cpu.md`).
//!
//! La modalità utente ([`Cpu::step`] con una [`Memory`](crate::Memory))
//! resta com'era: le eccezioni tornano al chiamante e nessun registro EL1
//! entra in gioco. La modalità sistema ([`Cpu::step_system`]) consegna le
//! eccezioni al guest attraverso VBAR_EL1 e passa ogni accesso alla memoria
//! da un [`SysBus`] (la MMU di `vetro-mmu`); ciò che appartiene alla
//! piattaforma (linea IRQ, timer generico, interfaccia CPU del GIC) arriva da
//! un [`CpuEnv`].

mod except;
pub mod id;
mod mem;
mod regs;
mod state;
mod step;
#[cfg(test)]
mod tests;

pub use except::{ExceptionKind, ec};
pub use state::{Mode, PsciConduit, SysConfig, SysState, cpacr, cntkctl, sctlr, spsr};

use crate::mem::Access;
use crate::sysreg::EnvReg;

/// Registri che governano la traduzione stage 1, copiati dalla CPU a ogni
/// accesso: la CPU ne è l'unico proprietario.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TranslationRegs {
    pub sctlr: u64,
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub mair: u64,
}

/// Richiesta di traduzione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessReq {
    pub access: Access,
    /// Privilegio del controllo dei permessi: PSTATE.EL, oppure 0 per
    /// LDTR/STTR eseguite a EL1.
    pub el: u8,
    /// Falso se l'accesso non è allineato alla sua dimensione (o è un DC
    /// ZVA): su memoria Device diventa un fault di allineamento, controllato
    /// dopo il walk e prima dei permessi come nello pseudocodice.
    pub aligned: bool,
}

/// Esito negativo di una traduzione o di un accesso fisico.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusFault {
    /// Abort architetturale: codice DFSC/IFSC e bit EA (1 per uno slave
    /// error, 0 per un decode error, come QEMU).
    Abort { fsc: u8, ea: bool },
    /// Configurazione valida che Vetro non implementa (es. granulo 64 KiB):
    /// non si consegna al guest, si ferma l'esecuzione.
    Unimplemented(&'static str),
}

impl BusFault {
    /// Codice FSC di un abort esterno sincrono sull'accesso (non sul walk).
    pub const FSC_EXTERNAL: u8 = 0b01_0000;
    /// Codice FSC di un fault di allineamento.
    pub const FSC_ALIGNMENT: u8 = 0b10_0001;
}

/// Esito di un'istruzione AT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtResult {
    /// Valore da scrivere in PAR_EL1 (riuscita o fault riportato in PAR).
    Par(u64),
    /// Abort esterno durante il walk: si prende come Data Abort (CM = 1).
    Abort { fsc: u8, ea: bool },
    Unimplemented(&'static str),
}

/// La memoria del sistema vista dalla CPU: traduzione e accessi fisici.
/// La implementa `vetro-mmu` (`MmuBus`) sopra la memoria fisica della
/// piattaforma.
pub trait SysBus {
    /// Traduce `va` e controlla i permessi; restituisce l'indirizzo fisico.
    fn translate(&mut self, regs: &TranslationRegs, va: u64, req: AccessReq) -> Result<u64, BusFault>;
    /// Lettura fisica (un pezzo che non attraversa pagine).
    fn read_phys(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusFault>;
    /// Scrittura fisica (un pezzo che non attraversa pagine).
    fn write_phys(&mut self, pa: u64, data: &[u8]) -> Result<(), BusFault>;
    /// AT S1E{0,1}{R,W}: walk senza TLB.
    fn at(&mut self, regs: &TranslationRegs, va: u64, access: Access, el: u8) -> AtResult;
    /// TLBI dal decoder (le varianti IS vanno applicate a ogni core).
    fn tlbi(&mut self, op: TlbiOp, xt: u64);
    /// Svuota il TLB (scritture di SCTLR_EL1 e TCR_EL1, come QEMU).
    fn tlb_flush_all(&mut self);
}

/// Ciò che la piattaforma fornisce al core: linee di interrupt e registri
/// di sistema che non stanno nella CPU. Il tempo (CNTPCT) entra solo da qui,
/// così resta deterministico e registrabile.
pub trait CpuEnv {
    /// Livello della linea IRQ verso questo core (uscita del GIC).
    fn irq_line(&mut self) -> bool;
    /// Livello della linea FIQ (il GIC di Vetro non la pilota).
    fn fiq_line(&mut self) -> bool {
        false
    }
    /// MRS di un registro della piattaforma, già autorizzato dalla CPU.
    fn read_sysreg(&mut self, reg: EnvReg) -> u64;
    /// MSR di un registro della piattaforma, già autorizzato dalla CPU.
    fn write_sysreg(&mut self, reg: EnvReg, value: u64);
}

/// Esito di [`Cpu::step_system`](crate::Cpu::step_system).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysEvent {
    /// Un'istruzione eseguita.
    Executed,
    /// Eccezione presa: il PC è già al vettore. `esr` vale 0 per IRQ e FIQ
    /// (che non scrivono ESR_EL1). Nessuna istruzione eseguita.
    Exception { kind: ExceptionKind, esr: u64, from_el: u8 },
    /// WFI eseguita (PC già all'istruzione successiva): la piattaforma può
    /// far avanzare il tempo fino al prossimo interrupt.
    WaitForInterrupt,
    /// HVC del conduit PSCI: PC già dopo l'istruzione, argomenti in x0-x7,
    /// risultato da scrivere in x0 (tutte le HVC vanno al conduit, come in
    /// QEMU: una funzione sconosciuta restituisce NOT_SUPPORTED).
    Hvc(u16),
    /// SMC del conduit PSCI (se configurato così).
    Smc(u16),
    /// Istruzione o configurazione valida che Vetro non implementa. Stato
    /// invariato, PC all'istruzione (`raw` = 0 se il limite è nel fetch).
    Unimplemented { raw: u32, what: &'static str },
}

/// Istruzioni TLBI del regime EL1&0 (SYS #0, C8, CRm, #op2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlbiOp {
    Vmalle1,
    Vae1,
    Aside1,
    Vaae1,
    Vale1,
    Vaale1,
    Vmalle1is,
    Vae1is,
    Aside1is,
    Vaae1is,
    Vale1is,
    Vaale1is,
}

impl TlbiOp {
    /// Riconosce una TLBI dai campi di SYS (op0 = 1 implicito).
    pub fn from_sys(op1: u32, crn: u32, crm: u32, op2: u32) -> Option<TlbiOp> {
        if op1 != 0 || crn != 8 {
            return None;
        }
        use TlbiOp::*;
        Some(match (crm, op2) {
            (3, 0) => Vmalle1is,
            (3, 1) => Vae1is,
            (3, 2) => Aside1is,
            (3, 3) => Vaae1is,
            (3, 5) => Vale1is,
            (3, 7) => Vaale1is,
            (7, 0) => Vmalle1,
            (7, 1) => Vae1,
            (7, 2) => Aside1,
            (7, 3) => Vaae1,
            (7, 5) => Vale1,
            (7, 7) => Vaale1,
            _ => return None,
        })
    }

    /// Variante Inner Shareable: il sistema la applica al TLB di ogni core.
    pub fn is_broadcast(self) -> bool {
        use TlbiOp::*;
        matches!(self, Vmalle1is | Vae1is | Aside1is | Vaae1is | Vale1is | Vaale1is)
    }
}
