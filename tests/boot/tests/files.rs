//! Il gestore dei file di M8 (ADR 0020) sul kernel guest di M3: il demone
//! `vetro-files` nell'initramfs, avviato da `/init` perché c'è virtio-vsock,
//! e il client di `vetro_machine::files` dall'host.
//!
//! - list con i metadati (tipo, dimensione, modo, proprietario, destinazione
//!   dei collegamenti), lettura di un file piccolo e di uno grande a pezzi
//!   (1,2 MB, cinque READ), errori del guest (ENOENT, ENOTEMPTY);
//! - scrittura atomica che conserva modo e proprietario del file sostituito
//!   (letti dal guest con `stat`), che passa da un collegamento simbolico
//!   senza toccarlo, file nuovi con il proprietario della cartella, file
//!   grande scritto a pezzi e confrontato dal guest con `cmp`, nessun file
//!   temporaneo rimasto; create, mkdir, rename, delete (anche ricorsivo);
//! - watch con inotify: un processo del guest scrive un file e l'evento
//!   arriva all'host entro 1 s di tempo del guest; gli eventi dei file
//!   temporanei del demone non arrivano;
//! - determinismo: due esecuzioni danno lo stesso log, le stesse istruzioni
//!   e le stesse risposte;
//! - record & replay (ADR 0019): la sessione registrata (ogni operazione del
//!   client passa da `Machine::input`) si rigioca identica dall'avvio, senza
//!   client.
//!
//! Solo in release, come `vetro.rs`.

use vetro_boot_tests::*;
use vetro_machine::files::proto::{Kind, mask};
use vetro_machine::files::{FilesError, LinkState, Outcome};
use vetro_machine::{
    Devices, Digest, FilesClient, Input, Log, Machine, MachineConfig, RecordOptions, ReplayStatus, Stop,
};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;

fn machine() -> Machine {
    let devices = Devices { vsock_cid: Some(3), ..Devices::default() };
    Machine::with_devices(&MachineConfig::default(), &devices)
}

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let mut m = machine();
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    m
}

/// Il contenuto di `seq 1 n` di BusyBox.
fn seq(n: u32) -> Vec<u8> {
    (1..=n).flat_map(|i| format!("{i}\n").into_bytes()).collect()
}

struct Script {
    m: Machine,
    fc: FilesClient,
    log: Vec<u8>,
    /// Eventi arrivati, con il tempo del guest.
    events: Vec<(u64, vetro_machine::files::proto::Event)>,
    /// Ciò che l'host ha visto, per il confronto fra esecuzioni.
    seen: Vec<String>,
}

impl Script {
    /// Un quanto, poi il client (non a macchina spenta: un ingresso dopo
    /// l'ultima istruzione non arriverebbe mai al guest).
    fn quantum(&mut self) -> Stop {
        let s = self.m.run(QUANTUM);
        self.log.extend(self.m.console_output());
        if s == Stop::Budget {
            self.fc.pump(&mut self.m);
        }
        while let Some(e) = self.fc.take_event() {
            self.seen.push(format!("evento {e:?}"));
            self.events.push((self.m.guest_ns(), e));
        }
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

    /// Un comando alla shell: la sua uscita (fra due marcatori, così l'eco
    /// di una riga lunga, che l'editor di ash spezza, non conta).
    fn command(&mut self, cmd: &str) -> String {
        let from = self.log.len();
        let line = format!("echo VETRO-OUT-\"\"INIZIO; {cmd}; echo VETRO-OUT-\"\"FINE\n");
        self.m.input(Input::Console(line.into_bytes()));
        let end = self.until("VETRO-OUT-FINE", from);
        self.until(SHELL_PROMPT, end);
        let out = normalize(&String::from_utf8_lossy(&self.log[from..end]));
        let start = out.find("VETRO-OUT-INIZIO\n").expect("marcatore") + "VETRO-OUT-INIZIO\n".len();
        out[start..out.len() - "VETRO-OUT-FINE".len()].trim_end_matches('\n').to_string()
    }

    /// Esegue finché l'operazione `op` finisce.
    fn wait(&mut self, op: u32) -> Result<Outcome, FilesError> {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            while let Some(c) = self.fc.take_completion() {
                let brief = match &c.result {
                    Ok(Outcome::Data { size, data }) => format!("Data {size} {} byte", data.len()),
                    r => format!("{r:?}"),
                };
                self.seen.push(format!("op {} {brief}", c.op));
                if c.op == op {
                    return c.result;
                }
            }
            assert!(self.m.steps < limit, "operazione {op} non finita:\n{}", self.tail());
            assert_eq!(self.quantum(), Stop::Budget, "{}", self.tail());
        }
    }

    fn ok(&mut self, op: u32) -> Outcome {
        self.wait(op).unwrap_or_else(|e| panic!("operazione {op}: {e}\n{}", self.tail()))
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

struct Outcome2 {
    log: Vec<u8>,
    steps: u64,
    seen: Vec<String>,
    recording: Option<Log>,
    end: Digest,
}

fn session(image: &[u8], initrd: &[u8], record: bool) -> Outcome2 {
    let mut m = booted(image, initrd);
    if record {
        m.start_recording(RecordOptions::default());
    }
    let mut s =
        Script { m, fc: FilesClient::default(), log: Vec::new(), events: Vec::new(), seen: Vec::new() };
    s.until(SHELL_PROMPT, 0);
    s.command(
        "mkdir /tmp/f && chown 4321:8765 /tmp/f && echo ciao > /tmp/f/a.txt && chown 1234:5678 /tmp/f/a.txt \
         && chmod 640 /tmp/f/a.txt && ln -s a.txt /tmp/f/link && seq 1 200000 > /tmp/f/grande \
         && vetro-dev xattr-set /tmp/f security.selinux u:object_r:app_data_file:s0:c57 \
         && vetro-dev xattr-set /tmp/f/a.txt security.selinux u:object_r:app_data_file:s0:c1 \
         && vetro-dev xattr-set /tmp/f/a.txt user.vetro valore",
    );

    // ---- Collegamento e letture --------------------------------------------
    let list = s.fc.list("/tmp/f");
    let Outcome::List(entries) = s.ok(list) else { panic!() };
    assert!(matches!(s.fc.state(), LinkState::Ready(h) if h.max_chunk >= 256 << 10));
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.txt", "grande", "link"]);
    let a = &entries[0].stat;
    assert_eq!((a.kind, a.mode, a.uid, a.gid, a.size, a.nlink), (Kind::File, 0o100640, 1234, 5678, 5, 1));
    // Senza SELinux nel guest tmpfs tiene security.selinux come un xattr
    // qualsiasi: il demone lo legge come su Android.
    assert_eq!(a.selinux, "u:object_r:app_data_file:s0:c1");
    assert_eq!(entries[1].stat.selinux, "");
    let big = seq(200_000);
    assert_eq!(entries[1].stat.size, big.len() as u64);
    assert_eq!((entries[2].stat.kind, entries[2].stat.link.as_str()), (Kind::Symlink, "a.txt"));
    let r = s.fc.read_file("/tmp/f/a.txt");
    assert_eq!(s.ok(r), Outcome::Data { size: 5, data: b"ciao\n".to_vec() });
    let r = s.fc.read_file("/tmp/f/grande");
    let Outcome::Data { size, data } = s.ok(r) else { panic!() };
    assert_eq!(size, big.len() as u64);
    assert!(data == big, "file grande letto a pezzi diverso ({} byte)", data.len());
    let r = s.fc.read("/tmp/f/grande", 700_000, 10);
    assert_eq!(s.ok(r), Outcome::Data { size: big.len() as u64, data: big[700_000..700_010].to_vec() });
    let r = s.fc.read_file("/tmp/f/manca");
    assert_eq!(s.wait(r), Err(FilesError::Errno(2)));
    let st = s.fc.stat("/tmp/f");
    let Outcome::Stat(d) = s.ok(st) else { panic!() };
    assert_eq!((d.kind, d.uid, d.gid), (Kind::Dir, 4321, 8765));

    // ---- Osservazione e un processo del guest che scrive --------------------
    let w = s.fc.watch("/tmp/f");
    let Outcome::Watch(wd) = s.ok(w) else { panic!() };
    let t0 = s.m.guest_ns();
    s.m.input(Input::Console(b"echo dal-guest > /tmp/f/g.txt\n".to_vec()));
    let limit = s.m.steps + PHASE_BUDGET;
    let seen_at = loop {
        if let Some((t, _)) =
            s.events.iter().find(|(_, e)| e.wd == wd && e.name == "g.txt" && e.mask & mask::CLOSE_WRITE != 0)
        {
            break *t;
        }
        assert!(s.m.steps < limit, "evento di g.txt non arrivato: {:?}", s.events);
        s.quantum();
    };
    let ms = (seen_at - t0) as f64 / 1e6;
    eprintln!("evento della scrittura del guest dopo {ms:.1} ms di tempo del guest");
    assert!(seen_at - t0 < 1_000_000_000, "evento dopo {ms} ms (più di 1 s)");
    assert!(s.events.iter().any(|(_, e)| e.name == "g.txt" && e.mask & mask::CREATE != 0));
    s.until(SHELL_PROMPT, 0);

    // ---- Scritture -------------------------------------------------------
    let w = s.fc.write_file("/tmp/f/a.txt", b"nuovo contenuto\n", 0o600);
    let Outcome::Written(st) = s.ok(w) else { panic!() };
    assert_eq!(
        (st.mode, st.uid, st.gid, st.size),
        (0o100640, 1234, 5678, 16),
        "modo e proprietario conservati"
    );
    assert_eq!(st.selinux, "u:object_r:app_data_file:s0:c1", "contesto SELinux conservato");
    let out = s.command(
        "cat /tmp/f/a.txt; stat -c '%a %u %g %s' /tmp/f/a.txt; vetro-dev xattr-get /tmp/f/a.txt user.vetro; \
         vetro-dev xattr-get /tmp/f/a.txt security.selinux",
    );
    assert_eq!(
        out,
        "nuovo contenuto\n640 1234 5678 16\nvetro-dev: xattr user.vetro=valore\n\
         vetro-dev: xattr security.selinux=u:object_r:app_data_file:s0:c1"
    );
    // Attraverso il collegamento: cambia il file, il collegamento resta.
    let w = s.fc.write_file("/tmp/f/link", b"via-link\n", 0o600);
    s.ok(w);
    let out = s.command("cat /tmp/f/a.txt; readlink /tmp/f/link; stat -c '%a %u %g' /tmp/f/a.txt");
    assert_eq!(out, "via-link\na.txt\n640 1234 5678");
    // File nuovo: modo chiesto, proprietario della cartella.
    let w = s.fc.write_file("/tmp/f/nuovo.txt", b"creato dall'host\n", 0o604);
    let Outcome::Written(st) = s.ok(w) else { panic!() };
    assert_eq!((st.mode, st.uid, st.gid), (0o100604, 4321, 8765));
    assert_eq!(st.selinux, "u:object_r:app_data_file:s0:c57", "contesto della cartella");
    // Grande, a pezzi: il guest lo confronta con il suo.
    let w = s.fc.write_file("/tmp/f/copia", &big, 0o644);
    s.ok(w);
    let out = s.command("cmp /tmp/f/grande /tmp/f/copia && echo COPIA-UGUALE");
    assert_eq!(out, "COPIA-UGUALE");
    // create, mkdir, rename, delete.
    let c = s.fc.create("/tmp/f/vuoto", 0o640);
    assert_eq!(s.ok(c), Outcome::Done);
    let c = s.fc.create("/tmp/f/vuoto", 0o640);
    assert_eq!(s.wait(c), Err(FilesError::Errno(17)), "EEXIST");
    let c = s.fc.mkdir("/tmp/f/d", 0o750);
    assert_eq!(s.ok(c), Outcome::Done);
    let st = s.fc.stat("/tmp/f/d");
    let Outcome::Stat(d) = s.ok(st) else { panic!() };
    assert_eq!((d.kind, d.selinux.as_str()), (Kind::Dir, "u:object_r:app_data_file:s0:c57"));
    let c = s.fc.rename("/tmp/f/nuovo.txt", "/tmp/f/d/spostato.txt");
    assert_eq!(s.ok(c), Outcome::Done);
    let out = s.command(
        "stat -c '%n %a %u %g %s' /tmp/f/vuoto /tmp/f/d /tmp/f/d/spostato.txt; cat /tmp/f/d/spostato.txt; ls -a /tmp/f | cat",
    );
    assert_eq!(
        out,
        "/tmp/f/vuoto 640 4321 8765 0\n/tmp/f/d 750 4321 8765 60\n/tmp/f/d/spostato.txt 604 4321 8765 17\n\
         creato dall'host\n.\n..\na.txt\ncopia\nd\ng.txt\ngrande\nlink\nvuoto",
        "nessun file temporaneo rimasto"
    );
    let c = s.fc.delete("/tmp/f/d", false);
    assert_eq!(s.wait(c), Err(FilesError::Errno(39)), "ENOTEMPTY");
    let c = s.fc.delete("/tmp/f/d", true);
    assert_eq!(s.ok(c), Outcome::Done);
    let c = s.fc.delete("/tmp/f/vuoto", false);
    assert_eq!(s.ok(c), Outcome::Done);
    let out = s.command("ls /tmp/f | cat");
    assert_eq!(out, "a.txt\ncopia\ng.txt\ngrande\nlink");
    // Eventi delle scritture dell'host: il rename al posto del file vero,
    // mai i file temporanei.
    assert!(s.events.iter().any(|(_, e)| e.name == "a.txt" && e.mask & mask::MOVED_TO != 0));
    assert!(s.events.iter().all(|(_, e)| !e.name.starts_with(".vetro-tmp.")), "{:?}", s.events);
    let u = s.fc.unwatch(wd);
    assert_eq!(s.ok(u), Outcome::Done);
    assert_eq!(s.fc.generation(), 1, "una sola connessione");

    s.m.input(Input::Console(b"poweroff -f\n".to_vec()));
    let limit = s.m.steps + PHASE_BUDGET;
    let stop = loop {
        let st = s.quantum();
        if st != Stop::Budget || s.m.steps >= limit {
            break st;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "{}", s.tail());
    let recording = s.m.stop_recording();
    let end = s.m.digest();
    s.log.extend(s.m.console_output());
    Outcome2 { log: s.log, steps: s.m.steps, seen: s.seen, recording, end }
}

fn kernel() -> Option<(Vec<u8>, Vec<u8>)> {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "gestore dei file sul kernel guest solo in release");
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
fn gestore_dei_file_nel_guest() {
    let Some((image, initrd)) = kernel() else { return };
    let a = session(&image, &initrd, false);
    std::fs::write(repo_root().join("target/guest-kernel/vetro-files.log"), &a.log).unwrap();
    eprintln!("gestore dei file: {} istruzioni, {} passi dell'host", a.steps, a.seen.len());
    let b = session(&image, &initrd, false);
    assert_eq!(a.steps, b.steps, "istruzioni diverse fra due esecuzioni uguali");
    assert!(a.log == b.log, "log diversi fra due esecuzioni uguali");
    assert_eq!(a.seen, b.seen, "risposte diverse fra due esecuzioni uguali");
}

#[test]
fn gestore_dei_file_registrato_e_rigiocato() {
    let Some((image, initrd)) = kernel() else { return };
    let rec = session(&image, &initrd, true);
    let log = Log::decode(&rec.recording.expect("registrazione").encode()).expect("il log si rilegge");
    let vsock = log
        .events
        .iter()
        .filter(|e| matches!(e.kind, vetro_machine::record::EventKind::Input(Input::Vsock(_))))
        .count();
    assert!(vsock > 20, "{vsock} ingressi vsock registrati");
    assert!(
        log.events.iter().all(|e| matches!(e.kind, vetro_machine::record::EventKind::Input(_))),
        "nessun evento opaco"
    );
    // Il replay dall'avvio, senza client: gli ingressi vengono dal log.
    let mut m = booted(&image, &initrd);
    m.start_replay(&log).expect("stato di partenza");
    let mut out = m.console_output();
    let limit = m.steps + 20 * PHASE_BUDGET;
    while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
        assert!(m.steps < limit, "replay senza fine");
        m.run(3 * QUANTUM + 17);
        out.extend(m.console_output());
    }
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
    assert!(out == rec.log, "log del replay diverso dalla registrazione");
    assert_eq!(m.steps, rec.steps);
    assert_eq!(m.digest(), rec.end, "stato finale del replay");
    eprintln!(
        "replay del gestore dei file: {} eventi ({vsock} vsock), {} istruzioni",
        log.events.len(),
        m.steps
    );
}
