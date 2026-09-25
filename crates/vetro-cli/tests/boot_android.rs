//! `vetro boot --boot-img FILE --vendor-boot FILE`: il kernel guest di M3
//! impacchettato da `mkbootimg.py` (`tools/mkbootimg/`) in `boot.img` v4
//! (kernel e ramdisk generico) e `vendor_boot.img` v4 (un ramdisk e la
//! sezione bootconfig), senza `init_boot`. Il guest vede la riga di comando
//! composta dal bootloader, il file del ramdisk del vendor e in
//! `/proc/bootconfig` anche l'`androidboot.*` passato con `--append`;
//! `--android-dump` scrive i file per QEMU. Il confronto con QEMU e
//! `init_boot` stanno in `tests/boot/tests/android.rs`.
//!
//! In release, con `python3` (`VETRO_REQUIRE_ORACLE`) e il kernel guest
//! (`VETRO_REQUIRE_GUEST_KERNEL`).

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, repo_root, skip_or_fail, timeout};

/// Archivio cpio newc con un file nella radice.
fn cpio_one(name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (ino, name, mode, data) in [(1, name, 0o100644, data), (0, "TRAILER!!!", 0, &[][..])] {
        let hdr = format!(
            "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
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
    }
    out
}

#[test]
fn boot_da_boot_img_e_vendor_boot() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-android-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let vendor_rd = dir.join("vendor.cpio");
    std::fs::write(&vendor_rd, cpio_one("vendor-file", b"dal vendor_boot\n")).unwrap();
    let bc = dir.join("bootconfig");
    std::fs::write(&bc, "androidboot.hardware=vetro\n").unwrap();
    let (boot, vendor) = (dir.join("boot.img"), dir.join("vendor_boot.img"));
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
    let script = repo_root().join("tools/mkbootimg/mkbootimg.py");
    let args = [
        "--header_version",
        "4",
        "--kernel",
        &s(&image),
        "--ramdisk",
        &s(&initrd),
        "--cmdline",
        "console=ttyAMA0 vetro.noautotest bootconfig",
        "-o",
        &s(&boot),
        "--vendor_boot",
        &s(&vendor),
        "--vendor_cmdline",
        "vetro.vendor=1",
        "--vendor_bootconfig",
        &s(&bc),
        "--ramdisk_type",
        "platform",
        "--ramdisk_name",
        "vnd",
        "--vendor_ramdisk_fragment",
        &s(&vendor_rd),
    ];
    let made = Command::new("python3").env("PYTHONDONTWRITEBYTECODE", "1").arg(&script).args(args).output();
    match made {
        Ok(o) if o.status.success() => {}
        Ok(o) => panic!("mkbootimg.py: {}", String::from_utf8_lossy(&o.stderr)),
        Err(_) => {
            return skip_or_fail("VETRO_REQUIRE_ORACLE", "python3 assente (tools/mkbootimg/mkbootimg.py)");
        }
    }

    let dump = dir.join("dump");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    // Le opzioni nuove nella forma con lo spazio.
    cmd.arg("boot")
        .args(["--boot-img", &s(&boot), "--vendor-boot", &s(&vendor)])
        .arg("--append=androidboot.serialno=CLI01 vetro.cli=1")
        .args(["--android-dump", &s(&dump)])
        .arg("--mem=256")
        .arg("--no-devices");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("niente /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("niente shell:\n{}", c.log()));
    // "C"MD e simili: l'eco del comando non contiene i marcatori.
    c.send(concat!(
        "echo \"C\"MD=$(cat /proc/cmdline); echo \"B\"C=$(cat /proc/bootconfig | tr '\\n' ';'); ",
        "echo \"V\"ND=$(cat /vendor-file); poweroff -f\n"
    ));
    let (_, cmd) = c.wait_line("CMD=", at, limit).unwrap_or_else(|| panic!("niente CMD:\n{}", c.log()));
    let cmdline = "console=ttyAMA0 vetro.noautotest bootconfig vetro.vendor=1 vetro.cli=1";
    assert_eq!(cmd, format!("CMD={cmdline}"));
    let (_, bc) = c.wait_line("BC=", at, limit).unwrap_or_else(|| panic!("niente BC:\n{}", c.log()));
    assert_eq!(bc, "BC=androidboot.hardware = \"vetro\";androidboot.serialno = \"CLI01\";");
    let (_, vnd) = c.wait_line("VND=", at, limit).unwrap_or_else(|| panic!("niente VND:\n{}", c.log()));
    assert_eq!(vnd, "VND=dal vendor_boot");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f non ha fermato vetro:\n{}", c.log());

    assert_eq!(std::fs::read(dump.join("Image")).unwrap(), std::fs::read(&image).unwrap());
    assert_eq!(std::fs::read_to_string(dump.join("cmdline")).unwrap(), format!("{cmdline}\n"));
    let dumped = std::fs::read(dump.join("initrd")).unwrap();
    assert!(dumped.ends_with(b"#BOOTCONFIG\n"));
    std::fs::remove_dir_all(&dir).unwrap();
}
