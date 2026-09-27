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

#[test]
fn boot_da_boot_img_e_vendor_boot() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
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
