//! Introspezione dall'esterno sul kernel guest di M3 (ADR 0027): la base
//! degli hook TLS (M7), del decoder Binder (M8) e dello scripting (M9).
//!
//! Il profilo del kernel viene da `System.map` e dal BTF staccato
//! (`target/guest-kernel/vmlinux.btf`, `tools/guest-kernel/build.sh`).
//! Una sessione alla shell del guest, con la macchina ferma fra un quanto
//! e l'altro:
//!
//! - l'elenco dei processi letto dalla memoria è quello di `ps`;
//! - le mappe di un processo sono il testo di `/proc/<pid>/maps`, i file
//!   aperti quelli di `/proc/<pid>/fd`, la riga di comando quella vera;
//! - i simboli di `vetro-dev` (statico, non strippato) letti dal file nella
//!   page cache del guest sono quelli del file sull'host;
//! - le syscall tracciate di `cat` lanciato come utente 501 sono, in
//!   ordine, quelle di `qemu-aarch64 -strace` sullo stesso binario
//!   (oracolo), con percorso, byte letti e risultati;
//! - i punti d'arresto invisibili su `open` e `ioctl` di `vetro-dev`
//!   scattano una volta per chiamata (tante quante le syscall `openat` e
//!   `ioctl` del processo, con gli stessi argomenti), quello sul punto
//!   d'ingresso di BusyBox una volta per `exec`;
//! - **determinismo**: la stessa sessione senza agganci dà la stessa
//!   console e lo stesso stato finale (istruzioni, CPU, MMU, dispositivi,
//!   RAM); registrata senza agganci e rifatta col JIT con gli agganci, dà
//!   gli stessi eventi, e il replay è identico.
//!
//! Solo in release, come `vetro.rs`. Tempi in
//! `target/guest-kernel/introspect-misure.txt`.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Instant;

use vetro_boot_tests::*;
use vetro_machine::introspect::{BreakpointHit, SyscallTracer};
use vetro_machine::vetro_analysis::introspect::{Kernel, SyscallRecord, elf};
use vetro_machine::{
    Breakpoint, Devices, Input, Log, Machine, MachineConfig, RecordOptions, ReplayStatus, Stop,
};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const JIT_THRESHOLD: u32 = 16;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn kernel() -> Kernel {
    let dir = repo_root().join("target/guest-kernel");
    let map = std::fs::read_to_string(dir.join("System.map")).expect("System.map");
    let btf = std::fs::read(dir.join("vmlinux.btf")).expect("vmlinux.btf (tools/guest-kernel/build.sh)");
    Kernel::load(None, Some(&map), Some(&btf)).expect("profilo del kernel")
}

/// Indirizzi dai file sull'host: `open` e `ioctl` di vetro-dev, punto
/// d'ingresso di BusyBox.
struct Addrs {
    open: u64,
    ioctl: u64,
    busybox_entry: u64,
}

fn addrs() -> Addrs {
    let root = repo_root();
    let dev = std::fs::read(root.join("target/guest-kernel/vetro-dev")).expect("vetro-dev");
    let syms = elf::file_symbols(&dev);
    let bb = std::fs::read(root.join("target/guest-bins/busybox")).expect("busybox");
    Addrs {
        open: elf::find(&syms, "open").expect("open in vetro-dev").value,
        ioctl: elf::find(&syms, "ioctl").expect("ioctl in vetro-dev").value,
        busybox_entry: elf::header(&bb).expect("ELF di busybox").entry,
    }
}

struct Script {
    m: Machine,
    log: Vec<u8>,
}

impl Script {
    fn quantum(&mut self) -> Stop {
        let s = self.m.run(QUANTUM);
        self.log.extend(self.m.console_output());
        s
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            let stop = self.quantum();
            assert_eq!(stop, Stop::Budget, "{stop:?} in attesa di {needle:?}:\n{}", self.tail());
        }
    }

    /// Manda un comando e aspetta il prompt: posizione e uscita (senza
    /// l'eco del comando).
    fn command(&mut self, cmd: &str, from: usize) -> (usize, String) {
        self.m.input(Input::Console(format!("{cmd}\n").into_bytes()));
        let at = self.until(SHELL_PROMPT, from);
        let out = normalize(&String::from_utf8_lossy(&self.log[from..at]));
        // Dopo la riga dell'eco del comando, senza il prompt finale.
        let start = out.find(cmd).map_or(0, |i| i + cmd.len());
        let rest = out[start..].split_once('\n').map_or("", |(_, r)| r);
        let body: Vec<&str> = rest.lines().collect();
        let n = body.len().saturating_sub(1);
        (at, body[..n].join("\n"))
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

/// Esito di una sessione.
struct Session {
    log: Vec<u8>,
    end: vetro_machine::Digest,
    records: Vec<SyscallRecord>,
    hits: Vec<BreakpointHit>,
    recording: Option<Log>,
    secs: f64,
}

#[derive(Clone, Copy)]
struct Mode {
    /// Tracciatore, syscall e punti d'arresto dall'avvio.
    hooks: bool,
    /// Confronti con il guest (ps, maps, fd, simboli, oracolo).
    checks: bool,
    record: bool,
    jit: bool,
}

fn machine() -> Machine {
    Machine::with_devices(&MachineConfig::default(), &Devices::default())
}

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let mut m = machine();
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    m
}

fn arm(m: &mut Machine, k: &Kernel, a: &Addrs) -> [u32; 3] {
    m.set_tracer(Some(Box::new(SyscallTracer::new(k.clone()))));
    m.trace_syscalls(true);
    [
        m.add_breakpoint(Breakpoint { va: a.open, ttbr0: None }),
        m.add_breakpoint(Breakpoint { va: a.ioctl, ttbr0: None }),
        m.add_breakpoint(Breakpoint { va: a.busybox_entry, ttbr0: None }),
    ]
}

fn tracer(m: &mut Machine) -> &mut SyscallTracer {
    m.tracer_mut::<SyscallTracer>().expect("tracciatore")
}

fn session(image: &[u8], initrd: &[u8], k: &Kernel, a: &Addrs, mode: Mode) -> Session {
    let mut s = Script { m: booted(image, initrd), log: Vec::new() };
    if mode.jit {
        s.m.set_jit(Some(vetro_jit_native::system_jit(JIT_THRESHOLD)));
    }
    let bps = if mode.hooks { Some(arm(&mut s.m, k, a)) } else { None };
    if mode.record {
        s.m.start_recording(RecordOptions::default());
    }
    let t0 = Instant::now();
    let at = s.until(SHELL_PROMPT, 0);

    // 1. Processi: `ps` contro la lista letta dalla memoria.
    let (at, ps) = s.command("ps -o pid,ppid,user,comm", at);
    if mode.checks {
        check_ps(&s.m, k, &ps);
    }

    // 2. Un processo fermo in una read: mappe, file aperti, riga di
    // comando, simboli.
    s.m.input(Input::Console(b"vetro-dev input-read /dev/input/event0 1 &\n".to_vec()));
    let at = s.until("VETRO-INPUT-PRONTO", at);
    let (at, out) = s.command("echo P=$!", at);
    let pid: i32 = out.lines().find_map(|l| l.strip_prefix("P=")).expect("pid").trim().parse().unwrap();
    let (at, maps) = s.command(&format!("cat /proc/{pid}/maps"), at);
    let (at, fds) = s.command(&format!("ls -l /proc/{pid}/fd | cat"), at);
    if mode.checks {
        check_process(&s.m, k, a, pid, &maps, &fds);
    }

    // 3. Punti d'arresto su open e ioctl di vetro-dev.
    let from_rec = if mode.hooks { tracer(&mut s.m).records.len() } else { 0 };
    let from_hit = if mode.hooks { tracer(&mut s.m).hits.len() } else { 0 };
    let (at, out) = s.command("vetro-dev input", at);
    assert!(out.contains("event1"), "{out}");
    if mode.checks {
        let bps = bps.expect("agganci");
        let t = tracer(&mut s.m);
        check_breakpoints(&t.records[from_rec..], &t.hits[from_hit..], bps, "vetro-dev");
    }

    // 4. Punto d'ingresso di BusyBox e decodifica di openat/read.
    let from_rec = if mode.hooks { tracer(&mut s.m).records.len() } else { 0 };
    let from_hit = if mode.hooks { tracer(&mut s.m).hits.len() } else { 0 };
    let (at, version) = s.command("head -n 1 /proc/version", at);
    assert!(version.starts_with("Linux version 6.18"), "{version}");
    if mode.checks {
        let bps = bps.expect("agganci");
        let t = tracer(&mut s.m);
        let entries: Vec<_> = t.hits[from_hit..].iter().filter(|h| h.id == bps[2]).collect();
        assert_eq!(entries.len(), 1, "un exec di BusyBox (head): {entries:?}");
        assert_eq!(entries[0].comm, "head");
        let recs: Vec<&SyscallRecord> =
            t.records[from_rec..].iter().filter(|r| r.pid == entries[0].pid).collect();
        let open = recs.iter().find(|r| r.name() == "openat").expect("openat");
        assert_eq!(open.path.as_deref(), Some("/proc/version"));
        let fd = open.ret.expect("openat tornata");
        assert!(fd >= 3);
        let read = recs.iter().find(|r| r.name() == "read" && r.args[0] == fd as u64).expect("read");
        assert_eq!(read.fd_path.as_deref(), Some("/proc/version"));
        assert!(read.data.starts_with(b"Linux version 6.18"), "{}", read.line());
        assert_eq!(read.ret, Some(read.data.len() as i64));
        assert!(recs.iter().any(|r| r.name() == "exit_group"), "exit_group registrata all'ingresso");
    }

    // 5. `cat` come utente 501 (lo stesso percorso di BusyBox che fa
    // qemu-aarch64 in un utente normale), per l'oracolo.
    let from_rec = if mode.hooks { tracer(&mut s.m).records.len() } else { 0 };
    let (at, _) = s.command("cd /etc && vetro-dev run-as 501 20 cat autotest.sh >/dev/null; cd /", at);
    if mode.checks {
        let t = tracer(&mut s.m);
        check_oracle(&t.records[from_rec..]);
    }
    let _ = at;

    let secs = t0.elapsed().as_secs_f64();
    let recording = if mode.record { s.m.stop_recording() } else { None };
    let (records, hits) = if mode.hooks {
        let t = tracer(&mut s.m);
        (core::mem::take(&mut t.records), core::mem::take(&mut t.hits))
    } else {
        (Vec::new(), Vec::new())
    };
    let end = s.m.digest();
    Session { log: s.log, end, records, hits, recording, secs }
}

/// `ps -o pid,ppid,user,comm` contro la lista dei processi.
fn check_ps(m: &Machine, k: &Kernel, ps: &str) {
    let mut guest: BTreeMap<i32, (i32, String, String)> = BTreeMap::new();
    for l in ps.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let Some(pid) = w.first().and_then(|p| p.parse::<i32>().ok()) else { continue };
        guest.insert(pid, (w[1].parse().unwrap(), w[2].to_string(), w[3..].join(" ")));
    }
    // `ps` stesso è uscito.
    guest.retain(|_, (_, _, comm)| comm != "ps");
    let ours: BTreeMap<i32, (i32, String, String)> = m.linux(k, |lx| {
        lx.processes().into_iter().map(|t| (t.pid, (t.ppid, t.euid.to_string(), t.comm.clone()))).collect()
    });
    assert!(ours.len() > 20, "processi e thread del kernel: {ours:?}");
    let mut diff = Vec::new();
    for (pid, g) in &guest {
        match ours.get(pid) {
            // I worker dei workqueue: /proc aggiunge "-<lavoro>" al nome.
            Some(o) if o.0 == g.0 && o.1 == g.1 && (o.2 == g.2 || g.2.starts_with(&format!("{}-", o.2))) => {}
            o => diff.push(format!("pid {pid}: ps {g:?}, memoria {o:?}")),
        }
    }
    for pid in ours.keys().filter(|p| !guest.contains_key(p)) {
        diff.push(format!("pid {pid}: solo in memoria {:?}", ours[pid]));
    }
    assert!(diff.is_empty(), "ps e memoria diversi:\n{}\n\nps:\n{ps}", diff.join("\n"));
    eprintln!("ps: {} processi uguali", guest.len());
}

fn check_process(m: &Machine, k: &Kernel, a: &Addrs, pid: i32, maps: &str, fds: &str) {
    m.linux(k, |lx| {
        let t = lx.processes().into_iter().find(|t| t.pid == pid).expect("processo in memoria");
        assert_eq!(t.comm, "vetro-dev");
        assert_eq!(lx.threads(t.addr).iter().map(|x| x.pid).collect::<Vec<_>>(), [pid]);
        let ours = lx.maps(&t);
        assert_eq!(ours.trim_end(), maps.trim_end(), "/proc/{pid}/maps");
        assert!(
            maps.contains("[stack]") && maps.contains("[vdso]") && maps.contains("/bin/vetro-dev"),
            "{maps}"
        );
        // ls -l: "... N -> destinazione".
        let guest: BTreeMap<u32, String> = fds
            .lines()
            .filter_map(|l| {
                let (left, target) = l.split_once(" -> ")?;
                Some((left.split_whitespace().last()?.parse().ok()?, target.to_string()))
            })
            .collect();
        let ours: BTreeMap<u32, String> = lx.files(t.addr).into_iter().map(|f| (f.fd, f.path)).collect();
        assert_eq!(ours, guest, "/proc/{pid}/fd:\n{fds}");
        assert_eq!(ours.get(&3).map(String::as_str), Some("/dev/input/event0"));
        let cmd = lx.cmdline(&t).expect("cmdline");
        assert_eq!(cmd, b"vetro-dev\0input-read\0/dev/input/event0\x001\0");
        // Simboli dal file nella page cache del guest (.symtab) e punto
        // d'ingresso dall'ELF in memoria: gli stessi del file sull'host.
        assert_eq!(lx.user_symbol(&t, "vetro-dev", "open"), Some(a.open));
        assert_eq!(lx.user_symbol(&t, "vetro-dev", "ioctl"), Some(a.ioctl));
        let host = std::fs::read(repo_root().join("target/guest-kernel/vetro-dev")).unwrap();
        let inode = lx.file_inode(lx.module(&t, "vetro-dev").unwrap().2).unwrap();
        assert_eq!(lx.file_bytes(inode, 64 << 20).as_deref(), Some(host.as_slice()), "file dalla page cache");
        eprintln!("processo {pid}: {} regioni, {} file, simboli e file uguali", ours.len(), guest.len());
    });
}

/// Ogni chiamata di `open`/`ioctl` di vetro-dev scatta una volta, con gli
/// argomenti della syscall che fa.
fn check_breakpoints(records: &[SyscallRecord], hits: &[BreakpointHit], bps: [u32; 3], comm: &str) {
    let pid = hits.iter().find(|h| h.comm == comm && h.id == bps[0]).expect("open di vetro-dev").pid;
    let hits: Vec<&BreakpointHit> = hits.iter().filter(|h| h.pid == pid).collect();
    let recs: Vec<&SyscallRecord> = records.iter().filter(|r| r.pid == pid).collect();
    let opens: Vec<_> = hits.iter().filter(|h| h.id == bps[0]).collect();
    let ioctls: Vec<_> = hits.iter().filter(|h| h.id == bps[1]).collect();
    // Dopo l'exec: le syscall di vetro-dev (prima c'è la shell figlia).
    let exec = recs.iter().rposition(|r| r.name() == "execve").expect("execve");
    let sys_open: Vec<_> = recs[exec..].iter().filter(|r| r.name() == "openat").collect();
    let sys_ioctl: Vec<_> = recs[exec..].iter().filter(|r| r.name() == "ioctl").collect();
    assert!(opens.len() >= 2, "{opens:?}");
    assert_eq!(opens.len(), sys_open.len(), "open: punti d'arresto e syscall");
    assert_eq!(ioctls.len(), sys_ioctl.len(), "ioctl: punti d'arresto e syscall");
    for (h, r) in opens.iter().zip(&sys_open) {
        assert_eq!(h.args[0], r.args[1], "open(path) e openat(AT_FDCWD, path)");
        assert!(h.step < r.step, "il punto d'arresto viene prima della syscall");
    }
    for (h, r) in ioctls.iter().zip(&sys_ioctl) {
        // `int ioctl(int fd, int req, ...)`: musl estende il segno di req.
        assert_eq!(
            (h.args[0], h.args[1] as u32, h.args[2]),
            (r.args[0], r.args[1] as u32, r.args[2]),
            "ioctl: stessi argomenti"
        );
        assert_eq!(r.args[1], h.args[1] as u32 as i32 as i64 as u64);
    }
    assert!(sys_open.iter().all(|r| r.path.as_deref().is_some_and(|p| p.starts_with("/dev/input/event"))));
    eprintln!("punti d'arresto: {} open, {} ioctl uguali alle syscall", opens.len(), ioctls.len());
}

/// Le syscall di `cat autotest.sh` (utente 501) contro `qemu-aarch64 -strace`.
fn check_oracle(records: &[SyscallRecord]) {
    // Il processo di cat: quello con l'openat di autotest.sh.
    let pid =
        records.iter().find(|r| r.path.as_deref() == Some("autotest.sh")).expect("openat di autotest.sh").pid;
    let recs: Vec<&SyscallRecord> = records.iter().filter(|r| r.pid == pid).collect();
    let exec = recs.iter().rposition(|r| r.name() == "execve").expect("execve");
    assert!(recs[exec].diverted.is_some(), "execve riuscita: ritorno al nuovo programma");
    let ours: Vec<&str> = recs[exec + 1..].iter().map(|r| r.name()).collect();
    let open = recs.iter().find(|r| r.name() == "openat").unwrap();
    let sends: Vec<i64> = recs.iter().filter(|r| r.name() == "sendfile").filter_map(|r| r.ret).collect();
    let size =
        std::fs::metadata(repo_root().join("guest/kernel/initramfs/autotest.sh")).unwrap().len() as i64;
    assert_eq!(sends, [size, 0], "sendfile: tutto il file, poi 0");
    assert_eq!(open.ret, Some(3));
    let Some(qemu) = std::env::var_os("VETRO_QEMU_AARCH64") else {
        skip_or_fail("VETRO_REQUIRE_ORACLE", "oracolo delle syscall: manca VETRO_QEMU_AARCH64");
        return;
    };
    let root = repo_root();
    let out = Command::new(qemu)
        .arg("-strace")
        .arg(root.join("target/guest-bins/busybox"))
        .args(["cat", "autotest.sh"])
        .current_dir(root.join("guest/kernel/initramfs"))
        .output()
        .expect("qemu-aarch64");
    let text = String::from_utf8_lossy(&out.stderr);
    let theirs: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let (pid, rest) = l.split_once(' ')?;
            pid.parse::<u32>().ok()?;
            let name = rest.split('(').next()?;
            (!name.is_empty()
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .then(|| name.to_string())
        })
        .collect();
    assert_eq!(ours, theirs, "syscall di cat: Vetro (tracciate dall'esterno) e qemu-aarch64 -strace\n{text}");
    eprintln!("oracolo: {} syscall uguali a qemu-aarch64 -strace", ours.len());
}

fn same_log(what: &str, a: &[u8], b: &[u8]) {
    if a != b {
        let i = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        let s = i.saturating_sub(200);
        panic!(
            "{what}: console diversa al byte {i}\natteso: {}\nottenuto: {}",
            String::from_utf8_lossy(&a[s..(i + 200).min(a.len())]),
            String::from_utf8_lossy(&b[s..(i + 200).min(b.len())])
        );
    }
}

#[test]
fn introspezione_dall_esterno() {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "introspezione sul kernel guest solo in release");
        return;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "manca target/guest-kernel (tools/guest-kernel/build.sh)");
        return;
    };
    if !repo_root().join("target/guest-kernel/vmlinux.btf").is_file() {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "manca target/guest-kernel/vmlinux.btf (tools/guest-kernel/build.sh)",
        );
        return;
    }
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let k = kernel();
    let a = addrs();
    let jit = std::env::var("VETRO_JIT").is_ok_and(|v| v == "1");

    // Con gli agganci e i confronti.
    let traced = session(&image, &initrd, &k, &a, Mode { hooks: true, checks: true, record: false, jit });
    assert!(traced.records.len() > 1000, "{} syscall", traced.records.len());
    let binder_free = traced.records.iter().all(|r| r.binder.is_empty());
    assert!(binder_free, "niente binder nel guest di prova");

    // Senza agganci, registrata: stessa esecuzione.
    let plain = session(&image, &initrd, &k, &a, Mode { hooks: false, checks: false, record: true, jit });
    same_log("senza agganci", &traced.log, &plain.log);
    assert_eq!(plain.end, traced.end, "stato finale con e senza agganci");

    // Replay della sessione senza agganci col JIT (l'altro motore) e con
    // gli agganci dall'avvio: stessi eventi, replay identico.
    let log = plain.recording.expect("registrazione");
    let mut m = booted(&image, &initrd);
    m.set_jit(Some(vetro_jit_native::system_jit(JIT_THRESHOLD)));
    arm(&mut m, &k, &a);
    m.start_replay(&log).expect("stato di partenza");
    let mut out = Vec::new();
    while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
        let stop = m.run(QUANTUM);
        out.extend(m.console_output());
        assert!(matches!(stop, Stop::Budget | Stop::Idle), "{stop:?}");
    }
    out.extend(m.console_output());
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "replay con agganci e JIT");
    same_log("replay con agganci e JIT", &traced.log, &out);
    let t = tracer(&mut m);
    assert_eq!(t.records.len(), traced.records.len(), "syscall nel replay");
    assert_eq!(t.records, traced.records, "stesse syscall nel replay");
    assert_eq!(t.hits, traced.hits, "stessi punti d'arresto nel replay");

    // Costo degli agganci senza i confronti (che leggono molto): il
    // migliore di due esecuzioni alternate.
    let (mut hooks_secs, mut plain_secs) = (f64::MAX, plain.secs);
    for _ in 0..2 {
        let h = session(&image, &initrd, &k, &a, Mode { hooks: true, checks: false, record: false, jit });
        assert_eq!(h.end, traced.end);
        hooks_secs = hooks_secs.min(h.secs);
        let p = session(&image, &initrd, &k, &a, Mode { hooks: false, checks: false, record: false, jit });
        assert_eq!(p.end, traced.end);
        plain_secs = plain_secs.min(p.secs);
    }
    let text = format!(
        "sessione ({}): {} istruzioni, {} syscall, {} punti d'arresto\n\
         tempo: senza agganci {:.3} s, con syscall e punti d'arresto {:.3} s ({:+.1}%)\n",
        if jit { "JIT" } else { "interprete" },
        traced.end.steps,
        traced.records.len(),
        traced.hits.len(),
        plain_secs,
        hooks_secs,
        (hooks_secs / plain_secs - 1.0) * 100.0,
    );
    eprint!("{text}");
    let _ = std::fs::write(repo_root().join("target/guest-kernel/introspect-misure.txt"), text);
}

/// Profili dei kernel senza avviarli: la tabella kallsyms estratta
/// dall'`Image` del kernel di prova è uguale a `System.map` (indirizzo e
/// nome di ogni simbolo), e il suo BTF staccato dà tutti gli offset. Con
/// l'immagine AOSP (`target/aosp/out/boot.img`, da R2): kallsyms e BTF del
/// kernel GKI dentro `boot.img`, con le strutture di binder.
#[test]
fn profili_dei_kernel() {
    use vetro_machine::vetro_analysis::introspect::{Btf, Layout, Symbols, kallsyms};
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "profili dei kernel solo in release");
        return;
    }
    let dir = repo_root().join("target/guest-kernel");
    let (Ok(image), Ok(map)) =
        (std::fs::read(dir.join("Image")), std::fs::read_to_string(dir.join("System.map")))
    else {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "manca target/guest-kernel (tools/guest-kernel/build.sh)");
        return;
    };
    let map = Symbols::parse_system_map(&map);
    let pairs: std::collections::BTreeSet<(u64, &str)> =
        map.iter().map(|s| (s.addr, s.name.as_str())).collect();
    let ks = kallsyms::extract(&image).expect("kallsyms del kernel di prova");
    assert!(ks.len() > 10_000, "{} simboli", ks.len());
    let wrong: Vec<_> = ks.iter().filter(|s| !pairs.contains(&(s.addr, s.name.as_str()))).take(5).collect();
    assert!(wrong.is_empty(), "simboli diversi da System.map: {wrong:?}");
    assert_eq!(kernel().layout.task_comm_len, 16);
    eprintln!("6.18: {} simboli uguali a System.map", ks.len());

    let Ok(img) = std::fs::read(repo_root().join("target/aosp/out/boot.img")) else {
        skip_or_fail("VETRO_REQUIRE_ANDROID", "manca target/aosp/out/boot.img (artefatti da R2)");
        return;
    };
    let b = vetro_machine::android::BootImage::parse(&img).expect("boot.img");
    let gki = vetro_machine::android::decompress::decompress(b.kernel).expect("kernel GKI");
    let syms = Symbols::from_image(&gki).expect("kallsyms del GKI");
    for name in ["init_task", "vectors", "__entry_task", "special_mapping_vmops", "binder_ioctl"] {
        assert!(syms.get(name).is_some(), "{name} nel GKI");
    }
    let (_, btf) = Btf::find_in(&gki).expect("BTF del GKI");
    Layout::from_btf(&btf).expect("offset dal BTF del GKI");
    for s in ["binder_proc", "binder_thread", "binder_transaction", "binder_node"] {
        assert!(btf.struct_size(s).is_some(), "{s} nel BTF del GKI");
    }
    eprintln!("GKI: {} simboli, {} tipi BTF", syms.len(), btf.len());
}
