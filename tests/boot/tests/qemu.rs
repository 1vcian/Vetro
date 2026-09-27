//! The M3 guest kernel boots under the oracle up to the shell:
//!
//! ```text
//! qemu-system-aarch64 -M virt,gic-version=3,its=off -cpu cortex-a53 -m 1G -nic none \
//!     -global virtio-mmio.force-legacy=false -device virtio-gpu-device \
//!     -device virtio-keyboard-device -device virtio-tablet-device \
//!     -netdev user,id=n -device virtio-net-device,netdev=n -nographic \
//!     -kernel Image -initrd initramfs.cpio.gz -append "console=ttyAMA0"
//! ```
//!
//! Without qemu-system-aarch64 the test is skipped, unless
//! `VETRO_REQUIRE_SYSTEM_ORACLE=1`. It checks, each within `VETRO_BOOT_TIMEOUT`: the `/init` marker,
//! the end of the self-test without errors, an interactive shell that runs a
//! command written on the console, the power-off with `poweroff -f` (PSCI).
//! The complete log goes to `target/guest-kernel/qemu-boot.log`; the versioned
//! reference is `guest/kernel/reference/qemu-boot.log`.

use std::process::Command;
use std::time::Duration;
use vetro_boot_tests::*;

#[test]
fn qemu_boots_guest_kernel_to_shell() {
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
    };
    let Some(qemu) = qemu_system() else {
        return skip_or_fail(
            "VETRO_REQUIRE_SYSTEM_ORACLE",
            "qemu-system-aarch64 missing (on macOS: VETRO_QEMU_SYSTEM_AARCH64=tools/guest-kernel/qemu-system-aarch64-docker.sh)",
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
    let mut con = Console::spawn(cmd).expect("boot of qemu-system-aarch64");

    let report = |con: &Console, what: &str| -> String {
        let log = con.log();
        let tail: String =
            log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        format!("{what} after {:.1} s; last lines of the console:\n{tail}", con.elapsed().as_secs_f64())
    };

    let Some(at) = con.wait_for(BOOT_MARKER, 0, limit) else { panic!("{}", report(&con, "no boot marker")) };
    let t_boot = con.elapsed();
    // The whole line: the serial port can deliver the marker before the outcome.
    let Some((end, line)) = con.wait_line(AUTOTEST_END, at, limit) else {
        panic!("{}", report(&con, "self-test not finished"))
    };
    let t_autotest = con.elapsed();
    assert_eq!(line, AUTOTEST_OK, "{}", report(&con, "self-test with errors"));

    // Interactive shell: the result of the expansion distinguishes the output of the
    // command from the terminal's echo.
    // Input starts only at a complete prompt (SHELL_PROMPT), as under Vetro.
    let Some(prompt) = con.wait_for(SHELL_PROMPT, end, limit) else {
        panic!("{}", report(&con, "no prompt"))
    };
    con.send("echo VETRO-SHELL-$((6*7))\n");
    let Some(out) = con.wait_for("VETRO-SHELL-42", prompt, limit) else {
        panic!("{}", report(&con, "the shell does not respond"))
    };
    if con.wait_for(SHELL_PROMPT, out, limit).is_none() {
        panic!("{}", report(&con, "no prompt after the command"));
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
        "QEMU: /init at {:.1} s, self-test finished at {:.1} s, powered off at {:.1} s",
        t_boot.as_secs_f64(),
        t_autotest.as_secs_f64(),
        con.elapsed().as_secs_f64()
    );
    assert!(exited, "QEMU did not power off after poweroff -f");
}
