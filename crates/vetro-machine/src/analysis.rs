//! Tracciatori di analisi sopra gli agganci dell'introspezione (ADR 0027):
//! il decoder Binder di M8 ([`BinderTracer`]) e il contenitore
//! [`Tracers`] che ne ospita più d'uno (la macchina ha un solo
//! tracciatore). Nulla scrive nel guest.

use std::collections::BTreeMap;

use vetro_analysis::introspect::{BinderLog, Kernel, Linux, Party, Task};

use crate::hooks::{Event, GuestView, Tracer};

/// Il profilo del kernel da un file: `boot.img` di Android (kernel GKI
/// decompresso, con kallsyms e BTF dentro) o `Image` (con `System.map` e
/// BTF staccato facoltativi, come il kernel di prova).
pub fn kernel_profile(file: &[u8], system_map: Option<&str>, btf: Option<&[u8]>) -> Result<Kernel, String> {
    if file.starts_with(b"ANDROID!") {
        let b = crate::android::BootImage::parse(file).map_err(|e| e.to_string())?;
        let image = crate::android::decompress::decompress(b.kernel).map_err(|e| format!("{e:?}"))?;
        return Kernel::load(Some(&image), system_map, btf);
    }
    Kernel::load(Some(file), system_map, btf)
}

/// Più tracciatori insieme: gli eventi arrivano a tutti, nell'ordine.
#[derive(Default)]
pub struct Tracers(pub Vec<Box<dyn Tracer>>);

impl Tracers {
    /// Il tracciatore di tipo `T`, se c'è.
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

/// Nomi dei processi per tgid: la riga di comando (per le app il
/// pacchetto), letta una volta.
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
        // Un figlio di zygote appena creato si chiama ancora come zygote:
        // non si conserva finché non prende il nome dell'app.
        if !name.starts_with("zygote") && name != "<pre-initialized>" && name != "usap64" {
            self.0.insert(t.tgid, name.clone());
        }
        name
    }

    /// Il processo è uscito o ha fatto exec: il nome va riletto.
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

/// Decoder Binder (M8): ogni `ioctl(BINDER_WRITE_READ)` di EL0, con le
/// transazioni inviate (all'ingresso) e ricevute (all'uscita) decodificate
/// e accoppiate in [`BinderLog`] (mittente, destinatario, interfaccia,
/// metodo, accessi sensibili). Le altre syscall costano un confronto.
pub struct BinderTracer {
    pub kernel: Kernel,
    pub log: BinderLog,
    pub names: ProcessNames,
    /// Thread dentro una `ioctl` binder: chiave SP_EL1 -> (osservatore, arg).
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
            // execve: il nome del processo cambia.
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
                    // Chiamata non vista dal mittente (thread non
                    // tracciato prima): il mittente dal pid del kernel
                    // (0 per le oneway).
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
