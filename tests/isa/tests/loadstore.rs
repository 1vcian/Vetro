//! Load e store: dimensioni, estensioni, indirizzamenti, coppie, letterali.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::{SIGSEGV, case};

#[test]
fn store_load_sizes() {
    case(
        "store_load_sizes",
        &[
            0xf9000381, // str x1, [x28]
            0x39400382, // ldrb w2, [x28]
            0x79400783, // ldrh w3, [x28, #0x2]
            0xb9400784, // ldr w4, [x28, #0x4]
            0xf9400385, // ldr x5, [x28]
        ],
    )
    .x(1, 0x8877_6655_4433_2211)
    .want_x(2, 0x11)
    .want_x(3, 0x4433)
    .want_x(4, 0x8877_6655)
    .want_x(5, 0x8877_6655_4433_2211)
    .want_mem(0, &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88])
    .run();
}

#[test]
fn sign_extending_loads() {
    case(
        "sign_extending_loads",
        &[
            0x39800380, // ldrsb x0, [x28]
            0x39c00381, // ldrsb w1, [x28]
            0x79800782, // ldrsh x2, [x28, #0x2]
            0x79c00783, // ldrsh w3, [x28, #0x2]
            0xb9800784, // ldrsw x4, [x28, #0x4]
        ],
    )
    .mem(0, &[0x80, 0, 0x00, 0x90, 0xff, 0xff, 0xff, 0xff])
    .want_x(0, 0xffff_ffff_ffff_ff80)
    .want_x(1, 0xffff_ff80)
    .want_x(2, 0xffff_ffff_ffff_9000)
    .want_x(3, 0xffff_9000)
    .want_x(4, 0xffffffffffffffff)
    .run();
}

#[test]
fn narrow_stores() {
    case(
        "narrow_stores",
        &[
            0x39000381, // strb w1, [x28]
            0x79000781, // strh w1, [x28, #0x2]
            0xb9000781, // str w1, [x28, #0x4]
        ],
    )
    .x(1, 0xaabb_ccdd)
    .want_mem(0, &[0xdd, 0, 0xdd, 0xcc, 0xdd, 0xcc, 0xbb, 0xaa])
    .run();
}

#[test]
fn pre_post_index() {
    case(
        "pre_post_index",
        &[
            0xaa1c03e1, // mov x1, x28
            0xf8010c22, // str x2, [x1, #0x10]!
            0xf85f8423, // ldr x3, [x1], #-0x8
            0xf8408024, // ldur x4, [x1, #0x8]
            0xcb1c0025, // sub x5, x1, x28
        ],
    )
    .x(2, 0x1234)
    .want_x(3, 0x1234)
    .want_x(4, 0x1234)
    .want_x(5, 8)
    .want_mem(16, &0x1234u64.to_le_bytes())
    .run();
}

#[test]
fn register_offset() {
    case(
        "register_offset",
        &[
            0xf8617b80, // ldr x0, [x28, x1, lsl #3]
            0xb863db82, // ldr w2, [x28, w3, sxtw #2]
            0x38654b84, // ldrb w4, [x28, w5, uxtw]
            0x78276b86, // strh w6, [x28, x7]
        ],
    )
    .mem(16, &0xaaaa_bbbb_cccc_ddddu64.to_le_bytes())
    .mem(-8, &[1, 2, 3, 4])
    .x(1, 2)
    .x(3, 0xffff_fffe)
    .x(5, 0x1_0000_0012)
    .x(6, 0x5566)
    .x(7, 0x100)
    .want_x(0, 0xaaaa_bbbb_cccc_dddd)
    .want_x(2, 0x0403_0201)
    .want_x(4, 0xcc)
    .want_mem(0x100, &[0x66, 0x55])
    .run();
}

#[test]
fn unaligned_access_allowed() {
    case(
        "unaligned_access_allowed",
        &[
            0xf8003381, // stur x1, [x28, #0x3]
            0xf8403382, // ldur x2, [x28, #0x3]
            0xb8401383, // ldur w3, [x28, #0x1]
        ],
    )
    .x(1, 0x0102_0304_0506_0708)
    .want_x(2, 0x0102_0304_0506_0708)
    .want_x(3, 0x0708_0000)
    .run();
}

#[test]
fn pairs() {
    case(
        "pairs",
        &[
            0xa93f0b81, // stp x1, x2, [x28, #-0x10]
            0x297e1383, // ldp w3, w4, [x28, #-0x10]
            0x697f1b85, // ldpsw x5, x6, [x28, #-0x8]
            0xaa1c03e7, // mov x7, x28
            0x298108e1, // stp w1, w2, [x7, #0x8]!
            0xa8c124e8, // ldp x8, x9, [x7], #0x10
            0xcb1c00ea, // sub x10, x7, x28
        ],
    )
    .x(1, 0x1111_1111_8000_0000)
    .x(2, 0xffff_ffff_7fff_ffff)
    .want_x(3, 0x8000_0000)
    .want_x(4, 0x1111_1111)
    .want_x(5, 0x7fff_ffff)
    .want_x(6, 0xffffffffffffffff)
    .want_x(8, 0x7fff_ffff_8000_0000)
    .want_x(9, 0)
    .want_x(10, 24)
    .run();
}

#[test]
fn literal_loads() {
    case(
        "literal_loads",
        &[
            0x580000a0, // ldr x0, 0x14 <.text+0x14>
            0x18000081, // ldr w1, 0x14 <.text+0x14>
            0x98000062, // ldrsw x2, 0x14 <.text+0x14>
            0xd8000040, // prfm pldl1keep, 0x14 <.text+0x14>
            0x14000003, // b 0x1c <.text+0x1c>
            0x89abcdef, // .word 0x89abcdef
            0x01234567, // .word 0x01234567
            0xd503201f, // nop
        ],
    )
    .want_x(0, 0x0123_4567_89ab_cdef)
    .want_x(1, 0x89ab_cdef)
    .want_x(2, 0xffff_ffff_89ab_cdef)
    .run();
}

#[test]
fn load_to_xzr_discards() {
    case(
        "load_to_xzr_discards",
        &[
            0xf940039f, // ldr xzr, [x28]
            0xaa1f03e0, // mov x0, xzr
        ],
    )
    .mem(0, &[0xff; 8])
    .want_x(0, 0)
    .run();
}

#[test]
fn prfm_unmapped_does_not_fault() {
    case(
        "prfm_unmapped_does_not_fault",
        &[
            0xf9800020, // prfm pldl1keep, [x1]
        ],
    )
    .x(1, 0xdead_0000)
    .want_x(1, 0xdead_0000)
    .run();
}

#[test]
fn load_unmapped_is_sigsegv() {
    case(
        "load_unmapped_is_sigsegv",
        &[
            0xf9400020, // ldr x0, [x1]
        ],
    )
    .x(1, 0xdead_0000)
    .want_signal(SIGSEGV)
    .run();
}

#[test]
fn store_to_code_is_sigsegv() {
    case(
        "store_to_code_is_sigsegv",
        &[
            0x10000001, // adr x1, 0x0 <.text>
            0xf9000022, // str x2, [x1]
        ],
    )
    .want_signal(SIGSEGV)
    .run();
}
