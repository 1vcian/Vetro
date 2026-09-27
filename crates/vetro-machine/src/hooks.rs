//! Introspection hook points (ADR 0027): user-space syscalls
//! (SVC from EL0 and return to EL0) and "invisible" breakpoints on
//! user addresses, observed by the machine loop without changing
//! execution.
//!
//! - The tracer ([`Tracer`]) receives events with a read-only view
//!   of the machine ([`GuestView`]: registers and RAM). It cannot
//!   change anything: execution (instructions, interrupts, RAM, console) is
//!   identical with and without hooks, in replay too (M10).
//! - SVC and ERET are always executed by the interpreter (the system JIT ends
//!   blocks before an SVC and does not translate ERET): syscall
//!   events ask nothing of the JIT.
//! - Breakpoints do not write into guest memory (no BRK):
//!   the machine looks at the PC before every interpreter step, and the JIT
//!   does not put those addresses in its regions (`SysJit::set_stops`).
//! - None of this goes into snapshots or logs.

use std::any::Any;
use std::collections::BTreeMap;

use vetro_analysis::introspect::{CpuRegs, PhysMem};
use vetro_cpu::Cpu;

use crate::board::Ram;

/// The machine, read-only, during an event.
pub struct GuestView<'a> {
    pub cpu: &'a Cpu,
    pub(crate) ram: &'a Ram,
    /// Instructions executed (the machine's clock).
    pub steps: u64,
}

impl GuestView<'_> {
    /// Reads physical RAM.
    pub fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.ram.read(pa, buf)
    }

    /// The registers needed to read the kernel.
    pub fn cpu_regs(&self) -> CpuRegs {
        cpu_regs(self.cpu)
    }

    /// SP_EL1: with the CPU at EL0 it is the top of the thread's kernel stack
    /// (one per thread): the key that ties together entry and return of a
    /// syscall.
    pub fn sp_el1(&self) -> u64 {
        sp_el1(self.cpu)
    }

    /// SP_EL0: the user stack.
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

/// A syscall entered: SVC executed at EL0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyscallEntry {
    /// Instructions executed, SVC included.
    pub step: u64,
    /// x8.
    pub nr: u64,
    /// x0-x5.
    pub args: [u64; 6],
    /// Address of the SVC.
    pub pc: u64,
    /// SP_EL1 (top of the thread's kernel stack).
    pub key: u64,
    /// TTBR0_EL1 (the process's address space).
    pub ttbr0: u64,
}

/// An event for the tracer.
#[derive(Debug)]
pub enum Event<'a> {
    /// Right after the SVC (the CPU is at the vector, at EL1; general registers
    /// and memory are the program's).
    SyscallEnter(&'a SyscallEntry),
    /// At the thread's first return to EL0 (ERET) after the syscall: `pc` is
    /// where it resumes (the instruction after the SVC, or elsewhere for execve, a
    /// signal, a syscall to restart) and `ret` its x0.
    SyscallExit { entry: &'a SyscallEntry, ret: u64, pc: u64 },
    /// The EL0 PC reached a breakpoint and the instruction has been
    /// executed (once per execution: if an exception interrupts it,
    /// the event arrives when it is re-executed). `regs` are the registers from
    /// **before** the instruction (the arguments on entry to a
    /// function); the RAM of [`GuestView`] is the one from after.
    Breakpoint { id: u32, va: u64, regs: &'a Cpu },
}

/// Whoever receives the events. It must be `'static` so it can be taken back
/// with its type ([`crate::Machine::tracer_mut`]).
pub trait Tracer: Any {
    fn event(&mut self, ev: &Event<'_>, guest: &GuestView<'_>);
}

/// A breakpoint on an EL0 virtual address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub va: u64,
    /// Only in the space with this table (BADDR of TTBR0_EL1, i.e.
    /// the physical address of the process's `mm->pgd`); `None` = all.
    pub ttbr0: Option<u64>,
}

/// State of the hooks in the machine.
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) tracer: Option<Box<dyn Tracer>>,
    pub(crate) syscalls: bool,
    pending: BTreeMap<u64, SyscallEntry>,
    bps: BTreeMap<u32, Breakpoint>,
    by_va: BTreeMap<u64, Vec<u32>>,
    /// Fast filter: bit `(va >> 2) & 63` of the addresses with breakpoints.
    mask: u64,
    next_id: u32,
}

/// Registers from before a step with a breakpoint at the PC.
pub(crate) struct Pre {
    ids: Vec<u32>,
    va: u64,
    cpu: Box<Cpu>,
}

fn bit(va: u64) -> u64 {
    1 << ((va >> 2) & 63)
}

impl Hooks {
    /// Something to observe in the interpreter loop.
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

    /// The addresses with breakpoints (for the JIT).
    pub(crate) fn stops(&self) -> Vec<u64> {
        self.by_va.keys().copied().collect()
    }

    /// Before an interpreter step: the breakpoints at the PC for the
    /// current space.
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

    /// After an interpreter step.
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

    /// Forgets the syscalls in progress (after a jump in time).
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
