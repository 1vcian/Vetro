//! Operazioni logiche con immediati a maschera di bit e registri.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::case;

#[test]
fn and_orr_eor_imm() {
    case(
        "and_orr_eor_imm",
        &[
            0x92401c20, // and x0, x1, #0xff
            0x3200f062, // orr w2, w3, #0x55555555
            0xd2103ca4, // eor x4, x5, #0xffff0000ffff0000
        ],
    )
    .x(1, 0x1234)
    .x(3, 0x8000_0000)
    .x(5, 0xffffffffffffffff)
    .want_x(0, 0x34)
    .want_x(2, 0xd555_5555)
    .want_x(4, 0x0000_ffff_0000_ffff)
    .run();
}

#[test]
fn orr_imm_to_sp() {
    case(
        "orr_imm_to_sp",
        &[
            0xb26903ff, // orr sp, xzr, #0x800000
            0x910003e0, // mov x0, sp
            0xd2a01001, // mov x1, #0x800000           // =8388608
            0xf2880001, // movk x1, #0x4000
            0x9100003f, // mov sp, x1
        ],
    )
    .want_x(0, 0x80_0000)
    .run();
}

#[test]
fn ands_sets_n_and_z() {
    case(
        "ands_sets_n_and_z",
        &[
            0xf2410020, // ands x0, x1, #0x8000000000000000
            0xd53b4202, // mrs x2, NZCV
            0x7200007f, // tst w3, #0x1
        ],
    )
    .flags(0b0011)
    .x(1, 0xffffffffffffffff)
    .x(3, 2)
    .want_x(0, 0x8000_0000_0000_0000)
    .want_x(2, 0x8000_0000)
    .want_flags(0b0100)
    .run();
}

#[test]
fn logical_shifted() {
    case(
        "logical_shifted",
        &[
            0x8a022020, // and x0, x1, x2, lsl #8
            0x2ac51083, // orr w3, w4, w5, ror #4
            0xca88f0e6, // eor x6, x7, x8, asr #60
            0xaa4a07e9, // orr x9, xzr, x10, lsr #1
        ],
    )
    .x(1, 0xff00)
    .x(2, 0xff)
    .x(4, 0)
    .x(5, 0x1f)
    .x(7, 0)
    .x(8, 0x8000_0000_0000_0000)
    .x(10, 3)
    .want_x(0, 0xff00)
    .want_x(3, 0xf000_0001)
    .want_x(6, 0xffff_ffff_ffff_fff8)
    .want_x(9, 1)
    .run();
}

#[test]
fn inverted_ops() {
    case(
        "inverted_ops",
        &[
            0x8a220020, // bic x0, x1, x2
            0x2a250083, // orn w3, w4, w5
            0xca2800e6, // eon x6, x7, x8
            0x6a2b0149, // bics w9, w10, w11
            0xaa2d03ec, // mvn x12, x13
        ],
    )
    .x(1, 0xff)
    .x(2, 0x0f)
    .x(4, 0)
    .x(5, 0xffff_0000)
    .x(7, 0)
    .x(8, 0)
    .x(10, 0xf0)
    .x(11, 0xffff_ff0f)
    .x(13, 1)
    .want_x(0, 0xf0)
    .want_x(3, 0xffff)
    .want_x(6, 0xffffffffffffffff)
    .want_x(9, 0xf0)
    .want_x(12, 0xfffffffffffffffe)
    .want_flags(0b0000)
    .run();
}

#[test]
fn mov_register_w_clears_upper() {
    case(
        "mov_register_w_clears_upper",
        &[
            0x2a0103e0, // mov w0, w1
        ],
    )
    .x(1, 0xdead_beef_1234_5678)
    .want_x(0, 0x1234_5678)
    .run();
}
