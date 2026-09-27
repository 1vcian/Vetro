//! Boot of the M3 guest kernel with the console on stdio, markers and time
//! limits.
//!
//! The kernel and the initramfs come from `tools/guest-kernel/build.sh`
//! (`target/guest-kernel`). The oracle is `qemu-system-aarch64`: native on
//! Linux, `tools/guest-kernel/qemu-system-aarch64-docker.sh` on macOS
//! (variable `VETRO_QEMU_SYSTEM_AARCH64`).
//!
//! Variables:
//! - `VETRO_REQUIRE_ORACLE=1`: without the oracle the test fails instead of
//!   being skipped;
//! - `VETRO_REQUIRE_GUEST_KERNEL=1`: likewise if `target/guest-kernel` is missing;
//! - `VETRO_BOOT_TIMEOUT`: seconds granted to each phase (default 180);
//! - `VETRO_BOOT_UPDATE_REFERENCE=1`: rewrites the reference log in
//!   `guest/kernel/reference/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Printed by `/init` as soon as it starts (guest/kernel/initramfs/init).
pub const BOOT_MARKER: &str = "VETRO-BOOT-OK";
/// End of a successful autotest (guest/kernel/initramfs/autotest.sh).
pub const AUTOTEST_OK: &str = "VETRO-AUTOTEST-FINE: ok";
/// End of the autotest in general (also with errors).
pub const AUTOTEST_END: &str = "VETRO-AUTOTEST-FINE";
/// The ash (BusyBox) prompt ready to read: after `# ` the line editor
/// asks for the cursor position (`ESC[6n`), and only then the terminal is
/// in raw mode and ash does the echo. Input is sent after this sequence,
/// under QEMU as under Vetro. Sent earlier (native qemu-system-aarch64,
/// CI), ash finds the input already waiting, skips `ESC[6n` and the kernel
/// echoes it in canonical mode: the log depends on the host's timing.
pub const SHELL_PROMPT: &str = "# \x1b[6n";

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn env_is_1(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// Skips the test (with `SKIP`) or fails if the variable `require` is 1.
pub fn skip_or_fail(require: &str, msg: &str) {
    if env_is_1(require) {
        panic!("{msg} ({require}=1)");
    }
    eprintln!("SKIP: {msg}");
}

/// `Image` and `initramfs.cpio.gz`, if built.
pub fn guest_kernel() -> Option<(PathBuf, PathBuf)> {
    let dir = repo_root().join("target/guest-kernel");
    let (image, initrd) = (dir.join("Image"), dir.join("initramfs.cpio.gz"));
    (image.is_file() && initrd.is_file()).then_some((image, initrd))
}

/// The `qemu-system-aarch64` command: variable or PATH.
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

/// A running guest machine with the console on stdin/stdout.
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

    /// Everything the console has printed so far.
    pub fn log(&self) -> String {
        String::from_utf8_lossy(&self.log).into_owned()
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Waits for `needle` to appear after position `from` of the log;
    /// returns the position right after it, or `None` when the time runs out
    /// or at the end of the output.
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

    /// Like [`Console::wait_for`], but also waits for the end of the line that
    /// contains `needle`: the serial arrives in pieces, and the line
    /// `VETRO-AUTOTEST-FINE: ok` may arrive split after the marker.
    /// Returns the position after the `\n` and the line from `needle` on,
    /// without trailing `\r`/`\n`.
    pub fn wait_line(&mut self, needle: &str, from: usize, limit: Duration) -> Option<(usize, String)> {
        let deadline = Instant::now() + limit;
        let at = self.wait_for(needle, from, limit)?;
        let left = deadline.saturating_duration_since(Instant::now());
        let end = self.wait_for("\n", at, left)?;
        let line = String::from_utf8_lossy(&self.log[at - needle.len()..end]);
        Some((end, line.trim_end_matches(['\r', '\n']).to_string()))
    }

    /// Writes to the guest's console (as from the keyboard).
    pub fn send(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin chiuso");
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Waits for the process to end; when the time runs out it terminates it (first SIGTERM,
    /// which the Docker wrapper forwards to QEMU, then SIGKILL). `true` if it exited
    /// by itself.
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

/// Normalises a boot log for the comparison with the reference: removes the
/// serial's `\r` (the lines stay those of the guest).
pub fn normalize(log: &str) -> String {
    log.replace("\r\n", "\n").replace('\r', "\n")
}

/// `qemu-system-aarch64` options for the same machine as Vetro: virt
/// with GICv3 without ITS (Vetro has no LPIs), Cortex-A53, 1 GiB, without the
/// PCI network card that QEMU adds by itself (Vetro has no PCI, and its ROM
/// `efi-virtio.rom` isn't on the runners without ipxe-qemu).
///
/// The virtio devices are those of `vetro_machine::Devices::default`,
/// in the same order (hence in the same slots): GPU, keyboard, tablet,
/// network. The network is QEMU's user network (slirp), with the same addresses as the
/// `vetro-net` gateway (10.0.2.15/.2/.3) and the same guest MAC; the
/// same outcome holds only for DHCP and ping to the gateway (the autotest): DNS and TCP
/// in QEMU go out on the real network, in Vetro they go to the sinkhole
/// (`tests/boot/tests/net.rs`).
/// `force-legacy=false` because Vetro's virtio-mmio is version 2
/// (virtio 1.x); by default QEMU presents the legacy version 1, and the GPU
/// and input drivers (which want VIRTIO_F_VERSION_1) would not start.
/// No vsock: `vhost-vsock-device` wants the host's `/dev/vhost-vsock`, which
/// neither Docker Desktop nor the runners have; virtio-vsock is tested only under
/// Vetro (`tests/boot/tests/devices.rs`).
pub const QEMU_MACHINE: [&str; 20] = [
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
    "-netdev",
    "user,id=n",
    "-device",
    "virtio-net-device,netdev=n",
];

/// Known differences between the boot under QEMU and under Vetro, with the reason.
/// A line that contains one of these texts is ignored in the comparison.
pub const KNOWN_DIFFERENCES: &[(&str, &str)] = &[
    // QEMU's GICv3 declares LPIs (GICD_TYPER.LPIS) even with its=off;
    // Vetro's doesn't, and Linux prints nothing.
    ("ITS: No ITS available", "QEMU declares LPIs without ITS"),
    // Vetro doesn't execute AArch32 (ADR 0005): ID_AA64PFR0_EL1.EL0 = 1.
    ("CPU features: detected: 32-bit EL0 Support", "no AArch32 in Vetro (ADR 0005)"),
];

/// Lines whose numeric contents depend on the size of QEMU's device tree,
/// which also describes devices that Vetro doesn't have (PCIe, fw-cfg,
/// flash, GPIO): a few KiB more of reserved memory. They are compared without
/// the numbers.
pub const MEMORY_LINES: &[&str] = &["Memory: ", "rootfs on / type rootfs", "devtmpfs on /dev type devtmpfs"];

/// Lines of a log ready for the comparison: without kernel timestamps, without the
/// known differences, without the numbers of the memory lines, in alphabetical
/// order (the order of some asynchronous initcalls depends on the host's real
/// timing under QEMU).
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

/// Removes `[    1.234567] ` at the head of a kernel line.
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

/// Differences between two sets of lines (`-` only in the first, `+` only in the
/// second), for the test messages.
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
        // The marker arrives before the rest of the line, as from the PL011
        // under native qemu-system-aarch64 (CI): the line must be read whole.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'x\\r\\nVETRO-AUTOTEST-FINE'; sleep 0.5; printf ': ok\\r\\nresto'"]);
        let mut con = Console::spawn(cmd).unwrap();
        // The earlier check (marker, then `contains`) sees the line halfway.
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
