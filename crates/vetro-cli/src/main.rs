//! `vetro`: runner nativo headless.
//!
//! ```text
//! vetro run <elf>     esegue un ELF statico arm64 in user mode
//! ```

use std::process::ExitCode;
use vetro_cli::user::{Exit, Process};

const MAX_STEPS: u64 = 10_000_000_000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version" | "-V") => {
            println!("vetro {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("run") if args.len() >= 3 => run(&args[2]),
        _ => {
            eprintln!("uso: vetro run <elf>");
            ExitCode::from(2)
        }
    }
}

fn run(path: &str) -> ExitCode {
    let image = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let mut p = match Process::load(&image, path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    p.echo = true;
    match p.run(MAX_STEPS) {
        Exit::Code(c) => ExitCode::from(c as u8),
        Exit::Signal { signo, cause, pc } => {
            eprintln!("vetro: segnale {signo} a pc={pc:#x}: {cause:?}");
            ExitCode::from(128 + signo as u8)
        }
        Exit::UnsupportedSyscall { nr, pc } => {
            eprintln!("vetro: syscall {nr} non ancora implementata (pc={pc:#x})");
            ExitCode::from(125)
        }
        Exit::StepLimit => {
            eprintln!("vetro: limite di {MAX_STEPS} istruzioni raggiunto");
            ExitCode::from(124)
        }
    }
}
