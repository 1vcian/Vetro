//! Floating-point edge cases in the JIT (ADR 0026): every instruction
//! with a fast (or inline) path runs alone in the JIT and
//! in the interpreter on pairs of special values (zeros, denormals, the
//! smallest normal and its neighbours, 1 ± ulp, maxima, infinities, quiet
//! and signalling NaNs, integer limits), with various FPCR (FZ, DN,
//! rounding modes) and FPSR (IXC at 0 or 1). Whole CPU identical,
//! FPSR included. Among the cases: the tiny product that rounds to the
//! smallest normal (Arm signals UFC: the fast path must
//! leave it to the interpreter), inexact sums with IXC at 0,
//! conversions at the integer boundaries.

use vetro_cpu::{Cpu, Perm, UserMemory};
use vetro_jit::{JitConfig, JitCpu};
use vetro_jit_native::NativeEngine;

const CODE: u64 = 0x40_0000;
/// brk #0: closes the region after the instruction under test.
const BRK: u32 = 0xd420_0000;

const S: [u32; 37] = [
    0,
    0x8000_0000,
    1,
    0x007f_ffff,
    0x0080_0000,
    0x8080_0001,
    0x3f7f_ffff,
    0x3f80_0000,
    0x3f80_0001,
    0x3dcc_cccd,
    0x4040_0000,
    0x7f7f_ffff,
    0xff7f_ffff,
    0x7f80_0000,
    0xff80_0000,
    0x7fc0_0000,
    0x7fa0_0001,
    0x4f00_0000,
    0xcf00_0000,
    0x4eff_ffff,
    0x5f00_0000,
    0x1f80_0000,
    0x2000_0000,
    0xbf00_0000,
    0x3fc0_0000,
    0x4b80_0000,
    0x4b80_0001,
    // Half-precision boundaries (ADR 0045): 65504, the largest that
    // rounds to it, 65520 (overflows), 2^-14 and below, 2^-24, ties.
    0x477f_e000,
    0x477f_efff,
    0x477f_f000,
    0x3880_0000,
    0x387f_ffff,
    0x3380_0000,
    0x3f80_1000,
    0xbf80_3000,
    0x3f80_1001,
    0x4680_0fff,
];

/// Half-precision values (ADR 0045): zeros, denormals, the smallest normal,
/// 1 ± ulp, maxima, infinities, quiet and signalling NaNs.
const H: [u16; 18] = [
    0, 0x8000, 1, 0x03ff, 0x8400, 0x0400, 0x3bff, 0x3c00, 0x3c01, 0x7bff, 0xfbff, 0x7c00, 0xfc00, 0x7e00,
    0x7d01, 0x7c01, 0x3555, 0xd640,
];

const D: [u64; 29] = [
    0,
    1 << 63,
    1,
    0x000f_ffff_ffff_ffff,
    0x0010_0000_0000_0000,
    0x8010_0000_0000_0001,
    0x3fef_ffff_ffff_ffff,
    0x3ff0_0000_0000_0000,
    0x3ff0_0000_0000_0001,
    0x3fb9_9999_9999_999a,
    0x4008_0000_0000_0000,
    0x7fef_ffff_ffff_ffff,
    0xffef_ffff_ffff_ffff,
    0x7ff0_0000_0000_0000,
    0xfff0_0000_0000_0000,
    0x7ff8_0000_0000_0000,
    0x7ff4_0000_0000_0001,
    0x41e0_0000_0000_0000,
    0xc1e0_0000_0000_0000,
    0x41df_ffff_ffc0_0000,
    0x43e0_0000_0000_0000,
    0xc3e0_0000_0000_0000,
    0x43f0_0000_0000_0000,
    0x3fe0_0000_0000_0000,
    0xbfe0_0000_0000_0000,
    0x4340_0000_0000_0000,
    0x4340_0000_0000_0001,
    0x1ff0_0000_0000_0000,
    0x5fe0_0000_0000_0000,
];

const X: [u64; 13] = [
    0,
    1,
    u64::MAX,
    i64::MAX as u64,
    1 << 63,
    1 << 53,
    (1 << 53) + 1,
    1 << 24,
    (1 << 24) + 1,
    0xffff_ffff,
    0x8000_0000,
    0x7fff_ffff,
    12345,
];

/// Operand type of an instruction under test.
#[derive(Clone, Copy)]
enum T {
    S,
    D,
    H,
}

/// Instructions under test (encodings from tools/a64asm.sh): V1, V2, V3 and X1
/// sources, V0 and X0 destinations.
const CASES: &[(u32, T, &str)] = &[
    (0x1e220820, T::S, "fmul s0, s1, s2"),
    (0x1e620820, T::D, "fmul d0, d1, d2"),
    (0x1e222820, T::S, "fadd s0, s1, s2"),
    (0x1e622820, T::D, "fadd d0, d1, d2"),
    (0x1e223820, T::S, "fsub s0, s1, s2"),
    (0x1e623820, T::D, "fsub d0, d1, d2"),
    (0x1e221820, T::S, "fdiv s0, s1, s2"),
    (0x1e621820, T::D, "fdiv d0, d1, d2"),
    (0x1e228820, T::S, "fnmul s0, s1, s2"),
    (0x1e624820, T::D, "fmax d0, d1, d2"),
    (0x1e227820, T::S, "fminnm s0, s1, s2"),
    (0x1f020c20, T::S, "fmadd s0, s1, s2, s3"),
    (0x1f028c20, T::S, "fmsub s0, s1, s2, s3"),
    (0x1f220c20, T::S, "fnmadd s0, s1, s2, s3"),
    (0x1f420c20, T::D, "fmadd d0, d1, d2, d3"),
    (0x1f628c20, T::D, "fnmsub d0, d1, d2, d3"),
    (0x1e21c020, T::S, "fsqrt s0, s1"),
    (0x1e61c020, T::D, "fsqrt d0, d1"),
    (0x1e222020, T::S, "fcmp s1, s2"),
    (0x1e622030, T::D, "fcmpe d1, d2"),
    (0x1e602028, T::D, "fcmp d1, #0.0"),
    (0x1e624020, T::D, "fcvt s0, d1"),
    (0x1e22c020, T::S, "fcvt d0, s1"),
    (0x9e620020, T::D, "scvtf d0, x1"),
    (0x9e230020, T::S, "ucvtf s0, x1"),
    (0x1e220020, T::S, "scvtf s0, w1"),
    (0x1e630020, T::D, "ucvtf d0, w1"),
    (0x9e780020, T::D, "fcvtzs x0, d1"),
    (0x1e390020, T::S, "fcvtzu w0, s1"),
    (0x1e780020, T::D, "fcvtzs w0, d1"),
    (0x9e790020, T::D, "fcvtzu x0, d1"),
    (0x9e600020, T::D, "fcvtns x0, d1"),
    (0x1e300020, T::S, "fcvtms w0, s1"),
    (0x9e280020, T::S, "fcvtps x0, s1"),
    (0x1e644020, T::D, "frintn d0, d1"),
    (0x1e274020, T::S, "frintx s0, s1"),
    (0x1e254020, T::S, "frintm s0, s1"),
    (0x1e67c020, T::D, "frinti d0, d1"),
    (0x4e22cc20, T::S, "fmla v0.4s, v1.4s, v2.4s"),
    (0x0ea2cc20, T::S, "fmls v0.2s, v1.2s, v2.2s"),
    (0x4e62cc20, T::D, "fmla v0.2d, v1.2d, v2.2d"),
    (0x4fa29020, T::S, "fmul v0.4s, v1.4s, v2.s[1]"),
    (0x4fa21820, T::S, "fmla v0.4s, v1.4s, v2.s[3]"),
    (0x4fc21820, T::D, "fmla v0.2d, v1.2d, v2.d[1]"),
    (0x4fc29820, T::D, "fmul v0.2d, v1.2d, v2.d[1]"),
    (0x0e22d420, T::S, "fadd v0.2s, v1.2s, v2.2s"),
    (0x4e62d420, T::D, "fadd v0.2d, v1.2d, v2.2d"),
    (0x4ea2d420, T::S, "fsub v0.4s, v1.4s, v2.4s"),
    (0x6e22dc20, T::S, "fmul v0.4s, v1.4s, v2.4s"),
    (0x6e62fc20, T::D, "fdiv v0.2d, v1.2d, v2.2d"),
    (0x4e22f420, T::S, "fmax v0.4s, v1.4s, v2.4s"),
    (0x4ee2c420, T::D, "fminnm v0.2d, v1.2d, v2.2d"),
    (0x6e22e420, T::S, "fcmge v0.4s, v1.4s, v2.4s"),
    (0x4ee0c820, T::D, "fcmgt v0.2d, v1.2d, #0.0"),
    (0x6ea0d820, T::S, "fcmle v0.4s, v1.4s, #0.0"),
    (0x0ea0d820, T::S, "fcmeq v0.2s, v1.2s, #0.0"),
    (0x6ea1f820, T::S, "fsqrt v0.4s, v1.4s"),
    (0x1e62bc20, T::D, "fcsel d0, d1, d2, lt"),
    (0x4ea0f820, T::S, "fabs v0.4s, v1.4s"),
    (0x1e614020, T::D, "fneg d0, d1"),
    (0x4ea1b820, T::S, "fcvtzs v0.4s, v1.4s"),
    (0x6ee1b820, T::D, "fcvtzu v0.2d, v1.2d"),
    (0x4e21a820, T::S, "fcvtns v0.4s, v1.4s"),
    (0x0e21b820, T::S, "fcvtms v0.2s, v1.2s"),
    (0x4e61c820, T::D, "fcvtas v0.2d, v1.2d"),
    (0x6e21c820, T::S, "fcvtau v0.4s, v1.4s"),
    (0x4ee1a820, T::D, "fcvtps v0.2d, v1.2d"),
    (0x4e21d820, T::S, "scvtf v0.4s, v1.4s"),
    (0x6e61d820, T::D, "ucvtf v0.2d, v1.2d"),
    (0x0e21d820, T::S, "scvtf v0.2s, v1.2s"),
    (0x0e617820, T::S, "fcvtl v0.2d, v1.2s"),
    (0x4e617820, T::S, "fcvtl2 v0.2d, v1.4s"),
    (0x0e616820, T::D, "fcvtn v0.2s, v1.2d"),
    (0x4e616820, T::D, "fcvtn2 v0.4s, v1.2d"),
    (0x6e22d420, T::S, "faddp v0.4s, v1.4s, v2.4s"),
    (0x2e22d420, T::S, "faddp v0.2s, v1.2s, v2.2s"),
    (0x6e62d420, T::D, "faddp v0.2d, v1.2d, v2.2d"),
    (0x6ea2d420, T::S, "fabd v0.4s, v1.4s, v2.4s"),
    (0x6ee2d420, T::D, "fabd v0.2d, v1.2d, v2.2d"),
    (0x0e217820, T::H, "fcvtl v0.4s, v1.4h"),
    (0x4e217820, T::H, "fcvtl2 v0.4s, v1.8h"),
    (0x0e216820, T::S, "fcvtn v0.4h, v1.4s"),
    (0x4e216820, T::S, "fcvtn2 v0.8h, v1.4s"),
    (0x1ee24020, T::H, "fcvt s0, h1"),
    (0x1e23c020, T::S, "fcvt h0, s1"),
    (0x4e218820, T::S, "frintn v0.4s, v1.4s"),
    (0x4ee18820, T::D, "frintp v0.2d, v1.2d"),
    (0x4e219820, T::S, "frintm v0.4s, v1.4s"),
    (0x0e219820, T::S, "frintm v0.2s, v1.2s"),
    (0x4ee19820, T::D, "frintz v0.2d, v1.2d"),
    (0x6e218820, T::S, "frinta v0.4s, v1.4s"),
    (0x6e219820, T::S, "frintx v0.4s, v1.4s"),
    (0x6e619820, T::D, "frintx v0.2d, v1.2d"),
    (0x6ea19820, T::S, "frinti v0.4s, v1.4s"),
    (0x7e30d820, T::S, "faddp s0, v1.2s"),
    (0x7e70d820, T::D, "faddp d0, v1.2d"),
    (0x7e30c820, T::S, "fmaxnmp s0, v1.2s"),
    (0x7ef0f820, T::D, "fminp d0, v1.2d"),
    (0x7e30f820, T::S, "fmaxp s0, v1.2s"),
    (0x7eb0c820, T::S, "fminnmp s0, v1.2s"),
    (0x6e30f820, T::S, "fmaxv s0, v1.4s"),
    (0x6eb0c820, T::S, "fminnmv s0, v1.4s"),
    (0x6e30c820, T::S, "fmaxnmv s0, v1.4s"),
    (0x6eb0f820, T::S, "fminv s0, v1.4s"),
    (0x5fa29820, T::S, "fmul s0, s1, v2.s[3]"),
    (0x5fc29820, T::D, "fmul d0, d1, v2.d[1]"),
    (0x5fa21020, T::S, "fmla s0, s1, v2.s[1]"),
    (0x5f825820, T::S, "fmls s0, s1, v2.s[2]"),
    (0x5fc21820, T::D, "fmla d0, d1, v2.d[1]"),
    (0x5fc25020, T::D, "fmls d0, d1, v2.d[0]"),
    (0x5fb29820, T::S, "fmul s0, s1, v18.s[3]"),
    (0x4f3dfc20, T::S, "fcvtzs v0.4s, v1.4s, #3"),
    (0x6f76fc20, T::D, "fcvtzu v0.2d, v1.2d, #10"),
    (0x0f20fc20, T::S, "fcvtzs v0.2s, v1.2s, #32"),
    (0x6f3ffc20, T::S, "fcvtzu v0.4s, v1.4s, #1"),
    (0x4f40fc20, T::D, "fcvtzs v0.2d, v1.2d, #64"),
    (0x4f30fc20, T::S, "fcvtzs v0.4s, v1.4s, #16"),
    (0x1e264020, T::S, "frinta s0, s1"),
    (0x1e664020, T::D, "frinta d0, d1"),
    (0x9e640020, T::D, "fcvtas x0, d1"),
    (0x1e250020, T::S, "fcvtau w0, s1"),
];

/// V register with `vals` repeated in the lanes (starting from the first), and
/// with the lanes past the first differing from each other.
fn vreg(t: T, vals: &[u64], k: usize) -> u128 {
    match t {
        T::S => {
            let mut v = 0u128;
            for lane in 0..4 {
                v |= (vals[(k + lane * 7) % vals.len()] as u32 as u128) << (32 * lane);
            }
            v
        }
        T::D => vals[k % vals.len()] as u128 | (vals[(k + 5) % vals.len()] as u128) << 64,
        T::H => {
            let mut v = 0u128;
            for lane in 0..8 {
                v |= (vals[(k + lane * 5) % vals.len()] as u16 as u128) << (16 * lane);
            }
            v
        }
    }
}

fn run_one(jit: &mut JitCpu<NativeEngine>, word: u32, cpu: &Cpu) -> (Cpu, Cpu) {
    let mut code = Vec::new();
    for w in [word, BRK] {
        code.extend_from_slice(&w.to_le_bytes());
    }
    let mut mem = UserMemory::new();
    mem.map(CODE, code, Perm::RX).unwrap();
    let mut want = cpu.clone();
    want.step(&mut mem.clone()).expect("one interpreter step");
    let mut got = cpu.clone();
    let before = jit.stats.jit_steps;
    let (n, r) = jit.run(&mut got, &mut mem, 1);
    assert_eq!((n, r), (1, Ok(())));
    assert_eq!(jit.stats.jit_steps, before + 1, "{word:#010x} not executed by the JIT");
    (want, got)
}

#[test]
fn casi_limite_come_interprete() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let s: Vec<u64> = S.iter().map(|&x| x as u64).collect();
    let h: Vec<u64> = H.iter().map(|&x| x as u64).collect();
    let fpcrs = [0u32, 1 << 24, 3 << 22, 1 << 25, 1 << 22, 7 << 24, 1 << 26];
    let mut runs = 0u64;
    for &(word, t, name) in CASES {
        let vals: &[u64] = match t {
            T::S => &s,
            T::D => &D,
            T::H => &h,
        };
        let n = vals.len();
        for i in 0..n {
            for j in 0..n {
                for (fi, &fpcr) in fpcrs.iter().enumerate() {
                    for fpsr in [0u32, 0x10] {
                        // Third operand (FMA, accumulator): few values.
                        let k3 = (i * 3 + j + fi) % n;
                        let mut cpu = Cpu::new();
                        cpu.pc = CODE;
                        cpu.v[0] = vreg(t, vals, k3 + 1);
                        cpu.v[1] = vreg(t, vals, i);
                        cpu.v[2] = vreg(t, vals, j);
                        cpu.v[3] = vreg(t, vals, k3);
                        cpu.v[18] = vreg(t, vals, j + 3);
                        cpu.x[1] = X[(i + j) % X.len()];
                        cpu.nzcv = ((i + 2 * j) as u32 & 0xf) << 28;
                        cpu.fpcr = fpcr;
                        cpu.fpsr = fpsr;
                        let (want, got) = run_one(&mut jit, word, &cpu);
                        runs += 1;
                        assert!(
                            want == got,
                            "{name} ({word:#010x}) con v1={:#x} v2={:#x} v3={:#x} x1={:#x} fpcr={fpcr:#x} fpsr={fpsr:#x}:\n\
                             interprete v0={:#034x} x0={:#x} nzcv={:#x} fpsr={:#x}\n\
                             JIT        v0={:#034x} x0={:#x} nzcv={:#x} fpsr={:#x}",
                            cpu.v[1],
                            cpu.v[2],
                            cpu.v[3],
                            cpu.x[1],
                            want.v[0],
                            want.x[0],
                            want.nzcv,
                            want.fpsr,
                            got.v[0],
                            got.x[0],
                            got.nzcv,
                            got.fpsr
                        );
                    }
                }
            }
        }
    }
    eprintln!("{runs} cases; {:?}", jit.stats);
}

/// The fast paths really are used: with normal values, FPCR = 0 and IXC
/// already at 1 no instruction in the table calls `env.simd` (a
/// broken fast path, which always falls back to the interpreter, would
/// still give correct results: only this test finds it).
#[test]
fn percorsi_veloci_usati() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for &(word, t, name) in CASES {
        // 1.5 × 2^32 and 2^64 do not fit: those conversions saturate (IOC).
        if name.ends_with("#32") || name.ends_with("#64") {
            continue;
        }
        let (a, b, c) = match t {
            T::S => (0x3fc0_0000u128 * 0x1_0000_0001_0000_0001_0000_0001, 0x4040_0000u128, 0x3dcc_cccdu128),
            T::D => (
                0x3ff8_0000_0000_0000u128 * 0x1_0000_0000_0000_0001,
                0x4008_0000_0000_0000,
                0x3fb9_9999_9999_999a,
            ),
            // 1.5, 3, 0.1 and a denormal in half precision
            T::H => (0x3e00_4200_2e66_0001_3e00_4200_2e66_0001u128, 0x4200, 0x2e66),
        };
        // FPCR = 0, and flush-to-zero with default NaN (ADR 0045:
        // without denormals and NaNs they change nothing).
        for fpcr in [0, 3 << 24] {
            let mut cpu = Cpu::new();
            cpu.pc = CODE;
            cpu.v[1] = a;
            cpu.v[2] = b | b << 32 | b << 64 | b << 96;
            cpu.v[3] = c;
            cpu.v[18] = cpu.v[2];
            cpu.v[0] = c;
            cpu.x[1] = 12345;
            cpu.fpsr = 0x10;
            cpu.fpcr = fpcr;
            let before = vetro_jit::helper::calls();
            let (want, got) = run_one(&mut jit, word, &cpu);
            assert_eq!(want, got, "{name} fpcr={fpcr:#x}");
            assert_eq!(vetro_jit::helper::calls(), before, "{name} fpcr={fpcr:#x}: called env.simd");
        }
    }
}

/// With IXC at 0 and inexact results, the fast paths that know exactly
/// whether their result is inexact raise IXC themselves (ADR 0045) instead
/// of calling `env.simd`: same result and FPSR as the interpreter, IXC set.
#[test]
fn fast_paths_raise_ixc() {
    const INEXACT: &[(u32, T, &str)] = &[
        (0x1e222820, T::S, "fadd s0, s1, s2"),
        (0x1e623820, T::D, "fsub d0, d1, d2"),
        (0x1e220820, T::S, "fmul s0, s1, s2"),
        (0x1e221820, T::S, "fdiv s0, s1, s2"),
        (0x1e21c020, T::S, "fsqrt s0, s1"),
        (0x1f020c20, T::S, "fmadd s0, s1, s2, s3"),
        (0x1e624020, T::D, "fcvt s0, d1"),
        (0x1e274020, T::S, "frintx s0, s1"),
        (0x1e380020, T::S, "fcvtzs w0, s1"),
        (0x9e600020, T::D, "fcvtns x0, d1"),
        (0x9e620020, T::D, "scvtf d0, x1"),
        (0x9e230020, T::S, "ucvtf s0, x1"),
        (0x4e22d420, T::S, "fadd v0.4s, v1.4s, v2.4s"),
        (0x4ee2d420, T::D, "fsub v0.2d, v1.2d, v2.2d"),
        (0x4ea1b820, T::S, "fcvtzs v0.4s, v1.4s"),
        (0x6ee1b820, T::D, "fcvtzu v0.2d, v1.2d"),
        (0x0e616820, T::D, "fcvtn v0.2s, v1.2d"),
        (0x0e216820, T::S, "fcvtn v0.4h, v1.4s"),
        (0x6e219820, T::S, "frintx v0.4s, v1.4s"),
    ];
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for &(word, t, name) in INEXACT {
        // 1/3 and 3 (and 0.1): sums, products, quotients, roots, roundings
        // and narrowing conversions all inexact.
        let (a, b, c) = match t {
            T::S => (0x3eaa_aaabu128 * 0x1_0000_0001_0000_0001_0000_0001, 0x4040_0000u128, 0x3dcc_cccdu128),
            T::D => (
                0x3fd5_5555_5555_5555u128 * 0x1_0000_0000_0000_0001,
                0x4008_0000_0000_0000,
                0x3fb9_9999_9999_999a,
            ),
            T::H => unreachable!(),
        };
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        cpu.v[1] = a;
        cpu.v[2] = b | b << 32 | b << 64 | b << 96;
        cpu.v[3] = c;
        cpu.v[0] = c;
        cpu.x[1] = (1 << 54) + 1;
        let before = vetro_jit::helper::calls();
        let (want, got) = run_one(&mut jit, word, &cpu);
        assert_eq!(want, got, "{name}");
        assert_eq!(got.fpsr, 0x10, "{name}: IXC");
        assert_eq!(vetro_jit::helper::calls(), before, "{name}: called env.simd");
    }
}

/// Products with a zero factor (vector FMUL, by-element FMUL, FMLA/FMLS
/// whose product is zero) are exact zeros or the accumulator: fast paths,
/// no `env.simd` (ADR 0045: SwiftShader multiplies by zero lanes often).
#[test]
fn zero_products_stay_in_the_fast_path() {
    const ZERO: &[(u32, &str)] = &[
        (0x6e22dc20, "fmul v0.4s, v1.4s, v2.4s"),
        (0x6e62dc20, "fmul v0.2d, v1.2d, v2.2d"),
        (0x4fa29020, "fmul v0.4s, v1.4s, v2.s[1]"),
        (0x4fa21820, "fmla v0.4s, v1.4s, v2.s[3]"),
        (0x4e22cc20, "fmla v0.4s, v1.4s, v2.4s"),
        (0x0ea2cc20, "fmls v0.2s, v1.2s, v2.2s"),
    ];
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for &(word, name) in ZERO {
        let d = name.contains(".2d");
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        // v1: 1.5, -0, 2.5, +0 (single) / 1.5, -0 (double); v2: 0, 3, -0, 3 /
        // 3, 3; accumulator: 0.1, -0, 0, 1.5
        if d {
            cpu.v[1] = 0x3ff8_0000_0000_0000 | (1u128 << 127);
            cpu.v[2] = 0x4008_0000_0000_0000 | 0x4008_0000_0000_0000u128 << 64;
            cpu.v[0] = 0x3fb9_9999_9999_999a;
        } else {
            cpu.v[1] = 0x3fc0_0000 | 0x8000_0000u128 << 32 | 0x4020_0000u128 << 64;
            cpu.v[2] = 0x4040_0000u128 << 32 | 0x8000_0000u128 << 64 | 0x4040_0000u128 << 96;
            cpu.v[0] = 0x3dcc_cccd | 0x8000_0000u128 << 32 | 0x3fc0_0000u128 << 96;
        }
        cpu.fpsr = 0x10;
        let before = vetro_jit::helper::calls();
        let (want, got) = run_one(&mut jit, word, &cpu);
        assert_eq!(want, got, "{name}");
        assert_eq!(vetro_jit::helper::calls(), before, "{name}: called env.simd");
    }
}

/// FMA (round-to-odd in single, emulated FMA in double) on
/// random triples: arbitrary mantissas, close exponents (cancellations) and
/// distant ones, with and without IXC. Bit for bit like the interpreter.
#[test]
fn fma_casuali_come_interprete() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    struct R(u64);
    impl R {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// f64 with exponent close to `base` (cancellations) or arbitrary.
        fn d(&mut self, base: u64) -> u64 {
            let r = self.next();
            let e = if r & 3 == 0 { (r >> 2) % 0x7fe + 1 } else { base + (r >> 2) % 60 - 30 };
            (r & (1 << 63)) | e.min(0x7fe) << 52 | self.next() >> 12
        }
        fn s(&mut self, base: u32) -> u32 {
            let r = self.next();
            let e =
                if r & 3 == 0 { ((r >> 2) % 0xfe + 1) as u32 } else { base + ((r >> 2) % 40) as u32 - 20 };
            ((r >> 32) as u32 & 0x8000_0000) | e.min(0xfe) << 23 | (self.next() >> 41) as u32
        }
    }
    let mut rng = R(0x1234_5678_9abc_def1);
    let cases = std::env::var("VETRO_JIT_FMA_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000u64);
    let words = [
        0x1f420c20u32, // fmadd d0, d1, d2, d3
        0x1f628c20,    // fnmsub d0, d1, d2, d3
        0x1f020c20,    // fmadd s0, s1, s2, s3
        0x1f220c20,    // fnmadd s0, s1, s2, s3
        0x4e62cc20,    // fmla v0.2d, v1.2d, v2.2d
        0x4fc21820,    // fmla v0.2d, v1.2d, v2.d[1]
        0x4e22cc20,    // fmla v0.4s, v1.4s, v2.4s
        0x4fa21820,    // fmla v0.4s, v1.4s, v2.s[3]
    ];
    for i in 0..cases {
        let word = words[(i % words.len() as u64) as usize];
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        let double = matches!(word, 0x1f420c20 | 0x1f628c20 | 0x4e62cc20 | 0x4fc21820);
        for v in cpu.v.iter_mut().take(4) {
            *v = if double {
                rng.d(1023) as u128 | (rng.d(1023) as u128) << 64
            } else {
                (0..4).fold(0u128, |v, k| v | (rng.s(127) as u128) << (32 * k))
            };
        }
        cpu.fpsr = if i % 3 == 0 { 0 } else { 0x10 };
        let (want, got) = run_one(&mut jit, word, &cpu);
        assert!(
            want == got,
            "{word:#010x} v1={:#x} v2={:#x} v3={:#x} v0={:#x}: interpreter {:#x}/{:#x}, JIT {:#x}/{:#x}",
            cpu.v[1],
            cpu.v[2],
            cpu.v[3],
            cpu.v[0],
            want.v[0],
            want.fpsr,
            got.v[0],
            got.fpsr
        );
    }
}

/// The double rounding that round-to-odd avoids: the
/// exact product is a midpoint (1 + 2^-24 in single, 1 + 2^-53 in
/// double: 24929 × 673 = 2^24 + 1, 321 × 28059810762433 = 2^53 + 1) and
/// the tiny addend moves it just above. The correct FMA rounds
/// up; without round-to-odd it would round to even, down.
#[test]
fn fma_doppio_arrotondamento() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for (word, n, m, a, want) in [
        // fmadd s0, s1, s2, s3
        (0x1f020c20u32, 0x3f42_c200u128, 0x3fa8_4000u128, 0x1780_0000u128, 0x3f80_0001u128),
        // fmadd d0, d1, d2, d3
        (
            0x1f420c20,
            0x3fe4_1000_0000_0000,
            0x3ff9_852f_0d8e_c100,
            0x39b0_0000_0000_0000,
            0x3ff0_0000_0000_0001,
        ),
    ] {
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        (cpu.v[1], cpu.v[2], cpu.v[3]) = (n, m, a);
        cpu.fpsr = 0x10;
        let before = vetro_jit::helper::calls();
        let (i, j) = run_one(&mut jit, word, &cpu);
        assert_eq!(i.v[0], want, "{word:#x}: the interpreter rounds up");
        assert_eq!(i, j, "{word:#x}");
        assert_eq!(vetro_jit::helper::calls(), before, "{word:#x}: fast path");
    }
}

/// The tiny product that rounds to the smallest normal: Arm
/// signals UFC and IXC (tininess before rounding), WASM gives
/// a normal. With FPSR at IXC the fast path, if it accepted it, would lose
/// UFC.
#[test]
fn minuscolo_arrotondato_al_normale() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for (word, a, b) in [
        (0x1e220820u32, 0x3f7f_ffffu128, 0x0080_0000u128), // fmul s0, s1, s2
        (0x1e620820, 0x3fef_ffff_ffff_ffff, 0x0010_0000_0000_0000), // fmul d0, d1, d2
        (
            0x6e22dc20,
            0x3f7f_ffff_3f7f_ffff_3f7f_ffff_3f7f_ffff,
            0x0080_0000 * 0x1_0000_0001_0000_0001_0000_0001,
        ), // fmul v0.4s
    ] {
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        cpu.v[1] = a;
        cpu.v[2] = b;
        cpu.fpsr = 0x10;
        let (want, got) = run_one(&mut jit, word, &cpu);
        assert_eq!(want.fpsr & 0x8, 0x8, "{word:#x}: the interpreter signals UFC");
        assert_eq!(want, got, "{word:#x}");
    }
}
