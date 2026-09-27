//! Regressions of the emulated kernel found in review: `tests/linux/c/
//! regress.c` prints the outcome of each case, and Vetro must print what
//! QEMU prints (which passes the syscalls to the host kernel).

use vetro_linux_tests::{case, guest_bin};

#[test]
fn regress() {
    let Some(bin) = guest_bin("regress", "regress") else { return };
    case("regress", &bin, &["regress"]).check();
}
