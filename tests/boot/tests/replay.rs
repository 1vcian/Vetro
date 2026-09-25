//! Criterio del nucleo di M10: record & replay della macchina (ADR 0019) sul
//! kernel guest di M3.
//!
//! Una sessione interattiva gira **registrata**: comandi battuti alla shell
//! un byte alla volta, DHCP, HTTP e ping verso il sinkhole, un frame ICMP
//! consegnato dall'host al guest (`Input::NetFrame`: il guest risponde, e
//! `/proc/net/snmp` lo conta), eventi della tastiera virtio-input, una
//! connessione dall'host a un servizio del guest (`Input::HostNet`, eco con
//! `nc -e cat`), `sleep` (WFI che saltano il tempo), spegnimento. Poi:
//!
//! - la stessa sessione **senza** registrazione dà la stessa esecuzione
//!   (registrare non cambia niente) e misura il costo della registrazione;
//! - il **replay** dall'avvio, con l'interprete e quanti diversi, e dal
//!   keyframe iniziale del log col **JIT**: stesso log della console, stesse
//!   istruzioni, stessa CPU, RAM e stato dei dispositivi (col JIT tutto
//!   tranne il TLB, ADR 0013);
//! - il **salto** (`Machine::goto`) a punti della sessione, da una macchina
//!   nuova, con l'interprete e col JIT: registri e RAM uguali a quelli
//!   dell'esecuzione registrata in quel punto;
//! - un **ingresso tolto dal log** (un tasto della shell, il frame ICMP)
//!   rende il replay diverso, e il replay lo dice.
//!
//! Misure in `target/guest-kernel/replay-misure.txt`. Solo in release, come
//! `vetro.rs`.

use std::time::Instant;

use vetro_boot_tests::*;
use vetro_machine::vetro_net::{HostConnState, TcpReply};
use vetro_machine::vetro_snapshot::hash64;
use vetro_machine::{
    Devices, Divergence, HostNetOp, Input, Log, Machine, MachineConfig, NetSetup, RecordOptions,
    ReplayStatus, Reply, Stop,
};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const JIT_THRESHOLD: u32 = 16;
const KEYFRAME_EVERY: u64 = 100_000_000;
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

fn devices() -> Devices {
    let mut net = NetSetup::default();
    let body = b"VETRO-SINKHOLE-OK\n";
    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let mut on_data = reply.into_bytes();
    on_data.extend_from_slice(body);
    net.sinkhole
        .tcp_by_port
        .insert(80, TcpReply { on_connect: Vec::new(), on_data, close_after_reply: true });
    Devices { net: Some(net), ..Devices::default() }
}

fn machine() -> Machine {
    Machine::with_devices(&MachineConfig::default(), &devices())
}

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let mut m = machine();
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    m
}

fn jit() -> Option<Box<dyn vetro_machine::SysJitDyn>> {
    Some(vetro_jit_native::system_jit(JIT_THRESHOLD))
}

/// Somma di controllo IP (complemento a uno).
fn checksum(b: &[u8]) -> [u8; 2] {
    let mut s = 0u32;
    for c in b.chunks(2) {
        s += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    (!(s as u16)).to_be_bytes()
}

/// Echo request ICMP dal gateway (10.0.2.2) al guest, in un frame Ethernet.
fn icmp_echo_request() -> Vec<u8> {
    let mut icmp = vec![8, 0, 0, 0, 0x56, 0x45, 0, 1];
    icmp.extend_from_slice(b"VETRO-ICMP-DALL-HOST");
    let c = checksum(&icmp);
    icmp[2..4].copy_from_slice(&c);
    let mut ip = vec![0x45, 0, 0, 0, 0x12, 0x34, 0, 0, 64, 1, 0, 0, 10, 0, 2, 2, 10, 0, 2, 15];
    let total = (ip.len() + icmp.len()) as u16;
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    let c = checksum(&ip);
    ip[10..12].copy_from_slice(&c);
    let mut f = GUEST_MAC.to_vec();
    f.extend_from_slice(&[0x52, 0x55, 0x0a, 0x00, 0x02, 0x02, 0x08, 0x00]);
    f.extend_from_slice(&ip);
    f.extend_from_slice(&icmp);
    f
}

/// `InEchos` nell'uscita di `grep Icmp: /proc/net/snmp`.
fn in_echos(out: &str) -> u64 {
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("Icmp: ")).collect();
    let header = lines.iter().find(|l| l.contains("InEchos")).expect("intestazione Icmp");
    let values =
        lines.iter().find(|l| l[6..].starts_with(|c: char| c.is_ascii_digit())).expect("valori Icmp");
    let i = header.split_whitespace().position(|w| w == "InEchos").unwrap();
    values.split_whitespace().nth(i).unwrap().parse().unwrap()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Il copione, dal punto di vista dell'host: guarda la console fra un
/// quanto e l'altro e dà gli ingressi con `Machine::input`.
struct Script {
    m: Machine,
    log: Vec<u8>,
    /// Punti della sessione: istruzioni, registri, hash della RAM.
    marks: Vec<(u64, String, u64)>,
}

impl Script {
    fn quantum(&mut self, q: u64) -> Stop {
        let s = self.m.run(q);
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
            let stop = self.quantum(QUANTUM);
            assert_eq!(stop, Stop::Budget, "{stop:?} in attesa di {needle:?}:\n{}", self.tail());
        }
    }

    fn input(&mut self, i: Input) -> Reply {
        let r = self.m.input(i);
        assert_ne!(r, Reply::NoDevice);
        r
    }

    fn command(&mut self, cmd: &str, from: usize) -> (usize, String) {
        self.input(Input::Console(format!("{cmd}\n").into_bytes()));
        let at = self.until(SHELL_PROMPT, from);
        (at, normalize(&String::from_utf8_lossy(&self.log[from..at])))
    }

    /// Batte `text` un byte alla volta, con una frazione di quanto fra un
    /// tasto e l'altro.
    fn type_slowly(&mut self, text: &str) {
        for b in text.bytes() {
            self.input(Input::Console(vec![b]));
            assert_eq!(self.quantum(QUANTUM / 3 + 7), Stop::Budget);
        }
    }

    fn mark(&mut self) {
        let ram = hash64(self.m.board.borrow().ram.bytes());
        self.marks.push((self.m.steps, self.m.registers_text(), ram));
    }

    fn net_state(&self, id: u64) -> Option<(usize, HostConnState)> {
        self.m.net_view(|s| s.host_conn(id).map(|i| (i.readable, i.state))).flatten()
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

/// Esito della sessione.
struct Session {
    log: Vec<u8>,
    steps: u64,
    marks: Vec<(u64, String, u64)>,
    recording: Option<Log>,
    end: vetro_machine::Digest,
    secs: f64,
}

fn session(image: &[u8], initrd: &[u8], record: Option<RecordOptions>) -> Session {
    let mut s = Script { m: booted(image, initrd), log: Vec::new(), marks: Vec::new() };
    let t0 = Instant::now();
    if let Some(opts) = record {
        s.m.start_recording(opts);
    }
    let at = s.until(SHELL_PROMPT, 0);

    // Tasti battuti alla shell uno per uno.
    s.type_slowly("echo \"V\"ETRO-$((6*7))\n");
    let at = s.until(SHELL_PROMPT, at);
    assert!(normalize(&String::from_utf8_lossy(&s.log[..at])).contains("VETRO-42"), "{}", s.tail());

    // Rete verso il sinkhole.
    let (at, out) = s.command("udhcpc -i eth0 -n -q", at);
    assert!(out.contains("bound eth0 10.0.2.15"), "{out}");
    let (at, out) = s.command("echo \"G\"ET=$(wget -q -O - http://vetro.example/prova)", at);
    assert!(out.contains("GET=VETRO-SINKHOLE-OK"), "{out}");
    let (at, out) = s.command("ping -c 1 -W 5 10.0.2.2 | grep -c ttl=", at);
    assert!(out.lines().any(|l| l.trim() == "1"), "{out}");

    // Un frame dall'host: il guest risponde all'echo request.
    let (at, out) = s.command("grep Icmp: /proc/net/snmp", at);
    assert_eq!(in_echos(&out), 0, "{out}");
    s.mark();
    assert_eq!(s.input(Input::NetFrame(icmp_echo_request())), Reply::Done);
    for _ in 0..3 {
        assert_eq!(s.quantum(QUANTUM), Stop::Budget);
    }
    let (at, out) = s.command("grep Icmp: /proc/net/snmp", at);
    assert_eq!(in_echos(&out), 1, "il frame dell'host non è arrivato:\n{out}");

    // Tastiera virtio-input (event1).
    s.input(Input::Console(b"vetro-dev input-read /dev/input/event1 4\n".to_vec()));
    let ready = s.until("VETRO-INPUT-PRONTO", at);
    s.input(Input::Keyboard(Input::key_events(30, true)));
    s.input(Input::Keyboard(Input::key_events(30, false)));
    let at = s.until(SHELL_PROMPT, ready);
    let out = normalize(&String::from_utf8_lossy(&s.log[ready..at]));
    assert!(out.contains("vetro-dev: evento 1 30 1") && out.contains("vetro-dev: evento 1 30 0"), "{out}");
    s.mark();

    // Una connessione dall'host verso un servizio del guest (eco).
    s.input(Input::Console(b"nc -n -v -l -p 5555 -e cat\n".to_vec()));
    s.until("listening on", at);
    let Reply::HostConn(Some(id)) = s.input(Input::HostNet(HostNetOp::Connect(5555))) else {
        panic!("connessione dell'host")
    };
    let data: Vec<u8> = (0..20_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let (mut sent, mut got, mut shut) = (0usize, Vec::new(), false);
    let limit = s.m.steps + PHASE_BUDGET;
    loop {
        if sent < data.len()
            && let Reply::Accepted(n) = s.input(Input::HostNet(HostNetOp::Send(id, data[sent..].to_vec())))
        {
            sent += n as usize;
        }
        while s.net_state(id).is_some_and(|(r, _)| r > 0) {
            let Reply::Data(d) = s.input(Input::HostNet(HostNetOp::Recv(id, 4096))) else { panic!() };
            got.extend(d);
        }
        if !shut && got.len() == data.len() {
            s.input(Input::HostNet(HostNetOp::Shutdown(id)));
            shut = true;
        }
        if matches!(s.net_state(id), Some((0, HostConnState::Closed(_)))) {
            break;
        }
        assert!(s.m.steps < limit, "eco dall'host non finita:\n{}", s.tail());
        assert_eq!(s.quantum(QUANTUM / 2), Stop::Budget);
    }
    assert!(got == data, "eco dall'host diversa ({} byte su {})", got.len(), data.len());
    s.input(Input::HostNet(HostNetOp::Release(id)));
    let at = s.until(SHELL_PROMPT, at);
    s.mark();

    // Il tempo che salta nelle WFI, poi lo spegnimento.
    let (_, out) = s.command("sleep 2; echo \"D\"OPO", at);
    assert!(out.contains("DOPO"), "{out}");
    s.input(Input::Console(b"poweroff -f\n".to_vec()));
    let limit = s.m.steps + PHASE_BUDGET;
    let stop = loop {
        let st = s.quantum(QUANTUM);
        if st != Stop::Budget || s.m.steps >= limit {
            break st;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "{}", s.tail());
    let recording = s.m.stop_recording();
    let secs = t0.elapsed().as_secs_f64();
    let end = s.m.digest();
    s.log.extend(s.m.console_output());
    Session { log: s.log, steps: s.m.steps, marks: s.marks, recording, end, secs }
}

/// Replay fino alla fine a quanti di `q`: log della console.
fn replay(m: &mut Machine, q: u64) -> Vec<u8> {
    let mut log = m.console_output();
    let limit = m.steps + 20 * PHASE_BUDGET;
    while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
        assert!(m.steps < limit, "replay senza fine");
        let s = m.run(q);
        log.extend(m.console_output());
        if !matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
            break;
        }
        assert_eq!(s, Stop::Budget, "{s:?} a {} istruzioni durante il replay", m.steps);
    }
    log
}

fn same_log(what: &str, a: &[u8], b: &[u8]) {
    if a != b {
        let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
        let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        panic!(
            "{what}: log diverso dalla riga {}:\nregistrato: {:?}\nreplay:     {:?}",
            i + 1,
            a.get(i),
            b.get(i)
        );
    }
}

fn kernel() -> Option<(Vec<u8>, Vec<u8>)> {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "replay sul kernel guest solo in release");
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

#[test]
fn sessione_registrata_e_riprodotta() {
    let Some((image, initrd)) = kernel() else { return };
    let rec = session(&image, &initrd, Some(RecordOptions { keyframe_every: KEYFRAME_EVERY }));
    let log = rec.recording.clone().expect("registrazione");
    let bytes = log.encode();
    let log = Log::decode(&bytes).expect("il log si rilegge");
    let inputs = log.events.len();
    assert!(inputs > 40, "{inputs} eventi");
    assert!(log.events.iter().all(|e| matches!(e.kind, vetro_machine::record::EventKind::Input(_))));
    assert_eq!(log.end, rec.end);
    assert_eq!(log.end.steps, rec.steps);

    // Registrare (con o senza keyframe) non cambia l'esecuzione; misura il
    // costo.
    // Tempi: il migliore di due esecuzioni alternate (la macchina che fa
    // girare i test può essere carica).
    let (mut plain_secs, mut bare_secs) = (f64::MAX, f64::MAX);
    for _ in 0..2 {
        let plain = session(&image, &initrd, None);
        same_log("senza registrazione", &rec.log, &plain.log);
        assert_eq!(plain.end, rec.end, "stato finale senza registrazione");
        assert_eq!(plain.marks, rec.marks);
        plain_secs = plain_secs.min(plain.secs);
        let bare = session(&image, &initrd, Some(RecordOptions::default()));
        let bare_log = bare.recording.as_ref().unwrap();
        assert!(bare_log.keyframes.is_empty());
        assert_eq!(bare_log.events, log.events, "stessi eventi con e senza keyframe");
        assert_eq!(bare.end, rec.end);
        bare_secs = bare_secs.min(bare.secs);
    }

    // Replay dall'avvio con l'interprete, a quanti diversi da quelli del
    // copione.
    let t0 = Instant::now();
    let mut m = booted(&image, &initrd);
    m.start_replay(&log).expect("stato di partenza");
    let out = replay(&mut m, 3_333_333);
    let replay_secs = t0.elapsed().as_secs_f64();
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "replay con l'interprete");
    same_log("replay con l'interprete", &rec.log, &out);
    assert_eq!(m.steps, rec.steps);
    assert_eq!(m.digest(), rec.end, "stato finale del replay (TLB compreso)");

    // Replay col JIT dal keyframe iniziale, su una macchina nuova senza
    // kernel caricato.
    let mut m = machine();
    m.set_jit(jit());
    m.replay_from(&log, 0).expect("keyframe iniziale");
    let out = replay(&mut m, QUANTUM);
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "replay col JIT");
    same_log("replay col JIT", &rec.log, &out);
    let end = m.digest();
    assert_eq!(end.diff(&rec.end, false), None, "stato finale col JIT (senza TLB)");
    assert!(m.jit_stats().unwrap().jit_steps > 0, "il JIT ha lavorato");

    // Salto ai punti della sessione, da macchine nuove.
    for with_jit in [false, true] {
        let mut m = machine();
        if with_jit {
            m.set_jit(jit());
        }
        for (steps, regs, ram) in rec.marks.iter().rev() {
            assert_eq!(m.goto(&log, *steps).unwrap(), *steps);
            assert_eq!(m.registers_text(), *regs, "registri a {steps} (JIT {with_jit})");
            assert_eq!(hash64(m.board.borrow().ram.bytes()), *ram, "RAM a {steps} (JIT {with_jit})");
        }
    }

    // Misure.
    let kf: usize = log.keyframes.iter().map(|k| k.snapshot.len()).sum();
    let text = format!(
        "sessione: {} istruzioni, {} eventi, {} keyframe\n\
         log: {} byte in tutto, {} byte senza keyframe, keyframe {} byte\n\
         tempo: senza registrazione {:.3} s, registrata senza keyframe {:.3} s ({:+.1}%), \
         con keyframe {:.3} s ({:+.1}%), replay {:.3} s\n",
        rec.steps,
        inputs,
        log.keyframes.len(),
        bytes.len(),
        log.events_len(),
        kf,
        plain_secs,
        bare_secs,
        (bare_secs / plain_secs - 1.0) * 100.0,
        rec.secs,
        (rec.secs / plain_secs - 1.0) * 100.0,
        replay_secs
    );
    eprint!("{text}");
    let path = repo_root().join("target/guest-kernel/replay-misure.txt");
    let _ = std::fs::write(path, text);

    // Un ingresso tolto dal log: il replay se ne accorge.
    let key = log
        .events
        .iter()
        .position(|e| e.kind == vetro_machine::record::EventKind::Input(Input::Console(b"7".to_vec())))
        .expect("il tasto 7 battuto alla shell");
    let frame = log
        .events
        .iter()
        .position(|e| matches!(e.kind, vetro_machine::record::EventKind::Input(Input::NetFrame(_))))
        .expect("il frame dell'host");
    for (what, i) in [("tasto", key), ("frame di rete", frame)] {
        let mut cut = log.clone();
        cut.events.remove(i);
        let mut m = machine();
        m.replay_from(&cut, 0).unwrap();
        replay(&mut m, QUANTUM);
        match m.replay_status() {
            Some(ReplayStatus::Diverged(d)) => {
                eprintln!("senza il {what}: {d}");
                if let Divergence::Event { index, .. } = d {
                    assert!(*index >= i, "divergenza prima dell'ingresso tolto");
                }
            }
            other => panic!("senza il {what} il replay dovrebbe divergere: {other:?}"),
        }
    }
}
