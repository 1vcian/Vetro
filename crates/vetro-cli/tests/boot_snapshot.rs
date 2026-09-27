//! `vetro boot --save-at=N:FILE` and `--restore=FILE` (M6, ADR 0015): the
//! snapshot taken during the guest kernel boot resumes in another
//! process, without a kernel, and prints exactly the rest of the original
//! boot up to the shell; a snapshot is refused, with a clear
//! message, on a machine configured differently.
//!
//! In release (`cargo test --release -p vetro-cli`), like the other boots.

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, normalize, skip_or_fail, timeout};

/// Starts `vetro boot` with `args`, waits for the prompt, powers off; returns
/// the log up to and including the prompt.
fn until_shell(args: &[String]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot").args(args);
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let at = c.wait_for(SHELL_PROMPT, 0, timeout()).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    let log = c.log()[..at].to_string();
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
    log
}

#[test]
fn salva_durante_l_avvio_e_riparte_da_li() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (early, late) = (dir.join("presto.vsnap"), dir.join("tardi.vsnap"));
    let base = |extra: &[String]| {
        let mut v = vec!["--append=console=ttyAMA0 vetro.noautotest".to_string(), "--mem=512".to_string()];
        v.extend_from_slice(extra);
        v
    };
    let full = until_shell(&base(&[
        format!("--kernel={}", image.display()),
        format!("--initrd={}", initrd.display()),
        format!("--save-at=3000000:{}", early.display()),
        format!("--save-at=40000000:{}", late.display()),
    ]));
    assert!(full.contains(BOOT_MARKER));
    for snap in [&early, &late] {
        let resumed = until_shell(&base(&[format!("--restore={}", snap.display())]));
        assert!(resumed.len() > 100, "{}: no output after the restore", snap.display());
        assert!(
            full.ends_with(&resumed),
            "{}: the rest doesn't match the original boot:\n--- restored ---\n{}",
            snap.display(),
            normalize(&resumed)
        );
    }

    // A machine with different RAM refuses the snapshot.
    let out = Command::new(env!("CARGO_BIN_EXE_vetro"))
        .args(["boot", "--mem=256", &format!("--restore={}", early.display())])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("configured differently"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
