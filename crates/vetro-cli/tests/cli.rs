//! The `vetro run` command: the CPUs seen by the guest don't depend on the host.

use std::process::Command;

fn busybox() -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guest-bins/busybox");
    if p.is_file() {
        return Some(p);
    }
    if std::env::var("VETRO_REQUIRE_GUEST_BINS").is_ok_and(|v| v == "1") {
        panic!("target/guest-bins/busybox missing (tools/guest-bins/build.sh)");
    }
    eprintln!("SKIP: target/guest-bins/busybox missing");
    None
}

fn nproc(extra: &[&str]) -> String {
    let bb = busybox().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vetro"))
        .arg("run")
        .args(extra)
        .arg(&bb)
        .arg("nproc")
        .output()
        .expect("vetro run");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cpu_del_guest_deterministiche() {
    if busybox().is_none() {
        return;
    }
    assert_eq!(nproc(&[]), "1\n", "one CPU by default, whatever the host");
    assert_eq!(nproc(&["--cpus=3"]), "3\n");
}
