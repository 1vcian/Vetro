//! `vetro`: headless native runner.
//!
//! ```text
//! vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [args...]
//! vetro boot (--kernel=Image [--initrd=FILE] | --boot-img=FILE [--vendor-boot=FILE] [--init-boot=FILE] [--recovery] [--android-dump=DIR]) [--append=LINE] [--mem=MiB] [--smp=N] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORT-:GUEST_PORT]... [--pcap=FILE] [--har=FILE] [--net-requests] [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=INSTRUCTIONS:FILE]... [--save-on=TEXT:FILE [--save-delay=S] [--exit-after-save]] [--restore=FILE] [--record=FILE [--keyframes=N]] [--replay=FILE [--goto=INSTRUCTION [--dump=VA:BYTES]]] [--vsock] [--files-ls=PATH]... [--files-cat=PATH]... [--files-put=PATH:FILE]... [--kernel-profile=FILE [--system-map=FILE] [--kernel-btf=FILE]] [--tls] [--binder-log=FILE] [--profile=NAME|FILE]
//! ```
//!
//! `boot` starts the virt machine (M3) with the PL011 console on stdin/stdout.
//! Options that take a value are written `--option=value` or `--option
//! value`.
//!
//! Android images (M5, `docs/specs/android-boot.md`): instead of `--kernel`
//! and `--initrd`, `--boot-img` (header v0–v4) with optional `--vendor-boot`
//! (v3/v4) and `--init-boot`. Vetro's bootloader
//! (`vetro_machine::android`) decompresses the kernel (gzip, LZ4), concatenates
//! the vendor ramdisks (without the recovery ones, unless `--recovery`) and the
//! generic ramdisk, builds the command line (boot, vendor, then `--append`)
//! and, with `vendor_boot` v4, puts the `androidboot.*` of `--append` in the
//! bootconfig block at the end of the initrd. `--android-dump=DIR` writes to DIR
//! `Image`, `initrd` and `cmdline` as the kernel receives them: the same files
//! go to `qemu-system-aarch64 -kernel -initrd -append`.
//! The default devices (GPU, keyboard, tablet) occupy virtio-mmio slots
//! 31, 30, 29, and the network (virtio-net with the `vetro-net` stack
//! and the sinkhole: DHCP 10.0.2.15, gateway 10.0.2.2, fake DNS 10.0.2.3)
//! slot 28; `--no-devices` removes them all, `--no-net` only the network, `--net`
//! puts it back even after `--no-devices`. `--net-events` prints to stderr the
//! network event log (DHCP, DNS, connections, bytes, closes)
//! as they happen, in virtual time. `--hostfwd` (repeatable, QEMU's
//! syntax) opens a listening socket on the host (127.0.0.1 if the
//! address is missing; port 0 = chosen by the system, printed on stderr) and
//! forwards every connection to that guest port, which sees it arrive
//! from 10.0.2.2 (see `vetro_cli::hostfwd`). `--pcap=FILE` (or `--pcap FILE`)
//! writes at the end of the run the Ethernet frames seen by virtio-net as pcapng,
//! with the guest's virtual time; `--har=FILE` the HTTP requests
//! reconstructed as HAR 1.2; `--net-requests` prints to stderr the list
//! of the network inspector (M7, ADR 0016). Each `--disk` adds
//! a virtio-blk in the highest free slot, in command-line order
//! (like QEMU's `-device virtio-blk-device`): the file stays intact, the
//! guest's writes stay in memory (`snapshot=on`). `--overlay=FILE`
//! after a `--disk` keeps them in FILE (created if missing; format of
//! `vetro_snapshot::overlay`, the same as the browser's, ADR 0017) and on the
//! next boot reapplies them; an overlay made on a different base image
//! (name, size, modification date) is discarded with a warning. `--guest-secs`
//! stops the machine after N seconds of guest time.
//! `--jit` runs with the JIT to WASM (M4, wasmtime; in `boot` the
//! system-mode JIT, ADR 0013, with `--jit-threshold=N` entries before
//! translating a block); `--stats` prints to stderr instructions, time and MIPS
//! (and the JIT counters).
//!
//! Snapshots (M6, ADR 0015): `--save-at=N:FILE` saves the whole machine to
//! FILE at the first boundary between two quanta with at least N instructions
//! executed (a WFI can jump past N), and continues; it can be repeated.
//! `--save-on=TEXT:FILE` saves when TEXT appears on the console (for
//! Android `sys.boot_completed=1`), after a further `--save-delay=S` seconds of
//! guest time; with `--exit-after-save` it then exits (code 0). `--restore=FILE`
//! restarts from a snapshot instead of from the kernel (`--kernel` is not needed):
//! RAM, devices and options (`--mem`, devices, `--disk` with the same
//! files) must be those of the saved machine, otherwise the snapshot
//! is rejected. Disk files are links: their content does not
//! go into the snapshot, the guest's writes (copy-on-write) do.
//!
//! Record & replay (M10, ADR 0019): `--record=FILE` records every host
//! input (console from stdin, `--hostfwd` connections) with its
//! instruction number, plus a snapshot every `--keyframes=N` instructions (default
//! 100 million, 0 = none), and writes the log on exit (power-off,
//! reset, `--guest-secs`, stdin closed). `--replay=FILE` replays the recorded
//! session: same devices and disks, RAM, time and seed from the log; it starts
//! from `--kernel` (same `--initrd`/`--append`) or from `--restore`, like the
//! recording, or, with neither of them, from the first keyframe of the log.
//! Stdin does not count; at the end it compares console, instructions, CPU, RAM and
//! devices with the recording and says whether the replay is identical (code
//! 0) or where it diverges (code 1). `--goto=N` goes to instruction N (from the
//! nearest keyframe) and prints the registers; `--dump=VA:BYTES` adds the
//! bytes of virtual memory at that address (current tables).
//!
//! File manager (M8, ADR 0020): `--vsock` mounts virtio-vsock (CID 3),
//! on which `/init` starts the `vetro-files` daemon. `--files-ls=PATH`,
//! `--files-cat=PATH` and `--files-put=GUEST_PATH:HOST_FILE`
//! (repeatable, imply `--vsock`) run the operations in order
//! as soon as the daemon answers and write the results to stdout; once they
//! have all finished, `vetro` exits with 0 if they succeeded, 1 otherwise (see
//! `vetro_cli::files`).
//!
//! Analysis from the outside (M7/M8, ADR 0027, `vetro_cli::analysis`): it needs
//! the kernel profile (`--kernel-profile=FILE`: an Android `boot.img` or
//! an `Image`, with `--system-map`/`--kernel-btf` for the test kernel;
//! without it, `--boot-img`/`--kernel` is used). `--tls` hooks
//! `SSL_write`/`SSL_read` of `libssl` (BoringSSL, Conscrypt too): the
//! decrypted HTTPS requests end up in the HAR (`--har`) and in the list
//! (`--net-requests`) like the plaintext ones, tied to process and library.
//! `--binder-log=FILE` writes the decoded Binder calls (AIDL interface
//! and method, sender and recipient) as JSON (`.json`) or as lines of
//! text, and prints the sensitive accesses to stderr (privacy inspector).
//!
//! Accelerated graphics (M5, ADR 0037): `--gpu=gfxstream` gives the GPU
//! virgl (3D) with the gfxstream host decoder (`vetro-gfxstream`) and, with
//! `--boot-img`, adds `vetro_machine::android::GFXSTREAM_PARAMS` after
//! `--append` (the image's EGL becomes gfxstream over virtio-gpu). Natively
//! nothing is drawn (no WebGL: readbacks read zeros); `--gl-record=FILE`
//! writes the WebGL2 op stream for a browser replay, and the decoder's log
//! goes to stderr (`vetro-gl:`).
//!
//! Device profiles (M10, ADR 0035, `vetro_machine::profile`):
//! `--profile=NAME` (a starter profile: `default`, `phone`, `small-phone`,
//! `tablet`) or `--profile=FILE` (a profile JSON file) sets the RAM (an
//! explicit `--mem` wins), the scanout size of the virtio-gpu and, with
//! `--boot-img`, adds the profile's `androidboot.*` parameters after
//! `--append`. The settings applied through adb after the boot (time zone,
//! device name, locale) are printed to stderr as `adb shell` commands.

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
        "usage: vetro run [--strace] [--host-clock] [--sysroot=DIR] [--cpus=N] [--jit] [--jit-threshold=N] [--stats] <elf> [args...]"
    );
    eprintln!(
        "       vetro boot (--kernel=Image [--initrd=FILE] | --boot-img=FILE [--vendor-boot=FILE] [--init-boot=FILE] [--recovery] [--android-dump=DIR]) [--append=LINE] [--mem=MiB] [--smp=N] [--no-devices] [--net] [--no-net] [--net-events] [--hostfwd=tcp:[ADDR]:PORT-:GUEST_PORT]... [--pcap=FILE] [--har=FILE] [--net-requests] [--disk=FILE [--overlay=FILE]]... [--guest-secs=N] [--jit] [--jit-threshold=N] [--stats] [--save-at=INSTRUCTIONS:FILE]... [--save-on=TEXT:FILE [--save-delay=S] [--exit-after-save]] [--restore=FILE] [--record=FILE [--keyframes=N]] [--replay=FILE [--goto=INSTRUCTION [--dump=VA:BYTES]]] [--vsock] [--files-ls=PATH]... [--files-cat=PATH]... [--files-put=PATH:FILE]... [--kernel-profile=FILE [--system-map=FILE] [--kernel-btf=FILE]] [--tls] [--binder-log=FILE] [--profile=NAME|FILE] [--gpu=gfxstream [--gl-record=FILE]]"
    );
    ExitCode::from(2)
}

fn run(args: &[String]) -> ExitCode {
    // The CPUs seen by the guest are fixed (one, deterministic) unless --cpus=N:
    // they do not depend on the machine running it.
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
        eprintln!("vetro: {} instructions in {s:.3} s = {:.1} MIPS", out.steps, out.steps as f64 / s / 1e6);
        if let Some(j) = out.jit {
            eprintln!("vetro: jit {j:?}");
        }
    }
    match out.exit {
        Exit::Code(c) => ExitCode::from(c as u8),
        Exit::Signal { signo, cause, pc } => {
            match cause {
                Some(c) => eprintln!("vetro: signal {signo} at pc={pc:#x}: {c:?}"),
                None => eprintln!("vetro: killed by signal {signo}"),
            }
            ExitCode::from(128 + signo as u8)
        }
        Exit::UnsupportedSyscall { nr, pc } => {
            let name = vetro_analysis::syscall::name(nr);
            eprintln!("vetro: syscall {nr} ({name}) not implemented yet (pc={pc:#x})");
            ExitCode::from(125)
        }
        Exit::Unimplemented { raw, what, pc } => {
            eprintln!("vetro: instruction {raw:#010x} not implemented yet ({what}) at pc={pc:#x}");
            ExitCode::from(126)
        }
        Exit::StepLimit => {
            eprintln!("vetro: instruction limit reached");
            ExitCode::from(124)
        }
        Exit::Deadlock => {
            eprintln!("vetro: all processes blocked");
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
    let (mut profile, mut mem_set) = (None::<vetro_machine::profile::Profile>, false);
    // (image, overlay).
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
    let (mut gfxstream, mut gl_record, mut gl_trace) = (false, None::<String>, false);
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
            Some(("--gpu", "gfxstream")) => gfxstream = true,
            Some(("--gl-record", v)) => gl_record = Some(v.to_string()),
            Some(("--gl-trace", "1")) => gl_trace = true,
            // ADR 0042: cores, in turns on one thread (deterministic).
            Some(("--smp", v)) => match v.parse::<u32>() {
                Ok(n) if (1..=vetro_machine::smp::MAX_CPUS).contains(&n) => cfg.cpus = n,
                _ => return usage(),
            },
            Some(("--mem", v)) => match v.parse::<u64>() {
                Ok(m) => {
                    cfg.ram_size = m << 20;
                    mem_set = true;
                }
                Err(_) => return usage(),
            },
            Some(("--profile", v)) => match vetro_machine::profile::load(v) {
                Ok(p) => profile = Some(p),
                Err(e) => {
                    eprintln!("vetro: {e}");
                    return ExitCode::from(2);
                }
            },
            _ => return usage(),
        }
    }
    // Device profile (ADR 0035): RAM (an explicit --mem wins), scanout size,
    // and with Android images the androidboot.* parameters after --append.
    if let Some(p) = &profile {
        if !mem_set {
            cfg.ram_size = u64::from(p.ram_mib) << 20;
        }
        if let Some(gpu) = devices.gpu.as_mut() {
            p.apply_gpu(gpu);
        }
        let params = p.android_params();
        if boot_img.is_some() && !params.is_empty() {
            append = Some(match append.take() {
                Some(a) if !a.trim().is_empty() => format!("{} {params}", a.trim()),
                _ => params,
            });
        }
        eprintln!(
            "vetro: profile {} ({}x{} at {} dpi, {} MiB){}",
            p.id,
            p.width,
            p.height,
            p.density,
            cfg.ram_size >> 20,
            if p.adb_commands().is_empty() { String::new() } else { ", after the boot:".to_string() }
        );
        for c in p.adb_commands() {
            eprintln!("vetro:   adb shell \"{c}\"");
        }
    }
    // Accelerated graphics (ADR 0037): virgl on the GPU, and the boot
    // parameters that select gfxstream in the image.
    if gfxstream {
        let Some(gpu) = devices.gpu.as_mut() else {
            eprintln!("vetro: --gpu=gfxstream needs the GPU (not with --no-devices)");
            return ExitCode::from(2);
        };
        gpu.virgl = true;
        if boot_img.is_some() {
            let params = vetro_machine::android::GFXSTREAM_PARAMS;
            append = Some(match append.take() {
                Some(a) if !a.trim().is_empty() => format!("{} {params}", a.trim()),
                _ => params.to_string(),
            });
        }
    } else if gl_record.is_some() {
        eprintln!("vetro: --gl-record needs --gpu=gfxstream");
        return ExitCode::from(2);
    }
    let android = boot_img.is_some();
    if (kernel.is_none() && !android && restore.is_none() && replay.is_none())
        || (android && (kernel.is_some() || initrd.is_some()))
        || (!android && (vendor_boot.is_some() || init_boot.is_some() || recovery || android_dump.is_some()))
    {
        return usage();
    }
    if (goto.is_some() && replay.is_none()) || (dump.is_some() && goto.is_none()) {
        eprintln!("vetro: --goto requires --replay, --dump requires --goto");
        return ExitCode::from(2);
    }
    if record.is_some() && replay.is_some() {
        eprintln!("vetro: --record and --replay together make no sense");
        return ExitCode::from(2);
    }
    if replay.is_some() && !forwards.is_empty() {
        eprintln!("vetro: in replay the host network comes from the log: no --hostfwd");
        return ExitCode::from(2);
    }
    // The saves in instruction order, the first one at the end.
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
    // Android images: the bootloader prepares kernel, initrd and command line.
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
                        "vetro: kernel {} ({} bytes), ramdisk: {}{}",
                        a.kernel_format,
                        a.kernel.len(),
                        if a.ramdisks.is_empty() { "none".to_string() } else { a.ramdisks.join(", ") },
                        if a.bootconfig.is_empty() {
                            String::new()
                        } else {
                            format!(", bootconfig {} bytes", a.bootconfig.len())
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
        // RAM, time and seed of the recorded machine.
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
        eprintln!("vetro: in replay the inputs come from the log: no --files-*");
        return ExitCode::from(2);
    }
    let mut files = (!file_cmds.is_empty()).then(|| FilesTask::new(file_cmds));
    if !forwards.is_empty() && devices.net.is_none() {
        eprintln!("vetro: --hostfwd requires the network (--net)");
        return ExitCode::from(2);
    }
    let mut m = Machine::with_devices(&cfg, &devices);
    if gl_trace {
        use vetro_machine::vetro_gfxstream::Gfxstream;
        m.gpu(|g| g.renderer_as_mut::<Gfxstream>().map(|r| r.gl.trace = true));
    }
    if gl_record.is_some() {
        use vetro_machine::vetro_gfxstream::{Gfxstream, NullExecutor, Recorder};
        m.gpu(|g| {
            if let Some(r) = g.renderer_as_mut::<Gfxstream>() {
                r.gl.set_executor(Box::new(Recorder::new(NullExecutor::default())));
            }
        });
    }
    // Persistent overlays: (disk slot, overlay).
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
            eprintln!("vetro: too many virtio devices");
            return ExitCode::from(2);
        };
        if let Some(f) = overlay {
            overlays.push((slot, f));
        }
    }
    // Writes the changed clusters to the overlays (between one quantum and the next, and
    // before exiting). Host reads: the guest does not notice.
    let persist = |m: &Machine, overlays: &mut Vec<(u32, FileOverlay)>| -> Result<(), ExitCode> {
        for (slot, f) in overlays.iter_mut() {
            let mut b = m.board.borrow_mut();
            let cow = b
                .virt
                .virtio_mut(*slot)
                .and_then(|t| t.device_as_mut::<VirtioBlk>())
                .and_then(|blk| blk.backend_as_mut::<CowBackend<FileBackend>>())
                .expect("disk with overlay");
            if let Err(e) = f.persist(cow) {
                eprintln!("vetro: overlay of the disk in slot {slot}: {e}");
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
        eprintln!("vetro: restored {path} at {} instructions", m.steps);
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
        eprintln!("vetro: --pcap, --har and --net-requests require the network (--net)");
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
                                eprintln!("vetro: {at:#x} is not mapped");
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
        // Without kernel or snapshot we start from the log's initial keyframe.
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
            "vetro: replay of {path}: {} events, from {} to {} instructions",
            l.events.len(),
            m.steps,
            l.end.steps
        );
    }
    if record.is_some() {
        m.start_recording(vetro_machine::RecordOptions { keyframe_every: keyframes });
    }
    // stdin in a thread, the --hostfwd sockets in their own: everything arrives on
    // a channel and goes to the machine between one quantum and the next.
    let (tx, rx) = std::sync::mpsc::channel::<Input>();
    let stdin_tx = tx.clone();
    // In replay the inputs come from the log: stdin is not read.
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
                "vetro: {} instructions ({:.3} s of guest) in {s:.3} s = {:.1} MIPS",
                m.steps,
                m.guest_ns() as f64 / 1e9,
                m.steps as f64 / s / 1e6
            );
            if let Some(j) = m.jit_stats() {
                eprintln!("vetro: jit {j:?}");
            }
        }
        if let Some(p) = m.jit_profile() {
            eprintln!("vetro: {}vetro: calls to env.simd: {}", p.report(40), vetro_jit::helper::calls());
            if let Some(r) = vetro_jit::helper::profile_report(20) {
                eprint!("vetro: env.simd, {r}");
            }
        }
    };
    // Network events already printed (the log is read without touching it:
    // execution does not change with --net-events).
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
    let mut gl_seen = 0usize;
    let code = loop {
        if let Err(c) = persist(&m, &mut overlays) {
            break c;
        }
        if m.guest_ns() >= guest_ns {
            report(&m);
            eprintln!("vetro: guest time limit reached");
            break ExitCode::from(124);
        }
        // A quantum does not go past the next save.
        let budget = save_at.last().map_or(2_000_000, |s| s.0.saturating_sub(m.steps).clamp(1, 2_000_000));
        let stop = m.run(budget);
        analysis.tls_service(&mut m);
        print_net(&m);
        if gfxstream {
            print_gl(&m, &mut gl_seen);
        }
        if capture.wanted() {
            capture.collect(&mut m);
        }
        let o = m.console_output();
        if !o.is_empty() {
            let _ = out.write_all(&o);
            let _ = out.flush();
        }
        // --save-on: the text on the console fixes the moment of the save.
        if let Some((text, _)) = &save_on
            && save_on_at.is_none()
        {
            console_tail.extend_from_slice(&o);
            if console_tail.windows(text.len()).any(|w| w == text.as_bytes()) {
                save_on_at = Some(m.guest_ns().saturating_add(save_delay_ns));
                eprintln!(
                    "vetro: {text:?} on the console at {:.1} s of guest: snapshot in {:.1} s of guest",
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
                "vetro: snapshot at {} instructions ({:.1} s of guest, {:.0} s real) in {path} ({} bytes, saved in {:.1} s)",
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
                eprintln!("vetro: replay identical to the recording ({} instructions)", m.steps);
                break ExitCode::SUCCESS;
            }
            Some(vetro_machine::ReplayStatus::Diverged(d)) if log.is_some() => {
                report(&m);
                eprintln!("vetro: replay differs from the recording: {d}");
                break ExitCode::from(1);
            }
            _ => {}
        }
        while save_at.last().is_some_and(|s| m.steps >= s.0) {
            let (_, path) = save_at.pop().expect("checked above");
            let snap = m.save();
            if let Err(e) = std::fs::write(&path, &snap) {
                eprintln!("vetro: {path}: {e}");
                return ExitCode::from(2);
            }
            eprintln!("vetro: snapshot at {} instructions in {path} ({} bytes)", m.steps, snap.len());
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
            // The file manager has requests for the guest: keep going.
            Stop::Idle if files.is_some() => {}
            Stop::Idle => {
                // Nothing to do for the guest: wait for a host
                // input (console or network).
                let waiting = console_open || fwd.is_some();
                match rx.recv() {
                    Ok(i) if waiting => handle(&mut m, i, &mut console_open, &mut fwd),
                    _ => {
                        eprintln!("vetro: the guest is waiting for input and stdin is closed");
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
                eprintln!("vetro: the guest requested a reset");
                break ExitCode::SUCCESS;
            }
            Stop::Blocked => {
                // File-backed disks are always ready: this does not happen.
                eprintln!("vetro: a disk has no data ready");
                break ExitCode::from(2);
            }
            Stop::Unimplemented { pc, raw, what } => {
                report(&m);
                eprintln!("vetro: {raw:#010x} not implemented yet ({what}) at pc={pc:#x}");
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
            "vetro: recording in {path}: {} events, {} keyframes, {} instructions, {} bytes ({} without keyframes)",
            l.events.len(),
            l.keyframes.len(),
            l.end.steps,
            bytes.len(),
            l.events_len()
        );
    }
    if gfxstream {
        use vetro_machine::vetro_gfxstream::{Gfxstream, NullExecutor, Recorder};
        print_gl(&m, &mut gl_seen);
        let taken = m.gpu(|g| {
            let r = g.renderer_as_mut::<Gfxstream>()?;
            r.flush_ops();
            let stats = r.gl.stats.clone();
            let exec = r.gl.set_executor(Box::new(NullExecutor::default()));
            Some((stats, exec))
        });
        if let Some(Some((s, exec))) = taken {
            eprintln!(
                "vetro-gl: {} GLES calls, {} batches, {} frames presented, {} bytes read back, {} unhandled",
                s.calls, s.batches, s.presents, s.readback_bytes, s.unhandled
            );
            if let Some(path) = &gl_record {
                // The recorder is the only executor installed with --gl-record.
                let any: Box<dyn std::any::Any> = exec;
                if let Ok(rec) = any.downcast::<Recorder<NullExecutor>>() {
                    if let Err(e) = std::fs::write(path, &rec.log) {
                        eprintln!("vetro: {path}: {e}");
                        return ExitCode::from(2);
                    }
                    eprintln!("vetro-gl: op stream in {path} ({} bytes)", rec.log.len());
                }
            }
        }
    }
    code
}

/// New lines of the gfxstream decoder's log (unhandled calls…).
fn print_gl(m: &vetro_machine::Machine, seen: &mut usize) {
    use vetro_machine::vetro_gfxstream::Gfxstream;
    m.gpu_view(|g| {
        if let Some(r) = g.renderer_as::<Gfxstream>() {
            for l in r.gl.log.iter().skip(*seen) {
                eprintln!("vetro-gl: {l}");
            }
            *seen = r.gl.log.len();
        }
    });
}

/// `boot` options that take a value: `--option value` becomes
/// `--option=value`.
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
    "--profile",
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

/// `Image`, `initrd` and `cmdline` as the kernel receives them.
fn dump_android(dir: &std::path::Path, a: &vetro_machine::android::AndroidBoot) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("Image"), &a.kernel)?;
    std::fs::write(dir.join("initrd"), &a.initrd)?;
    std::fs::write(dir.join("cmdline"), format!("{}\n", a.cmdline))?;
    eprintln!("vetro: Image, initrd and cmdline in {}", dir.display());
    Ok(())
}

/// An address in hexadecimal (`0x...`) or in decimal.
fn parse_addr(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Bytes in rows of 16: address, hexadecimal, ASCII.
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
