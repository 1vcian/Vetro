//! Casi limite della virgola mobile nel JIT (ADR 0026): ogni istruzione
//! con un percorso veloce (o in linea) gira da sola nel JIT e
//! nell'interprete su coppie di valori speciali (zeri, denormali, il più
//! piccolo normale e i suoi vicini, 1 ± ulp, massimi, infiniti, NaN
//! silenziosi e segnalanti, limiti degli interi), con FPCR (FZ, DN,
//! arrotondamenti) e FPSR (IXC a 0 o a 1) diversi. CPU intera identica,
//! FPSR compreso. Fra i casi: il prodotto minuscolo che si arrotonda al
//! più piccolo normale (l'Arm segnala UFC: il percorso veloce deve
//! lasciarlo all'interprete), le somme inesatte con IXC a 0, le
//! conversioni ai bordi degli interi.

use vetro_cpu::{Cpu, Perm, UserMemory};
use vetro_jit::{JitConfig, JitCpu};
use vetro_jit_native::NativeEngine;

const CODE: u64 = 0x40_0000;
/// brk #0: chiude la regione dopo l'istruzione in prova.
const BRK: u32 = 0xd420_0000;

const S: [u32; 27] = [
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

/// Tipo degli operandi di un'istruzione in prova.
#[derive(Clone, Copy)]
enum T {
    S,
    D,
}

/// Istruzioni in prova (codifiche da tools/a64asm.sh): V1, V2, V3 e X1
/// sorgenti, V0 e X0 destinazioni.
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
    (0x1e264020, T::S, "frinta s0, s1"),
    (0x1e664020, T::D, "frinta d0, d1"),
    (0x9e640020, T::D, "fcvtas x0, d1"),
    (0x1e250020, T::S, "fcvtau w0, s1"),
];

/// Registro V con `vals` ripetuti nelle corsie (a partire dal primo), e
/// con le corsie oltre la prima diverse fra loro.
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
    want.step(&mut mem.clone()).expect("un passo dell'interprete");
    let mut got = cpu.clone();
    let before = jit.stats.jit_steps;
    let (n, r) = jit.run(&mut got, &mut mem, 1);
    assert_eq!((n, r), (1, Ok(())));
    assert_eq!(jit.stats.jit_steps, before + 1, "{word:#010x} non eseguita dal JIT");
    (want, got)
}

#[test]
fn casi_limite_come_interprete() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let s: Vec<u64> = S.iter().map(|&x| x as u64).collect();
    let fpcrs = [0u32, 1 << 24, 3 << 22, 1 << 25, 1 << 22];
    let mut runs = 0u64;
    for &(word, t, name) in CASES {
        let vals: &[u64] = match t {
            T::S => &s,
            T::D => &D,
        };
        let n = vals.len();
        for i in 0..n {
            for j in 0..n {
                for (fi, &fpcr) in fpcrs.iter().enumerate() {
                    for fpsr in [0u32, 0x10] {
                        // Terzo operando (FMA, accumulatore): pochi valori.
                        let k3 = (i * 3 + j + fi) % n;
                        let mut cpu = Cpu::new();
                        cpu.pc = CODE;
                        cpu.v[0] = vreg(t, vals, k3 + 1);
                        cpu.v[1] = vreg(t, vals, i);
                        cpu.v[2] = vreg(t, vals, j);
                        cpu.v[3] = vreg(t, vals, k3);
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
    eprintln!("{runs} casi; {:?}", jit.stats);
}

/// I percorsi veloci servono davvero: con valori normali, FPCR = 0 e IXC
/// già a 1 nessuna istruzione della tabella chiama `env.simd` (un
/// percorso veloce rotto, che ripiega sempre sull'interprete, darebbe
/// comunque risultati giusti: lo trova solo questo test).
#[test]
fn percorsi_veloci_usati() {
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    for &(word, t, name) in CASES {
        let (a, b, c) = match t {
            T::S => (0x3fc0_0000u128 * 0x1_0000_0001_0000_0001_0000_0001, 0x4040_0000u128, 0x3dcc_cccdu128),
            T::D => (
                0x3ff8_0000_0000_0000u128 * 0x1_0000_0000_0000_0001,
                0x4008_0000_0000_0000,
                0x3fb9_9999_9999_999a,
            ),
        };
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        cpu.v[1] = a;
        cpu.v[2] = b | b << 32 | b << 64 | b << 96;
        cpu.v[3] = c;
        cpu.v[0] = c;
        cpu.x[1] = 12345;
        cpu.fpsr = 0x10;
        let before = vetro_jit::helper::calls();
        let (want, got) = run_one(&mut jit, word, &cpu);
        assert_eq!(want, got, "{name}");
        assert_eq!(vetro_jit::helper::calls(), before, "{name}: ha chiamato env.simd");
    }
}

/// FMA (arrotondamento a dispari in singola, FMA emulata in doppia) su
/// terne casuali: mantisse qualsiasi, esponenti vicini (cancellazioni) e
/// lontani, con e senza IXC. Bit per bit come l'interprete.
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
        /// f64 con esponente vicino a `base` (cancellazioni) o qualsiasi.
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
            "{word:#010x} v1={:#x} v2={:#x} v3={:#x} v0={:#x}: interprete {:#x}/{:#x}, JIT {:#x}/{:#x}",
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

/// Il doppio arrotondamento che l'arrotondamento a dispari evita: il
/// prodotto esatto è un punto medio (1 + 2^-24 in singola, 1 + 2^-53 in
/// doppia: 24929 × 673 = 2^24 + 1, 321 × 28059810762433 = 2^53 + 1) e
/// l'addendo minuscolo lo sposta appena sopra. La FMA corretta arrotonda in
/// su; senza l'arrotondamento a dispari si arrotonderebbe al pari, in giù.
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
        assert_eq!(i.v[0], want, "{word:#x}: l'interprete arrotonda in su");
        assert_eq!(i, j, "{word:#x}");
        assert_eq!(vetro_jit::helper::calls(), before, "{word:#x}: percorso veloce");
    }
}

/// Il prodotto minuscolo che si arrotonda al più piccolo normale: l'Arm
/// segnala UFC e IXC (minuscolità prima dell'arrotondamento), il WASM dà
/// un normale. Con FPSR a IXC il percorso veloce, se lo accettasse, perderebbe
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
        assert_eq!(want.fpsr & 0x8, 0x8, "{word:#x}: l'interprete segnala UFC");
        assert_eq!(want, got, "{word:#x}");
    }
}
