//! Introspection from the outside on the M3 guest kernel (ADR 0027): the basis
//! of the TLS hooks (M7), the Binder decoder (M8) and scripting (M9).
//!
//! The kernel profile comes from `System.map` and the detached BTF
//! (`target/guest-kernel/vmlinux.btf`, `tools/guest-kernel/build.sh`).
//! A session at the guest's shell, with the machine stopped between one quantum
//! and the next:
//!
//! - the process list read from memory is that of `ps`;
//! - the mappings of a process are the text of `/proc/<pid>/maps`, the open
//!   files those of `/proc/<pid>/fd`, the command line the real one;
//! - the symbols of `vetro-dev` (static, not stripped) read from the file in the
//!   guest's page cache are those of the file on the host;
//! - the traced syscalls of `cat` launched as user 501 are, in
//!   order, those of `qemu-aarch64 -strace` on the same binary
//!   (oracle), with path, bytes read and results;
//! - the invisible breakpoints on `open` and `ioctl` of `vetro-dev`
//!   fire once per call (as many as the process's `openat` and
//!   `ioctl` syscalls, with the same arguments), the one on BusyBox's entry
//!   point once per `exec`;
//! - **determinism**: the same session without hooks gives the same
//!   console and the same final state (instructions, CPU, MMU, devices,
//!   RAM); recorded without hooks and redone with the JIT with the hooks, it gives
//!   the same events, and the replay is identical.
//!
//! Release only, like `vetro.rs`. Timings in
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
    Kernel::load(None, Some(&map), Some(&btf)).expect("kernel profile")
}

/// Addresses from the files on the host: `open` and `ioctl` of vetro-dev, BusyBox's
/// entry point.
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
        busybox_entry: elf::header(&bb).expect("busybox ELF").entry,
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
            assert!(self.m.steps < limit, "{needle:?} did not arrive:\n{}", self.tail());
            let stop = self.quantum();
            assert_eq!(stop, Stop::Budget, "{stop:?} while waiting for {needle:?}:\n{}", self.tail());
        }
    }

    /// Sends a command and waits for the prompt: position and output (without
    /// the echo of the command).
    fn command(&mut self, cmd: &str, from: usize) -> (usize, String) {
        self.m.input(Input::Console(format!("{cmd}\n").into_bytes()));
        let at = self.until(SHELL_PROMPT, from);
        let out = normalize(&String::from_utf8_lossy(&self.log[from..at]));
        // After the line with the command's echo, without the final prompt.
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

/// Outcome of a session.
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
    /// Tracer, syscalls and breakpoints since boot.
    hooks: bool,
    /// Comparisons with the guest (ps, maps, fd, symbols, oracle).
    checks: bool,
    record: bool,
    jit: bool,
}

fn machine() -> Machine {
    Machine::with_devices(&MachineConfig::default(), &Devices::default())
}

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let mut m = machine();
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("kernel load");
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
    m.tracer_mut::<SyscallTracer>().expect("tracer")
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

    // 1. Processes: `ps` against the list read from memory.
    let (at, ps) = s.command("ps -o pid,ppid,user,comm", at);
    if mode.checks {
        check_ps(&s.m, k, &ps);
    }

    // 2. A process stopped in a read: mappings, open files, command
    // line, symbols.
    s.m.input(Input::Console(b"vetro-dev input-read /dev/input/event0 1 &\n".to_vec()));
    let at = s.until("VETRO-INPUT-PRONTO", at);
    let (at, out) = s.command("echo P=$!", at);
    let pid: i32 = out.lines().find_map(|l| l.strip_prefix("P=")).expect("pid").trim().parse().unwrap();
    let (at, maps) = s.command(&format!("cat /proc/{pid}/maps"), at);
    let (at, fds) = s.command(&format!("ls -l /proc/{pid}/fd | cat"), at);
    if mode.checks {
        check_process(&s.m, k, a, pid, &maps, &fds);
    }

    // 3. Breakpoints on open and ioctl of vetro-dev.
    let from_rec = if mode.hooks { tracer(&mut s.m).records.len() } else { 0 };
    let from_hit = if mode.hooks { tracer(&mut s.m).hits.len() } else { 0 };
    let (at, out) = s.command("vetro-dev input", at);
    assert!(out.contains("event1"), "{out}");
    if mode.checks {
        let bps = bps.expect("hooks");
        let t = tracer(&mut s.m);
        check_breakpoints(&t.records[from_rec..], &t.hits[from_hit..], bps, "vetro-dev");
    }

    // 4. BusyBox's entry point and decoding of openat/read.
    let from_rec = if mode.hooks { tracer(&mut s.m).records.len() } else { 0 };
    let from_hit = if mode.hooks { tracer(&mut s.m).hits.len() } else { 0 };
    let (at, version) = s.command("head -n 1 /proc/version", at);
    assert!(version.starts_with("Linux version 6.18"), "{version}");
    if mode.checks {
        let bps = bps.expect("hooks");
        let t = tracer(&mut s.m);
        let entries: Vec<_> = t.hits[from_hit..].iter().filter(|h| h.id == bps[2]).collect();
        assert_eq!(entries.len(), 1, "one BusyBox exec (head): {entries:?}");
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
        assert!(recs.iter().any(|r| r.name() == "exit_group"), "exit_group recorded at entry");
    }

    // 5. `cat` as user 501 (the same BusyBox path that
    // qemu-aarch64 takes as a normal user), for the oracle.
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

/// `ps -o pid,ppid,user,comm` against the process list.
fn check_ps(m: &Machine, k: &Kernel, ps: &str) {
    let mut guest: BTreeMap<i32, (i32, String, String)> = BTreeMap::new();
    for l in ps.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let Some(pid) = w.first().and_then(|p| p.parse::<i32>().ok()) else { continue };
        guest.insert(pid, (w[1].parse().unwrap(), w[2].to_string(), w[3..].join(" ")));
    }
    // `ps` itself has exited.
    guest.retain(|_, (_, _, comm)| comm != "ps");
    let ours: BTreeMap<i32, (i32, String, String)> = m.linux(k, |lx| {
        lx.processes().into_iter().map(|t| (t.pid, (t.ppid, t.euid.to_string(), t.comm.clone()))).collect()
    });
    assert!(ours.len() > 20, "processes and kernel threads: {ours:?}");
    let mut diff = Vec::new();
    for (pid, g) in &guest {
        match ours.get(pid) {
            // Workqueue workers: /proc adds "-<work>" to the name.
            Some(o) if o.0 == g.0 && o.1 == g.1 && (o.2 == g.2 || g.2.starts_with(&format!("{}-", o.2))) => {}
            o => diff.push(format!("pid {pid}: ps {g:?}, memory {o:?}")),
        }
    }
    for pid in ours.keys().filter(|p| !guest.contains_key(p)) {
        diff.push(format!("pid {pid}: only in memory {:?}", ours[pid]));
    }
    assert!(diff.is_empty(), "ps and memory differ:\n{}\n\nps:\n{ps}", diff.join("\n"));
    eprintln!("ps: {} processes equal", guest.len());
}

fn check_process(m: &Machine, k: &Kernel, a: &Addrs, pid: i32, maps: &str, fds: &str) {
    m.linux(k, |lx| {
        let t = lx.processes().into_iter().find(|t| t.pid == pid).expect("process in memory");
        assert_eq!(t.comm, "vetro-dev");
        assert_eq!(lx.threads(t.addr).iter().map(|x| x.pid).collect::<Vec<_>>(), [pid]);
        let ours = lx.maps(&t);
        assert_eq!(ours.trim_end(), maps.trim_end(), "/proc/{pid}/maps");
        assert!(
            maps.contains("[stack]") && maps.contains("[vdso]") && maps.contains("/bin/vetro-dev"),
            "{maps}"
        );
        // ls -l: "... N -> target".
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
        // Symbols from the file in the guest's page cache (.symtab) and entry
        // point from the ELF in memory: the same as the file on the host.
        assert_eq!(lx.user_symbol(&t, "vetro-dev", "open"), Some(a.open));
        assert_eq!(lx.user_symbol(&t, "vetro-dev", "ioctl"), Some(a.ioctl));
        let host = std::fs::read(repo_root().join("target/guest-kernel/vetro-dev")).unwrap();
        let inode = lx.file_inode(lx.module(&t, "vetro-dev").unwrap().2).unwrap();
        assert_eq!(
            lx.file_bytes(inode, 64 << 20).as_deref(),
            Some(host.as_slice()),
            "file from the page cache"
        );
        eprintln!("process {pid}: {} regions, {} files, symbols and files equal", ours.len(), guest.len());
    });
}

/// Every call of `open`/`ioctl` of vetro-dev fires once, with the
/// arguments of the syscall it makes.
fn check_breakpoints(records: &[SyscallRecord], hits: &[BreakpointHit], bps: [u32; 3], comm: &str) {
    let pid = hits.iter().find(|h| h.comm == comm && h.id == bps[0]).expect("open of vetro-dev").pid;
    let hits: Vec<&BreakpointHit> = hits.iter().filter(|h| h.pid == pid).collect();
    let recs: Vec<&SyscallRecord> = records.iter().filter(|r| r.pid == pid).collect();
    let opens: Vec<_> = hits.iter().filter(|h| h.id == bps[0]).collect();
    let ioctls: Vec<_> = hits.iter().filter(|h| h.id == bps[1]).collect();
    // After the exec: the syscalls of vetro-dev (before that there is the child shell).
    let exec = recs.iter().rposition(|r| r.name() == "execve").expect("execve");
    let sys_open: Vec<_> = recs[exec..].iter().filter(|r| r.name() == "openat").collect();
    let sys_ioctl: Vec<_> = recs[exec..].iter().filter(|r| r.name() == "ioctl").collect();
    assert!(opens.len() >= 2, "{opens:?}");
    assert_eq!(opens.len(), sys_open.len(), "open: breakpoints and syscalls");
    assert_eq!(ioctls.len(), sys_ioctl.len(), "ioctl: breakpoints and syscalls");
    for (h, r) in opens.iter().zip(&sys_open) {
        assert_eq!(h.args[0], r.args[1], "open(path) e openat(AT_FDCWD, path)");
        assert!(h.step < r.step, "the breakpoint comes before the syscall");
    }
    for (h, r) in ioctls.iter().zip(&sys_ioctl) {
        // `int ioctl(int fd, int req, ...)`: musl sign-extends req.
        assert_eq!(
            (h.args[0], h.args[1] as u32, h.args[2]),
            (r.args[0], r.args[1] as u32, r.args[2]),
            "ioctl: same arguments"
        );
        assert_eq!(r.args[1], h.args[1] as u32 as i32 as i64 as u64);
    }
    assert!(sys_open.iter().all(|r| r.path.as_deref().is_some_and(|p| p.starts_with("/dev/input/event"))));
    eprintln!("breakpoints: {} open, {} ioctl matching the syscalls", opens.len(), ioctls.len());
}

/// The syscalls of `cat autotest.sh` (user 501) against `qemu-aarch64 -strace`.
fn check_oracle(records: &[SyscallRecord]) {
    // The cat process: the one with the openat of autotest.sh.
    let pid =
        records.iter().find(|r| r.path.as_deref() == Some("autotest.sh")).expect("openat of autotest.sh").pid;
    let recs: Vec<&SyscallRecord> = records.iter().filter(|r| r.pid == pid).collect();
    let exec = recs.iter().rposition(|r| r.name() == "execve").expect("execve");
    assert!(recs[exec].diverted.is_some(), "execve succeeded: return to the new program");
    let ours: Vec<&str> = recs[exec + 1..].iter().map(|r| r.name()).collect();
    let open = recs.iter().find(|r| r.name() == "openat").unwrap();
    let sends: Vec<i64> = recs.iter().filter(|r| r.name() == "sendfile").filter_map(|r| r.ret).collect();
    let size =
        std::fs::metadata(repo_root().join("guest/kernel/initramfs/autotest.sh")).unwrap().len() as i64;
    assert_eq!(sends, [size, 0], "sendfile: the whole file, then 0");
    assert_eq!(open.ret, Some(3));
    // Like tests/diff/src/qemu.rs: the variable, otherwise qemu-aarch64 in the PATH.
    let qemu = std::env::var_os("VETRO_QEMU_AARCH64").map(std::path::PathBuf::from).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("qemu-aarch64"))
            .find(|p| p.is_file())
    });
    let Some(qemu) = qemu else {
        skip_or_fail(
            "VETRO_REQUIRE_ORACLE",
            "syscall oracle: qemu-aarch64 missing (VETRO_QEMU_AARCH64 or PATH)",
        );
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
    assert_eq!(ours, theirs, "cat syscalls: Vetro (traced from outside) and qemu-aarch64 -strace\n{text}");
    eprintln!("oracle: {} syscalls equal to qemu-aarch64 -strace", ours.len());
}

fn same_log(what: &str, a: &[u8], b: &[u8]) {
    if a != b {
        let i = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        let s = i.saturating_sub(200);
        panic!(
            "{what}: console differs at byte {i}\nexpected: {}\ngot: {}",
            String::from_utf8_lossy(&a[s..(i + 200).min(a.len())]),
            String::from_utf8_lossy(&b[s..(i + 200).min(b.len())])
        );
    }
}

#[test]
fn introspezione_dall_esterno() {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "introspection on the guest kernel only in release");
        return;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "missing target/guest-kernel (tools/guest-kernel/build.sh)",
        );
        return;
    };
    if !repo_root().join("target/guest-kernel/vmlinux.btf").is_file() {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "missing target/guest-kernel/vmlinux.btf (tools/guest-kernel/build.sh)",
        );
        return;
    }
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let k = kernel();
    let a = addrs();
    let jit = std::env::var("VETRO_JIT").is_ok_and(|v| v == "1");

    // With the hooks and the comparisons.
    let traced = session(&image, &initrd, &k, &a, Mode { hooks: true, checks: true, record: false, jit });
    assert!(traced.records.len() > 1000, "{} syscall", traced.records.len());
    let binder_free = traced.records.iter().all(|r| r.binder.is_empty());
    assert!(binder_free, "no binder in the test guest");

    // Without hooks, recorded: same execution.
    let plain = session(&image, &initrd, &k, &a, Mode { hooks: false, checks: false, record: true, jit });
    same_log("without hooks", &traced.log, &plain.log);
    assert_eq!(plain.end, traced.end, "final state with and without hooks");

    // Replay of the session without hooks with the JIT (the other engine) and with
    // the hooks from boot: same events, identical replay.
    let log = plain.recording.expect("recording");
    let mut m = booted(&image, &initrd);
    m.set_jit(Some(vetro_jit_native::system_jit(JIT_THRESHOLD)));
    arm(&mut m, &k, &a);
    m.start_replay(&log).expect("starting state");
    let mut out = Vec::new();
    while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
        let stop = m.run(QUANTUM);
        out.extend(m.console_output());
        assert!(matches!(stop, Stop::Budget | Stop::Idle), "{stop:?}");
    }
    out.extend(m.console_output());
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "replay with hooks and JIT");
    same_log("replay with hooks and JIT", &traced.log, &out);
    let t = tracer(&mut m);
    assert_eq!(t.records.len(), traced.records.len(), "syscalls in the replay");
    assert_eq!(t.records, traced.records, "same syscalls in the replay");
    assert_eq!(t.hits, traced.hits, "same breakpoints in the replay");

    // Cost of the hooks without the comparisons (which read a lot): the
    // best of two alternating runs.
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

/// Kernel profiles without booting them: the kallsyms table extracted
/// from the test kernel's `Image` equals `System.map` (address and
/// name of every symbol), and its detached BTF gives all the offsets. With
/// the AOSP image (`target/aosp/out/boot.img`, from R2): kallsyms and BTF of the
/// GKI kernel inside `boot.img`, with the binder structures.
#[test]
fn profili_dei_kernel() {
    use vetro_machine::vetro_analysis::introspect::{Btf, Layout, Symbols, kallsyms};
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "kernel profiles only in release");
        return;
    }
    let dir = repo_root().join("target/guest-kernel");
    let (Ok(image), Ok(map)) =
        (std::fs::read(dir.join("Image")), std::fs::read_to_string(dir.join("System.map")))
    else {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "missing target/guest-kernel (tools/guest-kernel/build.sh)",
        );
        return;
    };
    let map = Symbols::parse_system_map(&map);
    let pairs: std::collections::BTreeSet<(u64, &str)> =
        map.iter().map(|s| (s.addr, s.name.as_str())).collect();
    let ks = kallsyms::extract(&image).expect("kallsyms of the test kernel");
    assert!(ks.len() > 10_000, "{} symbols", ks.len());
    let wrong: Vec<_> = ks.iter().filter(|s| !pairs.contains(&(s.addr, s.name.as_str()))).take(5).collect();
    assert!(wrong.is_empty(), "symbols different from System.map: {wrong:?}");
    assert_eq!(kernel().layout.task_comm_len, 16);
    eprintln!("6.18: {} symbols equal to System.map", ks.len());

    let Ok(img) = std::fs::read(repo_root().join("target/aosp/out/boot.img")) else {
        skip_or_fail("VETRO_REQUIRE_ANDROID", "missing target/aosp/out/boot.img (artifacts from R2)");
        return;
    };
    let b = vetro_machine::android::BootImage::parse(&img).expect("boot.img");
    let gki = vetro_machine::android::decompress::decompress(b.kernel).expect("kernel GKI");
    let syms = Symbols::from_image(&gki).expect("GKI kallsyms");
    for name in ["init_task", "vectors", "__entry_task", "special_mapping_vmops", "binder_ioctl"] {
        assert!(syms.get(name).is_some(), "{name} in the GKI");
    }
    let (_, btf) = Btf::find_in(&gki).expect("GKI BTF");
    Layout::from_btf(&btf).expect("offsets from the GKI BTF");
    for s in ["binder_proc", "binder_thread", "binder_transaction", "binder_node"] {
        assert!(btf.struct_size(s).is_some(), "{s} in the GKI BTF");
    }
    eprintln!("GKI: {} symbols, {} BTF types", syms.len(), btf.len());
}
