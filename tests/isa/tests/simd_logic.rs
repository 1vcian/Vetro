//! Operazioni logiche vettoriali: `size` sceglie l'operazione, quindi sono
//! valide anche con size = 11 e Q = 0 (bug trovato da RISU). Codifiche da
//! tools/a64asm.sh; valori verificati anche contro QEMU.

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
    // ORN: Vn | !Vm, solo 64 bit bassi
    .want_v(0, 0xf0ff_f0ff_00ff_ffff)
    // BIF: Vd = (Vd & Vm) | (Vn & !Vm), solo 64 bit bassi
    .want_v(3, 0x22f0_22f0_2200_22ff)
    // BSL: Vd = (Vd & Vn) | (!Vd & Vm)
    .want_v(4, 0xffff_00ff_ffff_00ff_f000_f000_f000_ff0f)
    .run();
}
