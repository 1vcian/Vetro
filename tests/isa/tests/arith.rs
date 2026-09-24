//! Addizioni, sottrazioni, flag, confronti condizionali e selezioni.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::case;

#[test]
fn add_imm() {
    case(
        "add_imm",
        &[
            0x91000420, // add x0, x1, #0x1
            0x113ffc62, // add w2, w3, #0xfff
            0x914004a4, // add x4, x5, #0x1, lsl #12   // =0x1000
        ],
    )
    .x(1, 41)
    .x(3, 0xffff_ffff)
    .x(5, 1)
    .want_x(0, 42)
    .want_x(2, 0xffe)
    .want_x(4, 0x1001)
    .run();
}

#[test]
fn add_imm_sp() {
    case(
        "add_imm_sp",
        &[
            0x910043e0, // add x0, sp, #0x10
            0xd10083ff, // sub sp, sp, #0x20
            0x910003e1, // mov x1, sp
        ],
    )
    .want_x(0, 0x804010)
    .want_x(1, 0x803fe0)
    .want_sp(0x803fe0)
    .run();
}

#[test]
fn adds_carry_and_zero() {
    case(
        "adds_carry_and_zero",
        &[
            0xb1000420, // adds x0, x1, #0x1
        ],
    )
    .x(1, 0xffffffffffffffff)
    .want_x(0, 0)
    .want_flags(0b0110)
    .run();
}

#[test]
fn adds_signed_overflow_32() {
    case(
        "adds_signed_overflow_32",
        &[
            0x31000420, // adds w0, w1, #0x1
        ],
    )
    .x(1, 0x7fff_ffff)
    .want_x(0, 0x8000_0000)
    .want_flags(0b1001)
    .run();
}

#[test]
fn subs_borrow() {
    case(
        "subs_borrow",
        &[
            0xf1000820, // subs x0, x1, #0x2
        ],
    )
    .x(1, 1)
    .want_x(0, 0xffffffffffffffff)
    .want_flags(0b1000)
    .run();
}

#[test]
fn cmp_equal_sets_zc() {
    case(
        "cmp_equal_sets_zc",
        &[
            0xf100143f, // cmp x1, #0x5
        ],
    )
    .x(1, 5)
    .want_flags(0b0110)
    .run();
}

#[test]
fn cmp_w_ignores_upper_bits() {
    case(
        "cmp_w_ignores_upper_bits",
        &[
            0x6b02003f, // cmp w1, w2
        ],
    )
    .x(1, 0xdead_0000_0007)
    .x(2, 7)
    .want_flags(0b0110)
    .run();
}

#[test]
fn add_shifted() {
    case(
        "add_shifted",
        &[
            0x8b021020, // add x0, x1, x2, lsl #4
            0x4b857c83, // sub w3, w4, w5, asr #31
            0x8b48fce6, // add x6, x7, x8, lsr #63
        ],
    )
    .x(1, 1)
    .x(2, 2)
    .x(4, 10)
    .x(5, 0x8000_0000)
    .x(7, 5)
    .x(8, 0xffffffffffffffff)
    .want_x(0, 33)
    .want_x(3, 11)
    .want_x(6, 6)
    .run();
}

#[test]
fn add_extended() {
    case(
        "add_extended",
        &[
            0x8b22c820, // add x0, x1, w2, sxtw #2
            0x8b2403e3, // add x3, sp, w4, uxtb
            0xcb27a4c5, // sub x5, x6, w7, sxth #1
            0x0b2a3128, // add w8, w9, w10, uxth #4
        ],
    )
    .x(1, 100)
    .x(2, 0xffff_fffe)
    .x(4, 0x1ff)
    .x(6, 0)
    .x(7, 0x8000)
    .x(9, 1)
    .x(10, 0x1_ffff)
    .want_x(0, 92)
    .want_x(3, 0x8040ff)
    .want_x(5, 0x10000)
    .want_x(8, 0xffff1)
    .run();
}

#[test]
fn adds_extended_to_xzr_is_cmn() {
    case(
        "adds_extended_to_xzr_is_cmn",
        &[
            0xab22c03f, // cmn x1, w2, sxtw
        ],
    )
    .x(1, 1)
    .x(2, 0xffff_ffff)
    .want_flags(0b0110)
    .run();
}

#[test]
fn adc_sbc() {
    case(
        "adc_sbc",
        &[
            0x9a020020, // adc x0, x1, x2
            0xda050083, // sbc x3, x4, x5
            0x3a0800e6, // adcs w6, w7, w8
            0xfa0b0149, // sbcs x9, x10, x11
        ],
    )
    .flags(0b0010)
    .x(1, 1)
    .x(2, 2)
    .x(4, 10)
    .x(5, 3)
    .x(7, 0xffff_ffff)
    .x(8, 0)
    .x(10, 5)
    .x(11, 5)
    .want_x(0, 4)
    .want_x(3, 7)
    .want_x(6, 0)
    .want_x(9, 0)
    .want_flags(0b0110)
    .run();
}

#[test]
fn sbc_without_carry() {
    case(
        "sbc_without_carry",
        &[
            0xda020020, // sbc x0, x1, x2
        ],
    )
    .flags(0b0000)
    .x(1, 10)
    .x(2, 3)
    .want_x(0, 6)
    .run();
}

#[test]
fn ngc_negs() {
    case(
        "ngc_negs",
        &[
            0xda0103e0, // ngc x0, x1
            0xeb0303e2, // negs x2, x3
        ],
    )
    .flags(0b0010)
    .x(1, 5)
    .x(3, 0x8000_0000_0000_0000)
    .want_x(0, 0xfffffffffffffffb)
    .want_x(2, 0x8000_0000_0000_0000)
    .want_flags(0b1001)
    .run();
}

#[test]
fn ccmp_true_compares() {
    case(
        "ccmp_true_compares",
        &[
            0xeb01003f, // cmp x1, x1
            0xfa430840, // ccmp x2, #0x3, #0x0, eq
        ],
    )
    .x(1, 7)
    .x(2, 3)
    .want_flags(0b0110)
    .run();
}

#[test]
fn ccmp_false_uses_nzcv_imm() {
    case(
        "ccmp_false_uses_nzcv_imm",
        &[
            0xf100003f, // cmp x1, #0x0
            0xfa430049, // ccmp x2, x3, #0x9, eq
        ],
    )
    .x(1, 1)
    .x(2, 3)
    .x(3, 3)
    .want_flags(0b1001)
    .run();
}

#[test]
fn ccmn_w() {
    case(
        "ccmn_w",
        &[
            0x3a41e820, // ccmn w1, #0x1, #0x0, al
        ],
    )
    .x(1, 0xffff_ffff)
    .want_flags(0b0110)
    .run();
}

#[test]
fn csel_family() {
    case(
        "csel_family",
        &[
            0xf100013f, // cmp x9, #0x0
            0x9a820020, // csel x0, x1, x2, eq
            0x9a821423, // csinc x3, x1, x2, ne
            0x5a821024, // csinv w4, w1, w2, ne
            0xda821425, // csneg x5, x1, x2, ne
            0x9a9f17e6, // cset x6, eq
            0x5a9f13e7, // csetm w7, eq
        ],
    )
    .x(9, 0)
    .x(1, 10)
    .x(2, 20)
    .want_x(0, 10)
    .want_x(3, 21)
    .want_x(4, 0xffff_ffeb)
    .want_x(5, 0xffffffffffffffec)
    .want_x(6, 1)
    .want_x(7, 0xffff_ffff)
    .run();
}

#[test]
fn conditions_all() {
    case(
        "conditions_all",
        &[
            0xd2800000, // mov x0, #0x0                // =0
            0x9a9f17e1, // cset x1, eq
            0x9a9f37e2, // cset x2, hs
            0x9a9f57e3, // cset x3, mi
            0x9a9f77e4, // cset x4, vs
            0x9a9f97e5, // cset x5, hi
            0x9a9fb7e6, // cset x6, ge
            0x9a9fd7e7, // cset x7, gt
        ],
    )
    .flags(0b1011)
    .want_x(1, 0)
    .want_x(2, 1)
    .want_x(3, 1)
    .want_x(4, 1)
    .want_x(5, 1)
    .want_x(6, 1)
    .want_x(7, 1)
    .run();
}
