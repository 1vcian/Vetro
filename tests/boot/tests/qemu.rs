//! Il kernel guest di M3 si avvia sotto l'oracolo fino alla shell:
//!
//! ```text
//! qemu-system-aarch64 -M virt,gic-version=3,its=off -cpu cortex-a53 -m 1G -nic none -nographic \
//!     -kernel Image -initrd initramfs.cpio.gz -append "console=ttyAMA0"
//! ```
//!
//! Senza qemu-system-aarch64 il test viene saltato, salvo
//! `VETRO_REQUIRE_SYSTEM_ORACLE=1`. Controlla, ciascuno entro `VETRO_BOOT_TIMEOUT`: il marcatore di `/init`,
//! la fine dell'autotest senza errori, una shell interattiva che esegue un
//! comando scritto sulla console, lo spegnimento con `poweroff -f` (PSCI).
//! Il log completo va in `target/guest-kernel/qemu-boot.log`; il riferimento
//! versionato è `guest/kernel/reference/qemu-boot.log`.

use std::process::Command;
use std::time::Duration;
use vetro_boot_tests::*;

#[test]
fn qemu_boots_guest_kernel_to_shell() {
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let Some(qemu) = qemu_system() else {
        return skip_or_fail(
            "VETRO_REQUIRE_SYSTEM_ORACLE",
            "qemu-system-aarch64 assente (su macOS: VETRO_QEMU_SYSTEM_AARCH64=tools/guest-kernel/qemu-system-aarch64-docker.sh)",
        );
    };
    let mut cmd = Command::new(qemu);
    cmd.args(QEMU_MACHINE)
        .args(["-nographic", "-kernel"])
        .arg(&image)
        .arg("-initrd")
        .arg(&initrd)
        .args(["-append", "console=ttyAMA0"]);
    let limit = timeout();
    let mut con = Console::spawn(cmd).expect("avvio di qemu-system-aarch64");

    let report = |con: &Console, what: &str| -> String {
        let log = con.log();
        let tail: String =
            log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        format!("{what} dopo {:.1} s; ultime righe della console:\n{tail}", con.elapsed().as_secs_f64())
    };

    let Some(at) = con.wait_for(BOOT_MARKER, 0, limit) else {
        panic!("{}", report(&con, "nessun marcatore di avvio"))
    };
    let t_boot = con.elapsed();
    // La riga intera: la seriale può consegnare il marcatore prima dell'esito.
    let Some((end, line)) = con.wait_line(AUTOTEST_END, at, limit) else {
        panic!("{}", report(&con, "autotest non finito"))
    };
    let t_autotest = con.elapsed();
    assert_eq!(line, AUTOTEST_OK, "{}", report(&con, "autotest con errori"));

    // Shell interattiva: il risultato dell'espansione distingue l'uscita del
    // comando dall'eco del terminale.
    let Some(prompt) = con.wait_for("# ", end, limit) else { panic!("{}", report(&con, "nessun prompt")) };
    con.send("echo VETRO-SHELL-$((6*7))\n");
    if con.wait_for("VETRO-SHELL-42", prompt, limit).is_none() {
        panic!("{}", report(&con, "la shell non risponde"));
    }
    con.send("poweroff -f\n");
    let exited = con.finish(Duration::from_secs(30));
    let log = normalize(&con.log());

    let root = repo_root();
    std::fs::write(root.join("target/guest-kernel/qemu-boot.log"), &log).unwrap();
    if std::env::var("VETRO_BOOT_UPDATE_REFERENCE").is_ok_and(|v| v == "1") {
        let dir = root.join("guest/kernel/reference");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("qemu-boot.log"), &log).unwrap();
    }
    eprintln!(
        "QEMU: /init a {:.1} s, autotest finito a {:.1} s, spento a {:.1} s",
        t_boot.as_secs_f64(),
        t_autotest.as_secs_f64(),
        con.elapsed().as_secs_f64()
    );
    assert!(exited, "QEMU non si è spento dopo poweroff -f");
}
