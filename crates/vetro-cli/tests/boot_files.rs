//! `vetro boot --files-put/--files-ls/--files-cat` (M8, ADR 0020): il
//! demone `vetro-files` del kernel guest risponde alla riga di comando;
//! `vetro` esce da solo quando le operazioni sono finite.
//!
//! In release (`cargo test --release -p vetro-cli`), come gli altri avvii.

use std::process::{Command, Stdio};

use vetro_boot_tests::{guest_kernel, skip_or_fail};

#[test]
fn operazioni_sui_file_dalla_riga_di_comando() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
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
    assert_eq!(out.status.code(), Some(1), "--files-ls su un file non è una cartella\n{stdout}\n{stderr}");
    assert!(stderr.contains("vetro-files: scritto /tmp/x.txt (25 byte, -rw-r--r--)"), "{stderr}");
    assert!(stderr.contains("vetro-files: /bin/sh: ENOTDIR (20)"), "{stderr}");
    let line = stdout.lines().find(|l| l.ends_with(" x.txt")).unwrap_or_else(|| panic!("{stdout}"));
    assert!(line.contains("-rw-r--r-- 0 0 25 "), "{line}");
    assert!(stdout.contains("ciao dal disco dell'host\n"), "{stdout}");

    // Tutto riuscito: codice 0.
    let out = run(&["--files-cat=/etc/autotest.sh".to_string()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("VETRO-AUTOTEST-FINE"));
    std::fs::remove_dir_all(&dir).unwrap();
}
