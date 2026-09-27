//! Vector logical operations: `size` selects the operation, so they are
//! valid also with size = 11 and Q = 0 (bug found by RISU). Encodings from
//! tools/a64asm.sh; values also verified against QEMU.

use vetro_isa_tests::case;

#[test]
fn logic_size3_with_q0() {
    case(
        "logic_size3_with_q0",
        &[
            0x0ee21c20, // orn v0.8b, v1.8b, v2.8b
            0x2ee21c23, // bif v3.8b, v1.8b, v2.8b
            0x6e621c24, // bsl v4.16b, v1.16b, v2.16b
        ],
    )
    .v(1, 0xffff_0000_ffff_0000_f0f0_f0f0_0000_ffff)
    .v(2, 0x00ff_00ff_00ff_00ff_ff00_ff00_ff00_ff00)
    .v(3, 0x1111_1111_1111_1111_2222_2222_2222_2222)
    .v(4, 0xff00_ff00_ff00_ff00_0f0f_0f0f_0f0f_0f0f)
    // ORN: Vn | !Vm, low 64 bits only
    .want_v(0, 0xf0ff_f0ff_00ff_ffff)
    // BIF: Vd = (Vd & Vm) | (Vn & !Vm), low 64 bits only
    .want_v(3, 0x22f0_22f0_2200_22ff)
    // BSL: Vd = (Vd & Vn) | (!Vd & Vm)
    .want_v(4, 0xffff_00ff_ffff_00ff_f000_f000_f000_ff0f)
    .run();
}
