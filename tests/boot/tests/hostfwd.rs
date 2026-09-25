//! Inoltro di porte verso il guest (`Stack::host_connect`, come `hostfwd`
//! della rete user di QEMU): l'host apre connessioni TCP verso un servizio
//! del kernel guest di M3 (BusyBox `nc -l -e cat`, l'eco).
//!
//! Sotto Vetro (API diretta dello stack, tra un quanto e l'altro):
//! - il guest vede la connessione arrivare da 10.0.2.2 (`nc -v`);
//! - eco di 200 KB (più della finestra di 64 KiB e della coda dell'host),
//!   confrontato byte per byte;
//! - chiusura pulita dall'host (FIN, TIME-WAIT, `Normal`) e dal guest (il
//!   servizio scrive e chiude: l'host vede la fine del flusso);
//! - porta senza servizio: il guest risponde RST (`Refused`);
//! - reset dall'host (`RemoteReset`) e dal guest (chiusura con dati non
//!   letti: `GuestReset`);
//! - determinismo: due esecuzioni danno stesso log, istruzioni e registro.
//!
//! Confronto con QEMU (`-netdev user,hostfwd=tcp:127.0.0.1:PORTA-:5555`):
//! lo stesso `nc -n -v -l -p 5555 -e cat` nel guest, un client dall'host
//! (su macOS dentro il container di QEMU, perché le connessioni che Docker
//! inoltra arrivano dal suo gateway e non da localhost): stessa riga
//! `connect to ... from 10.0.2.2:...` (porta sorgente a parte) e stessa eco.
//!
//! Solo in release, come `net.rs`.

use std::io::{Read, Write};
use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::*;
use vetro_machine::vetro_net::{CloseReason, ConnId, EventKind, HostConnState, Sinkhole, Stack};
use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};

const PHASE_BUDGET: u64 = 6_000_000_000;
const PORT: u16 = 5555;
/// Il servizio del confronto con QEMU: eco, con la riga di connessione.
const ECHO_SERVER: &str = "nc -n -v -l -p 5555 -e cat";
const LISTENING: &str = "listening on";
const MESSAGE: &[u8] = b"VETRO-ECO-HOSTFWD\n";

fn payload() -> Vec<u8> {
    (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 9) as u8).collect()
}

struct Run {
    m: Machine,
    log: Vec<u8>,
}

impl Run {
    fn step(&mut self, what: &str) {
        let stop = self.m.run(1_000_000);
        self.log.extend(self.m.console_output());
        assert!(matches!(stop, Stop::Budget), "{stop:?} in attesa di {what}:\n{}", self.tail());
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            self.step(needle);
        }
    }

    fn command(&mut self, cmd: &str, from: usize) -> (usize, String) {
        self.m.console_input(format!("{cmd}\n").as_bytes());
        let at = self.until(SHELL_PROMPT, from);
        (at, normalize(&String::from_utf8_lossy(&self.log[from..at])))
    }

    /// Esegue quanti di istruzioni e, tra un quanto e l'altro, dà allo
    /// stack di rete la funzione dell'host finché questa non restituisce
    /// vero. È il punto in cui gli ingressi dell'host entrano nella macchina.
    fn pump(&mut self, what: &str, mut host: impl FnMut(&mut Stack<Sinkhole>) -> bool) {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if self.m.net(&mut host).expect("virtio-net") {
                return;
            }
            assert!(self.m.steps < limit, "{what}: tempo scaduto:\n{}", self.tail());
            self.step(what);
        }
    }

    fn state(&self, id: ConnId) -> HostConnState {
        self.m.net_view(|s| s.host_conn(id).expect("connessione").state).unwrap()
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

/// Le righe `listening on ...` e `connect to ... from ...` di `nc -v`, con
/// le porte sorgente (effimere: 49152 in Vetro, quella del client in QEMU)
/// sostituite.
fn connect_line(out: &str) -> String {
    let listening = out
        .lines()
        .find(|l| l.starts_with(LISTENING))
        .unwrap_or_else(|| panic!("manca la riga di ascolto di nc:\n{out}"));
    let line = out
        .lines()
        .find(|l| l.contains("connect to"))
        .unwrap_or_else(|| panic!("manca la riga di connessione di nc:\n{out}"));
    let mut r = String::new();
    let mut rest = line.trim();
    while let Some(i) = rest.find("10.0.2.2:") {
        r.push_str(&rest[..i + "10.0.2.2:".len()]);
        rest = rest[i + "10.0.2.2:".len()..].trim_start_matches(|c: char| c.is_ascii_digit());
        r.push_str("PORTA");
    }
    r.push_str(rest);
    format!("{} | {r}", listening.trim())
}

/// Connessione dall'host con i byte da mandare e quelli ricevuti; chiude il
/// suo verso quando ha mandato tutto e ricevuto `expect` byte.
struct Transfer {
    id: ConnId,
    data: Vec<u8>,
    sent: usize,
    got: Vec<u8>,
    expect: usize,
    shut: bool,
}

impl Transfer {
    fn new(s: &mut Stack<Sinkhole>, data: Vec<u8>, expect: usize) -> Self {
        let id = s.host_connect(PORT).expect("porta effimera");
        Transfer { id, data, sent: 0, got: Vec::new(), expect, shut: false }
    }

    /// Un giro dell'host; vero quando la connessione è chiusa.
    fn turn(&mut self, s: &mut Stack<Sinkhole>) -> bool {
        self.sent += s.host_send(self.id, &self.data[self.sent..]);
        let mut buf = [0u8; 65536];
        loop {
            let n = s.host_recv(self.id, &mut buf);
            if n == 0 {
                break;
            }
            self.got.extend_from_slice(&buf[..n]);
        }
        let info = s.host_conn(self.id).unwrap();
        if !self.shut && self.sent == self.data.len() && (self.got.len() >= self.expect || info.guest_eof) {
            s.host_shutdown(self.id);
            self.shut = true;
        }
        matches!(info.state, HostConnState::Closed(_))
    }
}

struct Session {
    log: String,
    steps: u64,
    events: String,
    connect_line: String,
    echo: Vec<u8>,
}

fn session(image: &[u8], initrd: &[u8]) -> Session {
    let devices = Devices { net: Some(NetSetup::default()), ..Devices::default() };
    let mut m = Machine::with_devices(&MachineConfig::default(), &devices);
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    let mut r = Run { m, log: Vec::new() };
    let at = r.until(SHELL_PROMPT, 0);
    let (at, out) = r.command("udhcpc -i eth0 -n -q", at);
    assert!(out.contains("udhcpc: bound eth0 10.0.2.15/255.255.255.0 router 10.0.2.2"), "{out}");
    let (at, _) = r.command("printf '#!/bin/sh\\nsleep 2\\n' > /tmp/dorme; chmod +x /tmp/dorme", at);
    let (at, _) = r.command("printf '#!/bin/sh\\necho ciao\\n' > /tmp/saluta; chmod +x /tmp/saluta", at);

    // 1. Il copione del confronto con QEMU: un messaggio breve, eco.
    r.m.console_input(format!("{ECHO_SERVER}\n").as_bytes());
    r.until(LISTENING, at);
    let mut t = r.m.net(|s| Transfer::new(s, MESSAGE.to_vec(), MESSAGE.len())).unwrap();
    r.pump("eco breve", |s| t.turn(s));
    assert_eq!(r.state(t.id), HostConnState::Closed(CloseReason::Normal));
    let (at, out) = {
        let at = r.until(SHELL_PROMPT, at);
        (at, normalize(&String::from_utf8_lossy(&r.log[..at])))
    };
    let connect_line = connect_line(&out);
    let echo = t.got.clone();
    assert_eq!(echo, MESSAGE);

    // 2. Eco di 200 KB.
    r.m.console_input(format!("{ECHO_SERVER}\n").as_bytes());
    r.until(LISTENING, at);
    let data = payload();
    let mut t = r.m.net(|s| Transfer::new(s, data.clone(), data.len())).unwrap();
    r.pump("eco di 200 KB", |s| t.turn(s));
    assert_eq!(t.got.len(), data.len());
    assert!(t.got == data, "eco di 200 KB diversa");
    assert_eq!(r.state(t.id), HostConnState::Closed(CloseReason::Normal));
    let big = t.id;
    let at = r.until(SHELL_PROMPT, at);

    // 3. Porta senza servizio: RST del guest.
    let refused = r.m.net(|s| s.host_connect(5556).unwrap()).unwrap();
    r.pump("rifiuto", |s| matches!(s.host_conn(refused).unwrap().state, HostConnState::Closed(_)));
    assert_eq!(r.state(refused), HostConnState::Closed(CloseReason::Refused));

    // 4. Il servizio scrive e chiude per primo.
    r.m.console_input(b"nc -n -v -l -p 5555 -e /tmp/saluta\n");
    r.until(LISTENING, at);
    let mut t = r.m.net(|s| Transfer::new(s, Vec::new(), usize::MAX)).unwrap();
    r.pump("chiusura dal guest", |s| t.turn(s));
    assert_eq!(t.got, b"ciao\n");
    assert_eq!(r.state(t.id), HostConnState::Closed(CloseReason::Normal));
    let at = r.until(SHELL_PROMPT, at);

    // 5. Reset dall'host a connessione aperta.
    r.m.console_input(format!("{ECHO_SERVER}\n").as_bytes());
    r.until(LISTENING, at);
    let mut t = r.m.net(|s| Transfer::new(s, b"x".to_vec(), usize::MAX)).unwrap();
    r.pump("eco prima del reset", |s| {
        t.turn(s);
        t.got == b"x"
    });
    r.m.net(|s| s.host_abort(t.id)).unwrap();
    r.pump("reset dall'host", |s| matches!(s.host_conn(t.id).unwrap().state, HostConnState::Closed(_)));
    assert_eq!(r.state(t.id), HostConnState::Closed(CloseReason::RemoteReset));
    let at = r.until(SHELL_PROMPT, at);

    // 6. Reset dal guest: il servizio esce senza leggere i dati arrivati.
    r.m.console_input(b"nc -n -v -l -p 5555 -e /tmp/dorme\n");
    r.until(LISTENING, at);
    let mut t = r.m.net(|s| Transfer::new(s, b"non letti".to_vec(), usize::MAX)).unwrap();
    r.pump("reset dal guest", |s| {
        t.turn(s);
        matches!(s.host_conn(t.id).unwrap().state, HostConnState::Closed(_))
    });
    assert_eq!(r.state(t.id), HostConnState::Closed(CloseReason::GuestReset));
    let at = r.until(SHELL_PROMPT, at);
    let (_, out) = r.command("echo \"V\"IVO=$((6*7))", at);
    assert!(out.contains("VIVO=42"), "{out}");

    r.m.console_input(b"poweroff -f\n");
    let limit = r.m.steps + PHASE_BUDGET;
    let stop = loop {
        let s = r.m.run(1_000_000);
        r.log.extend(r.m.console_output());
        if s != Stop::Budget || r.m.steps >= limit {
            break s;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "{}", r.tail());

    let events =
        r.m.net_view(|s| {
            let st = s.stats();
            assert_eq!((st.bad_checksum, st.malformed), (0, 0), "{st:?}");
            let ev: Vec<&EventKind> = s.events().iter().map(|e| &e.kind).collect();
            let connects = ev.iter().filter(|k| matches!(k, EventKind::TcpConnect { .. })).count();
            assert_eq!(connects, 6, "{ev:#?}");
            assert!(ev.iter().any(|k| matches!(k, EventKind::TcpClosed { id, reason: CloseReason::Normal,
                bytes_to_remote, bytes_to_guest } if *id == big
                    && *bytes_to_remote == 200_000 && *bytes_to_guest == 200_000)));
            s.events().iter().map(|e| format!("{e}\n")).collect::<String>()
        })
        .unwrap();
    Session { log: normalize(&String::from_utf8_lossy(&r.log)), steps: r.m.steps, events, connect_line, echo }
}

#[test]
fn inoltro_di_porte_verso_il_guest() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "inoltro di porte sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image_path, initrd_path) = (image, initrd);
    let (image, initrd) = (std::fs::read(&image_path).unwrap(), std::fs::read(&initrd_path).unwrap());
    let a = session(&image, &initrd);
    let dir = repo_root().join("target/guest-kernel");
    std::fs::write(dir.join("vetro-hostfwd.log"), &a.log).unwrap();
    std::fs::write(dir.join("vetro-hostfwd-events.log"), &a.events).unwrap();
    eprintln!("Vetro: {} ({} istruzioni)", a.connect_line, a.steps);
    assert_eq!(
        a.connect_line,
        "listening on 0.0.0.0:5555 ... | connect to 10.0.2.15:5555 from 10.0.2.2:PORTA (10.0.2.2:PORTA)"
    );
    let b = session(&image, &initrd);
    assert_eq!(a.steps, b.steps, "istruzioni diverse fra due esecuzioni uguali");
    assert!(a.log == b.log, "log diversi fra due esecuzioni uguali");
    assert!(a.events == b.events, "registri di rete diversi fra due esecuzioni uguali");
    assert_eq!(a.echo, MESSAGE);

    // Confronto con QEMU, se c'è: stessa riga di connessione, stessa eco.
    match qemu_hostfwd(&image_path, &initrd_path) {
        Some((line, echo)) => {
            eprintln!("QEMU:  {line}");
            assert_eq!(line, a.connect_line, "il guest vede la connessione come sotto QEMU");
            assert_eq!(echo, a.echo, "eco come sotto QEMU");
        }
        None => skip_or_fail(
            "VETRO_REQUIRE_SYSTEM_ORACLE",
            "qemu-system-aarch64 assente: confronto dell'inoltro di porte con QEMU saltato",
        ),
    }
}

/// Lo stesso copione sotto QEMU con `hostfwd`; `None` senza oracolo.
fn qemu_hostfwd(image: &std::path::Path, initrd: &std::path::Path) -> Option<(String, Vec<u8>)> {
    let qemu = qemu_system()?;
    let docker = qemu.file_name().is_some_and(|n| n.to_string_lossy().contains("docker"));
    let host_port = if docker {
        15555
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    };
    let name = format!("vetro-hostfwd-{}", std::process::id());
    let mut args: Vec<String> = QEMU_MACHINE.iter().map(|s| s.to_string()).collect();
    let i = args.iter().position(|a| a == "user,id=n").unwrap();
    args[i] = format!("user,id=n,hostfwd=tcp:127.0.0.1:{host_port}-:{PORT}");
    let mut cmd = Command::new(&qemu);
    cmd.env("VETRO_ORACLE_NAME", &name)
        .args(&args)
        .args(["-nographic", "-kernel"])
        .arg(image)
        .arg("-initrd")
        .arg(initrd)
        .args(["-append", "console=ttyAMA0 vetro.noautotest"]);
    let limit = timeout();
    let mut con = Console::spawn(cmd).expect("avvio di qemu-system-aarch64");
    let fail = |con: &Console, what: &str| -> ! {
        let log = normalize(&con.log());
        let tail: Vec<&str> = log.lines().rev().take(40).collect();
        panic!("QEMU: {what}:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    };
    let at = con.wait_for(SHELL_PROMPT, 0, limit).unwrap_or_else(|| fail(&con, "nessun prompt"));
    con.send("udhcpc -i eth0 -n -q\n");
    let at = con.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| fail(&con, "udhcpc"));
    con.send(&format!("{ECHO_SERVER}\n"));
    con.wait_for(LISTENING, at, limit).unwrap_or_else(|| fail(&con, "nc non in ascolto"));
    let echo = if docker {
        // Da localhost dentro il container, come un client sull'host.
        let script = format!(
            "exec 3<>/dev/tcp/127.0.0.1/{host_port} && printf '%s' \"$1\" >&3 && IFS= read -r l <&3 && printf '%s\\n' \"$l\""
        );
        let out = Command::new("docker")
            .args(["exec", &name, "bash", "-c", &script, "eco"])
            .arg(String::from_utf8_lossy(MESSAGE).as_ref())
            .output()
            .expect("docker exec");
        assert!(out.status.success(), "client nel container: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    } else {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", host_port)).expect("connessione a hostfwd");
        s.set_read_timeout(Some(limit)).unwrap();
        s.write_all(MESSAGE).unwrap();
        let mut got = vec![0u8; MESSAGE.len()];
        s.read_exact(&mut got).expect("eco da QEMU");
        got
    };
    let end = con.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| fail(&con, "nc non è uscito"));
    let out = normalize(&con.log()[..end]);
    con.send("poweroff -f\n");
    con.finish(Duration::from_secs(30));
    std::fs::write(repo_root().join("target/guest-kernel/qemu-hostfwd.log"), normalize(&con.log())).unwrap();
    Some((connect_line(&out), echo))
}
