//! TLS hooks (M7, ADR 0027): the plaintext of `SSL_write`/`SSL_read`
//! (and `_ex`) of `libssl.so` (BoringSSL, including Conscrypt's in the
//! APEXes), captured with invisible breakpoints resolved from each process's
//! ELF symbols, tied to the connection (fd -> socket -> 4-tuple),
//! to the process and to the library. The conversations end up in
//! [`vetro_analysis::net::TlsConversation`], which the inspector and the HAR
//! merge with the plaintext requests (`NetworkAnalysis::merge_tls`).
//!
//! - The connection is derived from the syscall sequence: `connect` on the
//!   same thread gives the socket fd, and `Linux::socket_endpoints` the
//!   4-tuple from the kernel's `struct sock` (no BoringSSL internal
//!   structures, which change between versions).
//! - `SSL_write` has the plaintext already on entry (buf, num).
//!   `SSL_read` does not: the byte count is the return value, so the
//!   buffer is read when the function returns, with a breakpoint
//!   on the return address (LR) set the first time it is seen.
//!
//! Nothing writes into the guest.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;
use std::rc::Rc;

use vetro_analysis::introspect::mem::ttbr_base;
use vetro_analysis::introspect::{Kernel, Linux, Task};
use vetro_analysis::net::{TlsConversation, TlsMessage};

use crate::analysis::ProcessNames;
use crate::hooks::{Breakpoint, Event, GuestView, Tracer};

/// The `libssl` functions it hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Write,
    Read,
    WriteEx,
    ReadEx,
}

const SYMBOLS: &[(&str, Func)] = &[
    ("SSL_write", Func::Write),
    ("SSL_read", Func::Read),
    ("SSL_write_ex", Func::WriteEx),
    ("SSL_read_ex", Func::ReadEx),
];

/// A hook planned for a process (to be installed between two quanta).
#[derive(Clone, Debug)]
pub struct Planned {
    pub ttbr0: u64,
    pub va: u64,
    pub func: Func,
    pub library: String,
}

/// A read in progress, waiting for the return.
#[derive(Clone, Debug)]
struct PendingRead {
    ex: bool,
    buf: u64,
    /// For `SSL_read_ex`: `size_t *readbytes`.
    readbytes: u64,
    client: SocketAddrV4,
    server: SocketAddrV4,
    lr: u64,
}

pub struct TlsTracer {
    pub kernel: Rc<Kernel>,
    names: ProcessNames,
    /// `ttbr0` of the processes already hooked.
    hooked: BTreeSet<u64>,
    /// (ttbr0, va) -> function and library.
    sites: BTreeMap<(u64, u64), (Func, String)>,
    /// Return addresses of `SSL_read` with an active breakpoint.
    return_sites: BTreeSet<(u64, u64)>,
    /// `SSL_read` returns to install (drained by the service).
    want_returns: Vec<(u64, u64)>,
    /// Last fd passed to `connect` per thread (ttbr0, tid).
    connected: BTreeMap<(u64, i32), u32>,
    /// Reads in progress per thread.
    pending: BTreeMap<(u64, i32), PendingRead>,
    /// Conversations by (ttbr0, client, server).
    convs: BTreeMap<(u64, SocketAddrV4, SocketAddrV4), usize>,
    pub conversations: Vec<TlsConversation>,
    /// Maximum plaintext bytes per message.
    pub cap: usize,
}

impl TlsTracer {
    pub fn new(kernel: Kernel) -> Self {
        TlsTracer {
            kernel: Rc::new(kernel),
            names: ProcessNames::default(),
            hooked: BTreeSet::new(),
            sites: BTreeMap::new(),
            return_sites: BTreeSet::new(),
            want_returns: Vec::new(),
            connected: BTreeMap::new(),
            pending: BTreeMap::new(),
            convs: BTreeMap::new(),
            conversations: Vec::new(),
            cap: 256 << 10,
        }
    }

    /// Library label from the module path.
    fn library(path: &str) -> String {
        if path.contains("conscrypt") {
            "Conscrypt (libssl)".into()
        } else if path.contains("com.android.") {
            format!("libssl ({})", path.rsplit('/').nth(2).unwrap_or("apex"))
        } else {
            "libssl (system)".into()
        }
    }

    /// A copy of the `ttbr0`s already hooked (for the service).
    pub fn hooked(&self) -> BTreeSet<u64> {
        self.hooked.clone()
    }

    /// Plans the hooks for processes not yet in `hooked` (called
    /// by the service with a view of the guest).
    pub fn scan<M: vetro_analysis::introspect::PhysMem + ?Sized>(
        lx: &Linux<'_, M>,
        hooked: &BTreeSet<u64>,
    ) -> Vec<Planned> {
        let mut out = Vec::new();
        for t in lx.processes() {
            if t.mm == 0 {
                continue;
            }
            let Some(space) = lx.user_space(t.mm) else { continue };
            let ttbr0 = ttbr_base(space.ttbr0);
            if hooked.contains(&ttbr0) {
                continue;
            }
            let Some((path, _, _)) = lx.module(&t, "libssl.so") else { continue };
            let library = Self::library(&path);
            for (name, func) in SYMBOLS {
                if let Some(va) = lx.user_symbol(&t, "libssl.so", name) {
                    out.push(Planned { ttbr0, va, func: *func, library: library.clone() });
                }
            }
        }
        out
    }

    /// Records an installed hook.
    pub fn registered(&mut self, p: &Planned) {
        self.sites.insert((p.ttbr0, p.va), (p.func, p.library.clone()));
        self.hooked.insert(p.ttbr0);
    }

    /// The `SSL_read` returns to install now (once each).
    pub fn take_return_requests(&mut self) -> Vec<Breakpoint> {
        std::mem::take(&mut self.want_returns)
            .into_iter()
            .filter(|k| self.return_sites.insert(*k))
            .map(|(ttbr0, va)| Breakpoint { va, ttbr0: Some(ttbr0) })
            .collect()
    }

    fn current<'a, M: vetro_analysis::introspect::PhysMem + ?Sized>(
        lx: &Linux<'a, M>,
        g: &GuestView<'_>,
    ) -> Option<Task> {
        lx.task(lx.current(g.cpu.sys.tpidr_el1)?)
    }

    /// The 4-tuple of the fd most recently connected by this thread.
    fn endpoints<M: vetro_analysis::introspect::PhysMem + ?Sized>(
        &self,
        lx: &Linux<'_, M>,
        key: (u64, i32),
        task: &Task,
    ) -> Option<(SocketAddrV4, SocketAddrV4)> {
        if let Some(&fd) = self.connected.get(&key)
            && let Some(e) = lx.socket_endpoints(task.addr, fd)
        {
            return Some(e);
        }
        // Fallback: the first connected IPv4 socket among the open fds.
        for f in lx.files(task.addr) {
            if let Some(e) = lx.socket_endpoints(task.addr, f.fd)
                && e.1.port() != 0
            {
                return Some(e);
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn append(
        &mut self,
        ttbr0: u64,
        at_us: u64,
        to_server: bool,
        ends: (SocketAddrV4, SocketAddrV4),
        data: Vec<u8>,
        meta: (i32, i32, String, Option<String>, String),
    ) {
        let key = (ttbr0, ends.0, ends.1);
        let convs = &mut self.convs;
        let conversations = &mut self.conversations;
        let idx = *convs.entry(key).or_insert_with(|| {
            let (pid, tid, process, package, library) = meta;
            conversations.push(TlsConversation {
                client: ends.0,
                server: ends.1,
                host: None,
                pid,
                tid,
                process,
                package,
                library,
                messages: Vec::new(),
            });
            conversations.len() - 1
        });
        conversations[idx].messages.push(TlsMessage { at_us, to_server, data });
    }
}

impl Tracer for TlsTracer {
    fn event(&mut self, ev: &Event<'_>, g: &GuestView<'_>) {
        let at_us = g.steps / 100;
        match ev {
            // connect(fd, ...): remember the thread's fd.
            Event::SyscallEnter(e) if e.nr == 203 => {
                let ttbr0 = ttbr_base(e.ttbr0);
                let kernel = self.kernel.clone();
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &kernel, &regs);
                if let Some(t) = Self::current(&lx, g) {
                    self.connected.insert((ttbr0, t.pid), e.args[0] as u32);
                }
            }
            Event::SyscallEnter(e) if matches!(e.nr, 93 | 94 | 221) => {
                // exit/execve: forget the process name.
                let kernel = self.kernel.clone();
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &kernel, &regs);
                if let Some(t) = Self::current(&lx, g) {
                    self.names.forget(t.tgid);
                }
            }
            Event::Breakpoint { va, regs: cpu, .. } => {
                let ttbr0 = ttbr_base(cpu.sys.ttbr0_el1);
                let kernel = self.kernel.clone();
                let regs = g.cpu_regs();
                let lx = Linux::new(g, &kernel, &regs);
                let Some(task) = Self::current(&lx, g) else { return };
                let key = (ttbr0, task.pid);
                // Return from SSL_read?
                if self.return_sites.contains(&(ttbr0, *va))
                    && let Some(p) = self.pending.remove(&key)
                    && p.lr == *va
                {
                    let n = if p.ex {
                        // SSL_read_ex: ret 1 = ok, bytes in *readbytes.
                        if cpu.x[0] == 0 {
                            0
                        } else {
                            let mut b = [0u8; 8];
                            lx.space.read(g, p.readbytes, &mut b);
                            u64::from_le_bytes(b) as i64
                        }
                    } else {
                        cpu.x[0] as i64
                    };
                    if n > 0 {
                        let take = (n as usize).min(self.cap);
                        let mut buf = vec![0u8; take];
                        if lx.space.read(g, p.buf, &mut buf) {
                            let lib = self.site_library(ttbr0);
                            let meta = self.party(&lx, &task, &lib);
                            self.append(ttbr0, at_us, false, (p.client, p.server), buf, meta);
                        }
                    }
                    return;
                }
                // Entry into a hooked function.
                let Some((func, library)) = self.sites.get(&(ttbr0, *va)).cloned() else { return };
                let Some(ends) = self.endpoints(&lx, key, &task) else { return };
                match func {
                    Func::Write | Func::WriteEx => {
                        let (buf, num) = (cpu.x[1], cpu.x[2]);
                        let take = (num as usize).min(self.cap);
                        let mut data = vec![0u8; take];
                        if take > 0 && lx.space.read(g, buf, &mut data) {
                            let meta = self.party(&lx, &task, &library);
                            self.append(ttbr0, at_us, true, ends, data, meta);
                        }
                    }
                    Func::Read | Func::ReadEx => {
                        let ex = func == Func::ReadEx;
                        let (buf, readbytes) = (cpu.x[1], if ex { cpu.x[3] } else { 0 });
                        self.pending.insert(
                            key,
                            PendingRead { ex, buf, readbytes, client: ends.0, server: ends.1, lr: cpu.x[30] },
                        );
                        self.want_returns.push((ttbr0, cpu.x[30]));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Installs and updates the TLS hooks between two quanta (M7): finds
/// `libssl` in new processes, sets breakpoints on
/// `SSL_write`/`SSL_read`/`_ex` and the breakpoints on the `SSL_read` returns
/// requested by the hooks. It must be called now and then during execution:
/// processes (and apps) appear over time.
pub fn tls_service(m: &mut crate::Machine) {
    let Some(kernel) = m
        .tracer_mut::<crate::analysis::Tracers>()
        .and_then(|t| t.get::<TlsTracer>())
        .map(|t| t.kernel.clone())
    else {
        return;
    };
    let hooked = m
        .tracer_mut::<crate::analysis::Tracers>()
        .and_then(|t| t.get::<TlsTracer>())
        .map(TlsTracer::hooked)
        .unwrap_or_default();
    let plan = m.linux(kernel.as_ref(), |lx| TlsTracer::scan(lx, &hooked));
    for p in plan {
        m.add_breakpoint(Breakpoint { va: p.va, ttbr0: Some(p.ttbr0) });
        if let Some(t) = m.tracer_mut::<crate::analysis::Tracers>().and_then(|t| t.get_mut::<TlsTracer>()) {
            t.registered(&p);
        }
    }
    let returns = m
        .tracer_mut::<crate::analysis::Tracers>()
        .and_then(|t| t.get_mut::<TlsTracer>())
        .map(|t| t.take_return_requests())
        .unwrap_or_default();
    for bp in returns {
        m.add_breakpoint(bp);
    }
}

impl TlsTracer {
    fn site_library(&self, ttbr0: u64) -> String {
        self.sites
            .range((ttbr0, 0)..=(ttbr0, u64::MAX))
            .next()
            .map(|(_, (_, l))| l.clone())
            .unwrap_or_else(|| "libssl".into())
    }

    fn party<M: vetro_analysis::introspect::PhysMem + ?Sized>(
        &mut self,
        lx: &Linux<'_, M>,
        task: &Task,
        library: &str,
    ) -> (i32, i32, String, Option<String>, String) {
        let p = self.names.party(lx, task);
        let package = p.package().map(str::to_string);
        (p.pid, p.tid, p.process, package, library.to_string())
    }
}
