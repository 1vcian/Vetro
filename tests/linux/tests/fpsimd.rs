//! Floating-point and SIMD workload (`tests/linux/c/fpsimd.c`, M4): the same
//! summary of the results on Vetro (with the interpreter and, with `VETRO_JIT=1`,
//! with the JIT) and on QEMU.

use vetro_linux_tests::{case, guest_bin};

#[test]
fn fpsimd() {
    let Some(bin) = guest_bin("fpsimd", "fpsimd") else { return };
    case("fpsimd", &bin, &["fpsimd"]).check();
}
