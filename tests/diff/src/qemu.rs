//! Esecuzione dell'oracolo `qemu-aarch64` (QEMU in user mode).
//!
//! Il binario si sceglie così, in ordine:
//! 1. variabile `VETRO_QEMU_AARCH64` (percorso o wrapper, es.
//!    `tools/oracle/qemu-aarch64-docker.sh` su macOS);
//! 2. `qemu-aarch64` nel PATH.
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

pub fn locate() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VETRO_QEMU_AARCH64") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("qemu-aarch64"))
        .find(|p| p.is_file())
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
    let mut child = Command::new(qemu)
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
