//! The guest's Linux kernel read from outside (ADR 0027), on top of the
//! machine's hook points ([`crate::hooks`]) and the structures of
//! `vetro-analysis` ([`vetro_analysis::introspect`]).
//!
//! - [`Machine::linux`]: processes, threads, maps, files open right now;
//! - [`SyscallTracer`]: EL0 syscalls with process, arguments, result
//!   and decoding (paths, data, addresses, binder transactions), and the
//!   breakpoints hit, with registers and process.
//!
//! Nothing writes into the guest: execution stays the same, with or without.

use std::collections::BTreeMap;

use vetro_analysis::introspect::{Kernel, Linux, SyscallRecord, Task};

use crate::Machine;
use crate::hooks::{Event, GuestView, Tracer};

impl Machine {
    /// Reads the guest's Linux kernel as it is right now (between two quanta).
    pub fn linux<R>(&self, kernel: &Kernel, f: impl FnOnce(&Linux<'_, GuestView<'_>>) -> R) -> R {
        self.with_guest(|g| {
            let regs = g.cpu_regs();
            f(&Linux::new(g, kernel, &regs))
        })
    }
}

/// A breakpoint that was hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointHit {
    pub id: u32,
    pub va: u64,
    /// Instructions executed (including the one at the breakpoint).
    pub step: u64,
    pub pid: i32,
    pub tid: i32,
    pub comm: String,
    /// x0-x7 before the instruction (a function's arguments).
    pub args: [u64; 8],
    /// x30 (return address) and SP_EL0 before the instruction.
    pub lr: u64,
    pub sp: u64,
}

/// Tracer of syscalls and breakpoints with the decoding of
/// `vetro-analysis`.
pub struct SyscallTracer {
    pub kernel: Kernel,
    /// Completed syscalls, in return order.
    pub records: Vec<SyscallRecord>,
    /// Breakpoints hit, in order.
    pub hits: Vec<BreakpointHit>,
    /// Only the syscalls of this process (tgid), if set.
    pub only_pid: Option<i32>,
    /// Maximum records kept (beyond that, they are counted in `dropped`).
    pub max_records: usize,
    pub dropped: u64,
    pending: BTreeMap<u64, SyscallRecord>,
}

impl SyscallTracer {
    pub fn new(kernel: Kernel) -> Self {
        SyscallTracer {
            kernel,
            records: Vec::new(),
            hits: Vec::new(),
            only_pid: None,
            max_records: 1 << 20,
            dropped: 0,
            pending: BTreeMap::new(),
        }
    }

    fn push(&mut self, r: SyscallRecord) {
        if self.records.len() < self.max_records {
            self.records.push(r);
        } else {
            self.dropped += 1;
        }
    }

    /// The task running right now.
    fn current(lx: &Linux<'_, GuestView<'_>>, g: &GuestView<'_>) -> Option<Task> {
        lx.task(lx.current(g.cpu.sys.tpidr_el1)?)
    }
}

impl Tracer for SyscallTracer {
    fn event(&mut self, ev: &Event<'_>, g: &GuestView<'_>) {
        let regs = g.cpu_regs();
        let lx = Linux::new(g, &self.kernel, &regs);
        let user = |va: u64, buf: &mut [u8]| lx.space.read(g, va, buf);
        match ev {
            Event::SyscallEnter(e) => {
                let task = Self::current(&lx, g);
                let (pid, tid, comm, addr) =
                    task.map_or((-1, -1, String::new(), 0), |t| (t.tgid, t.pid, t.comm, t.addr));
                if self.only_pid.is_some_and(|p| p != pid) {
                    return;
                }
                let mut r = SyscallRecord {
                    step: e.step,
                    pid,
                    tid,
                    comm,
                    nr: e.nr,
                    args: e.args,
                    pc: e.pc,
                    ..Default::default()
                };
                let fd_path = |fd: u32| lx.fd_file(addr, fd).map(|f| lx.file_path(f));
                r.decode_entry(&user, &fd_path);
                if matches!(e.nr, 93 | 94) {
                    // exit and exit_group do not return: the record is complete as is.
                    self.push(r);
                } else {
                    self.pending.insert(e.key, r);
                }
            }
            Event::SyscallExit { entry, ret, pc } => {
                let Some(mut r) = self.pending.remove(&entry.key) else { return };
                r.exit_step = Some(g.steps);
                if *pc == entry.pc.wrapping_add(4) {
                    r.ret = Some(*ret as i64);
                    r.decode_exit(&user);
                } else {
                    r.diverted = Some(*pc);
                }
                self.push(r);
            }
            Event::Breakpoint { id, va, regs: cpu } => {
                let task = Self::current(&lx, g);
                let (pid, tid, comm) = task.map_or((-1, -1, String::new()), |t| (t.tgid, t.pid, t.comm));
                let mut args = [0u64; 8];
                args.copy_from_slice(&cpu.x[..8]);
                self.hits.push(BreakpointHit {
                    id: *id,
                    va: *va,
                    step: g.steps,
                    pid,
                    tid,
                    comm,
                    args,
                    lr: cpu.x[30],
                    sp: cpu.sp,
                });
            }
        }
    }
}
