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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_and_normalize() {
        assert_eq!(find(b"abcVETRO", b"VETRO"), Some(3));
        assert_eq!(find(b"abc", b"VETRO"), None);
        assert_eq!(normalize("a\r\nb\rc"), "a\nb\nc");
    }
}
