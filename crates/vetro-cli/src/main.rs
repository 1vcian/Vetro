//! `vetro`: runner nativo headless.
//!
//! ```text
//! vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [argomenti...]
//! vetro boot (--kernel=Image [--initrd=FILE] | --boot-img=FILE [--vendor-boot=FILE] [--init-boot=FILE] [--recovery] [--android-dump=DIR]) [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORTA-:PORTA_GUEST]... [--pcap=FILE] [--har=FILE] [--net-requests] [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=ISTRUZIONI:FILE]... [--save-on=TESTO:FILE [--save-delay=S] [--exit-after-save]] [--restore=FILE] [--record=FILE [--keyframes=N]] [--replay=FILE [--goto=ISTRUZIONE [--dump=VA:BYTE]]] [--vsock] [--files-ls=PERCORSO]... [--files-cat=PERCORSO]... [--files-put=PERCORSO:FILE]... [--kernel-profile=FILE [--system-map=FILE] [--kernel-btf=FILE]] [--tls] [--binder-log=FILE]
//! ```
//!
//! `boot` avvia la macchina virt (M3) con la console PL011 su stdin/stdout.
//! Le opzioni con un valore si scrivono `--opzione=valore` o `--opzione
//! valore`.
//!
//! Immagini Android (M5, `docs/specs/android-boot.md`): invece di `--kernel`
//! e `--initrd`, `--boot-img` (header v0–v4) con `--vendor-boot` (v3/v4) e
//! `--init-boot` facoltativi. Il bootloader di Vetro
//! (`vetro_machine::android`) decomprime il kernel (gzip, LZ4), concatena i
//! ramdisk del vendor (senza quelli di recovery, salvo `--recovery`) e il
//! ramdisk generico, compone la riga di comando (boot, vendor, poi `--append`)
//! e, con `vendor_boot` v4, mette gli `androidboot.*` di `--append` nel
//! blocco bootconfig in coda all'initrd. `--android-dump=DIR` scrive in DIR
//! `Image`, `initrd` e `cmdline` come li riceve il kernel: gli stessi file
//! vanno a `qemu-system-aarch64 -kernel -initrd -append`.
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
//! da 10.0.2.2 (vedi `vetro_cli::hostfwd`). `--pcap=FILE` (o `--pcap FILE`)
//! scrive a fine esecuzione i frame Ethernet visti da virtio-net in pcapng,
//! con il tempo virtuale del guest; `--har=FILE` le richieste HTTP
//! ricostruite in HAR 1.2; `--net-requests` stampa su stderr la lista
//! dell'ispettore di rete (M7, ADR 0016). Ogni `--disk` aggiunge
//! un virtio-blk nello slot libero più alto, nell'ordine della riga di comando
//! (come i `-device virtio-blk-device` di QEMU): il file resta intatto, le
//! scritture del guest restano in memoria (`snapshot=on`). `--overlay=FILE`
//! dopo un `--disk` le conserva in FILE (creato se manca; formato di
//! `vetro_snapshot::overlay`, lo stesso del browser, ADR 0017) e al
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
//! (una WFI può saltare oltre N), e continua; si può ripetere.
//! `--save-on=TESTO:FILE` salva quando TESTO compare sulla console (per
//! Android `sys.boot_completed=1`), dopo altri `--save-delay=S` secondi di
//! guest; con `--exit-after-save` poi esce (codice 0). `--restore=FILE`
//! riparte da uno snapshot invece che dal kernel (`--kernel` non serve):
//! RAM, dispositivi e opzioni (`--mem`, dispositivi, `--disk` con gli stessi
//! file) devono essere quelli della macchina salvata, altrimenti lo snapshot
//! si rifiuta. I file dei dischi sono collegamenti: il loro contenuto non
//! entra nello snapshot, le scritture del guest (copy-on-write) sì.
//!
//! Record & replay (M10, ADR 0019): `--record=FILE` registra ogni ingresso
//! dell'host (console da stdin, connessioni di `--hostfwd`) con il numero
//! d'istruzione, più uno snapshot ogni `--keyframes=N` istruzioni (default
//! 100 milioni, 0 = nessuno), e scrive il log all'uscita (spegnimento,
//! reset, `--guest-secs`, stdin chiuso). `--replay=FILE` rifà la sessione
//! registrata: stessi dispositivi e dischi, RAM, ora e seme dal log; parte
//! da `--kernel` (stessi `--initrd`/`--append`) o da `--restore`, come la
//! registrazione, oppure, senza nessuno dei due, dal primo keyframe del log.
//! Stdin non conta; alla fine confronta console, istruzioni, CPU, RAM e
//! dispositivi con la registrazione e dice se il replay è identico (codice
//! 0) o dove diverge (codice 1). `--goto=N` va all'istruzione N (dal
//! keyframe più vicino) e stampa i registri; `--dump=VA:BYTE` aggiunge i
//! byte della memoria virtuale a quell'indirizzo (tabelle correnti).
//!
//! Gestore dei file (M8, ADR 0020): `--vsock` monta virtio-vsock (CID 3),
//! su cui `/init` avvia il demone `vetro-files`. `--files-ls=PERCORSO`,
//! `--files-cat=PERCORSO` e `--files-put=PERCORSO_GUEST:FILE_HOST`
//! (ripetibili, implicano `--vsock`) eseguono nell'ordine le operazioni
//! appena il demone risponde e scrivono i risultati su stdout; finite
//! tutte, `vetro` esce con 0 se sono riuscite, 1 altrimenti (vedi
//! `vetro_cli::files`).
//!
//! Analisi dall'esterno (M7/M8, ADR 0027, `vetro_cli::analysis`): serve il
//! profilo del kernel (`--kernel-profile=FILE`: un `boot.img` di Android o
//! un `Image`, con `--system-map`/`--kernel-btf` per il kernel di prova;
//! senza, si usa `--boot-img`/`--kernel`). `--tls` aggancia
//! `SSL_write`/`SSL_read` di `libssl` (BoringSSL, anche Conscrypt): le
//! richieste HTTPS in chiaro finiscono nell'HAR (`--har`) e nella lista
//! (`--net-requests`) come quelle in chiaro, legate a processo e libreria.
//! `--binder-log=FILE` scrive le chiamate Binder decodificate (interfaccia
//! e metodo AIDL, mittente e destinatario) in JSON (`.json`) o in righe di
//! testo, e stampa su stderr gli accessi sensibili (ispettore privacy).

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
        "     vetro boot (--kernel=Image [--initrd=FILE] | --boot-img=FILE [--vendor-boot=FILE] [--init-boot=FILE] [--recovery] [--android-dump=DIR]) [--append=RIGA] [--mem=MiB] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORTA-:PORTA_GUEST]... [--pcap=FILE] [--har=FILE] [--net-requests] [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=ISTRUZIONI:FILE]... [--save-on=TESTO:FILE [--save-delay=S] [--exit-after-save]] [--restore=FILE] [--record=FILE [--keyframes=N]] [--replay=FILE [--goto=ISTRUZIONE [--dump=VA:BYTE]]] [--vsock] [--files-ls=PERCORSO]... [--files-cat=PERCORSO]... [--files-put=PERCORSO:FILE]... [--kernel-profile=FILE [--system-map=FILE] [--kernel-btf=FILE]] [--tls] [--binder-log=FILE]"
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
    use vetro_cli::files::{FileCmd, FilesTask};
    use vetro_cli::hostfwd::{HostFwd, Input};
    use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};
    use vetro_platform::virtio::{CowBackend, VirtioBlk, VirtioBlkConfig};
    let (mut kernel, mut initrd, mut append) = (None, None, None);
    let (mut boot_img, mut vendor_boot, mut init_boot) = (None, None, None);
    let (mut recovery, mut android_dump) = (false, None::<String>);
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
    let mut save_on: Option<(String, String)> = None;
    let (mut save_delay_ns, mut exit_after_save) = (0u64, false);
    let mut restore = None;
    let (mut record, mut replay, mut goto, mut dump) = (None, None, None, None);
    let mut keyframes = 100_000_000u64;
    let mut capture = vetro_cli::netcap::NetCapture::default();
    let mut vsock = false;
    let mut file_cmds: Vec<FileCmd> = Vec::new();
    let mut analysis = vetro_cli::analysis::AnalysisOptions::default();
    for a in &join_values(&vetro_cli::netcap::join_values(args)) {
        if analysis.parse(a) {
            continue;
        }
        match a.as_str() {
            "--recovery" => {
                recovery = true;
                continue;
            }
            "--net-requests" => {
                capture.requests = true;
                continue;
            }
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
            "--vsock" => {
                vsock = true;
                continue;
            }
            "--exit-after-save" => {
                exit_after_save = true;
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
            Some(("--save-on", v)) => match v.rsplit_once(':') {
                Some((t, f)) if !t.is_empty() && !f.is_empty() => {
                    save_on = Some((t.to_string(), f.to_string()))
                }
                _ => return usage(),
            },
            Some(("--save-delay", v)) => match v.parse::<f64>() {
                Ok(s) if s >= 0.0 => save_delay_ns = (s * 1e9) as u64,
                _ => return usage(),
            },
            Some(("--restore", v)) => restore = Some(v.to_string()),
            Some(("--pcap", v)) => capture.pcap = Some(v.into()),
            Some(("--har", v)) => capture.har = Some(v.into()),
            Some(("--record", v)) => record = Some(v.to_string()),
            Some(("--replay", v)) => replay = Some(v.to_string()),
            Some(("--keyframes", v)) => match v.parse::<u64>() {
                Ok(n) => keyframes = n,
                Err(_) => return usage(),
            },
            Some(("--goto", v)) => match v.parse::<u64>() {
                Ok(n) => goto = Some(n),
                Err(_) => return usage(),
            },
            Some(("--dump", v)) => {
                match v.split_once(':').map(|(a, n)| (parse_addr(a), n.parse::<usize>())) {
                    Some((Some(a), Ok(n))) => dump = Some((a, n)),
                    _ => return usage(),
                }
            }
            Some(("--files-ls", v)) => file_cmds.push(FileCmd::Ls(v.to_string())),
            Some(("--files-cat", v)) => file_cmds.push(FileCmd::Cat(v.to_string())),
            Some(("--files-put", v)) => match vetro_cli::files::parse_put(v) {
                Ok(c) => file_cmds.push(c),
                Err(e) => {
                    eprintln!("vetro: {e}");
                    return ExitCode::from(2);
                }
            },
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
            Some(("--append", v)) => append = Some(v.to_string()),
            Some(("--boot-img", v)) => boot_img = Some(v.to_string()),
            Some(("--vendor-boot", v)) => vendor_boot = Some(v.to_string()),
            Some(("--init-boot", v)) => init_boot = Some(v.to_string()),
            Some(("--android-dump", v)) => android_dump = Some(v.to_string()),
            Some(("--mem", v)) => match v.parse::<u64>() {
                Ok(m) => cfg.ram_size = m << 20,
                Err(_) => return usage(),
            },
            _ => return usage(),
        }
    }
    let android = boot_img.is_some();
    if (kernel.is_none() && !android && restore.is_none() && replay.is_none())
        || (android && (kernel.is_some() || initrd.is_some()))
        || (!android && (vendor_boot.is_some() || init_boot.is_some() || recovery || android_dump.is_some()))
    {
        return usage();
    }
    if (goto.is_some() && replay.is_none()) || (dump.is_some() && goto.is_none()) {
        eprintln!("vetro: --goto richiede --replay, --dump richiede --goto");
        return ExitCode::from(2);
    }
    if record.is_some() && replay.is_some() {
        eprintln!("vetro: --record e --replay insieme non hanno senso");
        return ExitCode::from(2);
    }
    if replay.is_some() && !forwards.is_empty() {
        eprintln!("vetro: in replay la rete dell'host viene dal log: niente --hostfwd");
        return ExitCode::from(2);
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
    // Immagini Android: il bootloader prepara kernel, initrd e riga di comando.
    let android = match boot_img.as_deref().filter(|_| restore.is_none()) {
        None => None,
        Some(path) => {
            let mut files = Vec::new();
            for p in [Some(path), vendor_boot.as_deref(), init_boot.as_deref()] {
                files.push(match p.map(read).transpose() {
                    Ok(f) => f,
                    Err(c) => return c,
                });
            }
            let opts =
                vetro_machine::android::BootOptions { params: append.clone().unwrap_or_default(), recovery };
            let boot = files[0].as_deref().unwrap_or_default();
            match vetro_machine::android::AndroidBoot::from_images(
                boot,
                files[1].as_deref(),
                files[2].as_deref(),
                &opts,
            ) {
                Ok(a) => {
                    eprintln!(
                        "vetro: kernel {} ({} byte), ramdisk: {}{}",
                        a.kernel_format,
                        a.kernel.len(),
                        if a.ramdisks.is_empty() { "nessuno".to_string() } else { a.ramdisks.join(", ") },
                        if a.bootconfig.is_empty() {
                            String::new()
                        } else {
                            format!(", bootconfig {} byte", a.bootconfig.len())
                        }
                    );
                    if let Some(dir) = &android_dump
                        && let Err(e) = dump_android(std::path::Path::new(dir), &a)
                    {
                        eprintln!("vetro: {dir}: {e}");
                        return ExitCode::from(2);
                    }
                    Some(a)
                }
                Err(e) => {
                    eprintln!("vetro: {e}");
                    return ExitCode::from(2);
                }
            }
        }
    };
    let append = append.unwrap_or_else(|| "console=ttyAMA0".to_string());
    let log = match replay.as_deref().map(read).transpose() {
        Ok(None) => None,
        Ok(Some(b)) => match vetro_machine::Log::decode(&b) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("vetro: {}: {e}", replay.as_deref().unwrap_or_default());
                return ExitCode::from(2);
            }
        },
        Err(c) => return c,
    };
    if let Some(l) = &log {
        // RAM, ora e seme della macchina registrata.
        cfg = l.config.clone();
    }
    match net {
        Some(true) if devices.net.is_none() => devices.net = Some(NetSetup::default()),
        Some(false) => devices.net = None,
        _ => {}
    }
    if vsock || !file_cmds.is_empty() {
        devices.vsock_cid = Some(3);
    }
    if replay.is_some() && !file_cmds.is_empty() {
        eprintln!("vetro: in replay gli ingressi vengono dal log: niente --files-*");
        return ExitCode::from(2);
    }
    let mut files = (!file_cmds.is_empty()).then(|| FilesTask::new(file_cmds));
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
    } else if let Some(a) = &android {
        if let Err(e) = m.load_android(a) {
            eprintln!("vetro: {}: {e}", boot_img.as_deref().unwrap_or_default());
            return ExitCode::from(2);
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
    if capture.wanted() && !m.net_tap(true) {
        eprintln!("vetro: --pcap, --har e --net-requests richiedono la rete (--net)");
        return ExitCode::from(2);
    }
    if let Err(e) = analysis.install(&mut m, boot_img.as_deref().or(kernel.as_deref())) {
        eprintln!("vetro: {e}");
        return ExitCode::from(2);
    }
    if let Some(l) = &log {
        let path = replay.as_deref().unwrap_or_default();
        if let Some(n) = goto {
            return match m.goto(l, n) {
                Ok(_) => {
                    print!("{}", m.registers_text());
                    if let Some((va, len)) = dump {
                        let mut buf = vec![0u8; len];
                        match m.read_virt(va, &mut buf) {
                            Ok(()) => print!("{}", hex_dump(va, &buf)),
                            Err(at) => {
                                eprintln!("vetro: {at:#x} non è mappato");
                                return ExitCode::from(1);
                            }
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(d) => {
                    eprintln!("vetro: {path}: {d}");
                    ExitCode::from(1)
                }
            };
        }
        // Senza kernel né snapshot si parte dal keyframe iniziale del log.
        let start = if image.is_none() && snapshot.is_none() && android.is_none() {
            m.replay_from(l, l.start.steps)
        } else {
            m.start_replay(l)
        };
        if let Err(d) = start {
            eprintln!("vetro: {path}: {d}");
            return ExitCode::from(1);
        }
        eprintln!(
            "vetro: replay di {path}: {} eventi, da {} a {} istruzioni",
            l.events.len(),
            m.steps,
            l.end.steps
        );
    }
    if record.is_some() {
        m.start_recording(vetro_machine::RecordOptions { keyframe_every: keyframes });
    }
    // stdin in un thread, i socket di --hostfwd nei loro: tutto arriva su
    // un canale e passa alla macchina tra un quanto e l'altro.
    let (tx, rx) = std::sync::mpsc::channel::<Input>();
    let stdin_tx = tx.clone();
    // In replay gli ingressi vengono dal log: stdin non si legge.
    let replaying = log.is_some();
    std::thread::spawn(move || {
        if replaying {
            let _ = stdin_tx.send(Input::ConsoleClosed);
            return;
        }
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
        if let Some(p) = m.jit_profile() {
            eprintln!("vetro: {}vetro: chiamate a env.simd: {}", p.report(40), vetro_jit::helper::calls());
            if let Some(r) = vetro_jit::helper::profile_report(20) {
                eprint!("vetro: env.simd, {r}");
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
    let (mut console_tail, mut save_on_at) = (Vec::<u8>::new(), None::<u64>);
    let code = loop {
        if let Err(c) = persist(&m, &mut overlays) {
            break c;
        }
        if m.guest_ns() >= guest_ns {
            report(&m);
            eprintln!("vetro: raggiunto il limite di tempo del guest");
            break ExitCode::from(124);
        }
        // Un quanto non supera il prossimo salvataggio.
        let budget = save_at.last().map_or(2_000_000, |s| s.0.saturating_sub(m.steps).clamp(1, 2_000_000));
        let stop = m.run(budget);
        analysis.tls_service(&mut m);
        print_net(&m);
        if capture.wanted() {
            capture.collect(&mut m);
        }
        let o = m.console_output();
        if !o.is_empty() {
            let _ = out.write_all(&o);
            let _ = out.flush();
        }
        // --save-on: il testo sulla console fissa l'istante del salvataggio.
        if let Some((text, _)) = &save_on
            && save_on_at.is_none()
        {
            console_tail.extend_from_slice(&o);
            if console_tail.windows(text.len()).any(|w| w == text.as_bytes()) {
                save_on_at = Some(m.guest_ns().saturating_add(save_delay_ns));
                eprintln!(
                    "vetro: {text:?} sulla console a {:.1} s di guest: snapshot fra {:.1} s di guest",
                    m.guest_ns() as f64 / 1e9,
                    save_delay_ns as f64 / 1e9
                );
            }
            let keep = console_tail.len().saturating_sub(text.len());
            console_tail.drain(..keep);
        }
        if let Some(at) = save_on_at
            && m.guest_ns() >= at
            && let Some((_, path)) = save_on.take()
        {
            let t = std::time::Instant::now();
            let snap = m.save();
            if let Err(e) = std::fs::write(&path, &snap) {
                eprintln!("vetro: {path}: {e}");
                return ExitCode::from(2);
            }
            eprintln!(
                "vetro: snapshot a {} istruzioni ({:.1} s di guest, {:.0} s reali) in {path} ({} byte, salvato in {:.1} s)",
                m.steps,
                m.guest_ns() as f64 / 1e9,
                t0.elapsed().as_secs_f64(),
                snap.len(),
                t.elapsed().as_secs_f64()
            );
            if exit_after_save {
                report(&m);
                break ExitCode::SUCCESS;
            }
        }
        match m.replay_status() {
            Some(vetro_machine::ReplayStatus::Finished) if log.is_some() => {
                report(&m);
                eprintln!("vetro: replay identico alla registrazione ({} istruzioni)", m.steps);
                break ExitCode::SUCCESS;
            }
            Some(vetro_machine::ReplayStatus::Diverged(d)) if log.is_some() => {
                report(&m);
                eprintln!("vetro: replay diverso dalla registrazione: {d}");
                break ExitCode::from(1);
            }
            _ => {}
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
        if let Some(t) = files.as_mut()
            && let Some(ok) = t.step(&mut m, &mut out)
        {
            report(&m);
            break if ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
        }
        match stop {
            Stop::Budget => {}
            // Il gestore dei file ha richieste per il guest: si continua.
            Stop::Idle if files.is_some() => {}
            Stop::Idle => {
                // Niente da fare per il guest: si aspetta un ingresso
                // dell'host (console o rete).
                let waiting = console_open || fwd.is_some();
                match rx.recv() {
                    Ok(i) if waiting => handle(&mut m, i, &mut console_open, &mut fwd),
                    _ => {
                        eprintln!("vetro: il guest aspetta un ingresso e stdin è chiuso");
                        break ExitCode::from(3);
                    }
                }
            }
            Stop::PowerOff => {
                report(&m);
                if let Err(c) = persist(&m, &mut overlays) {
                    break c;
                }
                break ExitCode::SUCCESS;
            }
            Stop::Reset => {
                report(&m);
                if let Err(c) = persist(&m, &mut overlays) {
                    return c;
                }
                eprintln!("vetro: il guest ha chiesto un reset");
                break ExitCode::SUCCESS;
            }
            Stop::Blocked => {
                // I dischi da file sono sempre pronti: non succede.
                eprintln!("vetro: un disco non ha dati pronti");
                break ExitCode::from(2);
            }
            Stop::Unimplemented { pc, raw, what } => {
                report(&m);
                eprintln!("vetro: {raw:#010x} non ancora implementata ({what}) a pc={pc:#x}");
                break ExitCode::from(125);
            }
        }
    };
    match analysis.finish(&mut m) {
        Ok(lines) => lines.iter().for_each(|l| eprintln!("vetro: {l}")),
        Err(e) => {
            eprintln!("vetro: {e}");
            return ExitCode::from(2);
        }
    }
    if capture.wanted() {
        capture.collect(&mut m);
        capture.set_tls(analysis.tls_conversations(&mut m));
        match capture.finish() {
            Ok(lines) => lines.iter().for_each(|l| eprintln!("vetro: {l}")),
            Err(e) => {
                eprintln!("vetro: {e}");
                return ExitCode::from(2);
            }
        }
    }
    if let Some(path) = &record
        && let Some(l) = m.stop_recording()
    {
        let bytes = l.encode();
        if let Err(e) = std::fs::write(path, &bytes) {
            eprintln!("vetro: {path}: {e}");
            return ExitCode::from(2);
        }
        eprintln!(
            "vetro: registrazione in {path}: {} eventi, {} keyframe, {} istruzioni, {} byte ({} senza keyframe)",
            l.events.len(),
            l.keyframes.len(),
            l.end.steps,
            bytes.len(),
            l.events_len()
        );
    }
    code
}

/// Opzioni di `boot` che vogliono un valore: `--opzione valore` diventa
/// `--opzione=valore`.
const BOOT_VALUE_OPTIONS: &[&str] = &[
    "--kernel",
    "--initrd",
    "--append",
    "--mem",
    "--disk",
    "--hostfwd",
    "--guest-secs",
    "--jit-threshold",
    "--save-at",
    "--save-on",
    "--save-delay",
    "--restore",
    "--overlay",
    "--boot-img",
    "--vendor-boot",
    "--init-boot",
    "--android-dump",
    "--record",
    "--keyframes",
    "--replay",
    "--goto",
    "--dump",
    "--files-ls",
    "--files-cat",
    "--files-put",
    "--kernel-profile",
    "--system-map",
    "--kernel-btf",
    "--binder-log",
];

fn join_values(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match it.as_slice().first() {
            Some(v) if BOOT_VALUE_OPTIONS.contains(&a.as_str()) => {
                out.push(format!("{a}={v}"));
                it.next();
            }
            _ => out.push(a.clone()),
        }
    }
    out
}

/// `Image`, `initrd` e `cmdline` come li riceve il kernel.
fn dump_android(dir: &std::path::Path, a: &vetro_machine::android::AndroidBoot) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("Image"), &a.kernel)?;
    std::fs::write(dir.join("initrd"), &a.initrd)?;
    std::fs::write(dir.join("cmdline"), format!("{}\n", a.cmdline))?;
    eprintln!("vetro: Image, initrd e cmdline in {}", dir.display());
    Ok(())
}

/// Un indirizzo in esadecimale (`0x...`) o in decimale.
fn parse_addr(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Byte in righe da 16: indirizzo, esadecimale, ASCII.
fn hex_dump(va: u64, bytes: &[u8]) -> String {
    let mut t = String::new();
    for (i, row) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String =
            row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        t.push_str(&format!("{:016x}  {:<47}  {ascii}\n", va + 16 * i as u64, hex.join(" ")));
    }
    t
}
