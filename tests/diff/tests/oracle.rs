//! Criterio di uscita M0: l'oracolo QEMU si lancia su un ELF statico minimo
//! e se ne leggono codice di uscita e output.

use vetro_diff::a64::{self, sys};
use vetro_diff::{elf, qemu};

#[test]
fn qemu_exit_code() {
    let Some(q) = qemu::locate_or_skip("qemu_exit_code") else {
        return;
    };

    // mov x0, #42 ; mov x8, #93 ; svc #0
    let code = [a64::movz(0, 42, 0), a64::movz(8, sys::EXIT, 0), a64::svc(0)];
    let path = qemu::write_temp_elf("exit42", &elf::build(&code, &[])).unwrap();

    let out = qemu::run(&q, &path, qemu::DEFAULT_TIMEOUT).unwrap();
    assert_eq!(
        out.exit_code,
        Some(42),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn qemu_stdout() {
    let Some(q) = qemu::locate_or_skip("qemu_stdout") else {
        return;
    };

    let msg = b"vetro\n";
    // write(1, msg, len) ; exit(0)
    let n_insns = 1 + 4 + 1 + 1 + 1 + 1 + 1 + 1;
    let mut code = vec![a64::movz(0, 1, 0)];
    code.extend(a64::mov64(1, elf::data_addr(n_insns, 0)));
    code.extend([
        a64::movz(2, msg.len() as u16, 0),
        a64::movz(8, sys::WRITE, 0),
        a64::svc(0),
        a64::movz(0, 0, 0),
        a64::movz(8, sys::EXIT, 0),
        a64::svc(0),
    ]);
    assert_eq!(code.len(), n_insns);
    let path = qemu::write_temp_elf("hello", &elf::build(&code, msg)).unwrap();

    let out = qemu::run(&q, &path, qemu::DEFAULT_TIMEOUT).unwrap();
    assert_eq!(
        out.exit_code,
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, msg);
}
