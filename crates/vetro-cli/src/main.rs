//! `vetro`: runner nativo headless.
//!
//! ```text
//! vetro run [--strace] [--host-clock] [--sysroot=DIR] <elf> [argomenti...]
//! vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB]
//! ```
//!
//! `boot` avvia la macchina virt (M3) con la console PL011 su stdin/stdout.

use std::process::ExitCode;
use vetro_cli::linux::{ClockMode, Config, Exit};

fn main() -> ExitCode {
    let _ = vetro_cli::raise_fd_limit();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version" | "-V") => {
            println!("vetro {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("run") => run(&args[2..]),
        Some("boot") => boot(&args[2..]),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("uso: vetro run [--strace] [--host-clock] [--sysroot=DIR] <elf> [argomenti...]");
    eprintln!("     vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB]");
    ExitCode::from(2)
}

fn run(args: &[String]) -> ExitCode {
    let mut cfg = Config {
        echo: true,
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        ..Config::default()
    };
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        match args[i].as_str() {
            "--strace" => cfg.strace = true,
            "--host-clock" => cfg.clock = ClockMode::Host,
            a if a.starts_with("--sysroot=") => cfg.sysroot = Some(a["--sysroot=".len()..].to_string()),
            _ => return usage(),
        }
        i += 1;
    }
    let Some(path) = args.get(i) else { return usage() };
    let image = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let argv: Vec<&str> = args[i..].iter().map(String::as_str).collect();
    let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let envp: Vec<&str> = env.iter().map(String::as_str).collect();
    let exe = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.clone());
    let out = match vetro_cli::run_elf(&image, &argv, &envp, &exe, cfg) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    match out.exit {
        Exit::Code(c) => ExitCode::from(c as u8),
        Exit::Signal { signo, cause, pc } => {
            match cause {
                Some(c) => eprintln!("vetro: segnale {signo} a pc={pc:#x}: {c:?}"),
                None => eprintln!("vetro: terminato dal segnale {signo}"),
            }
            ExitCode::from(128 + signo as u8)
        }
        Exit::UnsupportedSyscall { nr, pc } => {
            let name = vetro_analysis::syscall::name(nr);
            eprintln!("vetro: syscall {nr} ({name}) non ancora implementata (pc={pc:#x})");
            ExitCode::from(125)
        }
        Exit::Unimplemented { raw, what, pc } => {
            eprintln!("vetro: istruzione {raw:#010x} non ancora implementata ({what}) a pc={pc:#x}");
            ExitCode::from(126)
        }
        Exit::StepLimit => {
            eprintln!("vetro: limite di istruzioni raggiunto");
            ExitCode::from(124)
        }
        Exit::Deadlock => {
            eprintln!("vetro: tutti i processi bloccati");
            ExitCode::from(123)
        }
    }
}

fn boot(args: &[String]) -> ExitCode {
    use std::io::{Read, Write};
    use vetro_machine::{Machine, MachineConfig, Stop};
    let (mut kernel, mut initrd, mut append) = (None, None, "console=ttyAMA0".to_string());
    let mut cfg = MachineConfig::default();
    for a in args {
        match a.split_once('=') {
            Some(("--kernel", v)) => kernel = Some(v.to_string()),
            Some(("--initrd", v)) => initrd = Some(v.to_string()),
            Some(("--append", v)) => append = v.to_string(),
            Some(("--mem", v)) => match v.parse::<u64>() {
                Ok(m) => cfg.ram_size = m << 20,
                Err(_) => return usage(),
            },
            _ => return usage(),
        }
    }
    let Some(kernel) = kernel else { return usage() };
    let read = |p: &str| {
        std::fs::read(p).map_err(|e| {
            eprintln!("vetro: {p}: {e}");
            ExitCode::from(2)
        })
    };
    let image = match read(&kernel) {
        Ok(b) => b,
        Err(c) => return c,
    };
    let initrd = match initrd.as_deref().map(read).transpose() {
        Ok(i) => i,
        Err(c) => return c,
    };
    let mut m = Machine::new(&cfg);
    if let Err(e) = m.load_linux(&image, initrd.as_deref(), &append) {
        eprintln!("vetro: {kernel}: {e}");
        return ExitCode::from(2);
    }
    // stdin in un thread: i byte arrivano alla PL011 tra un quanto e l'altro.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut out = std::io::stdout();
    loop {
        let stop = m.run(2_000_000);
        let o = m.console_output();
        if !o.is_empty() {
            let _ = out.write_all(&o);
            let _ = out.flush();
        }
        while let Ok(b) = rx.try_recv() {
            m.console_input(&b);
        }
        match stop {
            Stop::Budget => {}
            Stop::Idle => match rx.recv() {
                Ok(b) => m.console_input(&b),
                Err(_) => {
                    eprintln!("vetro: il guest aspetta un ingresso e stdin è chiuso");
                    return ExitCode::from(3);
                }
            },
            Stop::PowerOff => return ExitCode::SUCCESS,
            Stop::Reset => {
                eprintln!("vetro: il guest ha chiesto un reset");
                return ExitCode::SUCCESS;
            }
            Stop::Unimplemented { pc, raw, what } => {
                eprintln!("vetro: {raw:#010x} non ancora implementata ({what}) a pc={pc:#x}");
                return ExitCode::from(125);
            }
        }
    }
}
