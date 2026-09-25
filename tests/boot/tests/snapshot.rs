//! Criterio della prima parte di M6: save/restore della macchina intera
//! (ADR 0015) sul kernel guest di M3.
//!
//! Ogni copione (avvio fino allo spegnimento, rete, disco, dispositivi) gira
//! una volta senza interruzioni e poi con dei **tagli**: fra un quanto e
//! l'altro la macchina si salva e il copione continua su una macchina
//! **nuova** ripristinata dallo snapshot (con o senza JIT), oppure la
//! macchina va avanti e poi torna indietro allo snapshot nello stesso posto
//! (ripristino sopra una macchina usata, col JIT che ha tradotto codice nel
//! frattempo). Il copione, cioè l'host, non se ne accorge. Alla fine devono
//! coincidere con l'esecuzione senza tagli: il log della console byte per
//! byte, il numero di istruzioni, la RAM e lo stato di tutti i dispositivi
//! (lo snapshot finale); col JIT tutto tranne il TLB, che col JIT vede meno
//! accessi (ADR 0013, "Differenza ammessa"). A ogni taglio due salvataggi
//! danno gli stessi byte, e la macchina ripristinata risalva gli stessi byte.
//!
//! In più: dimensione dello snapshot e tempi di salvataggio e ripristino alla
//! shell (1 GiB di RAM), stampati e scritti in
//! `target/guest-kernel/snapshot-misure.txt`.
//!
//! Solo in release, come `vetro.rs`.

use std::collections::BTreeMap;
use std::time::Instant;

use vetro_boot_tests::*;
use vetro_machine::vetro_net::TcpReply;
use vetro_machine::vetro_snapshot::{Snapshot, Writer, hash64};
use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};
use vetro_platform::virtio::{
    CowBackend, MemBackend, MemDisplay, VirtioBlk, VirtioBlkConfig, VsockConn, VsockState,
};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const JIT_THRESHOLD: u32 = 16;

/// Che cosa succede a un taglio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// Salva, ripristina in una macchina nuova e continua lì. `jit`: la
    /// macchina nuova ha il JIT.
    Swap { jit: bool },
    /// Salva, va avanti `quanta` quanti (uscita scartata), poi ripristina
    /// lo snapshot nella stessa macchina (JIT compreso, con i suoi blocchi)
    /// e continua.
    Rewind { quanta: u64 },
}

/// Quando tagliare: a un numero di istruzioni, o qualche quanto dopo un
/// punto del copione (`Run::mark`).
#[derive(Clone, Debug, Default)]
struct Plan {
    at: Vec<(u64, Cut)>,
    marks: BTreeMap<&'static str, (u64, Cut)>,
    /// La prima macchina ha il JIT.
    jit_from_start: bool,
}

struct Run {
    m: Machine,
    log: Vec<u8>,
    /// Una macchina nuova, configurata come quella del copione (dischi
    /// compresi), senza JIT e senza kernel.
    fresh: Box<dyn Fn() -> Machine>,
    pending: Vec<(u64, Cut)>,
    marks: BTreeMap<&'static str, (u64, Cut)>,
    jit: bool,
    /// Tagli eseguiti (istruzioni, tipo).
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

    /// Punto del copione: se il piano lo chiede, un taglio fra `quanta`
    /// quanti.
    fn mark(&mut self, name: &'static str) {
        if let Some((quanta, cut)) = self.marks.remove(name) {
            self.pending.push((self.m.steps + quanta * QUANTUM, cut));
            self.pending.sort_by_key(|c| std::cmp::Reverse(c.0));
        }
    }

    fn cut(&mut self, cut: Cut) {
        let snap = self.m.save();
        assert!(snap == self.m.save(), "due salvataggi nello stesso punto danno byte diversi");
        match cut {
            Cut::Swap { jit } => {
                let mut n = (self.fresh)();
                n.load_state(&snap).expect("ripristino");
                assert!(n.save() == snap, "la macchina ripristinata non risalva gli stessi byte");
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
                self.m.load_state(&snap).expect("ripristino sopra la macchina usata");
                assert!(
                    self.m.save() == snap,
                    "il ripristino sopra la macchina usata non risalva gli stessi byte"
                );
            }
        }
        self.done.push((self.m.steps, cut));
    }

    /// Un quanto (dopo gli eventuali tagli dovuti), con l'uscita nel log.
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
            assert!(self.m.steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            let stop = self.quantum();
            host(&mut self.m);
            assert_eq!(stop, Stop::Budget, "{stop:?} in attesa di {needle:?}:\n{}", self.tail());
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
        assert!(self.pending.is_empty() && self.marks.is_empty(), "tagli non eseguiti: {:?}", self.pending);
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

/// Fine di un copione: ciò che si confronta.
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

/// `run` coincide con `reference` (senza tagli e senza JIT).
fn same(what: &str, reference: &Outcome, run: &Outcome) {
    assert!(!run.cuts.is_empty(), "{what}: nessun taglio eseguito");
    eprintln!("{what}: tagli {:?}", run.cuts);
    if run.log != reference.log {
        let (a, b) = (String::from_utf8_lossy(&reference.log), String::from_utf8_lossy(&run.log));
        let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        panic!(
            "{what}: log diverso dalla riga {}:\nsenza tagli: {:?}\ncon tagli:   {:?}",
            i + 1,
            a.get(i),
            b.get(i)
        );
    }
    assert_eq!(run.steps, reference.steps, "{what}: istruzioni");
    assert_eq!(run.cpu, reference.cpu, "{what}: CPU");
    assert_eq!(run.ram, reference.ram, "{what}: RAM");
    assert_eq!(run.platform, reference.platform, "{what}: stato dei dispositivi");
    if !run.jit {
        assert_eq!(run.mmu, reference.mmu, "{what}: MMU e TLB");
    }
}

fn kernel() -> Option<(Vec<u8>, Vec<u8>)> {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "snapshot sul kernel guest solo in release");
        return None;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
        return None;
    };
    Some((std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap()))
}

// ---- Avvio fino allo spegnimento (il copione di vetro.rs) --------------------

fn boot_script(image: &[u8], initrd: &[u8], plan: &Plan) -> Outcome {
    let fresh = Box::new(|| Machine::new(&MachineConfig::default()));
    let mut r = Run::new(
        fresh,
        |m| {
            m.load_linux(image, Some(initrd), "console=ttyAMA0").expect("caricamento del kernel");
        },
        plan,
    );
    let at = r.until(BOOT_MARKER, 0);
    let at_end = r.until(AUTOTEST_END, at);
    let end = r.until("\n", at_end);
    let line = String::from_utf8_lossy(&r.log[at_end - AUTOTEST_END.len()..end]).into_owned();
    assert_eq!(line.trim_end(), AUTOTEST_OK, "autotest con errori:\n{}", r.tail());
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
    eprintln!("avvio senza tagli: {} istruzioni in {:.2} s", reference.steps, t0.elapsed().as_secs_f64());

    // Solo interprete: tagli presto (prima dell'MMU accesa), durante l'avvio
    // del kernel, a metà dell'autotest e alla shell; un ritorno indietro.
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
    same("avvio con tagli (interprete)", &reference, &boot_script(&image, &initrd, &plan));

    // JIT prima e dopo: interprete, poi JIT da un taglio durante l'avvio,
    // JIT tenuto a un altro taglio, ritorno indietro col JIT (blocchi da
    // scartare), poi di nuovo interprete alla shell.
    let plan = Plan {
        at: vec![
            (30 * QUANTUM, Cut::Swap { jit: true }),
            (60 * QUANTUM, Cut::Rewind { quanta: 5 }),
            (90 * QUANTUM, Cut::Swap { jit: true }),
        ],
        marks: [("shell", (0, Cut::Swap { jit: false }))].into(),
        jit_from_start: false,
    };
    same("avvio con tagli (interprete, JIT, interprete)", &reference, &boot_script(&image, &initrd, &plan));
}

/// Dimensione dello snapshot e tempi di salvataggio e ripristino alla shell
/// (1 GiB di RAM, dispositivi di default), su un avvio vero.
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
        "snapshot alla shell ({} istruzioni, RAM 1 GiB, {} pagine non a zero = {:.1} MiB): \
         {} byte ({:.1} MiB); salvataggio {:.0} ms, macchina nuova {:.0} ms, ripristino {:.0} ms\n",
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
    // La macchina ripristinata prosegue come l'originale.
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

// ---- Rete (il copione di net.rs, ridotto) ------------------------------------

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
    // Tagli con connessioni TCP a metà trasferimento (300 KB verso il guest,
    // POST da 108 KB verso l'host), durante un ping e con TIME-WAIT in corso.
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
    same("rete con tagli", &reference, &run);
    assert!(events == ref_events, "registro di rete e sinkhole diversi");
}

// ---- Disco (il copione di web.rs) --------------------------------------------

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
        // La base in sola lettura è il collegamento (come il file o l'HTTP
        // Range); il livello copy-on-write è stato ed entra nello snapshot.
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
    assert!(r.text(0).contains("\nVETRO-SCRITTO\n"), "{}", r.tail());
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
    same("disco con tagli", &reference, &disk_script(&image, &initrd, &plan));
}

// ---- GPU, input e vsock (il copione di devices.rs, ridotto) ------------------

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

    // GPU: il taglio cade mentre lo scanout mostra il motivo del guest; il
    // display (MemDisplay, un collegamento) della macchina nuova lo riceve
    // dallo stato ripristinato.
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

    // Input: tasti con il lettore evdev in attesa.
    r.m.console_input(b"vetro-dev input-read /dev/input/event1 4\n");
    let ready = r.until("VETRO-INPUT-PRONTO", at);
    r.m.keyboard(|k| {
        k.key(30, true);
        k.key(30, false);
    });
    r.mark("input");
    let at = r.until(SHELL_PROMPT, ready);

    // vsock: il guest si collega all'host e riceve 300 KB (più del suo
    // credito); il taglio cade a trasferimento in corso.
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
    same("dispositivi con tagli", &reference, &run);
    assert_eq!(host, ref_host, "ciò che l'host vede (scanout, cursore, vsock)");
}
