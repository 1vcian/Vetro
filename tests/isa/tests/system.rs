//! Registri di sistema a EL0, hint, barriere, DC ZVA, eccezioni.
//!
//! Codifiche generate con `tools/a64asm.sh`; valori attesi verificati
//! anche contro QEMU quando l'oracolo è disponibile.

use vetro_isa_tests::{SIGBUS, SIGILL, SIGTRAP, case};

#[test]
fn nzcv_roundtrip() {
    case(
        "nzcv_roundtrip",
        &[
            0xd53b4200, // mrs x0, NZCV
            0xd51b4201, // msr NZCV, x1
            0xd53b4202, // mrs x2, NZCV
        ],
    )
    .flags(0b1010)
    .x(1, 0xffffffffffffffff)
    .want_x(0, 0xa000_0000)
    .want_x(2, 0xf000_0000)
    .want_flags(0b1111)
    .run();
}

#[test]
fn tpidr_el0_roundtrip() {
    case(
        "tpidr_el0_roundtrip",
        &[
            0xd51bd041, // msr TPIDR_EL0, x1
            0xd53bd040, // mrs x0, TPIDR_EL0
        ],
    )
    .x(1, 0x1234_5678_9abc_def0)
    .want_x(0, 0x1234_5678_9abc_def0)
    .run();
}

#[test]
fn tpidrro_el0_reads_zero() {
    case(
        "tpidrro_el0_reads_zero",
        &[
            0xd53bd060, // mrs x0, TPIDRRO_EL0
        ],
    )
    .x(0, 5)
    .want_x(0, 0)
    .run();
}

#[test]
fn tpidrro_el0_write_is_sigill() {
    case(
        "tpidrro_el0_write_is_sigill",
        &[
            0xd51bd060, // msr TPIDRRO_EL0, x0
        ],
    )
    .want_signal(SIGILL)
    .run();
}

#[test]
fn hints_and_barriers_are_nops() {
    case(
        "hints_and_barriers_are_nops",
        &[
            0xd503201f, // nop
            0xd503203f, // yield
            0xd503245f, // bti c
            0xd503233f, // paciasp
            0xd5033bbf, // dmb ish
            0xd5033f9f, // dsb sy
            0xd5033fdf, // isb
            0xd2800020, // mov x0, #0x1                // =1
        ],
    )
    .want_x(0, 1)
    .run();
}

#[test]
fn dc_zva_zeroes_64_bytes() {
    case(
        "dc_zva_zeroes_64_bytes",
        &[
            0x91011b81, // add x1, x28, #0x46
            0xd50b7421, // dc zva, x1
        ],
    )
    .mem(62, &[0xff; 76])
    .want_mem(62, &[0xff, 0xff])
    .want_mem(64, &[0; 64])
    .want_mem(128, &[0xff; 10])
    .run();
}

#[test]
fn brk_is_sigtrap() {
    case(
        "brk_is_sigtrap",
        &[
            0xd4200020, // brk #0x1
        ],
    )
    .want_signal(SIGTRAP)
    .run();
}

#[test]
fn udf_is_sigill() {
    case(
        "udf_is_sigill",
        &[
            0x00000000, // udf #0x0
        ],
    )
    .want_signal(SIGILL)
    .run();
}

#[test]
fn hvc_at_el0_is_sigill() {
    case(
        "hvc_at_el0_is_sigill",
        &[
            0xd4000002, // hvc #0
        ],
    )
    .want_signal(SIGILL)
    .run();
}

#[test]
fn msr_daifset_at_el0_is_sigill() {
    case(
        "msr_daifset_at_el0_is_sigill",
        &[
            0xd50342df, // msr DAIFSet, #0x2
        ],
    )
    .want_signal(SIGILL)
    .run();
}

#[test]
fn lse_is_undefined_on_v8_0() {
    case(
        "lse_is_undefined_on_v8_0",
        &[
            0xf8200020, // ldadd x0, x0, [x1]
        ],
    )
    .want_signal(SIGILL)
    .run();
}

#[test]
fn misaligned_branch_target_is_sigbus() {
    case(
        "misaligned_branch_target_is_sigbus",
        &[
            0x10000061, // adr x1, 0xc <.text+0xc>
            0x91000821, // add x1, x1, #0x2
            0xd61f0020, // br x1
            0xd503201f, // nop
        ],
    )
    .want_signal(SIGBUS)
    .run();
}

#[test]
fn debug_comms_channel_at_el0_is_sigill() {
    // QEMU user, come Linux, accende MDSCR_EL1.TDCC: ogni accesso da EL0 al
    // canale di debug dà SIGILL.
    for (name, insn) in [
        ("mrs_mdccsr_el0_is_sigill", 0xd5330100),   // mrs x0, MDCCSR_EL0
        ("mrs_dbgdtr_el0_is_sigill", 0xd5330401),   // mrs x1, DBGDTR_EL0
        ("mrs_dbgdtrrx_el0_is_sigill", 0xd5330502), // mrs x2, DBGDTRRX_EL0
        ("msr_dbgdtr_el0_is_sigill", 0xd5130403),   // msr DBGDTR_EL0, x3
        ("msr_dbgdtrtx_el0_is_sigill", 0xd5130503), // msr DBGDTRTX_EL0, x3
        ("msr_mdccsr_el0_is_sigill", 0xd5130103),   // msr S2_3_C0_C1_0, x3
    ] {
        case(name, &[insn]).want_signal(SIGILL).run();
    }
}
