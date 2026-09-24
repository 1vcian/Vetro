//! Salti: incondizionati, condizionati, su registro, con link.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::{BODY, case};

#[test]
fn b_skips() {
    case(
        "b_skips",
        &[
            0xd2800020, // mov x0, #0x1                // =1
            0x14000002, // b 0xc <.text+0xc>
            0xd2800040, // mov x0, #0x2                // =2
            0x91002800, // add x0, x0, #0xa
        ],
    )
    .want_x(0, 11)
    .run();
}

#[test]
fn bl_sets_link() {
    case(
        "bl_sets_link",
        &[
            0x94000002, // bl 0x8 <.text+0x8>
            0xd2800021, // mov x1, #0x1                // =1
            0xd2800042, // mov x2, #0x2                // =2
        ],
    )
    .want_x(30, BODY + 4)
    .want_x(1, 0)
    .want_x(2, 2)
    .run();
}

#[test]
fn b_cond_taken_and_not() {
    case(
        "b_cond_taken_and_not",
        &[
            0xf1000c3f, // cmp x1, #0x3
            0x5400004b, // b.lt 0xc <.text+0xc>
            0xd2800020, // mov x0, #0x1                // =1
            0x5400004a, // b.ge 0x14 <.text+0x14>
            0xd2800042, // mov x2, #0x2                // =2
            0xd503201f, // nop
        ],
    )
    .x(1, 2)
    .want_x(0, 0)
    .want_x(2, 2)
    .run();
}

#[test]
fn b_backward_loop() {
    case(
        "b_backward_loop",
        &[
            0xd2800000, // mov x0, #0x0                // =0
            0xd28000a1, // mov x1, #0x5                // =5
            0x8b010000, // add x0, x0, x1
            0xf1000421, // subs x1, x1, #0x1
            0x54ffffc1, // b.ne 0x8 <.text+0x8>
        ],
    )
    .want_x(0, 15)
    .want_x(1, 0)
    .want_flags(0b0110)
    .run();
}

#[test]
fn cbz_w_ignores_upper() {
    case(
        "cbz_w_ignores_upper",
        &[
            0x34000041, // cbz w1, 0x8 <.text+0x8>
            0xd2800020, // mov x0, #0x1                // =1
            0xb5000041, // cbnz x1, 0x10 <.text+0x10>
            0xd2800022, // mov x2, #0x1                // =1
            0xd503201f, // nop
        ],
    )
    .x(1, 0x1_0000_0000)
    .want_x(0, 0)
    .want_x(2, 0)
    .run();
}

#[test]
fn tbz_tbnz() {
    case(
        "tbz_tbnz",
        &[
            0xb7f80041, // tbnz x1, #0x3f, 0x8 <.text+0x8>
            0xd2800020, // mov x0, #0x1                // =1
            0x36000041, // tbz w1, #0x0, 0x10 <.text+0x10>
            0xd2800022, // mov x2, #0x1                // =1
            0xb6f00041, // tbz x1, #0x3e, 0x18 <.text+0x18>
            0xd2800023, // mov x3, #0x1                // =1
            0xd503201f, // nop
        ],
    )
    .x(1, 0x8000_0000_0000_0000)
    .want_x(0, 0)
    .want_x(2, 0)
    .want_x(3, 0)
    .run();
}

#[test]
fn br_blr_ret() {
    case(
        "br_blr_ret",
        &[
            0x10000061, // adr x1, 0xc <.text+0xc>
            0xd61f0020, // br x1
            0xd2800020, // mov x0, #0x1                // =1
            0x10000062, // adr x2, 0x18 <.text+0x18>
            0xd63f0040, // blr x2
            0x14000003, // b 0x20 <.text+0x20>
            0xd2800063, // mov x3, #0x3                // =3
            0xd65f03c0, // ret
            0xd503201f, // nop
        ],
    )
    .want_x(0, 0)
    .want_x(3, 3)
    .want_x(30, BODY + 20)
    .run();
}

#[test]
fn blr_x30_uses_old_value() {
    case(
        "blr_x30_uses_old_value",
        &[
            0x1000005e, // adr x30, 0x8 <.text+0x8>
            0xd63f03c0, // blr x30
            0xaa1e03e0, // mov x0, x30
        ],
    )
    .want_x(0, BODY + 8)
    .run();
}
