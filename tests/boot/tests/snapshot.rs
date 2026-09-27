//! Criterion of the first part of M6: save/restore of the whole machine
//! (ADR 0015) on the M3 guest kernel.
//!
//! Every script (boot up to power-off, network, disk, devices) runs
//! once without interruptions and then with **cuts**: between one quantum and
//! the next the machine is saved and the script continues on a
//! **new** machine restored from the snapshot (with or without JIT), or the
//! machine goes on and then goes back to the snapshot in the same place
//! (restore over a used machine, with the JIT having translated code in the
//! meantime). The script, i.e. the host, doesn't notice. At the end they must
//! match the run without cuts: the console log byte for
//! byte, the number of instructions, the RAM and the state of all devices
//! (the final snapshot); with the JIT everything except the TLB, which with the JIT sees fewer
//! accesses (ADR 0013, "Allowed difference"). At every cut two saves
//! give the same bytes, and the restored machine saves the same bytes again.
//!
//! In addition: snapshot size and save and restore times at the
//! shell (1 GiB of RAM), printed and written to
//! `target/guest-kernel/snapshot-misure.txt`.
//!
//! Also with a connection opened by the host (port forwarding) halfway through a
//! transfer, and halfway through a file manager session (M8).
//!
//! Release only, like `vetro.rs`.

use std::collections::BTreeMap;
use std::time::Instant;

use vetro_boot_tests::*;
use vetro_machine::files::proto::display_name;
use vetro_machine::files::{FilesError, Outcome as FilesOutcome};
use vetro_machine::vetro_net::TcpReply;
use vetro_machine::vetro_snapshot::{Snapshot, Writer, hash64};
use vetro_machine::{Devices, FilesClient, Machine, MachineConfig, NetSetup, Stop};
use vetro_platform::virtio::{
    CowBackend, MemBackend, MemDisplay, VirtioBlk, VirtioBlkConfig, VsockConn, VsockState,
};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const JIT_THRESHOLD: u32 = 16;

/// What happens at a cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// Saves, restores into a new machine and continues there. `jit`: the
    /// new machine has the JIT.
    Swap { jit: bool },
    /// Saves, goes on for `quanta` quanta (output discarded), then restores
    /// the snapshot in the same machine (JIT included, with its blocks)
    /// and continues.
    Rewind { quanta: u64 },
}

/// When to cut: at a number of instructions, or a few quanta after a
/// point of the script (`Run::mark`).
#[derive(Clone, Debug, Default)]
struct Plan {
    at: Vec<(u64, Cut)>,
    marks: BTreeMap<&'static str, (u64, Cut)>,
    /// The first machine has the JIT.
    jit_from_start: bool,
}

struct Run {
    m: Machine,
    log: Vec<u8>,
    /// A new machine, configured like the script's (disks
    /// included), without JIT and without a kernel.
    fresh: Box<dyn Fn() -> Machine>,
    pending: Vec<(u64, Cut)>,
    marks: BTreeMap<&'static str, (u64, Cut)>,
    jit: bool,
    /// Cuts performed (instructions, kind).
    done: Vec<(u64, Cut)>,
}

impl Run {
    fn new(fresh: Box<dyn Fn() -> Machine>, setup: impl FnOnce(&mut Machine), plan: &Plan) -> Run {
        let mut m = fresh();
        setup(&mut m);
        if plan.jit_from_start {
            m.set_jit(Some(vetro_jit_native::system_jit(JIT_THRESHOLD)));
        }
        let mut pending = plan.at.clone();
        pending.sort_by_key(|c| std::cmp::Reverse(c.0));
        Run {
            m,
            log: Vec::new(),
            fresh,
            pending,
            marks: plan.marks.clone(),
            jit: plan.jit_from_start,
            done: Vec::new(),
        }
    }

    /// Point of the script: if the plan asks for it, a cut in `quanta`
    /// quanta.
    fn mark(&mut self, name: &'static str) {
        if let Some((quanta, cut)) = self.marks.remove(name) {
            self.pending.push((self.m.steps + quanta * QUANTUM, cut));
            self.pending.sort_by_key(|c| std::cmp::Reverse(c.0));
        }
    }

    fn cut(&mut self, cut: Cut) {
        let snap = self.m.save();
        assert!(snap == self.m.save(), "two saves at the same point give different bytes");
        match cut {
            Cut::Swap { jit } => {
                let mut n = (self.fresh)();
                n.load_state(&snap).expect("restore");
                assert!(n.save() == snap, "the restored machine does not save the same bytes again");
                if jit {
                    n.set_jit(Some(vetro_jit_native::system_jit(JIT_THRESHOLD)));
                }
                self.jit |= jit;
                self.m = n;
            }
            Cut::Rewind { quanta } => {
                for _ in 0..quanta {
                    if self.m.run(QUANTUM) != Stop::Budget {
                        break;
                    }
                }
                let _ = self.m.console_output();
                self.m.load_state(&snap).expect("restore over the used machine");
                assert!(
                    self.m.save() == snap,
                    "the restore over the used machine does not save the same bytes again"
                );
            }
        }
        self.done.push((self.m.steps, cut));
    }

    /// One quantum (after any due cuts), with the output in the log.
    fn quantum(&mut self) -> Stop {
        while self.pending.last().is_some_and(|c| c.0 <= self.m.steps) {
            let (_, cut) = self.pending.pop().unwrap();
            self.cut(cut);
        }
        let s = self.m.run(QUANTUM);
        self.log.extend(self.m.console_output());
        s
    }

    fn until_with(&mut self, needle: &str, from: usize, mut host: impl FnMut(&mut Machine)) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} did not arrive:\n{}", self.tail());
            let stop = self.quantum();
            host(&mut self.m);
            assert_eq!(stop, Stop::Budget, "{stop:?} while waiting for {needle:?}:\n{}", self.tail());
        }
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        self.until_with(needle, from, |_| {})
    }

    fn command(&mut self, cmd: &str, from: usize) -> usize {
        self.m.console_input(format!("{cmd}\n").as_bytes());
        self.until(SHELL_PROMPT, from)
    }

    fn text(&self, from: usize) -> String {
        normalize(&String::from_utf8_lossy(&self.log[from..]))
    }

    fn poweroff(&mut self) {
        self.m.console_input(b"poweroff -f\n");
        let limit = self.m.steps + PHASE_BUDGET;
        let stop = loop {
            let s = self.quantum();
            if s != Stop::Budget || self.m.steps >= limit {
                break s;
            }
        };
        assert_eq!(stop, Stop::PowerOff, "{}", self.tail());
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    fn finish(self) -> Outcome {
        assert!(self.pending.is_empty() && self.marks.is_empty(), "cuts not performed: {:?}", self.pending);
        let b = self.m.board.borrow();
        let mut mmu = Writer::new();
        self.m.mmu.save(&mut mmu);
        let mut plat = Writer::new();
        b.virt.save(&mut plat);
        Outcome {
            log: self.log.clone(),
            steps: self.m.steps,
            cpu: format!("{:?}", self.m.cpu),
            ram: hash64(b.ram.bytes()),
            platform: hash64(plat.as_bytes()),
            mmu: hash64(mmu.as_bytes()),
            jit: self.jit,
            cuts: self.done.clone(),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// End of a script: what gets compared.
struct Outcome {
    log: Vec<u8>,
    steps: u64,
    cpu: String,
    ram: u64,
    platform: u64,
    mmu: u64,
    jit: bool,
    cuts: Vec<(u64, Cut)>,
}

/// `run` matches `reference` (without cuts and without JIT).
fn same(what: &str, reference: &Outcome, run: &Outcome) {
    assert!(!run.cuts.is_empty(), "{what}: no cut performed");
    eprintln!("{what}: cuts {:?}", run.cuts);
    if run.log != reference.log {
        let (a, b) = (String::from_utf8_lossy(&reference.log), String::from_utf8_lossy(&run.log));
        let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        panic!(
            "{what}: log differs from line {}:\nwithout cuts: {:?}\nwith cuts:    {:?}",
            i + 1,
            a.get(i),
            b.get(i)
        );
    }
    assert_eq!(run.steps, reference.steps, "{what}: instructions");
    assert_eq!(run.cpu, reference.cpu, "{what}: CPU");
    assert_eq!(run.ram, reference.ram, "{what}: RAM");
    assert_eq!(run.platform, reference.platform, "{what}: device state");
    if !run.jit {
        assert_eq!(run.mmu, reference.mmu, "{what}: MMU e TLB");
    }
}

fn kernel() -> Option<(Vec<u8>, Vec<u8>)> {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "snapshots on the guest kernel only in release");
        return None;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
        return None;
    };
    Some((std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap()))
}

// ---- Boot up to power-off (the script of vetro.rs) ---------------------------

fn boot_script(image: &[u8], initrd: &[u8], plan: &Plan) -> Outcome {
    let fresh = Box::new(|| Machine::new(&MachineConfig::default()));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0").expect("kernel load");
        },
        plan,
    );
    let at = r.until(BOOT_MARKER, 0);
    let at_end = r.until(AUTOTEST_END, at);
    let end = r.until("\n", at_end);
    let line = String::from_utf8_lossy(&r.log[at_end - AUTOTEST_END.len()..end]).into_owned();
    assert_eq!(line.trim_end(), AUTOTEST_OK, "autotest with errors:\n{}", r.tail());
    let prompt = r.until(SHELL_PROMPT, end);
    r.mark("shell");
    r.m.console_input(b"echo VETRO-SHELL-$((6*7))\n");
    let out = r.until("VETRO-SHELL-42", prompt);
    r.until(SHELL_PROMPT, out);
    r.poweroff();
    r.finish()
}

#[test]
fn snapshot_durante_l_avvio_e_alla_shell() {
    let Some((image, initrd)) = kernel() else { return };
    let t0 = Instant::now();
    let reference = boot_script(&image, &initrd, &Plan::default());
    eprintln!("boot without cuts: {} instructions in {:.2} s", reference.steps, t0.elapsed().as_secs_f64());

    // Interpreter only: cuts early (before the MMU is on), during the kernel
    // boot, halfway through the autotest and at the shell; one going back.
    let plan = Plan {
        at: vec![
            (QUANTUM, Cut::Swap { jit: false }),
            (20 * QUANTUM, Cut::Swap { jit: false }),
            (70 * QUANTUM, Cut::Rewind { quanta: 3 }),
            (110 * QUANTUM, Cut::Swap { jit: false }),
        ],
        marks: [("shell", (0, Cut::Swap { jit: false }))].into(),
        jit_from_start: false,
    };
    same("boot with cuts (interpreter)", &reference, &boot_script(&image, &initrd, &plan));

    // JIT before and after: interpreter, then JIT from a cut during boot,
    // JIT kept at another cut, going back with the JIT (blocks to
    // discard), then interpreter again at the shell.
    let plan = Plan {
        at: vec![
            (30 * QUANTUM, Cut::Swap { jit: true }),
            (60 * QUANTUM, Cut::Rewind { quanta: 5 }),
            (90 * QUANTUM, Cut::Swap { jit: true }),
        ],
        marks: [("shell", (0, Cut::Swap { jit: false }))].into(),
        jit_from_start: false,
    };
    same("boot with cuts (interpreter, JIT, interpreter)", &reference, &boot_script(&image, &initrd, &plan));
}

/// Snapshot size and save and restore times at the shell
/// (1 GiB of RAM, default devices), on a real boot.
#[test]
fn misure_alla_shell() {
    let Some((image, initrd)) = kernel() else { return };
    let mut m = Machine::new(&MachineConfig::default());
    m.load_linux(&image, Some(&initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
    let mut log = Vec::new();
    while find(&log, SHELL_PROMPT.as_bytes()).is_none() {
        assert_eq!(m.run(QUANTUM), Stop::Budget);
        log.extend(m.console_output());
    }
    let t = Instant::now();
    let snap = m.save();
    let save = t.elapsed();
    let t = Instant::now();
    let fresh = Machine::new(&MachineConfig::default());
    let build = t.elapsed();
    let mut n = fresh;
    let t = Instant::now();
    n.load_state(&snap).unwrap();
    let restore = t.elapsed();
    let nonzero = m.board.borrow().ram.bytes().chunks(4096).filter(|p| p.iter().any(|&b| b != 0)).count();
    let text = format!(
        "snapshot at the shell ({} instructions, 1 GiB of RAM, {} non-zero pages = {:.1} MiB): \
         {} bytes ({:.1} MiB); save {:.0} ms, new machine {:.0} ms, restore {:.0} ms\n",
        m.steps,
        nonzero,
        nonzero as f64 * 4096.0 / (1 << 20) as f64,
        snap.len(),
        snap.len() as f64 / (1 << 20) as f64,
        save.as_secs_f64() * 1e3,
        build.as_secs_f64() * 1e3,
        restore.as_secs_f64() * 1e3,
    );
    eprint!("{text}");
    std::fs::write(repo_root().join("target/guest-kernel/snapshot-misure.txt"), &text).unwrap();
    // The restored machine continues like the original.
    for mm in [&mut m, &mut n] {
        mm.console_input(b"echo VETRO-DOPO-$((6*7))\n");
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        m.run(QUANTUM);
        n.run(QUANTUM);
        a.extend(m.console_output());
        b.extend(n.console_output());
    }
    assert!(String::from_utf8_lossy(&a).contains("VETRO-DOPO-42"));
    assert_eq!(a, b);
    assert_eq!(m.steps, n.steps);
    assert!(m.save() == n.save());
}

// ---- Network (the script of net.rs, reduced) ---------------------------------

fn big_body() -> Vec<u8> {
    (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

fn http_reply(body: &[u8]) -> TcpReply {
    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
        .into_bytes();
    r.extend_from_slice(body);
    TcpReply { on_connect: Vec::new(), on_data: r, close_after_reply: true }
}

fn net_devices() -> Devices {
    let mut net = NetSetup::default();
    net.sinkhole.tcp_by_port.insert(8080, http_reply(&big_body()));
    net.sinkhole.tcp_by_port.insert(81, http_reply(b"RICEVUTO\n"));
    Devices { net: Some(net), ..Devices::default() }
}

fn net_script(image: &[u8], initrd: &[u8], plan: &Plan) -> (Outcome, String) {
    let fresh = Box::new(|| Machine::with_devices(&MachineConfig::default(), &net_devices()));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
        },
        plan,
    );
    let at = r.until(SHELL_PROMPT, 0);
    let at = r.command("udhcpc -i eth0 -n -q", at);
    r.mark("wget");
    let at = r.command("echo \"C\"K=$(wget -q -O - http://grande.example:8080/dati | cksum)", at);
    assert!(r.text(0).contains("CK="), "{}", r.tail());
    let at = r.command("seq 1 20000 > /tmp/su", at);
    r.mark("post");
    let at = r.command("echo \"P\"OST=$(wget -q -O - --post-file=/tmp/su http://su.example:81/carica)", at);
    assert!(r.text(0).contains("POST=RICEVUTO"), "{}", r.tail());
    r.mark("ping");
    let at = r.command("ping -c 2 -W 5 10.0.2.2", at);
    let _ = r.command("sleep 5", at);
    r.poweroff();
    let events =
        r.m.net_view(|s| format!("{:?}{:?}", s.events(), s.upstream().tcp_connections().collect::<Vec<_>>()));
    (r.finish(), events.unwrap())
}

#[test]
fn snapshot_durante_l_uso_della_rete() {
    let Some((image, initrd)) = kernel() else { return };
    let (reference, ref_events) = net_script(&image, &initrd, &Plan::default());
    // Cuts with TCP connections halfway through a transfer (300 KB to the guest,
    // 108 KB POST to the host), during a ping and with TIME-WAIT in progress.
    let plan = Plan {
        at: vec![],
        marks: [
            ("wget", (4, Cut::Swap { jit: false })),
            ("post", (3, Cut::Rewind { quanta: 2 })),
            ("ping", (1, Cut::Swap { jit: false })),
        ]
        .into(),
        jit_from_start: false,
    };
    let (run, events) = net_script(&image, &initrd, &plan);
    same("network with cuts", &reference, &run);
    assert!(events == ref_events, "network event log and sinkhole differ");
}

// ---- Disk (the script of web.rs) ---------------------------------------------

const DISK_SIZE: usize = 3 * 1024 * 1024 + 5 * 512;

fn disk_image() -> Vec<u8> {
    let mut s: u32 = 0x9e37_79b9;
    (0..DISK_SIZE)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as u8
        })
        .collect()
}

fn disk_script(image: &[u8], initrd: &[u8], plan: &Plan) -> Outcome {
    let img = std::rc::Rc::new(disk_image());
    let fresh = Box::new(move || {
        let m = Machine::new(&MachineConfig::default());
        // The read-only base is the link (like the file or the HTTP
        // Range); the copy-on-write layer is state and goes into the snapshot.
        let base = MemBackend::from_vec(img.to_vec()).read_only();
        let blk = VirtioBlk::new(Box::new(CowBackend::new(base)), VirtioBlkConfig::default());
        m.board.borrow_mut().virt.attach_virtio_next(Box::new(blk)).unwrap();
        m
    });
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
        },
        plan,
    );
    let at = r.until(SHELL_PROMPT, 0);
    r.mark("md5");
    let at = r.command("md5sum /dev/vda", at);
    r.mark("dd");
    let at = r.command(
        "printf VETRO-SCRITTO | dd of=/dev/vda bs=1 seek=1000000 conv=notrunc 2>/dev/null; sync; \
         echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1 skip=1000000 count=13 2>/dev/null; \
         echo; md5sum /dev/vda",
        at,
    );
    // The kernel's message about drop_caches may arrive right after the text.
    assert!(r.text(0).contains("\nVETRO-SCRITTO"), "{}", r.tail());
    r.mark("after");
    let _ = r.command("dd if=/dev/vda bs=1 skip=1000000 count=13 2>/dev/null; echo", at);
    r.poweroff();
    r.finish()
}

#[test]
fn snapshot_durante_l_uso_del_disco() {
    let Some((image, initrd)) = kernel() else { return };
    let reference = disk_script(&image, &initrd, &Plan::default());
    let plan = Plan {
        at: vec![],
        marks: [
            ("md5", (1, Cut::Swap { jit: false })),
            ("dd", (2, Cut::Swap { jit: false })),
            ("after", (0, Cut::Rewind { quanta: 4 })),
        ]
        .into(),
        jit_from_start: false,
    };
    same("disk with cuts", &reference, &disk_script(&image, &initrd, &plan));
}

// ---- GPU, input and vsock (the script of devices.rs, reduced) ----------------

const GUEST_CID: u64 = 3;

fn devices_script(image: &[u8], initrd: &[u8], plan: &Plan) -> (Outcome, String) {
    let devices = Devices { vsock_cid: Some(GUEST_CID), ..Devices::default() };
    let fresh = Box::new(move || Machine::with_devices(&MachineConfig::default(), &devices));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
            m.vsock(|v| v.listen(1234).unwrap()).unwrap();
        },
        plan,
    );
    let at = r.until(SHELL_PROMPT, 0);

    // GPU: the cut falls while the scanout shows the guest's pattern; the
    // display (MemDisplay, a link) of the new machine receives it
    // from the restored state.
    r.m.console_input(b"vetro-dev drm-hold\n");
    r.mark("drm");
    let ready = r.until("VETRO-DRM-PRONTO", at);
    for _ in 0..3 {
        r.quantum();
    }
    let screen = r.m.gpu(|g| {
        let d = g.backend_as::<MemDisplay>().unwrap();
        d.screens.get(&0).map(|(w, h, px)| (*w, *h, hash64(px)))
    });
    let cursor = r.m.gpu(|g| g.cursor(0).cloned()).flatten();
    r.m.console_input(b"\n");
    let at = r.until(SHELL_PROMPT, ready);

    // Input: keys with the evdev reader waiting.
    r.m.console_input(b"vetro-dev input-read /dev/input/event1 4\n");
    let ready = r.until("VETRO-INPUT-PRONTO", at);
    r.m.keyboard(|k| {
        k.key(30, true);
        k.key(30, false);
    });
    r.mark("input");
    let at = r.until(SHELL_PROMPT, ready);

    // vsock: the guest connects to the host and receives 300 KB (more than its
    // credit); the cut falls with the transfer in progress.
    let reply: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut conn: Option<VsockConn> = None;
    let mut got = Vec::new();
    let mut replied = false;
    r.m.console_input(b"vetro-dev vsock-connect 1234 ciao-vetro\n");
    r.mark("vsock");
    let done = r.until_with("vsock risposta", at, |m| {
        m.vsock(|v| {
            if conn.is_none() {
                conn = v.accept(1234);
            }
            let Some(c) = conn else { return };
            got.extend(v.recv(c, usize::MAX));
            if v.eof(c) && !replied {
                v.send(c, &reply).unwrap();
                v.close(c);
                replied = true;
            }
        });
    });
    let _ = r.until(SHELL_PROMPT, done);
    assert_eq!(got, b"ciao-vetro");
    let state = r.m.vsock(|v| v.state(conn.unwrap())).unwrap();
    assert!(!r.text(0).contains("vetro-dev: ERRORE"), "{}", r.tail());
    r.poweroff();
    let host = format!("{screen:?} {cursor:?} {state:?}");
    assert!(state == Some(VsockState::Closed) || state.is_none(), "{host}");
    (r.finish(), host)
}

#[test]
fn snapshot_durante_l_uso_di_gpu_input_e_vsock() {
    let Some((image, initrd)) = kernel() else { return };
    let (reference, ref_host) = devices_script(&image, &initrd, &Plan::default());
    assert!(ref_host.contains("Some((1280, 800,"), "{ref_host}");
    let plan = Plan {
        at: vec![],
        marks: [
            ("drm", (2, Cut::Swap { jit: false })),
            ("input", (0, Cut::Swap { jit: false })),
            ("vsock", (2, Cut::Swap { jit: false })),
        ]
        .into(),
        jit_from_start: false,
    };
    let (run, host) = devices_script(&image, &initrd, &plan);
    same("devices with cuts", &reference, &run);
    assert_eq!(host, ref_host, "what the host sees (scanout, cursor, vsock)");
}

// ---- Port forwarding (the script of hostfwd.rs, reduced) ---------------------

/// `nc -l -e cat` in the guest and 200 KB of echo from a connection opened
/// by the host (`Stack::host_connect`); the cuts fall with the transfer in
/// progress: the forwarding queues, the connection in `SynSent` or established and
/// the ephemeral ports are state, the indices of the host connections
/// (here `id`) stay valid in the restored machine.
fn hostfwd_script(image: &[u8], initrd: &[u8], plan: &Plan) -> (Outcome, String) {
    let devices = Devices { net: Some(NetSetup::default()), ..Devices::default() };
    let fresh = Box::new(move || Machine::with_devices(&MachineConfig::default(), &devices));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
        },
        plan,
    );
    let at = r.until(SHELL_PROMPT, 0);
    let at = r.command("udhcpc -i eth0 -n -q", at);
    r.m.console_input(b"nc -n -v -l -p 5555 -e cat\n");
    let at = r.until("listening on", at);
    let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 9) as u8).collect();
    let id = r.m.net(|s| s.host_connect(5555)).flatten().expect("connection from the host");
    r.mark("syn");
    r.mark("eco");
    r.mark("eco-indietro");
    let (mut sent, mut got, mut shut) = (0usize, Vec::new(), false);
    let at = r.until_with(SHELL_PROMPT, at, |m| {
        m.net(|s| {
            sent += s.host_send(id, &data[sent..]);
            let mut buf = [0u8; 65536];
            loop {
                let n = s.host_recv(id, &mut buf);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            if !shut && got.len() == data.len() {
                s.host_shutdown(id);
                shut = true;
            }
        });
    });
    // TIME-WAIT of the host connection (4 s of virtual time).
    let _ = r.command("sleep 5", at);
    assert!(got == data, "200 KB echo differs ({} bytes)", got.len());
    let state = r.m.net_view(|s| s.host_conn(id).map(|i| i.state)).flatten();
    assert_eq!(
        state,
        Some(vetro_machine::vetro_net::HostConnState::Closed(vetro_machine::vetro_net::CloseReason::Normal))
    );
    r.poweroff();
    let events = r.m.net_view(|s| format!("{:?}", s.events())).unwrap();
    (r.finish(), format!("{state:?} {events}"))
}

#[test]
fn snapshot_durante_una_connessione_dall_host() {
    let Some((image, initrd)) = kernel() else { return };
    let (reference, ref_net) = hostfwd_script(&image, &initrd, &Plan::default());
    // With the SYN just requested (not yet sent), then with echo in progress in
    // both directions (a new machine, then going back).
    let plan = Plan {
        at: vec![],
        marks: [
            ("syn", (0, Cut::Swap { jit: false })),
            ("eco", (2, Cut::Swap { jit: false })),
            ("eco-indietro", (4, Cut::Rewind { quanta: 2 })),
        ]
        .into(),
        jit_from_start: false,
    };
    let (run, net) = hostfwd_script(&image, &initrd, &plan);
    same("port forwarding with cuts", &reference, &run);
    assert!(net == ref_net, "network state and event log differ");
}

// ---- File manager (M8, ADR 0020) ---------------------------------------------

/// Runs until file manager operation `op` finishes; what the
/// client sees (responses and events) ends up in `seen`.
fn files_wait(
    r: &mut Run,
    fc: &mut FilesClient,
    seen: &mut Vec<String>,
    op: u32,
) -> Result<FilesOutcome, FilesError> {
    let limit = r.m.steps + PHASE_BUDGET;
    loop {
        fc.pump(&mut r.m);
        while let Some(e) = fc.take_event() {
            seen.push(format!("evento {} {e:?}", display_name(&e.name)));
        }
        while let Some(c) = fc.take_completion() {
            let brief = match &c.result {
                Ok(FilesOutcome::Data { size, data }) => format!("Data {size} {:x}", hash64(data)),
                r => format!("{r:?}"),
            };
            seen.push(format!("op {} {brief}", c.op));
            if c.op == op {
                return c.result;
            }
        }
        assert!(r.m.steps < limit, "file manager operation {op} not finished:\n{}", r.tail());
        assert_eq!(r.quantum(), Stop::Budget, "{}", r.tail());
    }
}

/// A file manager session (the guest's `vetro-files` daemon and
/// the `vetro_machine::files` client). The client is the host: it stays the
/// same, the machine under it is cut with a chunked read in progress,
/// with a watch open and the event arriving, with a large write in
/// flight. Connection, credits and bytes in transit are virtio-vsock state.
fn files_script(image: &[u8], initrd: &[u8], plan: &Plan) -> (Outcome, String) {
    let devices = Devices { vsock_cid: Some(GUEST_CID), ..Devices::default() };
    let fresh = Box::new(move || Machine::with_devices(&MachineConfig::default(), &devices));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").unwrap();
        },
        plan,
    );
    let mut fc = FilesClient::default();
    let mut seen = Vec::new();
    let at = r.until(SHELL_PROMPT, 0);
    let at = r.command("mkdir /tmp/f && seq 1 200000 > /tmp/f/grande", at);
    let big: Vec<u8> = (1..=200_000u32).flat_map(|i| format!("{i}\n").into_bytes()).collect();
    let op = fc.list("/tmp/f");
    files_wait(&mut r, &mut fc, &mut seen, op).expect("list");
    r.mark("files-lettura");
    let op = fc.read_file("/tmp/f/grande");
    let Ok(FilesOutcome::Data { data, .. }) = files_wait(&mut r, &mut fc, &mut seen, op) else { panic!() };
    assert!(data == big, "chunked read differs ({} bytes)", data.len());
    let op = fc.watch("/tmp/f");
    files_wait(&mut r, &mut fc, &mut seen, op).expect("watch");
    r.m.console_input(b"echo dal-guest > /tmp/f/g.txt\n");
    r.mark("files-evento");
    let limit = r.m.steps + PHASE_BUDGET;
    while !seen.iter().any(|s| s.starts_with("evento g.txt ") && s.contains("mask: 8,")) {
        assert!(r.m.steps < limit, "event did not arrive: {seen:?}");
        assert_eq!(r.quantum(), Stop::Budget);
        fc.pump(&mut r.m);
        while let Some(e) = fc.take_event() {
            seen.push(format!("evento {} {e:?}", display_name(&e.name)));
        }
    }
    let at = r.until(SHELL_PROMPT, at);
    r.mark("files-scrittura");
    let op = fc.write_file("/tmp/f/copia", &big, 0o644);
    files_wait(&mut r, &mut fc, &mut seen, op).expect("write");
    r.m.console_input(b"cmp /tmp/f/grande /tmp/f/copia && echo COPIA-\"\"UGUALE\n");
    r.until("COPIA-UGUALE", at);
    r.poweroff();
    (r.finish(), seen.join("\n"))
}

#[test]
fn snapshot_durante_una_sessione_del_gestore_dei_file() {
    let Some((image, initrd)) = kernel() else { return };
    let (reference, ref_seen) = files_script(&image, &initrd, &Plan::default());
    let plan = Plan {
        at: vec![],
        marks: [
            ("files-lettura", (1, Cut::Swap { jit: false })),
            ("files-evento", (1, Cut::Swap { jit: true })),
            ("files-scrittura", (1, Cut::Rewind { quanta: 2 })),
        ]
        .into(),
        jit_from_start: false,
    };
    let (run, run_seen) = files_script(&image, &initrd, &plan);
    same("file manager with cuts", &reference, &run);
    assert!(run_seen == ref_seen, "file manager responses and events differ:\n{ref_seen}\n---\n{run_seen}");
}
