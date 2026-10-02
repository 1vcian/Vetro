//! `vetro boot --files-put/--files-ls/--files-cat` (M8, ADR 0020): the
//! `vetro-files` daemon of the guest kernel answers the command line;
//! `vetro` exits by itself when the operations are finished.
//!
//! In release (`cargo test --release -p vetro-cli`), like the other boots.

use std::process::{Command, Stdio};

use vetro_boot_tests::{guest_kernel, skip_or_fail};

#[test]
fn operazioni_sui_file_dalla_riga_di_comando() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel missing");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let host = dir.join("dall-host.txt");
    std::fs::write(&host, b"ciao dal disco dell'host\n").unwrap();
    let run = |extra: &[String]| {
        let mut args = vec![
            "boot".to_string(),
            format!("--kernel={}", image.display()),
            format!("--initrd={}", initrd.display()),
            "--append=console=ttyAMA0 vetro.noautotest".to_string(),
            "--mem=256".to_string(),
            "--guest-secs=60".to_string(),
        ];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_vetro")).args(&args).stdin(Stdio::null()).output().unwrap()
    };
    let out = run(&[
        format!("--files-put=/tmp/x.txt:{}", host.display()),
        "--files-ls=/tmp".to_string(),
        "--files-cat=/tmp/x.txt".to_string(),
        "--files-ls=/bin/sh".to_string(),
    ]);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "--files-ls on a file is not a folder\n{stdout}\n{stderr}");
    assert!(stderr.contains("vetro-files: wrote /tmp/x.txt (25 bytes, -rw-r--r--)"), "{stderr}");
    assert!(stderr.contains("vetro-files: /bin/sh: ENOTDIR (20)"), "{stderr}");
    let line = stdout.lines().find(|l| l.ends_with(" x.txt")).unwrap_or_else(|| panic!("{stdout}"));
    assert!(line.contains("-rw-r--r-- 0 0 25 "), "{line}");
    assert!(stdout.contains("ciao dal disco dell'host\n"), "{stdout}");

    // All successful: code 0.
    let out = run(&["--files-cat=/etc/autotest.sh".to_string()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("VETRO-AUTOTEST-FINE"));
    std::fs::remove_dir_all(&dir).unwrap();
}
