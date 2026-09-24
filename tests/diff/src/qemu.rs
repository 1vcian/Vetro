//! Esecuzione dell'oracolo `qemu-aarch64` (QEMU in user mode).
//!
//! Il binario si sceglie così, in ordine:
//! 1. variabile `VETRO_QEMU_AARCH64` (percorso o wrapper, es.
//!    `tools/oracle/qemu-aarch64-docker.sh` su macOS);
//! 2. `qemu-aarch64` nel PATH.
//!
//! QEMU emula sempre la CPU di [`cpu`] (default `cortex-a53`, ADR 0005).
//!
//! Se l'oracolo manca, [`locate`] restituisce `None`: i test lo segnalano e
//! si saltano, a meno che `VETRO_REQUIRE_ORACLE=1` (impostato in CI), nel
//! qual caso falliscono.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Outcome {
    /// Codice di uscita del programma guest; `None` se terminato da segnale.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Outcome {
    /// Segnale che ha terminato il guest, letto dal messaggio di QEMU
    /// ("uncaught target signal N"): vale sia in nativo sia via Docker.
    pub fn signal(&self) -> Option<i32> {
        let err = String::from_utf8_lossy(&self.stderr);
        let rest = err.split("uncaught target signal ").nth(1)?;
        rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    }
}

/// Modello di CPU passato a QEMU (`VETRO_QEMU_CPU`, default `cortex-a53`).
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

/// `true` se la CI (o l'utente) esige che l'oracolo sia presente.
pub fn required() -> bool {
    std::env::var("VETRO_REQUIRE_ORACLE").is_ok_and(|v| v == "1")
}

/// Restituisce l'oracolo, oppure `None` dopo aver stampato il motivo dello
/// skip. Va in panic se l'oracolo è obbligatorio ma assente.
pub fn locate_or_skip(test: &str) -> Option<PathBuf> {
    match locate() {
        Some(p) => Some(p),
        None if required() => {
            panic!("{test}: qemu-aarch64 non trovato e VETRO_REQUIRE_ORACLE=1")
        }
        None => {
            eprintln!(
                "SKIP {test}: qemu-aarch64 non trovato (imposta VETRO_QEMU_AARCH64 \
                 o installa qemu-user; su macOS vedi tools/oracle/README.md)"
            );
            None
        }
    }
}

/// Esegue `elf` sotto QEMU con timeout.
pub fn run(qemu: &Path, elf: &Path, timeout: Duration) -> std::io::Result<Outcome> {
    // Niente core dump: dopo un SIGILL del guest QEMU ne scriverebbe uno,
    // lentissimo, invece di uscire.
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

    // Lettura in thread separati per non bloccarsi su pipe piene.
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
                format!("qemu-aarch64 oltre {timeout:?} su {}", elf.display()),
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

/// Scrive `image` in un file eseguibile temporaneo e restituisce il percorso.
/// Il file resta in `target/` (o nella tmp di sistema) per poterlo ispezionare.
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

/// Esecuzione di un programma Linux con argomenti, ambiente, directory di
/// lavoro e stdin controllati. L'ambiente del guest è esattamente `env`
/// (passato con `-E`, su un processo QEMU senza variabili ereditate).
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

/// Come [`run_program`], con opzioni di QEMU in più (es. `-L sysroot`).
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
    cmd.arg("-c").arg("ulimit -c 0; exec \"$@\"").arg("sh").arg(qemu).arg("-cpu").arg(cpu()).args(qemu_opts);
    // QEMU passa al guest il proprio ambiente: toglie le variabili che
    // teniamo solo per il processo QEMU (e il wrapper Docker).
    for k in ["PATH", "HOME"] {
        if !env.iter().any(|(n, _)| n == k) {
            cmd.arg("-U").arg(k);
        }
    }
    for (k, v) in env {
        cmd.arg("-E").arg(format!("{k}={v}"));
    }
    cmd.arg(prog).args(args);
    // Il wrapper Docker ha bisogno di PATH per trovare docker.
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
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("qemu oltre {timeout:?}")));
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
