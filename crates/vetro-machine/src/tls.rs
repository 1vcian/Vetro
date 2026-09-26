//! Hook TLS (M7, ADR 0027): il testo in chiaro di `SSL_write`/`SSL_read`
//! (e `_ex`) di `libssl.so` (BoringSSL, anche quella di Conscrypt negli
//! APEX), catturato con punti d'arresto invisibili risolti dai simboli ELF
//! di ogni processo, legato alla connessione (fd -> socket -> 4-tupla),
//! al processo e alla libreria. Le conversazioni finiscono in
//! [`vetro_analysis::net::TlsConversation`], che l'ispettore e l'HAR
//! uniscono alle richieste in chiaro (`NetworkAnalysis::merge_tls`).
//!
//! - La connessione si ricava dalla sequenza di syscall: `connect` sullo
//!   stesso thread dà il fd del socket, e `Linux::socket_endpoints` la
//!   4-tupla dalla `struct sock` del kernel (niente strutture interne di
//!   BoringSSL, che cambiano versione).
//! - `SSL_write` ha il testo in chiaro già all'ingresso (buf, num).
//!   `SSL_read` no: il numero di byte è il valore di ritorno, quindi il
//!   buffer si legge quando la funzione ritorna, con un punto d'arresto
//!   sull'indirizzo di ritorno (LR) messo la prima volta che lo si vede.
//!
//! Nulla scrive nel guest.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;
use std::rc::Rc;

use vetro_analysis::introspect::mem::ttbr_base;
use vetro_analysis::introspect::{Kernel, Linux, Task};
use vetro_analysis::net::{TlsConversation, TlsMessage};

use crate::analysis::ProcessNames;
use crate::hooks::{Breakpoint, Event, GuestView, Tracer};

/// Le funzioni di `libssl` che aggancia.
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

/// Un aggancio pianificato per un processo (da installare fra due quanti).
#[derive(Clone, Debug)]
pub struct Planned {
    pub ttbr0: u64,
    pub va: u64,
    pub func: Func,
    pub library: String,
}

/// Una lettura in corso, in attesa del ritorno.
#[derive(Clone, Debug)]
struct PendingRead {
    ex: bool,
    buf: u64,
    /// Per `SSL_read_ex`: `size_t *readbytes`.
    readbytes: u64,
    client: SocketAddrV4,
    server: SocketAddrV4,
    lr: u64,
}

pub struct TlsTracer {
    pub kernel: Rc<Kernel>,
    names: ProcessNames,
    /// `ttbr0` dei processi già agganciati.
    hooked: BTreeSet<u64>,
    /// (ttbr0, va) -> funzione e libreria.
    sites: BTreeMap<(u64, u64), (Func, String)>,
    /// Indirizzi di ritorno di `SSL_read` con un punto d'arresto attivo.
    return_sites: BTreeSet<(u64, u64)>,
    /// Ritorni di `SSL_read` da installare (drenati dal servizio).
    want_returns: Vec<(u64, u64)>,
    /// Ultimo fd passato a `connect` per thread (ttbr0, tid).
    connected: BTreeMap<(u64, i32), u32>,
    /// Letture in corso per thread.
    pending: BTreeMap<(u64, i32), PendingRead>,
    /// Conversazioni per (ttbr0, client, server).
    convs: BTreeMap<(u64, SocketAddrV4, SocketAddrV4), usize>,
    pub conversations: Vec<TlsConversation>,
    /// Byte di testo in chiaro al più per messaggio.
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

    /// Etichetta della libreria dal percorso del modulo.
    fn library(path: &str) -> String {
        if path.contains("conscrypt") {
            "Conscrypt (libssl)".into()
        } else if path.contains("com.android.") {
            format!("libssl ({})", path.rsplit('/').nth(2).unwrap_or("apex"))
        } else {
            "libssl (sistema)".into()
        }
    }

    /// La copia dei `ttbr0` già agganciati (per il servizio).
    pub fn hooked(&self) -> BTreeSet<u64> {
        self.hooked.clone()
    }

    /// Pianifica gli agganci dei processi non ancora in `hooked` (chiamato
    /// dal servizio con una vista del guest).
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

    /// Registra un aggancio installato.
    pub fn registered(&mut self, p: &Planned) {
        self.sites.insert((p.ttbr0, p.va), (p.func, p.library.clone()));
        self.hooked.insert(p.ttbr0);
    }

    /// I ritorni di `SSL_read` da installare adesso (una volta ciascuno).
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

    /// La 4-tupla del fd connesso più di recente da questo thread.
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
        // Ripiego: il primo socket IPv4 connesso fra i fd aperti.
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
            // connect(fd, ...): ricorda il fd del thread.
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
                // exit/execve: dimentica il nome del processo.
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
                // Ritorno di SSL_read?
                if self.return_sites.contains(&(ttbr0, *va))
                    && let Some(p) = self.pending.remove(&key)
                    && p.lr == *va
                {
                    let n = if p.ex {
                        // SSL_read_ex: ret 1 = ok, byte in *readbytes.
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
                // Ingresso di una funzione agganciata.
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

/// Installa e aggiorna gli agganci TLS fra due quanti (M7): trova
/// `libssl` nei processi nuovi, mette i punti d'arresto su
/// `SSL_write`/`SSL_read`/`_ex` e i punti sui ritorni di `SSL_read`
/// richiesti dagli hook. Va chiamato ogni tanto durante l'esecuzione: i
/// processi (e le app) appaiono col tempo.
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
