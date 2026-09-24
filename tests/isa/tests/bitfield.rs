//! Move wide, bitfield, estrazione e indirizzi relativi al PC.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::{BODY, case};

#[test]
fn move_wide() {
    case(
        "move_wide",
        &[
            0xd2e24680, // mov x0, #0x1234000000000000 // =1311673391471656960
            0x12800001, // mov w1, #-0x1               // =-1
            0x92a01fe2, // mov x2, #-0xff0001          // =-16711681
            0xf2b7dde3, // movk x3, #0xbeef, lsl #16
            0x72800024, // movk w4, #0x1
        ],
    )
    .x(3, 0xffffffffffffffff)
    .x(4, 0xffff_ffff_ffff_0000)
    .want_x(0, 0x1234_0000_0000_0000)
    .want_x(1, 0xffff_ffff)
    .want_x(2, 0xffff_ffff_ff00_ffff)
    .want_x(3, 0xffff_ffff_beef_ffff)
    .want_x(4, 0xffff_0001)
    .run();
}

#[test]
fn ubfx_sbfx() {
    case(
        "ubfx_sbfx",
        &[
            0xd3442c20, // ubfx x0, x1, #4, #8
            0x93483c22, // sbfx x2, x1, #8, #8
            0x13000083, // sbfx w3, w4, #0, #1
            0xd3440cc5, // lsl x5, x6, #60
        ],
    )
    .x(1, 0x8765_43f1)
    .x(4, 1)
    .x(6, 0xff)
    .want_x(0, 0x3f)
    .want_x(2, 0x43)
    .want_x(3, 0xffff_ffff)
    .want_x(5, 0xf000_0000_0000_0000)
    .run();
}

#[test]
fn bfi_bfxil() {
    case(
        "bfi_bfxil",
        &[
            0xb3783c20, // bfi x0, x1, #8, #16
            0x331c7c62, // bfxil w2, w3, #28, #4
        ],
    )
    .x(0, 0xffffffffffffffff)
    .x(1, 0x1234)
    .x(2, 0xffff_ff00)
    .x(3, 0xa000_0000)
    .want_x(0, 0xffff_ffff_ff12_34ff)
    .want_x(2, 0xffff_ff0a)
    .run();
}

#[test]
fn shift_aliases() {
    case(
        "shift_aliases",
        &[
            0xd3410020, // lsl x0, x1, #63
            0x531f7c62, // lsr w2, w3, #31
            0x937ffca4, // asr x4, x5, #63
            0x13047ce6, // asr w6, w7, #4
        ],
    )
    .x(1, 3)
    .x(3, 0x8000_0000)
    .x(5, 0x8000_0000_0000_0000)
    .x(7, 0x8000_0000)
    .want_x(0, 0x8000_0000_0000_0000)
    .want_x(2, 1)
    .want_x(4, 0xffffffffffffffff)
    .want_x(6, 0xf800_0000)
    .run();
}

#[test]
fn extend_aliases() {
    case(
        "extend_aliases",
        &[
            0x93407c20, // sxtw x0, w1
            0x13001c62, // sxtb w2, w3
            0x53003ca4, // uxth w4, w5
            0x93403ce6, // sxth x6, w7
        ],
    )
    .x(1, 0x8000_0000)
    .x(3, 0x80)
    .x(5, 0x1_2345)
    .x(7, 0x7fff)
    .want_x(0, 0xffff_ffff_8000_0000)
    .want_x(2, 0xffff_ff80)
    .want_x(4, 0x2345)
    .want_x(6, 0x7fff)
    .run();
}

#[test]
fn extr_ror() {
    case(
        "extr_ror",
        &[
            0x93c21020, // extr x0, x1, x2, #0x4
            0x13842083, // ror w3, w4, #0x8
            0x13877cc5, // extr w5, w6, w7, #0x1f
        ],
    )
    .x(1, 0xf)
    .x(2, 0x10)
    .x(4, 0x1234_5678)
    .x(6, 1)
    .x(7, 0x8000_0000)
    .want_x(0, 0xf000_0000_0000_0001)
    .want_x(3, 0x7812_3456)
    .want_x(5, 3)
    .run();
}

#[test]
fn adr_adrp() {
    case(
        "adr_adrp",
        &[
            0x10000000, // adr x0, 0x0 <.text>
            0x10000041, // adr x1, 0xc <.text+0xc>
            0x90000002, // adrp x2, 0x0 <.text>
            0xb0000003, // adrp x3, 0x1000 <.text+0x1000>
        ],
    )
    .want_x(0, BODY)
    .want_x(1, BODY + 12)
    .want_x(2, BODY & !0xfff)
    .want_x(3, (BODY & !0xfff) + 0x1000)
    .run();
}
