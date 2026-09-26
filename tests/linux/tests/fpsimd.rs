//! Carico di virgola mobile e SIMD (`tests/linux/c/fpsimd.c`, M4): lo stesso
//! riassunto dei risultati su Vetro (con l'interprete e, con `VETRO_JIT=1`,
//! col JIT) e su QEMU.

use vetro_linux_tests::{case, guest_bin};

#[test]
fn fpsimd() {
    let Some(bin) = guest_bin("fpsimd", "fpsimd") else { return };
    case("fpsimd", &bin, &["fpsimd"]).check();
}
