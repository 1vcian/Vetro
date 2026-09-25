//! `vetro`: runner nativo headless.
//!
//! ```text
//! vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [argomenti...]
//! vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORTA-:PORTA_GUEST]... [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=ISTRUZIONI:FILE]... [--restore=FILE]
//! ```
//!
//! `boot` avvia la macchina virt (M3) con la console PL011 su stdin/stdout.
//! I dispositivi di default (GPU, tastiera, tablet) occupano gli slot
//! virtio-mmio 31, 30, 29, e la rete (virtio-net con lo stack di `vetro-net`
//! e il sinkhole: DHCP 10.0.2.15, gateway 10.0.2.2, DNS finto 10.0.2.3) il
//! 28; `--no-devices` li toglie tutti, `--no-net` solo la rete, `--net` la
//! rimette anche dopo `--no-devices`. `--net-events` stampa su stderr il
//! registro degli eventi di rete (DHCP, DNS, connessioni, byte, chiusure)
//! man mano che accadono, in tempo virtuale. `--hostfwd` (ripetibile, la
//! sintassi di QEMU) apre un socket in ascolto sull'host (127.0.0.1 se
//! l'indirizzo manca; porta 0 = scelta dal sistema, stampata su stderr) e
//! inoltra ogni connessione a quella porta del guest, che la vede arrivare
//! da 10.0.2.2 (vedi `vetro_cli::hostfwd`). Ogni `--disk` aggiunge
//! un virtio-blk nello slot libero più alto, nell'ordine della riga di comando
//! (come i `-device virtio-blk-device` di QEMU): il file resta intatto, le
//! scritture del guest restano in memoria (`snapshot=on`). `--overlay=FILE`
//! dopo un `--disk` le conserva in FILE (creato se manca; formato di
//! `vetro_snapshot::overlay`, lo stesso del browser, ADR 0016) e al
//! prossimo avvio le riapplica; un overlay fatto su un'altra immagine base
//! (nome, dimensione, data di modifica) si scarta con un avviso. `--guest-secs`
//! ferma la macchina dopo N secondi di tempo del guest.
//! `--jit` esegue col JIT verso WASM (M4, wasmtime; in `boot` il JIT della
//! modalità sistema, ADR 0013, con `--jit-threshold=N` ingressi prima di
//! tradurre un blocco); `--stats` stampa su stderr istruzioni, tempo e MIPS
//! (e i contatori del JIT).
//!
//! Snapshot (M6, ADR 0015): `--save-at=N:FILE` salva la macchina intera in
//! FILE al primo confine fra due quanti con almeno N istruzioni eseguite
//! (una WFI può saltare oltre N), e continua; si può ripetere. `--restore=FILE`
//! riparte da uno snapshot invece che dal kernel (`--kernel` non serve):
//! RAM, dispositivi e opzioni (`--mem`, dispositivi, `--disk` con gli stessi
//! file) devono essere quelli della macchina salvata, altrimenti lo snapshot
//! si rifiuta. I file dei dischi sono collegamenti: il loro contenuto non
//! entra nello snapshot, le scritture del guest (copy-on-write) sì.

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
        "     vetro boot --kernel=Image [--initrd=FILE] [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORTA-:PORTA_GUEST]... [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=ISTRUZIONI:FILE]... [--restore=FILE]"
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
    use vetro_cli::disk::{FileBackend, FileOverlay};
    use vetro_cli::hostfwd::{HostFwd, Input};
    use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};
    use vetro_platform::virtio::{CowBackend, VirtioBlk, VirtioBlkConfig};
    let (mut kernel, mut initrd, mut append) = (None, None, "console=ttyAMA0".to_string());
    let mut cfg = MachineConfig::default();
    let mut devices = Devices::default();
    // (immagine, overlay).
    let mut disks: Vec<(String, Option<String>)> = Vec::new();
    let mut guest_ns = u64::MAX;
    let mut stats = false;
    let (mut net, mut net_events) = (None, false);
    let mut forwards = Vec::new();
    let (mut jit, mut threshold) = (false, vetro_jit::SysJitConfig::default().hot_threshold);
    let mut save_at: Vec<(u64, String)> = Vec::new();
    let mut restore = None;
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
            Some(("--disk", v)) => disks.push((v.to_string(), None)),
            Some(("--overlay", v)) => match disks.last_mut() {
                Some((_, o @ None)) => *o = Some(v.to_string()),
                _ => return usage(),
            },
            Some(("--save-at", v)) => match v.split_once(':').map(|(n, f)| (n.parse::<u64>(), f)) {
                Some((Ok(n), f)) if !f.is_empty() => save_at.push((n, f.to_string())),
                _ => return usage(),
            },
            Some(("--restore", v)) => restore = Some(v.to_string()),
            Some(("--hostfwd", v)) => match vetro_cli::hostfwd::parse_rule(v) {
                Ok(r) => forwards.push(r),
                Err(e) => {
                    eprintln!("vetro: {e}");
                    return ExitCode::from(2);
                }
            },
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
    if kernel.is_none() && restore.is_none() {
        return usage();
    }
    // I salvataggi in ordine di istruzioni, il primo in fondo.
    save_at.sort_by_key(|s| std::cmp::Reverse(s.0));
    let read = |p: &str| {
        std::fs::read(p).map_err(|e| {
            eprintln!("vetro: {p}: {e}");
            ExitCode::from(2)
        })
    };
    let image = match kernel.as_deref().filter(|_| restore.is_none()).map(read).transpose() {
        Ok(b) => b,
        Err(c) => return c,
    };
    let initrd = match initrd.as_deref().filter(|_| restore.is_none()).map(read).transpose() {
        Ok(i) => i,
        Err(c) => return c,
    };
    let snapshot = match restore.as_deref().map(read).transpose() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match net {
        Some(true) if devices.net.is_none() => devices.net = Some(NetSetup::default()),
        Some(false) => devices.net = None,
        _ => {}
    }
    if !forwards.is_empty() && devices.net.is_none() {
        eprintln!("vetro: --hostfwd richiede la rete (--net)");
        return ExitCode::from(2);
    }
    let mut m = Machine::with_devices(&cfg, &devices);
    // Overlay persistenti: (slot del disco, overlay).
    let mut overlays: Vec<(u32, FileOverlay)> = Vec::new();
    for (d, ov) in &disks {
        let path = std::path::Path::new(d);
        let mut backend = match vetro_cli::disk::cow_disk(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vetro: {d}: {e}");
                return ExitCode::from(2);
            }
        };
        let overlay = match ov {
            None => None,
            Some(o) => match vetro_cli::disk::base_identity(path)
                .and_then(|id| FileOverlay::open(std::path::Path::new(o), &id, &mut backend))
            {
                Ok((f, discarded)) => {
                    if let Some(why) = discarded {
                        eprintln!("vetro: {o}: {why}");
                    }
                    Some(f)
                }
                Err(e) => {
                    eprintln!("vetro: {o}: {e}");
                    return ExitCode::from(2);
                }
            },
        };
        let blk = VirtioBlk::new(Box::new(backend), VirtioBlkConfig::default());
        let Ok(slot) = m.board.borrow_mut().virt.attach_virtio_next(Box::new(blk)) else {
            eprintln!("vetro: troppi dispositivi virtio");
            return ExitCode::from(2);
        };
        if let Some(f) = overlay {
            overlays.push((slot, f));
        }
    }
    // Scrive negli overlay i cluster cambiati (fra un quanto e l'altro, e
    // prima di uscire). Letture dell'host: il guest non se ne accorge.
    let persist = |m: &Machine, overlays: &mut Vec<(u32, FileOverlay)>| -> Result<(), ExitCode> {
        for (slot, f) in overlays.iter_mut() {
            let mut b = m.board.borrow_mut();
            let cow = b
                .virt
                .virtio_mut(*slot)
                .and_then(|t| t.device_as_mut::<VirtioBlk>())
                .and_then(|blk| blk.backend_as_mut::<CowBackend<FileBackend>>())
                .expect("disco con overlay");
            if let Err(e) = f.persist(cow) {
                eprintln!("vetro: overlay del disco nello slot {slot}: {e}");
                return Err(ExitCode::from(2));
            }
        }
        Ok(())
    };
    if let Some(snap) = &snapshot {
        let path = restore.as_deref().unwrap_or_default();
        if let Err(e) = m.load_state(snap) {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
        eprintln!("vetro: ripristinato {path} a {} istruzioni", m.steps);
        for (_, f) in overlays.iter_mut() {
            f.after_restore();
        }
    } else if let Some(image) = &image
        && let Err(e) = m.load_linux(image, initrd.as_deref(), &append)
    {
        eprintln!("vetro: {}: {e}", kernel.as_deref().unwrap_or_default());
        return ExitCode::from(2);
    }
    if jit {
        m.set_jit(Some(vetro_jit_native::system_jit(threshold)));
    }
    // stdin in un thread, i socket di --hostfwd nei loro: tutto arriva su
    // un canale e passa alla macchina tra un quanto e l'altro.
    let (tx, rx) = std::sync::mpsc::channel::<Input>();
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || stdin_tx.send(Input::Console(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        let _ = stdin_tx.send(Input::ConsoleClosed);
    });
    let mut fwd = None;
    if !forwards.is_empty() {
        match HostFwd::listen(&forwards, tx.clone()) {
            Ok((f, addrs)) => {
                for (a, r) in addrs.iter().zip(&forwards) {
                    eprintln!("vetro: hostfwd tcp {a} -> 10.0.2.15:{}", r.guest_port);
                }
                fwd = Some(f);
            }
            Err(e) => {
                eprintln!("vetro: --hostfwd: {e}");
                return ExitCode::from(2);
            }
        }
    }
    drop(tx);
    let mut console_open = true;
    let handle =
        |m: &mut Machine, input: Input, console_open: &mut bool, fwd: &mut Option<HostFwd>| match input {
            Input::Console(b) => m.console_input(&b),
            Input::ConsoleClosed => *console_open = false,
            other => {
                if let Some(f) = fwd.as_mut() {
                    f.input(m, other);
                }
            }
        };
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
        if let Err(c) = persist(&m, &mut overlays) {
            return c;
        }
        if m.guest_ns() >= guest_ns {
            report(&m);
            eprintln!("vetro: raggiunto il limite di tempo del guest");
            return ExitCode::from(124);
        }
        // Un quanto non supera il prossimo salvataggio.
        let budget = save_at.last().map_or(2_000_000, |s| s.0.saturating_sub(m.steps).clamp(1, 2_000_000));
        let stop = m.run(budget);
        print_net(&m);
        let o = m.console_output();
        if !o.is_empty() {
            let _ = out.write_all(&o);
            let _ = out.flush();
        }
        while save_at.last().is_some_and(|s| m.steps >= s.0) {
            let (_, path) = save_at.pop().expect("controllato sopra");
            let snap = m.save();
            if let Err(e) = std::fs::write(&path, &snap) {
                eprintln!("vetro: {path}: {e}");
                return ExitCode::from(2);
            }
            eprintln!("vetro: snapshot a {} istruzioni in {path} ({} byte)", m.steps, snap.len());
        }
        while let Ok(i) = rx.try_recv() {
            handle(&mut m, i, &mut console_open, &mut fwd);
        }
        if let Some(f) = fwd.as_mut() {
            f.service(&mut m);
        }
        match stop {
            Stop::Budget => {}
            Stop::Idle => {
                // Niente da fare per il guest: si aspetta un ingresso
                // dell'host (console o rete).
                let waiting = console_open || fwd.is_some();
                match rx.recv() {
                    Ok(i) if waiting => handle(&mut m, i, &mut console_open, &mut fwd),
                    _ => {
                        eprintln!("vetro: il guest aspetta un ingresso e stdin è chiuso");
                        return ExitCode::from(3);
                    }
                }
            }
            Stop::PowerOff => {
                report(&m);
                if let Err(c) = persist(&m, &mut overlays) {
                    return c;
                }
                return ExitCode::SUCCESS;
            }
            Stop::Reset => {
                report(&m);
                if let Err(c) = persist(&m, &mut overlays) {
                    return c;
                }
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
