//! I dispositivi virtio di M5 sotto Vetro, esercitati dal guest e dall'host
//! insieme: ciò che il confronto con QEMU (`vetro.rs`) non può coprire,
//! perché serve l'host dall'altra parte.
//!
//! - virtio-gpu: `vetro-dev drm-hold` disegna un motivo noto su un dumb
//!   buffer, fa il modeset, ridisegna un rettangolo (DIRTYFB) e definisce il
//!   cursore; l'host confronta ogni pixel dello scanout e il cursore;
//! - virtio-input: l'host inietta tasti e movimenti del tablet mentre
//!   `vetro-dev input-read` legge da evdev; il guest accende un LED e l'host
//!   lo vede dalla coda di stato;
//! - virtio-vsock: il guest si collega all'host (porta 1234) e riceve una
//!   risposta più grande del suo buffer (credito); l'host si collega al
//!   guest (porta 5000) e riceve l'eco in maiuscolo;
//! - determinismo: due esecuzioni danno lo stesso log e lo stesso numero di
//!   istruzioni.
//!
//! Solo in release, come `vetro.rs`.

use vetro_boot_tests::*;
use vetro_machine::{Devices, Machine, MachineConfig, Stop};
use vetro_platform::virtio::input::{BTN_LEFT, EV_ABS, EV_KEY, LED_CAPSL};
use vetro_platform::virtio::{MemDisplay, VsockConn, VsockState};

const PHASE_BUDGET: u64 = 6_000_000_000;
const GUEST_CID: u64 = 3;

struct Run {
    m: Machine,
    log: Vec<u8>,
}

impl Run {
    /// Esegue finché `needle` compare nel log dopo `from`, chiamando `host`
    /// fra un quanto e l'altro.
    fn until_with(&mut self, needle: &str, from: usize, mut host: impl FnMut(&mut Machine)) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            let stop = self.m.run(1_000_000);
            self.log.extend(self.m.console_output());
            host(&mut self.m);
            assert!(matches!(stop, Stop::Budget), "{stop:?} in attesa di {needle:?}:\n{}", self.tail());
        }
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        self.until_with(needle, from, |_| {})
    }

    /// Manda un comando alla shell e aspetta il prompt dopo di esso.
    fn command(&mut self, cmd: &str, from: usize) -> usize {
        self.m.console_input(format!("{cmd}\n").as_bytes());
        self.until(SHELL_PROMPT, from)
    }

    fn text(&self, from: usize) -> String {
        normalize(&String::from_utf8_lossy(&self.log[from..]))
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

/// Il motivo di `vetro-dev drm` (XRGB8888), in RGBA.
fn expected_pixel(x: u32, y: u32) -> [u8; 4] {
    if (32..96).contains(&x) && (16..48).contains(&y) {
        return [255, 255, 255, 255];
    }
    [x as u8, y as u8, (x ^ y) as u8, 255]
}

/// La somma di `vetro-dev vsock-connect` sulla risposta.
fn guest_sum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |s, &b| s.wrapping_mul(31).wrapping_add(u32::from(b)))
}

/// Un avvio completo con tutti gli esercizi; restituisce log e istruzioni.
fn session(image: &[u8], initrd: &[u8]) -> (String, u64) {
    let devices = Devices { vsock_cid: Some(GUEST_CID), ..Devices::default() };
    let mut m = Machine::with_devices(&MachineConfig::default(), &devices);
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    m.vsock(|v| v.listen(1234).unwrap()).expect("vsock montato");
    let mut r = Run { m, log: Vec::new() };
    let mut at = r.until(SHELL_PROMPT, 0);

    // ---- virtio-gpu ------------------------------------------------------
    r.m.console_input(b"vetro-dev drm-hold\n");
    let ready = r.until("VETRO-DRM-PRONTO", at);
    let (w, h, bad) =
        r.m.gpu(|g| {
            let d = g.backend_as::<MemDisplay>().unwrap();
            let (w, h, _) = d.screens[&0];
            let bad = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter(|&(x, y)| d.pixel(0, x, y) != Some(expected_pixel(x, y)))
                .count();
            (w, h, bad)
        })
        .expect("gpu montata");
    assert_eq!((w, h, bad), (1280, 800, 0), "scanout diverso dal motivo del guest");
    let cursor = r.m.gpu(|g| g.cursor(0).unwrap().clone()).unwrap();
    assert_ne!(cursor.resource_id, 0);
    assert_eq!((cursor.x, cursor.y, cursor.hot_x, cursor.hot_y), (100, 50, 0, 0));
    assert_eq!(cursor.image.len(), 64 * 64 * 4);
    // ARGB8888 0xff000000 | i, in memoria B G R A.
    assert_eq!(&cursor.image[4 * 65..4 * 66], &[65, 0, 0, 255]);
    r.m.console_input(b"\n");
    at = r.until(SHELL_PROMPT, ready);
    assert!(r.text(ready).contains("vetro-dev: drm chiuso"));
    // Chiuso il file, il kernel toglie il framebuffer: lo scanout si spegne.
    let on = r.m.gpu(|g| g.frame(0).is_some()).unwrap();
    assert!(!on, "scanout ancora acceso dopo la chiusura");

    // ---- virtio-input ----------------------------------------------------
    // event0 = tablet (slot più basso fra i due), event1 = tastiera.
    r.m.console_input(b"vetro-dev input-read /dev/input/event1 4\n");
    let ready = r.until("VETRO-INPUT-PRONTO", at);
    r.m.keyboard(|k| {
        k.key(30, true); // KEY_A
        k.key(30, false);
    });
    at = r.until(SHELL_PROMPT, ready);
    let out = r.text(ready);
    let events: Vec<&str> = out.lines().filter(|l| l.starts_with("vetro-dev: evento")).collect();
    assert_eq!(
        events,
        [
            "vetro-dev: evento 1 30 1",
            "vetro-dev: evento 0 0 0",
            "vetro-dev: evento 1 30 0",
            "vetro-dev: evento 0 0 0"
        ]
    );
    r.m.console_input(b"vetro-dev input-read /dev/input/event0 5\n");
    let ready = r.until("VETRO-INPUT-PRONTO", at);
    r.m.pointer(|p| {
        p.move_abs(0x1234, 0x7000);
        p.key(BTN_LEFT, true);
    });
    at = r.until(SHELL_PROMPT, ready);
    let out = r.text(ready);
    let events: Vec<String> =
        out.lines().filter(|l| l.starts_with("vetro-dev: evento")).map(String::from).collect();
    let e = |t: u16, c: u16, v: u32| format!("vetro-dev: evento {t} {c} {v}");
    assert_eq!(
        events,
        [e(EV_ABS, 0, 0x1234), e(EV_ABS, 1, 0x7000), e(0, 0, 0), e(EV_KEY, BTN_LEFT, 1), e(0, 0, 0)]
    );
    at = r.command(&format!("vetro-dev led /dev/input/event1 {LED_CAPSL} 1"), at);
    assert_eq!(r.m.keyboard(|k| k.leds()), Some(1 << LED_CAPSL));

    // ---- virtio-vsock: il guest si collega all'host ------------------------
    at = r.command("vetro-dev vsock-cid", at);
    assert!(r.text(0).contains(&format!("vetro-dev: vsock cid {GUEST_CID}")));
    let reply: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut conn: Option<VsockConn> = None;
    let mut got = Vec::new();
    let mut replied = false;
    r.m.console_input(b"vetro-dev vsock-connect 1234 ciao-vetro\n");
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
    at = r.until(SHELL_PROMPT, done);
    assert_eq!(got, b"ciao-vetro");
    let line = format!("vetro-dev: vsock risposta {} byte, somma {:08x}", reply.len(), guest_sum(&reply));
    assert!(r.text(0).contains(&line), "manca {line:?}:\n{}", r.tail());
    let c = conn.unwrap();
    assert_eq!(c.host_port, 1234);

    // ---- virtio-vsock: l'host si collega al guest --------------------------
    r.m.console_input(b"vetro-dev vsock-listen 5000\n");
    let ready = r.until("VETRO-VSOCK-ASCOLTO 5000", at);
    let data: Vec<u8> = (0..200_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let c =
        r.m.vsock(|v| {
            let c = v.connect(5000);
            v.send(c, &data).unwrap();
            v.shutdown_send(c);
            c
        })
        .unwrap();
    let mut echo = Vec::new();
    let done = r.until_with("vsock rimandati", ready, |m| {
        m.vsock(|v| echo.extend(v.recv(c, usize::MAX)));
    });
    at = r.until_with(SHELL_PROMPT, done, |m| {
        m.vsock(|v| echo.extend(v.recv(c, usize::MAX)));
    });
    // Il guest ha chiuso: gli ultimi byte e la chiusura arrivano entro poco.
    for _ in 0..50 {
        if r.m.vsock(|v| v.eof(c)).unwrap() {
            break;
        }
        r.m.run(1_000_000);
        r.log.extend(r.m.console_output());
        r.m.vsock(|v| echo.extend(v.recv(c, usize::MAX)));
    }
    assert_eq!(echo, data.to_ascii_uppercase());
    assert!(r.text(ready).contains(&format!("vetro-dev: vsock accettato da 2:{}", c.host_port)));
    assert!(r.text(ready).contains(&format!("vetro-dev: vsock rimandati {} byte", data.len())));
    assert_eq!(r.m.vsock(|v| v.state(c)), Some(Some(VsockState::Closed)));

    assert!(!r.text(0).contains("vetro-dev: ERRORE"), "errori nel guest:\n{}", r.tail());
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
    let _ = at;
    (r.text(0), r.m.steps)
}

#[test]
fn dispositivi_virtio_con_l_host() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "dispositivi sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let (log, steps) = session(&image, &initrd);
    std::fs::write(repo_root().join("target/guest-kernel/vetro-devices.log"), &log).unwrap();
    eprintln!("Vetro: dispositivi esercitati in {steps} istruzioni");
    let (log2, steps2) = session(&image, &initrd);
    assert_eq!(steps, steps2, "istruzioni diverse fra due esecuzioni uguali");
    assert!(log == log2, "log diversi fra due esecuzioni uguali");
}
