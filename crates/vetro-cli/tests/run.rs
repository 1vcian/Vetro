//! `vetro run` executes a static ELF: guest output and exit code.

use std::process::Command;
use vetro_diff::a64::{self, sys};
use vetro_diff::{elf, qemu};

fn hello_exit7() -> Vec<u8> {
    let msg = b"ciao da vetro\n";
    let n = 1 + 4 + 5 + 2 + 1;
    let mut code = vec![a64::movz(0, 1, 0)];
    code.extend(a64::mov64(1, elf::data_addr(n, 0)));
    code.extend([
        a64::movz(2, msg.len() as u16, 0),
        a64::movz(8, sys::WRITE, 0),
        a64::svc(0),
        a64::NOP,
        a64::NOP,
        a64::movz(0, 7, 0),
        a64::movz(8, sys::EXIT, 0),
        a64::svc(0),
    ]);
    assert_eq!(code.len(), n);
    elf::build(&code, msg)
}

#[test]
fn run_prints_and_exits() {
    let path = qemu::write_temp_elf("cli-hello", &hello_exit7()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vetro")).arg("run").arg(&path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ciao da vetro\n");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn run_reports_sigill() {
    // udf #0 → the process dies of SIGILL: code 128 + 4, like a shell.
    let path = qemu::write_temp_elf("cli-udf", &elf::build(&[0], &[])).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vetro")).arg("run").arg(&path).output().unwrap();
    assert_eq!(out.status.code(), Some(132));
    assert!(String::from_utf8_lossy(&out.stderr).contains("signal 4"));
}
