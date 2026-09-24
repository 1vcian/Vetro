//! Divisioni, shift variabili, bit, moltiplicazioni e CRC32.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::case;

#[test]
fn udiv_sdiv() {
    case(
        "udiv_sdiv",
        &[
            0x9ac20820, // udiv x0, x1, x2
            0x9ac50c83, // sdiv x3, x4, x5
            0x1adf08e6, // udiv w6, w7, wzr
            0x9aca0d28, // sdiv x8, x9, x10
            0x1acd0d8b, // sdiv w11, w12, w13
        ],
    )
    .x(1, 100)
    .x(2, 7)
    .x(4, 0xffffffffffffff9c)
    .x(5, 7)
    .x(7, 5)
    .x(9, 0x8000_0000_0000_0000)
    .x(10, 0xffffffffffffffff)
    .x(12, 0x8000_0000)
    .x(13, 0xffff_ffff)
    .want_x(0, 14)
    .want_x(3, 0xfffffffffffffff2)
    .want_x(6, 0)
    .want_x(8, 0x8000_0000_0000_0000)
    .want_x(11, 0x8000_0000)
    .run();
}

#[test]
fn variable_shifts() {
    case(
        "variable_shifts",
        &[
            0x9ac22020, // lsl x0, x1, x2
            0x1ac52483, // lsr w3, w4, w5
            0x9ac828e6, // asr x6, x7, x8
            0x1acb2d49, // ror w9, w10, w11
        ],
    )
    .x(1, 1)
    .x(2, 65)
    .x(4, 0x8000_0000)
    .x(5, 63)
    .x(7, 0x8000_0000_0000_0000)
    .x(8, 4)
    .x(10, 1)
    .x(11, 33)
    .want_x(0, 2)
    .want_x(3, 1)
    .want_x(6, 0xf800_0000_0000_0000)
    .want_x(9, 0x8000_0000)
    .run();
}

#[test]
fn bit_ops() {
    case(
        "bit_ops",
        &[
            0xdac00020, // rbit x0, x1
            0x5ac00062, // rbit w2, w3
            0xdac010a4, // clz x4, x5
            0x5ac013e6, // clz w6, wzr
            0xdac01507, // cls x7, x8
            0x5ac01549, // cls w9, w10
        ],
    )
    .x(1, 1)
    .x(3, 1)
    .x(5, 0x100)
    .x(8, 0xffffffffffffffff)
    .x(10, 0x0000_ffff)
    .want_x(0, 0x8000_0000_0000_0000)
    .want_x(2, 0x8000_0000)
    .want_x(4, 55)
    .want_x(6, 32)
    .want_x(7, 63)
    .want_x(9, 15)
    .run();
}

#[test]
fn byte_reverse() {
    case(
        "byte_reverse",
        &[
            0xdac00c20, // rev x0, x1
            0x5ac00862, // rev w2, w3
            0xdac00424, // rev16 x4, x1
            0xdac00825, // rev32 x5, x1
        ],
    )
    .x(1, 0x0102_0304_0506_0708)
    .x(3, 0x1122_3344)
    .want_x(0, 0x0807_0605_0403_0201)
    .want_x(2, 0x4433_2211)
    .want_x(4, 0x0201_0403_0605_0807)
    .want_x(5, 0x0403_0201_0807_0605)
    .run();
}

#[test]
fn multiply() {
    case(
        "multiply",
        &[
            0x9b020c20, // madd x0, x1, x2, x3
            0x1b069ca4, // msub w4, w5, w6, w7
            0x9b027c28, // mul x8, x1, x2
            0x9b02fc29, // mneg x9, x1, x2
        ],
    )
    .x(1, 6)
    .x(2, 7)
    .x(3, 100)
    .x(5, 0x10000)
    .x(6, 0x10000)
    .x(7, 5)
    .want_x(0, 142)
    .want_x(4, 5)
    .want_x(8, 42)
    .want_x(9, 0xffffffffffffffd6)
    .run();
}

#[test]
fn long_multiply() {
    case(
        "long_multiply",
        &[
            0x9b227c20, // smull x0, w1, w2
            0x9ba27c23, // umull x3, w1, w2
            0x9b221424, // smaddl x4, w1, w2, x5
            0x9ba29c26, // umsubl x6, w1, w2, x7
        ],
    )
    .x(1, 0xffff_ffff)
    .x(2, 2)
    .x(5, 10)
    .x(7, 0x2_0000_0000)
    .want_x(0, 0xfffffffffffffffe)
    .want_x(3, 0x1_ffff_fffe)
    .want_x(4, 8)
    .want_x(6, 2)
    .run();
}

#[test]
fn multiply_high() {
    case(
        "multiply_high",
        &[
            0x9b427c20, // smulh x0, x1, x2
            0x9bc27c23, // umulh x3, x1, x2
            0x9bc57ca4, // umulh x4, x5, x5
        ],
    )
    .x(1, 0xffffffffffffffff)
    .x(2, 2)
    .x(5, 0x1_0000_0000)
    .want_x(0, 0xffffffffffffffff)
    .want_x(3, 1)
    .want_x(4, 1)
    .run();
}

#[test]
fn crc32_check_value() {
    case(
        "crc32_check_value",
        &[
            0x12800000, // mov w0, #-0x1               // =-1
            0x9ac14c00, // crc32x w0, w0, x1
            0x1ac24000, // crc32b w0, w0, w2
            0x2a2003e0, // mvn w0, w0
            0x12800003, // mov w3, #-0x1               // =-1
            0x9ac15c63, // crc32cx w3, w3, x1
            0x1ac25063, // crc32cb w3, w3, w2
            0x2a2303e3, // mvn w3, w3
        ],
    )
    .x(1, u64::from_le_bytes(*b"12345678"))
    .x(2, b'9' as u64)
    .want_x(0, 0xcbf4_3926)
    .want_x(3, 0xe306_9283)
    .run();
}

#[test]
fn crc32_halfword_word() {
    case(
        "crc32_halfword_word",
        &[
            0x12800000, // mov w0, #-0x1               // =-1
            0x1ac14400, // crc32h w0, w0, w1
            0x1ac24800, // crc32w w0, w0, w2
            0x1ac34400, // crc32h w0, w0, w3
            0x1ac44000, // crc32b w0, w0, w4
            0x2a2003e0, // mvn w0, w0
        ],
    )
    .x(1, u16::from_le_bytes(*b"12") as u64 | 0xffff_0000)
    .x(2, u32::from_le_bytes(*b"3456") as u64)
    .x(3, u16::from_le_bytes(*b"78") as u64)
    .x(4, b'9' as u64)
    .want_x(0, 0xcbf4_3926)
    .run();
}
