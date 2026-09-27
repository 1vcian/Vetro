//! Running the `qemu-aarch64` oracle (QEMU in user mode).
//!
//! The binary is chosen like this, in order:
//! 1. variable `VETRO_QEMU_AARCH64` (path or wrapper, e.g.
//!    `tools/oracle/qemu-aarch64-docker.sh` on macOS);
//! 2. `qemu-aarch64` in the PATH.
//!
//! QEMU always emulates the CPU of [`cpu`] (default `cortex-a53`, ADR 0005).
//!
//! If the oracle is missing, [`locate`] returns `None`: the tests report it and
//! are skipped, unless `VETRO_REQUIRE_ORACLE=1` (set in CI), in
//! which case they fail.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Outcome {
    /// Exit code of the guest program; `None` if terminated by a signal.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    /// Signal that terminated the guest, read from QEMU's message
    /// ("uncaught target signal N"): works both natively and via Docker.
    pub fn signal(&self) -> Option<i32> {
        let err = String::from_utf8_lossy(&self.stderr);
        // The last message: the children die before the main process.
        let rest = err
            .rsplit("uncaught target signal ")
            .next()
            .filter(|_| err.contains("uncaught target signal "))?;
        rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    }
}

/// CPU model passed to QEMU (`VETRO_QEMU_CPU`, default `cortex-a53`).
pub fn cpu() -> String {
    std::env::var("VETRO_QEMU_CPU").unwrap_or_else(|_| "cortex-a53".into())
}

pub fn locate() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VETRO_QEMU_AARCH64") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join("qemu-aarch64")).find(|p| p.is_file())
}

/// `true` if the CI (or the user) requires the oracle to be present.
pub fn required() -> bool {
    std::env::var("VETRO_REQUIRE_ORACLE").is_ok_and(|v| v == "1")
}

/// Returns the oracle, or `None` after printing the reason for the
/// skip. Panics if the oracle is mandatory but absent.
pub fn locate_or_skip(test: &str) -> Option<PathBuf> {
    match locate() {
        Some(p) => Some(p),
        None if required() => {
            panic!("{test}: qemu-aarch64 not found and VETRO_REQUIRE_ORACLE=1")
        }
        None => {
            eprintln!(
                "SKIP {test}: qemu-aarch64 not found (set VETRO_QEMU_AARCH64 \
                 or install qemu-user; on macOS see tools/oracle/README.md)"
            );
            None
        }
    }
}

/// Runs `elf` under QEMU with a timeout.
pub fn run(qemu: &Path, elf: &Path, timeout: Duration) -> std::io::Result<Outcome> {
    // No core dump: after a guest SIGILL QEMU would write one,
    // very slowly, instead of exiting.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("ulimit -c 0; exec \"$@\"")
        .arg("sh")
        .arg(qemu)
        .arg("-cpu")
        .arg(cpu())
        .arg(elf)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Reading in separate threads so as not to block on full pipes.
    let mut out = child.stdout.take().expect("stdout piped");
    let mut err = child.stderr.take().expect("stderr piped");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        out.read_to_end(&mut b).map(|_| b)
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        err.read_to_end(&mut b).map(|_| b)
    });

    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("qemu-aarch64 over {timeout:?} on {}", elf.display()),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    Ok(Outcome {
        exit_code: status.code(),
        stdout: t_out.join().expect("thread stdout")?,
        stderr: t_err.join().expect("thread stderr")?,
    })
}

/// Writes `image` to a temporary executable file and returns the path.
/// The file stays in `target/` (or in the system tmp) so it can be inspected.
pub fn write_temp_elf(name: &str, image: &[u8]) -> std::io::Result<PathBuf> {
    let dir = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("vetro-oracle");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, image)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

/// Running a Linux program with controlled arguments, environment, working
/// directory and stdin. The guest's environment is exactly `env`
/// (passed with `-E`, on a QEMU process without inherited variables).
pub fn run_program(
    qemu: &Path,
    prog: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    stdin: &[u8],
    timeout: Duration,
) -> std::io::Result<Outcome> {
    run_program_with(qemu, &[], prog, args, env, cwd, stdin, timeout)
}

/// Like [`run_program`], with extra QEMU options (e.g. `-L sysroot`).
#[allow(clippy::too_many_arguments)]
pub fn run_program_with(
    qemu: &Path,
    qemu_opts: &[String],
    prog: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    stdin: &[u8],
    timeout: Duration,
) -> std::io::Result<Outcome> {
    use std::io::Write;
    let mut cmd = Command::new("sh");
    // VETRO_ORACLE_NOFILE: soft limit of descriptors to give back to QEMU if
    // whoever launches the oracle has raised its own (vetro_cli::raise_fd_limit).
    let nofile = std::env::var("VETRO_ORACLE_NOFILE")
        .ok()
        .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
        .map(|n| format!("ulimit -S -n {n}; "))
        .unwrap_or_default();
    cmd.arg("-c")
        .arg(format!("ulimit -c 0; {nofile}exec \"$@\""))
        .arg("sh")
        .arg(qemu)
        .arg("-cpu")
        .arg(cpu())
        .args(qemu_opts);
    // QEMU passes its own environment to the guest: remove the variables that
    // we keep only for the QEMU process (and the Docker wrapper).
    for k in ["PATH", "HOME"] {
        if !env.iter().any(|(n, _)| n == k) {
            cmd.arg("-U").arg(k);
        }
    }
    for (k, v) in env {
        cmd.arg("-E").arg(format!("{k}={v}"));
    }
    cmd.arg(prog).args(args);
    // The Docker wrapper needs PATH to find docker.
    cmd.env_clear();
    if let Some(p) = std::env::var_os("PATH") {
        cmd.env("PATH", p);
    }
    if let Some(h) = std::env::var_os("HOME") {
        cmd.env("HOME", h);
    }
    for k in ["VETRO_ORACLE_IMAGE", "VETRO_ORACLE_MOUNTS", "DOCKER_HOST"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    let mut child =
        cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut sin = child.stdin.take().expect("stdin piped");
    let data = stdin.to_vec();
    let t_in = std::thread::spawn(move || {
        let _ = sin.write_all(&data);
    });
    let mut out = child.stdout.take().expect("stdout piped");
    let mut err = child.stderr.take().expect("stderr piped");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        out.read_to_end(&mut b).map(|_| b)
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        err.read_to_end(&mut b).map(|_| b)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("qemu over {timeout:?}")));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let _ = t_in.join();
    Ok(Outcome {
        exit_code: status.code(),
        stdout: t_out.join().expect("thread stdout")?,
        stderr: t_err.join().expect("thread stderr")?,
    })
}
