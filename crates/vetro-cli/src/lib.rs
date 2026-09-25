//! Esecuzione nativa di programmi Linux arm64 statici in user mode.
//!
//! [`linux`] emula il kernel: processi, thread, file, memoria, segnali e
//! tempo virtuale. È il banco di prova della CPU prima dell'avvio del kernel
//! vero (M3) e il motore dei test differenziali contro QEMU.

pub mod boot;
pub mod elf;
pub mod linux;

use linux::{Config, Exit, Kernel};

/// Risultato di un'esecuzione completa.
pub struct Outcome {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Esegue un ELF con gli argomenti e l'ambiente dati.
/// Porta il limite soft dei descrittori dell'host al massimo consentito: ogni
/// file aperto dal guest è un descrittore dell'host, e il guest deve arrivare
/// al proprio RLIMIT_NOFILE (EMFILE) prima che l'host finisca i suoi.
/// Restituisce il limite soft di prima.
pub fn raise_fd_limit() -> u64 {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let before;
    // SAFETY: `r` è una struct rlimit valida per get/setrlimit.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) != 0 {
            return 1024;
        }
        before = r.rlim_cur;
        if r.rlim_cur < r.rlim_max {
            // macOS rifiuta valori oltre OPEN_MAX anche con hard illimitato.
            r.rlim_cur = r.rlim_max.min(1 << 20);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &r) != 0 {
                r.rlim_cur = 10240;
                libc::setrlimit(libc::RLIMIT_NOFILE, &r);
            }
        }
    }
    before
}

pub fn run_elf(
    image: &[u8],
    argv: &[&str],
    envp: &[&str],
    exe: &str,
    cfg: Config,
) -> Result<Outcome, String> {
    let mut k = Kernel::new(cfg);
    let argv: Vec<Vec<u8>> = argv.iter().map(|s| s.as_bytes().to_vec()).collect();
    let envp: Vec<Vec<u8>> = envp.iter().map(|s| s.as_bytes().to_vec()).collect();
    k.spawn(image, &argv, &envp, exe)?;
    let exit = k.run();
    Ok(Outcome { exit, stdout: k.stdout(), stderr: k.stderr() })
}
