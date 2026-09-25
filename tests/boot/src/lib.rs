//! Avvio del kernel guest di M3 con console su stdio, marcatori e tempi
//! limite.
//!
//! Il kernel e l'initramfs vengono da `tools/guest-kernel/build.sh`
//! (`target/guest-kernel`). L'oracolo è `qemu-system-aarch64`: nativo su
//! Linux, `tools/guest-kernel/qemu-system-aarch64-docker.sh` su macOS
//! (variabile `VETRO_QEMU_SYSTEM_AARCH64`).
//!
//! Variabili:
//! - `VETRO_REQUIRE_ORACLE=1`: senza oracolo il test fallisce invece di
//!   saltare;
//! - `VETRO_REQUIRE_GUEST_KERNEL=1`: idem se manca `target/guest-kernel`;
//! - `VETRO_BOOT_TIMEOUT`: secondi concessi a ogni fase (default 180);
//! - `VETRO_BOOT_UPDATE_REFERENCE=1`: riscrive il log di riferimento in
//!   `guest/kernel/reference/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Stampato da `/init` appena parte (guest/kernel/initramfs/init).
pub const BOOT_MARKER: &str = "VETRO-BOOT-OK";
/// Fine dell'autotest riuscito (guest/kernel/initramfs/autotest.sh).
pub const AUTOTEST_OK: &str = "VETRO-AUTOTEST-FINE: ok";
/// Fine dell'autotest in generale (anche con errori).
pub const AUTOTEST_END: &str = "VETRO-AUTOTEST-FINE";
/// Il prompt di ash (BusyBox) pronto a leggere: dopo `# ` l'editor di riga
/// chiede la posizione del cursore (`ESC[6n`), e solo allora il terminale è
/// in modo raw e l'eco lo fa ash. L'ingresso si manda dopo questa sequenza,
/// sotto QEMU come sotto Vetro. Mandato prima (qemu-system-aarch64 nativo,
/// CI), ash trova l'ingresso già in attesa, salta `ESC[6n` e il kernel ne fa
/// l'eco in modo canonico: il log dipende dai tempi dell'host.
pub const SHELL_PROMPT: &str = "# \x1b[6n";

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn env_is_1(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// Salta il test (con `SKIP`) o fallisce se la variabile `require` vale 1.
pub fn skip_or_fail(require: &str, msg: &str) {
    if env_is_1(require) {
        panic!("{msg} ({require}=1)");
    }
    eprintln!("SKIP: {msg}");
}

/// `Image` e `initramfs.cpio.gz`, se costruiti.
pub fn guest_kernel() -> Option<(PathBuf, PathBuf)> {
    let dir = repo_root().join("target/guest-kernel");
    let (image, initrd) = (dir.join("Image"), dir.join("initramfs.cpio.gz"));
    (image.is_file() && initrd.is_file()).then_some((image, initrd))
}

/// Il comando di `qemu-system-aarch64`: variabile o PATH.
pub fn qemu_system() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VETRO_QEMU_SYSTEM_AARCH64") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("qemu-system-aarch64")).find(|p| p.is_file())
}

pub fn timeout() -> Duration {
    let secs = std::env::var("VETRO_BOOT_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(180);
    Duration::from_secs(secs)
}

/// Una macchina guest in esecuzione con la console su stdin/stdout.
pub struct Console {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Vec<u8>>,
    log: Vec<u8>,
    start: Instant,
}

impl Console {
    pub fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Console { child, stdin, rx, log: Vec::new(), start: Instant::now() })
    }

    /// Tutto quello che la console ha stampato finora.
    pub fn log(&self) -> String {
        String::from_utf8_lossy(&self.log).into_owned()
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Aspetta che `needle` compaia dopo la posizione `from` del log;
    /// restituisce la posizione subito dopo, o `None` allo scadere del tempo
    /// o alla fine dell'uscita.
    pub fn wait_for(&mut self, needle: &str, from: usize, limit: Duration) -> Option<usize> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return Some(from + i + needle.len());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.log.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Come [`Console::wait_for`], ma aspetta anche la fine della riga che
    /// contiene `needle`: la seriale arriva a pezzi, e la riga
    /// `VETRO-AUTOTEST-FINE: ok` può arrivare spezzata dopo il marcatore.
    /// Restituisce la posizione dopo il `\n` e la riga da `needle` in poi,
    /// senza `\r`/`\n` finali.
    pub fn wait_line(&mut self, needle: &str, from: usize, limit: Duration) -> Option<(usize, String)> {
        let deadline = Instant::now() + limit;
        let at = self.wait_for(needle, from, limit)?;
        let left = deadline.saturating_duration_since(Instant::now());
        let end = self.wait_for("\n", at, left)?;
        let line = String::from_utf8_lossy(&self.log[at - needle.len()..end]);
        Some((end, line.trim_end_matches(['\r', '\n']).to_string()))
    }

    /// Scrive sulla console del guest (come dalla tastiera).
    pub fn send(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin chiuso");
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Aspetta la fine del processo; allo scadere lo termina (prima SIGTERM,
    /// che il wrapper Docker inoltra a QEMU, poi SIGKILL). `true` se è uscito
    /// da solo.
    pub fn finish(&mut self, limit: Duration) -> bool {
        self.stdin.take();
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                self.drain();
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
        let term_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < term_deadline {
            if self.child.try_wait().unwrap().is_some() {
                self.drain();
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.drain();
        false
    }

    fn drain(&mut self) {
        while let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(200)) {
            self.log.extend_from_slice(&chunk);
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
            std::thread::sleep(Duration::from_millis(500));
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Normalizza un log di avvio per il confronto con il riferimento: toglie i
/// `\r` della seriale (le righe restano quelle del guest).
pub fn normalize(log: &str) -> String {
    log.replace("\r\n", "\n").replace('\r', "\n")
}

/// Opzioni di `qemu-system-aarch64` per la stessa macchina di Vetro: virt
/// con GICv3 senza ITS (Vetro non ha LPI), Cortex-A53, 1 GiB, senza la
/// scheda di rete PCI che QEMU aggiunge da sé (Vetro non ha PCI, e la sua ROM
/// `efi-virtio.rom` non c'è sui runner senza ipxe-qemu).
///
/// I dispositivi virtio sono quelli di `vetro_machine::Devices::default`,
/// nello stesso ordine (quindi negli stessi slot): GPU, tastiera, tablet.
/// `force-legacy=false` perché il virtio-mmio di Vetro è la versione 2
/// (virtio 1.x); QEMU di default presenta la versione 1 legacy, e i driver
/// di GPU e input (che vogliono VIRTIO_F_VERSION_1) non partirebbero.
/// Niente vsock: `vhost-vsock-device` vuole `/dev/vhost-vsock` dell'host, che
/// né Docker Desktop né i runner hanno; virtio-vsock si prova solo sotto
/// Vetro (`tests/boot/tests/devices.rs`).
pub const QEMU_MACHINE: [&str; 16] = [
    "-M",
    "virt,gic-version=3,its=off",
    "-cpu",
    "cortex-a53",
    "-m",
    "1G",
    "-nic",
    "none",
    "-global",
    "virtio-mmio.force-legacy=false",
    "-device",
    "virtio-gpu-device",
    "-device",
    "virtio-keyboard-device",
    "-device",
    "virtio-tablet-device",
];

/// Differenze note tra l'avvio sotto QEMU e sotto Vetro, con il motivo.
/// Una riga che contiene uno di questi testi si ignora nel confronto.
pub const KNOWN_DIFFERENCES: &[(&str, &str)] = &[
    // Il GICv3 di QEMU dichiara gli LPI (GICD_TYPER.LPIS) anche con its=off;
    // quello di Vetro no, e Linux non stampa nulla.
    ("ITS: No ITS available", "QEMU dichiara gli LPI senza ITS"),
    // Vetro non esegue AArch32 (ADR 0005): ID_AA64PFR0_EL1.EL0 = 1.
    ("CPU features: detected: 32-bit EL0 Support", "niente AArch32 in Vetro (ADR 0005)"),
];

/// Righe il cui contenuto numerico dipende dalla dimensione del device tree
/// di QEMU, che descrive anche dispositivi che Vetro non ha (PCIe, fw-cfg,
/// flash, GPIO): pochi KiB di memoria in più riservati. Si confrontano senza
/// i numeri.
pub const MEMORY_LINES: &[&str] = &["Memory: ", "rootfs on / type rootfs", "devtmpfs on /dev type devtmpfs"];

/// Righe di un log pronte per il confronto: senza tempi del kernel, senza le
/// differenze note, senza i numeri delle righe di memoria, in ordine
/// alfabetico (l'ordine di alcuni initcall asincroni dipende dai tempi reali
/// dell'host sotto QEMU).
pub fn comparable_lines(log: &str) -> Vec<String> {
    let mut v: Vec<String> = normalize(log)
        .lines()
        .map(|l| strip_timestamp(l).trim_end().to_string())
        .filter(|l| !l.is_empty())
        .filter(|l| !KNOWN_DIFFERENCES.iter().any(|(k, _)| l.contains(k)))
        .map(|l| {
            if MEMORY_LINES.iter().any(|m| l.contains(m)) {
                l.chars().filter(|c| !c.is_ascii_digit()).collect()
            } else {
                l
            }
        })
        .collect();
    v.sort();
    v
}

/// Toglie `[    1.234567] ` in testa a una riga del kernel.
fn strip_timestamp(l: &str) -> &str {
    let t = l.trim_start();
    if let Some(rest) = t.strip_prefix('[')
        && let Some((ts, after)) = rest.split_once(']')
        && ts.trim().chars().all(|c| c.is_ascii_digit() || c == '.')
        && !ts.trim().is_empty()
    {
        return after.strip_prefix(' ').unwrap_or(after);
    }
    l
}

/// Differenze tra due insiemi di righe (`-` solo nel primo, `+` solo nel
/// secondo), per i messaggi dei test.
pub fn line_diff(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
            }
            (Some(x), Some(y)) if x < y => {
                out.push(format!("- {x}"));
                i += 1;
            }
            (Some(_), Some(y)) => {
                out.push(format!("+ {y}"));
                j += 1;
            }
            (Some(x), None) => {
                out.push(format!("- {x}"));
                i += 1;
            }
            (None, Some(y)) => {
                out.push(format!("+ {y}"));
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn righe_confrontabili() {
        let log = "[    0.000000] Booting Linux\r\n[    0.1] Memory: 1016024K/1048576K available\n\
                   [    0.2] CPU features: detected: 32-bit EL0 Support\nVETRO-BOOT-OK\n";
        assert_eq!(comparable_lines(log), ["Booting Linux", "Memory: K/K available", "VETRO-BOOT-OK"]);
        let a = vec!["a".to_string(), "c".into()];
        let b = vec!["b".to_string(), "c".into()];
        assert_eq!(line_diff(&a, &b), ["- a", "+ b"]);
    }

    #[test]
    fn riga_spezzata_dalla_seriale() {
        // Il marcatore arriva prima del resto della riga, come dalla PL011
        // sotto qemu-system-aarch64 nativo (CI): la riga va letta intera.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'x\\r\\nVETRO-AUTOTEST-FINE'; sleep 0.5; printf ': ok\\r\\nresto'"]);
        let mut con = Console::spawn(cmd).unwrap();
        // Il controllo di prima (marcatore, poi `contains`) vede la riga a metà.
        assert!(con.wait_for(AUTOTEST_END, 0, Duration::from_secs(10)).is_some());
        assert!(!con.log().contains(AUTOTEST_OK));
        let (end, line) = con.wait_line(AUTOTEST_END, 0, Duration::from_secs(10)).unwrap();
        assert_eq!(line, AUTOTEST_OK);
        assert_eq!(end, "x\r\nVETRO-AUTOTEST-FINE: ok\r\n".len());
        con.finish(Duration::from_secs(10));
    }

    #[test]
    fn find_and_normalize() {
        assert_eq!(find(b"abcVETRO", b"VETRO"), Some(3));
        assert_eq!(find(b"abc", b"VETRO"), None);
        assert_eq!(normalize("a\r\nb\rc"), "a\nb\nc");
    }
}
