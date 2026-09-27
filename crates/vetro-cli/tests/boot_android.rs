//! `vetro boot --boot-img FILE --vendor-boot FILE`: the M3 guest kernel
//! packed by `mkbootimg.py` (`tools/mkbootimg/`) into a v4 `boot.img`
//! (kernel and generic ramdisk) and a v4 `vendor_boot.img` (a ramdisk and the
//! bootconfig section), without `init_boot`. The guest sees the command line
//! composed by the bootloader, the vendor ramdisk file and in
//! `/proc/bootconfig` also the `androidboot.*` passed with `--append`;
//! `--android-dump` writes the files for QEMU. The comparison with QEMU and
//! `init_boot` live in `tests/boot/tests/android.rs`.
//!
//! In release, with `python3` (`VETRO_REQUIRE_ORACLE`) and the guest kernel
//! (`VETRO_REQUIRE_GUEST_KERNEL`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, repo_root, skip_or_fail, timeout};

/// newc cpio archive with one file in the root.
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

/// Images made by `mkbootimg.py`: (dir, guest Image, boot.img,
/// vendor_boot.img with `vendor_bootconfig`), or None after a skip.
fn images(tag: &str, vendor_bootconfig: &str) -> Option<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
        return None;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel missing");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-android-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let vendor_rd = dir.join("vendor.cpio");
    std::fs::write(&vendor_rd, cpio_one("vendor-file", b"dal vendor_boot\n")).unwrap();
    let bc = dir.join("bootconfig");
    std::fs::write(&bc, vendor_bootconfig).unwrap();
    let (boot, vendor) = (dir.join("boot.img"), dir.join("vendor_boot.img"));

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
            skip_or_fail("VETRO_REQUIRE_ORACLE", "python3 missing (tools/mkbootimg/mkbootimg.py)");
            return None;
        }
    }
    Some((dir, image, boot, vendor))
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

#[test]
fn boot_da_boot_img_e_vendor_boot() {
    let Some((dir, image, boot, vendor)) = images("plain", "androidboot.hardware=vetro\n") else {
        return;
    };
    let dump = dir.join("dump");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    // The new options in the form with the space.
    cmd.arg("boot")
        .args(["--boot-img", &s(&boot), "--vendor-boot", &s(&vendor)])
        .arg("--append=androidboot.serialno=CLI01 vetro.cli=1")
        .args(["--android-dump", &s(&dump)])
        .arg("--mem=256")
        .arg("--no-devices");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("no /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    // "C"MD and similar: the echo of the command doesn't contain the markers.
    c.send(concat!(
        "echo \"C\"MD=$(cat /proc/cmdline); echo \"B\"C=$(cat /proc/bootconfig | tr '\\n' ';'); ",
        "echo \"V\"ND=$(cat /vendor-file); poweroff -f\n"
    ));
    let (_, cmd) = c.wait_line("CMD=", at, limit).unwrap_or_else(|| panic!("no CMD:\n{}", c.log()));
    let cmdline = "console=ttyAMA0 vetro.noautotest bootconfig vetro.vendor=1 vetro.cli=1";
    assert_eq!(cmd, format!("CMD={cmdline}"));
    let (_, bc) = c.wait_line("BC=", at, limit).unwrap_or_else(|| panic!("no BC:\n{}", c.log()));
    assert_eq!(bc, "BC=androidboot.hardware = \"vetro\";androidboot.serialno = \"CLI01\";");
    let (_, vnd) = c.wait_line("VND=", at, limit).unwrap_or_else(|| panic!("no VND:\n{}", c.log()));
    assert_eq!(vnd, "VND=dal vendor_boot");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());

    assert_eq!(std::fs::read(dump.join("Image")).unwrap(), std::fs::read(&image).unwrap());
    assert_eq!(std::fs::read_to_string(dump.join("cmdline")).unwrap(), format!("{cmdline}\n"));
    let dumped = std::fs::read(dump.join("initrd")).unwrap();
    assert!(dumped.ends_with(b"#BOOTCONFIG\n"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `--profile=phone` (ADR 0035): the vendor bootconfig has the image's
/// density and serial, the profile's values replace those lines in place and
/// its SKU is appended; the virtio-gpu offers the profile's size as the
/// preferred mode (`vetro-dev drm`); the RAM is the profile's.
#[test]
fn boot_with_the_phone_profile() {
    let vendor_bc =
        "androidboot.hardware=vetro\nandroidboot.lcd_density=240\nandroidboot.serialno=VETRO00001\n";
    let Some((dir, _, boot, vendor)) = images("profile", vendor_bc) else {
        return;
    };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot")
        .args(["--boot-img", &s(&boot), "--vendor-boot", &s(&vendor)])
        .args(["--profile", "phone"])
        .arg("--append=vetro.cli=1")
        .arg("--no-net");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("no /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    c.send(concat!(
        "echo \"C\"MD=$(cat /proc/cmdline); echo \"B\"C=$(cat /proc/bootconfig | tr '\\n' ';'); ",
        "echo \"M\"EM=$(head -1 /proc/meminfo); vetro-dev drm | grep preferit[o]; poweroff -f\n"
    ));
    let (_, cmd) = c.wait_line("CMD=", at, limit).unwrap_or_else(|| panic!("no CMD:\n{}", c.log()));
    assert_eq!(cmd, "CMD=console=ttyAMA0 vetro.noautotest bootconfig vetro.vendor=1 vetro.cli=1");
    let (_, bc) = c.wait_line("BC=", at, limit).unwrap_or_else(|| panic!("no BC:\n{}", c.log()));
    // /proc/bootconfig lists the key tree depth first: `hardware.sku` next to
    // `hardware`, whatever the order in the block.
    assert_eq!(
        bc,
        "BC=androidboot.hardware = \"vetro\";androidboot.hardware.sku = \"phone\";\
         androidboot.lcd_density = \"320\";androidboot.serialno = \"VETROPHONE01\";"
    );
    let (_, mem) = c.wait_line("MEM=", at, limit).unwrap_or_else(|| panic!("no MEM:\n{}", c.log()));
    let kb: u64 = mem.split_whitespace().nth(1).and_then(|k| k.parse().ok()).unwrap_or(0);
    assert!(kb > 1900 << 10 && kb <= 2048 << 10, "2 GiB profile, {mem}");
    // Only the preferred mode's line passes the grep.
    let (_, mode) =
        c.wait_line("drm modo ", at, limit).unwrap_or_else(|| panic!("no DRM mode:\n{}", c.log()));
    assert!(mode.starts_with("drm modo 720x1280 ") && mode.ends_with("preferito"), "preferred mode: {mode}");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `--profile` errors and messages, without booting: an unknown name, a bad
/// file, and the summary with the adb commands before the images are read.
#[test]
fn profile_option_messages() {
    let vetro = env!("CARGO_BIN_EXE_vetro");
    let run = |args: &[&str]| {
        let o = Command::new(vetro).arg("boot").args(args).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let (code, err) = run(&["--profile=nope", "--boot-img=/nonexistent/boot.img"]);
    assert_eq!(code, Some(2));
    assert!(
        err.contains("nope: not a starter profile (light, default, phone, small-phone, tablet)"),
        "{err}"
    );
    let dir = std::env::temp_dir().join(format!("vetro-profile-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.json");
    std::fs::write(&bad, r#"{"vetroProfile":2}"#).unwrap();
    let (code, err) = run(&["--profile", &s(&bad), "--boot-img=/nonexistent/boot.img"]);
    assert_eq!(code, Some(2));
    assert!(err.contains("profile: vetroProfile: version 2 needs a newer Vetro"), "{err}");
    let (code, err) = run(&["--profile=tablet", "--boot-img=/nonexistent/boot.img"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("vetro: profile tablet (1280x800 at 213 dpi, 2048 MiB), after the boot:"), "{err}");
    assert!(err.contains("vetro:   adb shell \"cmd alarm set-timezone UTC\""), "{err}");
    assert!(err.contains("vetro:   adb shell \"settings put global device_name 'Vetro Tablet'\""), "{err}");
    // An explicit --mem wins over the profile's RAM.
    let (_, err) = run(&["--profile=small-phone", "--mem=3072", "--boot-img=/nonexistent/boot.img"]);
    assert!(err.contains("profile small-phone (480x800 at 240 dpi, 3072 MiB)"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
