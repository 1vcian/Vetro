//! Accessi esclusivi, acquire/release e monitor locale.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::{SIGBUS, case};

#[test]
fn ldxr_stxr_success() {
    case(
        "ldxr_stxr_success",
        &[
            0xc85f7f80, // ldxr x0, [x28]
            0x91000400, // add x0, x0, #0x1
            0xc8017f80, // stxr w1, x0, [x28]
            0xf9400382, // ldr x2, [x28]
        ],
    )
    .mem(0, &41u64.to_le_bytes())
    .want_x(1, 0)
    .want_x(2, 42)
    .run();
}

#[test]
fn stxr_without_monitor_fails() {
    case(
        "stxr_without_monitor_fails",
        &[
            0xc8017f80, // stxr w1, x0, [x28]
            0xf9400382, // ldr x2, [x28]
        ],
    )
    .x(0, 5)
    .want_x(1, 1)
    .want_x(2, 0)
    .run();
}

#[test]
fn clrex_clears_monitor() {
    case(
        "clrex_clears_monitor",
        &[
            0x885fff80, // ldaxr w0, [x28]
            0xd5033f5f, // clrex
            0x8801ff80, // stlxr w1, w0, [x28]
        ],
    )
    .want_x(1, 1)
    .run();
}

#[test]
fn second_stxr_fails() {
    case(
        "second_stxr_fails",
        &[
            0x085f7f80, // ldxrb w0, [x28]
            0x08017f80, // stxrb w1, w0, [x28]
            0x08027f80, // stxrb w2, w0, [x28]
        ],
    )
    .want_x(1, 0)
    .want_x(2, 1)
    .run();
}

#[test]
fn stxr_after_value_change_fails() {
    case(
        "stxr_after_value_change_fails",
        &[
            0x885f7f80, // ldxr w0, [x28]
            0x528000e3, // mov w3, #0x7                // =7
            0xb9000383, // str w3, [x28]
            0x88017f80, // stxr w1, w0, [x28]
        ],
    )
    .want_x(1, 1)
    .run();
}

#[test]
fn ldxp_stxp() {
    case(
        "ldxp_stxp",
        &[
            0xc87f0780, // ldxp x0, x1, [x28]
            0xc8220381, // stxp w2, x1, x0, [x28]
            0xa9401383, // ldp x3, x4, [x28]
        ],
    )
    .mem(0, &[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0])
    .want_x(0, 1)
    .want_x(1, 2)
    .want_x(2, 0)
    .want_x(3, 2)
    .want_x(4, 1)
    .run();
}

#[test]
fn ldxp_w_pair() {
    case(
        "ldxp_w_pair",
        &[
            0x887f0780, // ldxp w0, w1, [x28]
        ],
    )
    .mem(0, &[1, 2, 3, 4, 5, 6, 7, 8])
    .want_x(0, 0x0403_0201)
    .want_x(1, 0x0807_0605)
    .run();
}

#[test]
fn ldxr_unaligned_is_sigbus() {
    case(
        "ldxr_unaligned_is_sigbus",
        &[
            0x91001381, // add x1, x28, #0x4
            0xc85f7c20, // ldxr x0, [x1]
        ],
    )
    .want_signal(SIGBUS)
    .run();
}

#[test]
fn ldar_stlr() {
    case(
        "ldar_stlr",
        &[
            0x889fff81, // stlr w1, [x28]
            0xc8dfff82, // ldar x2, [x28]
            0x489fff81, // stlrh w1, [x28]
            0x08dfff83, // ldarb w3, [x28]
        ],
    )
    .x(1, 0x1234_5678)
    .want_x(2, 0x1234_5678)
    .want_x(3, 0x78)
    .run();
}

#[test]
fn ldar_unaligned_is_sigbus() {
    case(
        "ldar_unaligned_is_sigbus",
        &[
            0x91000b81, // add x1, x28, #0x2
            0x88dffc20, // ldar w0, [x1]
        ],
    )
    .want_signal(SIGBUS)
    .run();
}
