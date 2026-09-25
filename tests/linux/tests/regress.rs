//! Regressioni del kernel emulato trovate in revisione: `tests/linux/c/
//! regress.c` stampa l'esito di ogni caso, e Vetro deve stampare quello che
//! stampa QEMU (che passa le syscall al kernel dell'host).

use vetro_linux_tests::{case, guest_bin};

#[test]
fn regress() {
    let Some(bin) = guest_bin("regress", "regress") else { return };
    case("regress", &bin, &["regress"]).check();
}
