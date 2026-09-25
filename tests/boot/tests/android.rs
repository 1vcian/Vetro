//! Il kernel guest si avvia dalle immagini Android (M5): `boot.img`,
//! `vendor_boot.img` e `init_boot.img` v4 costruite da `mkbootimg.py` di AOSP
//! (`tools/mkbootimg/`) intorno all'`Image` e all'initramfs di
//! `tools/guest-kernel/build.sh`, come le costruisce una build GKI:
//!
//! - `boot.img`: il kernel compresso con `gzip -9` (come `boot-gz.img`);
//! - `init_boot.img`: il ramdisk generico, cioè l'initramfs del guest
//!   ricompresso in LZ4 legacy (il formato del GKI);
//! - `vendor_boot.img`: tre ramdisk nella tabella, `dlkm` (cpio non
//!   compresso), `reco` (tipo recovery, da non caricare) e `plat` (LZ4 legacy,
//!   con un `/init` sbagliato che il generico deve coprire), la riga di
//!   comando del vendor e una sezione bootconfig.
//!
//! Il bootloader di Vetro (`vetro_machine::android`) aggiunge due
//! `androidboot.*` e un parametro normale. Il guest deve vedere la riga di
//! comando e `/proc/bootconfig` attesi, i file dei ramdisk giusti e non quello
//! di recovery. Poi lo stesso spacchettamento (`Image`, initrd col blocco
//! bootconfig, riga di comando) va a `qemu-system-aarch64 -kernel -initrd
//! -append`: i due log, senza tempi e senza le differenze note, devono
//! coincidere.
//!
//! Servono il kernel guest (`VETRO_REQUIRE_GUEST_KERNEL`), `python3` e `gzip`
//! (`VETRO_REQUIRE_ORACLE`) e, per il confronto, qemu-system-aarch64
//! (`VETRO_REQUIRE_SYSTEM_ORACLE`). Solo in release.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use vetro_boot_tests::*;
use vetro_machine::android::{AndroidBoot, BootOptions, bootconfig, decompress};
use vetro_machine::{Machine, MachineConfig, Stop};

const PHASE_BUDGET: u64 = 6_000_000_000;
/// Comandi dati alla shell del guest; il marcatore finale non compare
/// nell'eco (lì c'è `$((6*7))`).
const SCRIPT: &str = "cat /proc/cmdline; cat /proc/bootconfig; cat /vendor-plat /vendor-dlkm; \
                      ls /vendor-recovery; echo VETRO-ANDROID-$((6*7))\n";
const END: &str = "VETRO-ANDROID-42";
const BOOT_CMDLINE: &str = "console=ttyAMA0 vetro.noautotest";
const VENDOR_CMDLINE: &str = "vetro.vendor=1";
const VENDOR_BOOTCONFIG: &str = "androidboot.hardware=vetro\nandroidboot.boot_devices=a003e00.virtio_mmio\n";
const PARAMS: &str = "androidboot.serialno=VETRO0001 vetro.bootloader=1 androidboot.slot_suffix=_a";
/// Riga di comando attesa: boot, vendor, parametri normali del bootloader,
/// `bootconfig` aggiunto dal bootloader.
const CMDLINE: &str = "console=ttyAMA0 vetro.noautotest vetro.vendor=1 vetro.bootloader=1 bootconfig";
/// `/proc/bootconfig` atteso: la sezione del vendor, poi i parametri del
/// bootloader.
const PROC_BOOTCONFIG: &str = "androidboot.hardware = \"vetro\"\nandroidboot.boot_devices = \"a003e00.virtio_mmio\"\n\
                               androidboot.serialno = \"VETRO0001\"\nandroidboot.slot_suffix = \"_a\"\n";

/// Archivio cpio newc con file regolari nella radice.
fn cpio(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut entry = |ino: usize, name: &str, mode: u32, data: &[u8]| {
        let hdr = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            ino,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0
        );
        out.extend_from_slice(hdr.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    };
    for (i, (name, mode, data)) in files.iter().enumerate() {
        entry(i + 1, name, *mode, data);
    }
    entry(0, "TRAILER!!!", 0, &[]);
    out
}

/// LZ4 legacy (`lz4 -l`) con blocchi di soli letterali: valido per ogni
/// decompressore LZ4, compreso quello del kernel.
fn lz4_legacy_literals(data: &[u8]) -> Vec<u8> {
    let mut out = decompress::LZ4_LEGACY_MAGIC.to_le_bytes().to_vec();
    for chunk in data.chunks(8 << 20) {
        let mut block = Vec::with_capacity(chunk.len() + chunk.len() / 255 + 16);
        if chunk.len() < 15 {
            block.push((chunk.len() as u8) << 4);
        } else {
            block.push(0xf0);
            let mut n = chunk.len() - 15;
            while n >= 255 {
                block.push(255);
                n -= 255;
            }
            block.push(n as u8);
        }
        block.extend_from_slice(chunk);
        out.extend_from_slice(&(block.len() as u32).to_le_bytes());
        out.extend_from_slice(&block);
    }
    out
}

fn gzip9(data: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("gzip")
        .args(["-n", "-9", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = data.to_vec();
    let w = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().ok()?;
    w.join().ok()?.ok()?;
    out.status.success().then_some(out.stdout)
}

fn mkbootimg(args: &[&str]) -> Result<(), String> {
    let script = repo_root().join("tools/mkbootimg/mkbootimg.py");
    let o = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| format!("python3: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).into_owned()) }
}

/// Le tre immagini, in `dir`. `None` se mancano gli strumenti.
fn build_images(dir: &Path, image: &[u8], initramfs_gz: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let kernel_gz = gzip9(image)?;
    let generic = lz4_legacy_literals(&decompress::gunzip(initramfs_gz).expect("initramfs gzip"));
    let bogus_init = b"#!/bin/busybox sh\necho VENDOR-INIT-SBAGLIATO\n";
    let plat = lz4_legacy_literals(&cpio(&[
        ("vendor-plat", 0o100644, b"ramdisk plat del vendor\n"),
        ("init", 0o100755, bogus_init),
    ]));
    let reco = cpio(&[("vendor-recovery", 0o100644, b"non va caricato\n")]);
    let dlkm = cpio(&[("vendor-dlkm", 0o100644, b"ramdisk dlkm del vendor\n")]);
    let file = |name: &str, data: &[u8]| {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p.to_str().unwrap().to_string()
    };
    let (k, g) = (file("Image.gz", &kernel_gz), file("generic.cpio.lz4", &generic));
    let (p, r, d) = (file("plat.cpio.lz4", &plat), file("reco.cpio", &reco), file("dlkm.cpio", &dlkm));
    let bc = file("bootconfig", VENDOR_BOOTCONFIG.as_bytes());
    let out = |n: &str| dir.join(n).to_str().unwrap().to_string();
    let (boot, vendor, init) = (out("boot.img"), out("vendor_boot.img"), out("init_boot.img"));
    let run = |args: &[&str]| mkbootimg(args).map_err(|e| eprintln!("mkbootimg.py {args:?}: {e}")).ok();
    run(&[
        "--header_version",
        "4",
        "--kernel",
        &k,
        "--cmdline",
        BOOT_CMDLINE,
        "--os_version",
        "15.0.0",
        "-o",
        &boot,
    ])?;
    run(&["--header_version", "4", "--ramdisk", &g, "-o", &init])?;
    run(&[
        "--header_version",
        "4",
        "--vendor_boot",
        &vendor,
        "--pagesize",
        "4096",
        "--vendor_cmdline",
        VENDOR_CMDLINE,
        "--vendor_bootconfig",
        &bc,
        "--board",
        "vetro-virt",
        "--ramdisk_type",
        "dlkm",
        "--ramdisk_name",
        "dlkm",
        "--vendor_ramdisk_fragment",
        &d,
        "--ramdisk_type",
        "recovery",
        "--ramdisk_name",
        "reco",
        "--vendor_ramdisk_fragment",
        &r,
        "--ramdisk_type",
        "platform",
        "--ramdisk_name",
        "plat",
        "--vendor_ramdisk_fragment",
        &p,
    ])?;
    let read = |p: &str| std::fs::read(p).unwrap();
    Some((read(&boot), read(&vendor), read(&init)))
}

/// Il guest sotto Vetro fino alla fine dei comandi, poi `poweroff -f`.
fn run_vetro(a: &AndroidBoot) -> String {
    let mut m = Machine::new(&MachineConfig::default());
    m.load_android(a).expect("caricamento del kernel");
    let mut log = Vec::new();
    let until = |m: &mut Machine, log: &mut Vec<u8>, needle: &str, from: usize| -> usize {
        let limit = m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) =
                log[from.min(log.len())..].windows(needle.len()).position(|w| w == needle.as_bytes())
            {
                return from + i + needle.len();
            }
            let stop = m.run(1_000_000);
            log.extend(m.console_output());
            let tail = || normalize(&String::from_utf8_lossy(log));
            assert!(m.steps < limit, "{needle:?} non arrivato; console:\n{}", tail());
            assert_eq!(stop, Stop::Budget, "in attesa di {needle:?}; console:\n{}", tail());
        }
    };
    let at = until(&mut m, &mut log, BOOT_MARKER, 0);
    let prompt = until(&mut m, &mut log, SHELL_PROMPT, at);
    m.console_input(SCRIPT.as_bytes());
    let end = until(&mut m, &mut log, END, prompt);
    until(&mut m, &mut log, SHELL_PROMPT, end);
    m.console_input(b"poweroff -f\n");
    let limit = m.steps + PHASE_BUDGET;
    let stop = loop {
        let s = m.run(1_000_000);
        log.extend(m.console_output());
        if s != Stop::Budget || m.steps >= limit {
            break s;
        }
    };
    let log = normalize(&String::from_utf8_lossy(&log));
    assert_eq!(stop, Stop::PowerOff, "poweroff -f non ha spento la macchina:\n{log}");
    log
}

/// Lo stesso sotto QEMU coi file spacchettati.
fn run_qemu(qemu: &Path, image: &Path, initrd: &Path, cmdline: &str) -> String {
    let mut cmd = Command::new(qemu);
    cmd.args(QEMU_MACHINE)
        .args(["-nographic", "-kernel"])
        .arg(image)
        .arg("-initrd")
        .arg(initrd)
        .args(["-append", cmdline]);
    let limit = timeout();
    let mut con = Console::spawn(cmd).expect("avvio di qemu-system-aarch64");
    let fail =
        |con: &Console, what: &str| -> ! { panic!("QEMU: {what}; console:\n{}", normalize(&con.log())) };
    let Some(at) = con.wait_for(BOOT_MARKER, 0, limit) else { fail(&con, "nessun marcatore di avvio") };
    let Some(prompt) = con.wait_for(SHELL_PROMPT, at, limit) else { fail(&con, "nessun prompt") };
    con.send(SCRIPT);
    let Some(end) = con.wait_for(END, prompt, limit) else { fail(&con, "comandi non finiti") };
    if con.wait_for(SHELL_PROMPT, end, limit).is_none() {
        fail(&con, "nessun prompt dopo i comandi");
    }
    con.send("poweroff -f\n");
    let exited = con.finish(Duration::from_secs(30));
    assert!(exited, "QEMU non si è spento dopo poweroff -f");
    normalize(&con.log())
}

/// Controlli sul log di un avvio (Vetro o QEMU).
fn check_guest_view(who: &str, log: &str) {
    assert!(log.contains("VETRO-BOOT-OK"), "{who}: /init del generico non partito:\n{log}");
    assert!(!log.contains("VENDOR-INIT-SBAGLIATO"), "{who}: è partito l'/init del vendor");
    assert!(log.contains("Load bootconfig: "), "{who}: il kernel non ha caricato il bootconfig:\n{log}");
    // Uscita dei comandi: dall'ultima riga di comando in /proc/cmdline al
    // marcatore finale.
    let start = log
        .rfind(&format!("\n{CMDLINE}\n"))
        .unwrap_or_else(|| panic!("{who}: /proc/cmdline diverso:\n{log}"));
    let end = log.rfind(END).unwrap();
    let out = &log[start + 1..end];
    let want = format!(
        "{CMDLINE}\n{PROC_BOOTCONFIG}ramdisk plat del vendor\nramdisk dlkm del vendor\n\
         ls: /vendor-recovery: No such file or directory\n"
    );
    assert_eq!(out, want, "{who}: il guest non vede quello che il bootloader ha preparato");
}

#[test]
fn android_boot_images_like_qemu() {
    if cfg!(debug_assertions) {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "avvio sotto Vetro solo in release (cargo test --release)",
        );
    }
    let Some((image_path, initrd_path)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image, initramfs) = (std::fs::read(&image_path).unwrap(), std::fs::read(&initrd_path).unwrap());
    let root = repo_root();
    let dir: PathBuf = root.join("target/guest-kernel/android");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let Some((boot, vendor, init)) = build_images(&dir, &image, &initramfs) else {
        return skip_or_fail("VETRO_REQUIRE_ORACLE", "servono python3 (tools/mkbootimg/mkbootimg.py) e gzip");
    };

    let opts = BootOptions { params: PARAMS.into(), recovery: false };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), Some(&init), &opts).expect("immagini Android");
    assert_eq!(a.kernel_format, decompress::Format::Gzip);
    assert_eq!(a.kernel, image, "il kernel decompresso non è l'Image di partenza");
    assert_eq!(a.cmdline, CMDLINE);
    let (len, _) = bootconfig::split(&a.initrd).expect("blocco bootconfig in coda all'initrd");
    eprintln!(
        "ramdisk: {:?}; initrd {} byte, bootconfig {} byte",
        a.ramdisks,
        a.initrd.len(),
        a.initrd.len() - len
    );

    let t0 = std::time::Instant::now();
    let vlog = run_vetro(&a);
    eprintln!("Vetro: {:.1} s", t0.elapsed().as_secs_f64());
    std::fs::write(root.join("target/guest-kernel/android-vetro.log"), &vlog).unwrap();
    check_guest_view("Vetro", &vlog);

    let Some(qemu) = qemu_system() else {
        return skip_or_fail(
            "VETRO_REQUIRE_SYSTEM_ORACLE",
            "qemu-system-aarch64 assente (su macOS: VETRO_QEMU_SYSTEM_AARCH64=tools/guest-kernel/qemu-system-aarch64-docker.sh)",
        );
    };
    // Lo stesso spacchettamento come file per -kernel e -initrd.
    let (ki, ii) = (dir.join("Image"), dir.join("initrd"));
    std::fs::write(&ki, &a.kernel).unwrap();
    std::fs::write(&ii, &a.initrd).unwrap();
    std::fs::write(dir.join("cmdline"), &a.cmdline).unwrap();
    let t0 = std::time::Instant::now();
    let qlog = run_qemu(&qemu, &ki, &ii, &a.cmdline);
    eprintln!("QEMU: {:.1} s", t0.elapsed().as_secs_f64());
    std::fs::write(root.join("target/guest-kernel/android-qemu.log"), &qlog).unwrap();
    check_guest_view("QEMU", &qlog);

    let diff = line_diff(&comparable_lines(&qlog), &comparable_lines(&vlog));
    assert!(
        diff.is_empty(),
        "il log di Vetro differisce da quello di QEMU (- solo QEMU, + solo Vetro):\n{}",
        diff.join("\n")
    );
}
