//! Punti di aggancio dell'introspezione (ADR 0027): syscall dello spazio
//! utente (SVC da EL0 e ritorno in EL0) e punti d'arresto "invisibili" su
//! indirizzi utente, osservati dal ciclo della macchina senza cambiare
//! l'esecuzione.
//!
//! - Il tracciatore ([`Tracer`]) riceve gli eventi con una vista in sola
//!   lettura della macchina ([`GuestView`]: registri e RAM). Non può
//!   cambiare niente: l'esecuzione (istruzioni, interrupt, RAM, console) è
//!   identica con e senza aggancio, anche nel replay (M10).
//! - SVC ed ERET le esegue sempre l'interprete (il JIT di sistema chiude i
//!   blocchi prima di una SVC e non traduce ERET): gli eventi delle
//!   syscall non chiedono niente al JIT.
//! - I punti d'arresto non scrivono nella memoria del guest (niente BRK):
//!   la macchina guarda il PC prima di ogni passo dell'interprete, e il JIT
//!   non mette quegli indirizzi nelle regioni (`SysJit::set_stops`).
//! - Niente di tutto questo entra negli snapshot o nei log.

use std::any::Any;
use std::collections::BTreeMap;

use vetro_analysis::introspect::{CpuRegs, PhysMem};
use vetro_cpu::Cpu;

use crate::board::Ram;

/// La macchina in sola lettura, durante un evento.
pub struct GuestView<'a> {
    pub cpu: &'a Cpu,
    pub(crate) ram: &'a Ram,
    /// Istruzioni eseguite (l'orologio della macchina).
    pub steps: u64,
}

impl GuestView<'_> {
    /// Legge la RAM fisica.
    pub fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.ram.read(pa, buf)
    }

    /// I registri che servono a leggere il kernel.
    pub fn cpu_regs(&self) -> CpuRegs {
        cpu_regs(self.cpu)
    }

    /// SP_EL1: con la CPU a EL0 è la cima della pila del kernel del thread
    /// (uno per thread): la chiave che lega ingresso e ritorno di una
    /// syscall.
    pub fn sp_el1(&self) -> u64 {
        sp_el1(self.cpu)
    }

    /// SP_EL0: la pila utente.
    pub fn sp_el0(&self) -> u64 {
        if self.cpu.sys.el == 0 || !self.cpu.sys.spsel { self.cpu.sp } else { self.cpu.sys.sp_el[0] }
    }
}

impl PhysMem for GuestView<'_> {
    fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.ram.read(pa, buf)
    }
}

pub(crate) fn cpu_regs(cpu: &Cpu) -> CpuRegs {
    let s = &cpu.sys;
    CpuRegs {
        tcr: s.tcr_el1,
        ttbr0: s.ttbr0_el1,
        ttbr1: s.ttbr1_el1,
        vbar: s.vbar_el1,
        tpidr_el1: s.tpidr_el1,
    }
}

pub(crate) fn sp_el1(cpu: &Cpu) -> u64 {
    if cpu.sys.el == 1 && cpu.sys.spsel { cpu.sp } else { cpu.sys.sp_el[1] }
}

/// Una syscall entrata: SVC eseguita a EL0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyscallEntry {
    /// Istruzioni eseguite, SVC compresa.
    pub step: u64,
    /// x8.
    pub nr: u64,
    /// x0-x5.
    pub args: [u64; 6],
    /// Indirizzo della SVC.
    pub pc: u64,
    /// SP_EL1 (cima della pila del kernel del thread).
    pub key: u64,
    /// TTBR0_EL1 (spazio d'indirizzi del processo).
    pub ttbr0: u64,
}

/// Un evento per il tracciatore.
#[derive(Debug)]
pub enum Event<'a> {
    /// Subito dopo la SVC (la CPU è al vettore, a EL1; registri generali
    /// e memoria sono quelli del programma).
    SyscallEnter(&'a SyscallEntry),
    /// Al primo ritorno in EL0 (ERET) del thread dopo la syscall: `pc` è
    /// dove riprende (l'istruzione dopo la SVC, o altrove per execve, un
    /// segnale, una syscall da ripetere) e `ret` il suo x0.
    SyscallExit { entry: &'a SyscallEntry, ret: u64, pc: u64 },
    /// Il PC di EL0 ha raggiunto un punto d'arresto e l'istruzione è stata
    /// eseguita (una volta per esecuzione: se un'eccezione la interrompe,
    /// l'evento arriva quando la si riesegue). `regs` sono i registri di
    /// **prima** dell'istruzione (gli argomenti all'ingresso di una
    /// funzione); la RAM di [`GuestView`] è quella di dopo.
    Breakpoint { id: u32, va: u64, regs: &'a Cpu },
}

/// Chi riceve gli eventi. Deve essere `'static` per poterlo riprendere
/// col suo tipo ([`crate::Machine::tracer_mut`]).
pub trait Tracer: Any {
    fn event(&mut self, ev: &Event<'_>, guest: &GuestView<'_>);
}

/// Un punto d'arresto su un indirizzo virtuale di EL0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub va: u64,
    /// Solo nello spazio con questa tabella (BADDR di TTBR0_EL1, cioè
    /// l'indirizzo fisico di `mm->pgd` del processo); `None` = tutti.
    pub ttbr0: Option<u64>,
}

/// Stato degli agganci nella macchina.
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) tracer: Option<Box<dyn Tracer>>,
    pub(crate) syscalls: bool,
    pending: BTreeMap<u64, SyscallEntry>,
    bps: BTreeMap<u32, Breakpoint>,
    by_va: BTreeMap<u64, Vec<u32>>,
    /// Filtro rapido: bit `(va >> 2) & 63` degli indirizzi con punti.
    mask: u64,
    next_id: u32,
}

/// Registri di prima di un passo con un punto d'arresto al PC.
pub(crate) struct Pre {
    ids: Vec<u32>,
    va: u64,
    cpu: Box<Cpu>,
}

fn bit(va: u64) -> u64 {
    1 << ((va >> 2) & 63)
}

impl Hooks {
    /// Qualcosa da osservare nel ciclo dell'interprete.
    #[inline]
    pub(crate) fn armed(&self) -> bool {
        self.tracer.is_some() && (self.syscalls || self.mask != 0)
    }

    pub(crate) fn add(&mut self, bp: Breakpoint) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.bps.insert(id, bp);
        self.by_va.entry(bp.va).or_default().push(id);
        self.mask |= bit(bp.va);
        id
    }

    pub(crate) fn remove(&mut self, id: u32) -> bool {
        let Some(bp) = self.bps.remove(&id) else { return false };
        if let Some(v) = self.by_va.get_mut(&bp.va) {
            v.retain(|&i| i != id);
            if v.is_empty() {
                self.by_va.remove(&bp.va);
            }
        }
        self.mask = self.by_va.keys().fold(0, |m, &va| m | bit(va));
        true
    }

    pub(crate) fn breakpoints(&self) -> Vec<(u32, Breakpoint)> {
        self.bps.iter().map(|(&i, &b)| (i, b)).collect()
    }

    /// Gli indirizzi con punti d'arresto (per il JIT).
    pub(crate) fn stops(&self) -> Vec<u64> {
        self.by_va.keys().copied().collect()
    }

    /// Prima di un passo dell'interprete: i punti d'arresto al PC per lo
    /// spazio corrente.
    #[inline]
    pub(crate) fn pre(&self, cpu: &Cpu) -> Option<Pre> {
        if self.mask & bit(cpu.pc) == 0 || cpu.sys.el != 0 {
            return None;
        }
        let ids = self.by_va.get(&cpu.pc)?;
        let ttbr0 = vetro_analysis::introspect::mem::ttbr_base(cpu.sys.ttbr0_el1);
        let ids: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|i| self.bps.get(i).is_some_and(|b| b.ttbr0.is_none_or(|t| t == ttbr0)))
            .collect();
        (!ids.is_empty()).then(|| Pre { ids, va: cpu.pc, cpu: Box::new(cpu.clone()) })
    }

    /// Dopo un passo dell'interprete.
    pub(crate) fn after(
        &mut self,
        old_el: u8,
        ev: &vetro_cpu::sys::SysEvent,
        pre: Option<Pre>,
        cpu: &Cpu,
        ram: &Ram,
        steps: u64,
    ) {
        use vetro_cpu::sys::{ExceptionKind, SysEvent, ec};
        let Some(tracer) = self.tracer.as_mut() else { return };
        let g = GuestView { cpu, ram, steps };
        let svc = matches!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr, from_el: 0 }
            if esr >> 26 == ec::SVC);
        if let Some(p) = pre
            && (svc || *ev == SysEvent::Executed)
        {
            for id in p.ids {
                tracer.event(&Event::Breakpoint { id, va: p.va, regs: &p.cpu }, &g);
            }
        }
        if !self.syscalls {
            return;
        }
        if svc {
            let x = &cpu.x;
            let e = SyscallEntry {
                step: steps,
                nr: x[8],
                args: [x[0], x[1], x[2], x[3], x[4], x[5]],
                pc: cpu.sys.elr_el1.wrapping_sub(4),
                key: sp_el1(cpu),
                ttbr0: cpu.sys.ttbr0_el1,
            };
            tracer.event(&Event::SyscallEnter(&e), &g);
            self.pending.insert(e.key, e);
        } else if old_el == 1
            && cpu.sys.el == 0
            && let Some(e) = self.pending.remove(&sp_el1(cpu))
        {
            tracer.event(&Event::SyscallExit { entry: &e, ret: cpu.x[0], pc: cpu.pc }, &g);
        }
    }

    /// Dimentica le syscall in corso (dopo un salto nel tempo).
    pub(crate) fn forget_pending(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn tracer_any(&mut self) -> Option<&mut dyn Any> {
        self.tracer.as_mut().map(|t| {
            let a: &mut dyn Any = t.as_mut();
            a
        })
    }
}
