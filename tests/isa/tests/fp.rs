//! Virgola mobile: NaN, arrotondamenti, eccezioni, conversioni, stime.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::case;

#[test]
fn nan_snan_second_wins() {
    case(
        "nan_snan_second_wins",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .v(1, 0x7fc0_0005)
    .v(2, 0x7f80_0001)
    .want_v(0, 0x7fc0_0001)
    .want_fpsr(1)
    .run();
}

#[test]
fn nan_first_qnan_wins() {
    case(
        "nan_first_qnan_wins",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .v(1, 0x7fc0_0005)
    .v(2, 0x7fc0_0007)
    .want_v(0, 0x7fc0_0005)
    .want_fpsr(0)
    .run();
}

#[test]
fn default_nan_mode() {
    case(
        "default_nan_mode",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .fpcr(1 << 25)
    .v(1, 0x7fc0_0005)
    .v(2, 0x3f80_0000)
    .want_v(0, 0x7fc0_0000)
    .want_fpsr(0)
    .run();
}

#[test]
fn fma_nan_priority() {
    case(
        "fma_nan_priority",
        &[
            0x1f020c20, // fmadd s0, s1, s2, s3
        ],
    )
    .v(1, 0x7f80_0001)
    .v(2, 0x3f80_0000)
    .v(3, 0x7fc0_0005)
    .want_v(0, 0x7fc0_0001)
    .want_fpsr(1)
    .run();
}

#[test]
fn fma_qnan_addend_inf_times_zero() {
    case(
        "fma_qnan_addend_inf_times_zero",
        &[
            0x1f020c20, // fmadd s0, s1, s2, s3
        ],
    )
    .v(1, 0x7f80_0000)
    .v(2, 0)
    .v(3, 0x7fc0_0005)
    .want_v(0, 0x7fc0_0000)
    .want_fpsr(1)
    .run();
}

#[test]
fn fmsub_negates_nan_operand() {
    case(
        "fmsub_negates_nan_operand",
        &[
            0x1f028c20, // fmsub s0, s1, s2, s3
        ],
    )
    .v(1, 0x7fc0_0005)
    .v(2, 0x3f80_0000)
    .v(3, 0x3f80_0000)
    .want_v(0, 0xffc0_0005)
    .want_fpsr(0)
    .run();
}

#[test]
fn inf_minus_inf() {
    case(
        "inf_minus_inf",
        &[
            0x1e223820, // fsub s0, s1, s2
        ],
    )
    .v(1, 0x7f80_0000)
    .v(2, 0x7f80_0000)
    .want_v(0, 0x7fc0_0000)
    .want_fpsr(1)
    .run();
}

#[test]
fn divide_by_zero() {
    case(
        "divide_by_zero",
        &[
            0x1e221820, // fdiv s0, s1, s2
        ],
    )
    .v(1, 0x3f80_0000)
    .v(2, 0x8000_0000)
    .want_v(0, 0xff80_0000)
    .want_fpsr(2)
    .run();
}

#[test]
fn exact_zero_sign_default() {
    case(
        "exact_zero_sign_default",
        &[
            0x1e213820, // fsub s0, s1, s1
        ],
    )
    .v(1, 0x3f80_0000)
    .want_v(0, 0)
    .run();
}

#[test]
fn exact_zero_sign_round_down() {
    case(
        "exact_zero_sign_round_down",
        &[
            0x1e213820, // fsub s0, s1, s1
        ],
    )
    .fpcr(2 << 22)
    .v(1, 0x3f80_0000)
    .want_v(0, 0x8000_0000)
    .run();
}

#[test]
fn tie_to_even() {
    case(
        "tie_to_even",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .v(1, 0x3f80_0000)
    .v(2, 0x3380_0000)
    .want_v(0, 0x3f80_0000)
    .want_fpsr(16)
    .run();
}

#[test]
fn round_toward_plus_inf() {
    case(
        "round_toward_plus_inf",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .fpcr(1 << 22)
    .v(1, 0x3f80_0000)
    .v(2, 0x3380_0000)
    .want_v(0, 0x3f80_0001)
    .want_fpsr(16)
    .run();
}

#[test]
fn overflow_to_infinity() {
    case(
        "overflow_to_infinity",
        &[
            0x1e210820, // fmul s0, s1, s1
        ],
    )
    .v(1, 0x7f7f_ffff)
    .want_v(0, 0x7f80_0000)
    .want_fpsr(4 | 16)
    .run();
}

#[test]
fn overflow_round_toward_zero() {
    case(
        "overflow_round_toward_zero",
        &[
            0x1e210820, // fmul s0, s1, s1
        ],
    )
    .fpcr(3 << 22)
    .v(1, 0x7f7f_ffff)
    .want_v(0, 0x7f7f_ffff)
    .want_fpsr(4 | 16)
    .run();
}

#[test]
fn exact_denormal_no_underflow() {
    case(
        "exact_denormal_no_underflow",
        &[
            0x1e220820, // fmul s0, s1, s2
        ],
    )
    .v(1, 0x0080_0000)
    .v(2, 0x3f00_0000)
    .want_v(0, 0x0040_0000)
    .want_fpsr(0)
    .run();
}

#[test]
fn inexact_denormal_underflows() {
    case(
        "inexact_denormal_underflows",
        &[
            0x1e220820, // fmul s0, s1, s2
        ],
    )
    .v(1, 0x0080_0001)
    .v(2, 0x3f00_0000)
    .want_v(0, 0x0040_0000)
    .want_fpsr(8 | 16)
    .run();
}

#[test]
fn flush_to_zero_output() {
    case(
        "flush_to_zero_output",
        &[
            0x1e220820, // fmul s0, s1, s2
        ],
    )
    .fpcr(1 << 24)
    .v(1, 0x0080_0000)
    .v(2, 0x3f00_0000)
    .want_v(0, 0)
    .want_fpsr(8)
    .run();
}

#[test]
fn flush_to_zero_input() {
    case(
        "flush_to_zero_input",
        &[
            0x1e222820, // fadd s0, s1, s2
        ],
    )
    .fpcr(1 << 24)
    .v(1, 1)
    .v(2, 0x3f80_0000)
    .want_v(0, 0x3f80_0000)
    .want_fpsr(128)
    .run();
}

#[test]
fn fcvtzs_saturates() {
    case(
        "fcvtzs_saturates",
        &[
            0x1e380020, // fcvtzs w0, s1
            0x1e380062, // fcvtzs w2, s3
            0x1e3900a4, // fcvtzu w4, s5
            0x1e3800e6, // fcvtzs w6, s7
            0xd53b4428, // mrs x8, FPSR
        ],
    )
    .v(1, 0x4f32_d05e)
    .v(3, 0x7fc0_0005)
    .v(5, 0xbf80_0000)
    .v(7, 0xc020_0000)
    .want_x(0, 0x7fff_ffff)
    .want_x(2, 0)
    .want_x(4, 0)
    .want_x(6, 0xffff_fffe)
    .want_x(8, 1 | 16)
    .run();
}

#[test]
fn fcvt_rounding_modes() {
    case(
        "fcvt_rounding_modes",
        &[
            0x1e240020, // fcvtas w0, s1
            0x1e200062, // fcvtns w2, s3
            0x1e2800a4, // fcvtps w4, s5
            0x1e3000e6, // fcvtms w6, s7
        ],
    )
    .v(1, 0xc020_0000)
    .v(3, 0x4020_0000)
    .v(5, 0x4006_6666)
    .v(7, 0xc006_6666)
    .want_x(0, 0xffff_fffd)
    .want_x(2, 2)
    .want_x(4, 3)
    .want_x(6, 0xffff_fffd)
    .run();
}

#[test]
fn int_to_float_rounds() {
    case(
        "int_to_float_rounds",
        &[
            0x1e220020, // scvtf s0, w1
            0x9e630062, // ucvtf d2, x3
            0x1e03f0a4, // ucvtf s4, w5, #0x4
        ],
    )
    .x(1, 0x7fff_ffff)
    .x(3, u64::MAX)
    .x(5, 40)
    .want_v(0, 0x4f00_0000)
    .want_v(2, 0x43f0_0000_0000_0000)
    .want_v(4, 0x4020_0000)
    .want_fpsr(16)
    .run();
}

#[test]
fn fcvt_snan_single_to_double() {
    case(
        "fcvt_snan_single_to_double",
        &[
            0x1e22c020, // fcvt d0, s1
        ],
    )
    .v(1, 0x7f80_0001)
    .want_v(0, 0x7ff8_0000_2000_0000)
    .want_fpsr(1)
    .run();
}

#[test]
fn fcvt_half_ieee_and_alternative() {
    case(
        "fcvt_half_ieee_and_alternative",
        &[
            0x1e23c020, // fcvt h0, s1
            0x1e23c062, // fcvt h2, s3
            0xd53b4424, // mrs x4, FPSR
            0xd51b4405, // msr FPCR, x5
            0xd51b443f, // msr FPSR, xzr
            0x1e23c066, // fcvt h6, s3
            0xd53b4427, // mrs x7, FPSR
        ],
    )
    .v(1, 0x3f80_0000)
    .v(3, 0x4974_2400)
    .x(5, 1 << 26)
    .want_v(0, 0x3c00)
    .want_v(2, 0x7c00)
    .want_x(4, 4 | 16)
    .want_v(6, 0x7fff)
    .want_x(7, 1)
    .run();
}

#[test]
fn max_min_signed_zeros() {
    case(
        "max_min_signed_zeros",
        &[
            0x1e224820, // fmax s0, s1, s2
            0x1e225823, // fmin s3, s1, s2
        ],
    )
    .v(1, 0)
    .v(2, 0x8000_0000)
    .want_v(0, 0)
    .want_v(3, 0x8000_0000)
    .run();
}

#[test]
fn maxnm_quiet_nan_loses() {
    case(
        "maxnm_quiet_nan_loses",
        &[
            0x1e226820, // fmaxnm s0, s1, s2
            0x1e217843, // fminnm s3, s2, s1
        ],
    )
    .v(1, 0x7fc0_0005)
    .v(2, 0x4000_0000)
    .want_v(0, 0x4000_0000)
    .want_v(3, 0x4000_0000)
    .want_fpsr(0)
    .run();
}

#[test]
fn maxnm_signaling_nan_wins() {
    case(
        "maxnm_signaling_nan_wins",
        &[
            0x1e226820, // fmaxnm s0, s1, s2
        ],
    )
    .v(1, 0x7f80_0001)
    .v(2, 0x4000_0000)
    .want_v(0, 0x7fc0_0001)
    .want_fpsr(1)
    .run();
}

#[test]
fn fcmp_unordered_and_fcmpe() {
    case(
        "fcmp_unordered_and_fcmpe",
        &[
            0x1e222020, // fcmp s1, s2
            0xd53b4200, // mrs x0, NZCV
            0x1e232030, // fcmpe s1, s3
            0xd53b4424, // mrs x4, FPSR
        ],
    )
    .v(1, 0x7fc0_0005)
    .v(2, 0x3f80_0000)
    .v(3, 0x3f80_0000)
    .want_x(0, 0x3000_0000)
    .want_x(4, 1)
    .run();
}

#[test]
fn fcmp_ordered() {
    case(
        "fcmp_ordered",
        &[
            0x1e222020, // fcmp s1, s2
            0xd53b4200, // mrs x0, NZCV
            0x1e212040, // fcmp s2, s1
            0xd53b4203, // mrs x3, NZCV
            0x1e202028, // fcmp s1, #0.0
            0xd53b4204, // mrs x4, NZCV
        ],
    )
    .v(1, 0x3f80_0000)
    .v(2, 0x4000_0000)
    .want_x(0, 0x8000_0000)
    .want_x(3, 0x2000_0000)
    .want_x(4, 0x2000_0000)
    .run();
}

#[test]
fn reciprocal_estimates() {
    case(
        "reciprocal_estimates",
        &[
            0x5ea1d820, // frecpe s0, s1
            0x7ea1d862, // frsqrte s2, s3
            0x5ea1d8a4, // frecpe s4, s5
        ],
    )
    .v(1, 0x3f80_0000)
    .v(3, 0x4080_0000)
    .v(5, 0)
    .want_v(0, 0x3f7f_8000)
    .want_v(2, 0x3eff_8000)
    .want_v(4, 0x7f80_0000)
    .want_fpsr(2)
    .run();
}

#[test]
fn reciprocal_steps() {
    case(
        "reciprocal_steps",
        &[
            0x5e22fc20, // frecps s0, s1, s2
            0x5e25fc83, // frecps s3, s4, s5
            0x5ea2fc26, // frsqrts s6, s1, s2
        ],
    )
    .v(1, 0x3f80_0000)
    .v(2, 0x3f00_0000)
    .v(4, 0x7f80_0000)
    .v(5, 0)
    .want_v(0, 0x3fc0_0000)
    .want_v(3, 0x4000_0000)
    .want_v(6, 0x3fa0_0000)
    .run();
}

#[test]
fn fmulx_infinity_times_zero() {
    case(
        "fmulx_infinity_times_zero",
        &[
            0x5e22dc20, // fmulx s0, s1, s2
        ],
    )
    .v(1, 0x7f80_0000)
    .v(2, 0x8000_0000)
    .want_v(0, 0xc000_0000)
    .want_fpsr(0)
    .run();
}

#[test]
fn round_to_integral() {
    case(
        "round_to_integral",
        &[
            0x1e274020, // frintx s0, s1
            0x1e264022, // frinta s2, s1
            0x1e25c083, // frintz s3, s4
            0x1e2540c5, // frintm s5, s6
            0x1e24c0c7, // frintp s7, s6
            0x1e244128, // frintn s8, s9
            0xd53b442a, // mrs x10, FPSR
        ],
    )
    .v(1, 0x4020_0000)
    .v(4, 0xc02c_cccd)
    .v(6, 0xc006_6666)
    .v(9, 0xc020_0000)
    .want_v(0, 0x4000_0000)
    .want_v(2, 0x4040_0000)
    .want_v(3, 0xc000_0000)
    .want_v(5, 0xc040_0000)
    .want_v(7, 0xc000_0000)
    .want_v(8, 0xc000_0000)
    .want_x(10, 16)
    .run();
}

#[test]
fn sqrt_and_negative_sqrt() {
    case(
        "sqrt_and_negative_sqrt",
        &[
            0x1e21c020, // fsqrt s0, s1
            0x1e21c062, // fsqrt s2, s3
        ],
    )
    .v(1, 0x4000_0000)
    .v(3, 0xbf80_0000)
    .want_v(0, 0x3fb5_04f3)
    .want_v(2, 0x7fc0_0000)
    .want_fpsr(1 | 16)
    .run();
}

#[test]
fn fabd_and_fnmul() {
    case(
        "fabd_and_fnmul",
        &[
            0x7ea2d420, // fabd s0, s1, s2
            0x1e228823, // fnmul s3, s1, s2
        ],
    )
    .v(1, 0x3f80_0000)
    .v(2, 0x4040_0000)
    .want_v(0, 0x4000_0000)
    .want_v(3, 0xc040_0000)
    .run();
}

#[test]
fn double_precision_fma() {
    case(
        "double_precision_fma",
        &[
            0x1f420c20, // fmadd d0, d1, d2, d3
        ],
    )
    .v(1, 0x3fb9_9999_9999_999a)
    .v(2, 0x4024_0000_0000_0000)
    .v(3, 0xbff0_0000_0000_0000)
    .want_v(0, 0x3c90_0000_0000_0000)
    .run();
}

#[test]
fn vector_fadd_faddp() {
    case(
        "vector_fadd_faddp",
        &[
            0x4e22d420, // fadd v0.4s, v1.4s, v2.4s
            0x6e22d423, // faddp v3.4s, v1.4s, v2.4s
            0x7e30d824, // faddp s4, v1.2s
        ],
    )
    .v(1, 0x4080_0000_4040_0000_4000_0000_3f80_0000)
    .v(2, 0x3f80_0000_3f80_0000_3f80_0000_3f80_0000)
    .want_v(0, 0x40a0_0000_4080_0000_4040_0000_4000_0000)
    .want_v(3, 0x4000_0000_4000_0000_40e0_0000_4040_0000)
    .want_v(4, 0x4040_0000)
    .run();
}

#[test]
fn vector_fmla_by_element() {
    case(
        "vector_fmla_by_element",
        &[
            0x4fa21820, // fmla v0.4s, v1.4s, v2.s[3]
        ],
    )
    .v(0, 0x3f80_0000_3f80_0000_3f80_0000_3f80_0000)
    .v(1, 0x4080_0000_4040_0000_4000_0000_3f80_0000)
    .v(2, 0x4000_0000_0000_0000_0000_0000_0000_0000)
    .want_v(0, 0x4110_0000_40e0_0000_40a0_0000_4040_0000)
    .run();
}

#[test]
fn vector_fcvtn_fcvtl() {
    case(
        "vector_fcvtn_fcvtl",
        &[
            0x0e616820, // fcvtn v0.2s, v1.2d
            0x0e617862, // fcvtl v2.2d, v3.2s
            0x7e6168a4, // fcvtxn s4, d5
        ],
    )
    .v(1, 0x4000_0000_0000_0000_3ff0_0000_0000_0000)
    .v(3, 0x4000_0000_3f80_0000)
    .v(5, 0x3ff0_0000_1000_0000)
    .want_v(0, 0x4000_0000_3f80_0000)
    .want_v(2, 0x4000_0000_0000_0000_3ff0_0000_0000_0000)
    .want_v(4, 0x3f80_0001)
    .run();
}
