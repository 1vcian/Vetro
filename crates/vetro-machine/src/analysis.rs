//! Analysis tracers on top of the introspection hooks (ADR 0027):
//! the M8 Binder decoder ([`BinderTracer`]) and the container
//! [`Tracers`] that holds more than one (the machine has a single
//! tracer). Nothing writes into the guest.

use std::collections::BTreeMap;

use vetro_analysis::introspect::{BinderLog, Kernel, Linux, Party, Task};

use crate::hooks::{Event, GuestView, Tracer};

/// The kernel profile from a file: Android `boot.img` (GKI kernel
/// decompressed, with kallsyms and BTF inside) or `Image` (with optional
/// `System.map` and detached BTF, like the test kernel).
pub fn kernel_profile(file: &[u8], system_map: Option<&str>, btf: Option<&[u8]>) -> Result<Kernel, String> {
    if file.starts_with(b"ANDROID!") {
        let b = crate::android::BootImage::parse(file).map_err(|e| e.to_string())?;
        let image = crate::android::decompress::decompress(b.kernel).map_err(|e| format!("{e:?}"))?;
        return Kernel::load(Some(&image), system_map, btf);
    }
    Kernel::load(Some(file), system_map, btf)
}

/// Several tracers together: events reach all of them, in order.
#[derive(Default)]
pub struct Tracers(pub Vec<Box<dyn Tracer>>);

impl Tracers {
    /// The tracer of type `T`, if present.
    pub fn get<T: Tracer>(&self) -> Option<&T> {
        self.0.iter().find_map(|t| {
            let a: &dyn std::any::Any = t.as_ref();
            a.downcast_ref::<T>()
        })
    }

    pub fn get_mut<T: Tracer>(&mut self) -> Option<&mut T> {
        self.0.iter_mut().find_map(|t| {
            let a: &mut dyn std::any::Any = t.as_mut();
            a.downcast_mut::<T>()
        })
    }
}

impl Tracer for Tracers {
    fn event(&mut self, ev: &Event<'_>, g: &GuestView<'_>) {
        for t in &mut self.0 {
            t.event(ev, g);
        }
    }
}

/// Process names by tgid: the command line (for apps, the
/// package), read once.
#[derive(Default)]
pub struct ProcessNames(BTreeMap<i32, String>);

impl ProcessNames {
    pub fn name<M: vetro_analysis::introspect::PhysMem + ?Sized>(
        &mut self,
        lx: &Linux<'_, M>,
        t: &Task,
    ) -> String {
        if let Some(n) = self.0.get(&t.tgid) {
            return n.clone();
        }
        let leader = if t.pid == t.tgid { Some(t.clone()) } else { lx.find_pid(t.tgid) };
        let name = leader
            .as_ref()
            .and_then(|l| lx.cmdline(l))
            .map(|c| String::from_utf8_lossy(c.split(|&b| b == 0).next().unwrap_or(&[])).into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| t.comm.clone());
        // A freshly created zygote child is still named like zygote:
        // it is not kept until it takes the app's name.
        if !name.starts_with("zygote") && name != "<pre-initialized>" && name != "usap64" {
            self.0.insert(t.tgid, name.clone());
        }
        name
    }

    /// The process exited or did an exec: the name must be read again.
    pub fn forget(&mut self, tgid: i32) {
        self.0.remove(&tgid);
    }

    pub fn party<M: vetro_analysis::introspect::PhysMem + ?Sized>(
        &mut self,
        lx: &Linux<'_, M>,
        t: &Task,
    ) -> Party {
        let process = self.name(lx, t);
        Party { pid: t.tgid, tid: t.pid, uid: t.uid, comm: t.comm.clone(), process }
    }
}

/// Binder decoder (M8): every EL0 `ioctl(BINDER_WRITE_READ)`, with the
/// transactions sent (on entry) and received (on exit) decoded
/// and paired in [`BinderLog`] (sender, recipient, interface,
/// method, sensitive accesses). Other syscalls cost one comparison.
pub struct BinderTracer {
    pub kernel: Kernel,
    pub log: BinderLog,
    pub names: ProcessNames,
    /// Threads inside a binder `ioctl`: key SP_EL1 -> (observer, arg).
    pending: BTreeMap<u64, (Party, u64)>,
}

impl BinderTracer {
    pub fn new(kernel: Kernel) -> Self {
        BinderTracer {
            kernel,
            log: BinderLog::new(),
            names: ProcessNames::default(),
            pending: BTreeMap::new(),
        }
    }
}

impl Tracer for BinderTracer {
    fn event(&mut self, ev: &Event<'_>, g: &GuestView<'_>) {
        use vetro_analysis::introspect::binder::BINDER_WRITE_READ;
        use vetro_analysis::introspect::strace::{binder_received, binder_sent};
        match ev {
            // execve: the process name changes.
            Event::SyscallEnter(e) if e.nr == 221 => {
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &self.kernel, &regs);
                if let Some(t) = lx.current(g.cpu.sys.tpidr_el1).and_then(|a| lx.task(a)) {
                    self.names.forget(t.tgid);
                }
            }
            Event::SyscallEnter(e) if e.nr == 29 && e.args[1] == BINDER_WRITE_READ => {
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &self.kernel, &regs);
                let Some(task) = lx.current(g.cpu.sys.tpidr_el1).and_then(|a| lx.task(a)) else { return };
                let me = self.names.party(&lx, &task);
                let user = |va: u64, buf: &mut [u8]| lx.space.read(g, va, buf);
                for t in binder_sent(&user, e.args[2]) {
                    self.log.observe(e.step, &t, me.clone());
                }
                self.pending.insert(e.key, (me, e.args[2]));
            }
            Event::SyscallExit { entry, ret, pc } => {
                let Some((me, arg)) = self.pending.remove(&entry.key) else { return };
                if *ret != 0 || *pc != entry.pc.wrapping_add(4) {
                    return;
                }
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &self.kernel, &regs);
                let user = |va: u64, buf: &mut [u8]| lx.space.read(g, va, buf);
                for t in binder_received(&user, arg) {
                    if t.reply {
                        continue;
                    }
                    let before = self.log.calls.len();
                    self.log.observe(g.steps, &t, me.clone());
                    // Call not seen from the sender (thread not
                    // traced before): the sender from the kernel's pid
                    // (0 for oneway calls).
                    if self.log.calls.len() > before
                        && t.sender_pid > 0
                        && let Some(s) = lx.find_pid(t.sender_pid)
                    {
                        let p = self.names.party(&lx, &s);
                        if let Some(c) = self.log.calls.last_mut() {
                            c.sender = Some(Party { uid: t.sender_euid, ..p });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
