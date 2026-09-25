//! `vetro`: runner nativo headless.
//!
//! ```text
//! vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [argomenti...]
//! vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--disk=FILE]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats]
//! ```
//!
//! `boot` avvia la macchina virt (M3) con la console PL011 su stdin/stdout.
//! I dispositivi di default (GPU, tastiera, tablet) occupano gli slot
//! virtio-mmio 31, 30, 29, e la rete (virtio-net con lo stack di `vetro-net`
//! e il sinkhole: DHCP 10.0.2.15, gateway 10.0.2.2, DNS finto 10.0.2.3) il
//! 28; `--no-devices` li toglie tutti, `--no-net` solo la rete, `--net` la
//! rimette anche dopo `--no-devices`. `--net-events` stampa su stderr il
//! registro degli eventi di rete (DHCP, DNS, connessioni, byte, chiusure)
//! man mano che accadono, in tempo virtuale. Ogni `--disk` aggiunge
//! un virtio-blk nello slot libero più alto, nell'ordine della riga di comando
//! (come i `-device virtio-blk-device` di QEMU): il file resta intatto, le
//! scritture del guest restano in memoria (`snapshot=on`). `--guest-secs`
//! ferma la macchina dopo N secondi di tempo del guest.
//! `--jit` esegue col JIT verso WASM (M4, wasmtime; in `boot` il JIT della
//! modalità sistema, ADR 0013, con `--jit-threshold=N` ingressi prima di
//! tradurre un blocco); `--stats` stampa su stderr istruzioni, tempo e MIPS
//! (e i contatori del JIT).

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
    eprintln!(
        "uso: vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [argomenti...]"
    );
    eprintln!(
        "     vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--disk=FILE]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats]"
    );
    ExitCode::from(2)
}

fn run(args: &[String]) -> ExitCode {
    // Le CPU viste dal guest sono fisse (una, deterministico) salvo --cpus=N:
    // non dipendono dalla macchina che esegue.
    let mut cfg = Config { echo: true, ..Config::default() };
    let mut stats = false;
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        match args[i].as_str() {
            "--strace" => cfg.strace = true,
            "--jit" => cfg.jit = true,
            "--stats" => stats = true,
            a if a.starts_with("--jit-threshold=") => match a["--jit-threshold=".len()..].parse::<u32>() {
                Ok(n) => cfg.jit_threshold = n,
                _ => return usage(),
            },
            "--host-clock" => cfg.clock = ClockMode::Host,
            a if a.starts_with("--sysroot=") => cfg.sysroot = Some(a["--sysroot=".len()..].to_string()),
            a if a.starts_with("--cpus=") => match a["--cpus=".len()..].parse::<usize>() {
                Ok(n @ 1..=64) => cfg.cpus = n,
                _ => return usage(),
            },
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
    let t0 = std::time::Instant::now();
    let out = match vetro_cli::run_elf(&image, &argv, &envp, &exe, cfg) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    if stats {
        let s = t0.elapsed().as_secs_f64();
        eprintln!("vetro: {} istruzioni in {s:.3} s = {:.1} MIPS", out.steps, out.steps as f64 / s / 1e6);
        if let Some(j) = out.jit {
            eprintln!("vetro: jit {j:?}");
        }
    }
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
    use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};
    use vetro_platform::virtio::{VirtioBlk, VirtioBlkConfig};
    let (mut kernel, mut initrd, mut append) = (None, None, "console=ttyAMA0".to_string());
    let mut cfg = MachineConfig::default();
    let mut devices = Devices::default();
    let mut disks = Vec::new();
    let mut guest_ns = u64::MAX;
    let mut stats = false;
    let (mut net, mut net_events) = (None, false);
    let (mut jit, mut threshold) = (false, vetro_jit::SysJitConfig::default().hot_threshold);
    for a in args {
        match a.as_str() {
            "--no-devices" => {
                devices = Devices::none();
                continue;
            }
            "--jit" => {
                jit = true;
                continue;
            }
            "--net" => {
                net = Some(true);
                continue;
            }
            "--no-net" => {
                net = Some(false);
                continue;
            }
            "--net-events" => {
                net_events = true;
                continue;
            }
            "--stats" => {
                stats = true;
                continue;
            }
            _ => {}
        }
        match a.split_once('=') {
            Some(("--jit-threshold", v)) => match v.parse::<u32>() {
                Ok(n) => threshold = n,
                Err(_) => return usage(),
            },
            Some(("--disk", v)) => disks.push(v.to_string()),
            Some(("--guest-secs", v)) => match v.parse::<u64>() {
                Ok(s) => guest_ns = s.saturating_mul(1_000_000_000),
                Err(_) => return usage(),
            },
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
    match net {
        Some(true) if devices.net.is_none() => devices.net = Some(NetSetup::default()),
        Some(false) => devices.net = None,
        _ => {}
    }
    let mut m = Machine::with_devices(&cfg, &devices);
    for d in &disks {
        let backend = match vetro_cli::disk::cow_disk(std::path::Path::new(d)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vetro: {d}: {e}");
                return ExitCode::from(2);
            }
        };
        let blk = VirtioBlk::new(Box::new(backend), VirtioBlkConfig::default());
        if m.board.borrow_mut().virt.attach_virtio_next(Box::new(blk)).is_err() {
            eprintln!("vetro: troppi dispositivi virtio");
            return ExitCode::from(2);
        }
    }
    if let Err(e) = m.load_linux(&image, initrd.as_deref(), &append) {
        eprintln!("vetro: {kernel}: {e}");
        return ExitCode::from(2);
    }
    if jit {
        m.set_jit(Some(vetro_jit_native::system_jit(threshold)));
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
    let t0 = std::time::Instant::now();
    let report = |m: &Machine| {
        if stats {
            let s = t0.elapsed().as_secs_f64();
            eprintln!(
                "vetro: {} istruzioni ({:.3} s di guest) in {s:.3} s = {:.1} MIPS",
                m.steps,
                m.guest_ns() as f64 / 1e9,
                m.steps as f64 / s / 1e6
            );
            if let Some(j) = m.jit_stats() {
                eprintln!("vetro: jit {j:?}");
            }
        }
    };
    // Eventi di rete già stampati (il registro si legge senza toccarlo:
    // l'esecuzione non cambia con --net-events).
    let mut net_seen = 0usize;
    let mut print_net = |m: &Machine| {
        if !net_events {
            return;
        }
        m.net_view(|s| {
            for e in &s.events()[net_seen..] {
                eprintln!("vetro-net: {e}");
            }
            net_seen = s.events().len();
        });
    };
    loop {
        if m.guest_ns() >= guest_ns {
            report(&m);
            eprintln!("vetro: raggiunto il limite di tempo del guest");
            return ExitCode::from(124);
        }
        let stop = m.run(2_000_000);
        print_net(&m);
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
            Stop::PowerOff => {
                report(&m);
                return ExitCode::SUCCESS;
            }
            Stop::Reset => {
                report(&m);
                eprintln!("vetro: il guest ha chiesto un reset");
                return ExitCode::SUCCESS;
            }
            Stop::Blocked => {
                // I dischi da file sono sempre pronti: non succede.
                eprintln!("vetro: un disco non ha dati pronti");
                return ExitCode::from(2);
            }
            Stop::Unimplemented { pc, raw, what } => {
                report(&m);
                eprintln!("vetro: {raw:#010x} non ancora implementata ({what}) a pc={pc:#x}");
                return ExitCode::from(125);
            }
        }
    }
}
