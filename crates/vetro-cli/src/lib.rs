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
