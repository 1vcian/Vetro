//! Parità interprete-JIT in processo, senza ELF né QEMU: programmi casuali
//! di istruzioni (quelle tradotte più alcune che restano all'interprete)
//! eseguiti da `Cpu::step` e da `JitCpu::run` a partire dallo stesso stato.
//! Si confrontano, passo per passo, le eccezioni (e a che passo arrivano),
//! lo stato finale e la memoria.
//!
//! A differenza dei programmi di `tests/diff`, qui ci sono anche salti
//! all'indietro (cicli), fault a metà blocco, store sul codice (codice che
//! si modifica da sé) e budget spezzati a caso, per esercitare le uscite
//! FAULT/STOP/SVC e l'invalidazione.
//!
//! `VETRO_JIT_PARITY_CASES` (default 3000) e `VETRO_JIT_PARITY_SEED`.

use vetro_cpu::{Cpu, Exception, Insn, Memory, Perm, UserMemory, decode};
use vetro_jit::translate::{Kind, kind};
use vetro_jit::{Engine, Host, JitConfig, JitCpu};
use vetro_jit_native::{NativeEngine, NativeModule};

const CODE: u64 = 0x40_0000;
const CODE_LEN: usize = 0x3000;
/// I programmi iniziano poco prima di un confine di pagina.
const START: u64 = CODE + 0xe00;
const PROG_LEN: usize = 256;
const DATA: u64 = 0x1000_0000;
const DATA_LEN: usize = 0x2_0000;
const STEP_LIMIT: u64 = 4000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Sostituisce il campo `[lo, lo+width)` di `w` con `v`.
fn with_field(w: u32, lo: u32, width: u32, v: i64) -> u32 {
    let mask = ((1u64 << width) - 1) as u32;
    (w & !(mask << lo)) | (((v as u32) & mask) << lo)
}

/// Un'istruzione casuale: bit casuali filtrati dal decoder, con gli offset
/// dei salti e dei load letterali riportati vicino.
fn random_insn(rng: &mut Rng, simd: bool) -> u32 {
    if simd && rng.below(4) != 0 {
        // Classi SIMD/FP (bit 27:25 = x111), anche i load/store.
        loop {
            let w = (rng.next() as u32 & !(7 << 25)) | 7 << 25;
            if matches!(decode(w), Insn::Simd(_)) {
                return w;
            }
        }
    }
    loop {
        let mut w = rng.next() as u32;
        let insn = decode(w);
        let k = kind(&insn);
        if k == Kind::Unsupported {
            // Qualche istruzione non tradotta (SIMD, esclusive, CRC...) resta,
            // per alternare JIT e interprete; mai UNDEFINED (fermerebbe tutto).
            if matches!(insn, Insn::Undefined | Insn::Unimplemented(_)) || rng.below(8) != 0 {
                continue;
            }
            return w;
        }
        let near = rng.below(40) as i64 - 20;
        w = match insn {
            Insn::B { .. } => with_field(w, 0, 26, near),
            Insn::BCond { .. } | Insn::Cbz { .. } | Insn::LdLiteral { .. } => with_field(w, 5, 19, near),
            Insn::Tbz { .. } => with_field(w, 5, 14, near),
            // Salti a registro: rari (il bersaglio è quasi sempre fuori).
            Insn::BranchReg { .. } if rng.below(4) != 0 => continue,
            _ => w,
        };
        debug_assert_eq!(kind(&decode(w)), k);
        return w;
    }
}

/// Coppie esclusive (codifiche da tools/a64asm.sh): in modalità utente il
/// JIT le traduce col monitor in `JitState` (ADR 0026).
const EXCLUSIVE_PAIRS: [(u32, u32); 3] = [
    (0xc85f7c20, 0xc8027c20), // ldxr x0, [x1] ; stxr w2, x0, [x1]
    (0xc87f0c20, 0xc8220c20), // ldxp x0, x3, [x1] ; stxp w2, x0, x3, [x1]
    (0x085f7c20, 0x48027c20), // ldxrb w0, [x1] ; stxrh w2, w0, [x1]
];

/// Programma casuale del seme `seed`; con `simd` tre istruzioni su quattro
/// sono SIMD/FP (ADR 0026).
fn setup_with(seed: u64, simd: bool) -> (Cpu, UserMemory) {
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5eed);
    let mut mem = UserMemory::new();
    let mut code = vec![0u8; CODE_LEN];
    let off = (START - CODE) as usize;
    let mut prog = Vec::with_capacity(PROG_LEN);
    while prog.len() < PROG_LEN {
        if prog.len() + 3 <= PROG_LEN && rng.below(24) == 0 {
            // LDXR, un'istruzione qualsiasi, STXR sulla stessa base.
            let (ld, st) = EXCLUSIVE_PAIRS[rng.below(EXCLUSIVE_PAIRS.len() as u64) as usize];
            let rn = [1, 5, 9][rng.below(3) as usize];
            prog.push(with_field(ld, 5, 5, rn));
            prog.push(random_insn(&mut rng, simd));
            prog.push(with_field(st, 5, 5, rn));
        } else {
            prog.push(random_insn(&mut rng, simd));
        }
    }
    for (i, w) in prog.iter().enumerate() {
        code[off + 4 * i..off + 4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    // Codice scrivibile: gli store possono cadere sul programma.
    mem.map(CODE, code, Perm::RWX).unwrap();
    let data: Vec<u8> = (0..DATA_LEN).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) as u8 >> 1).collect();
    mem.map(DATA, data, Perm::RW).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = START;
    cpu.sp = DATA + DATA_LEN as u64 / 2;
    cpu.nzcv = (rng.below(16) as u32) << 28;
    for x in cpu.x.iter_mut() {
        *x = match rng.below(8) {
            0..=2 => DATA + rng.below(DATA_LEN as u64 - 0x2000) + 0x1000,
            3 => START + 4 * rng.below(PROG_LEN as u64),
            4 => rng.below(64),
            5 => (rng.below(64) as i64 - 32) as u64,
            _ => rng.next(),
        };
    }
    // Registri SIMD/FP (ADR 0026): valori FP speciali (zeri, denormali,
    // infiniti, NaN silenziosi e segnalanti, limiti degli interi) e
    // casuali; FPCR con arrotondamenti, FZ e DN, FPSR con o senza flag.
    for v in cpu.v.iter_mut() {
        let lane = |rng: &mut Rng| -> u64 {
            const D: [u64; 12] = [
                0,
                1 << 63,
                0x7ff0_0000_0000_0000,
                0x7ff8_0000_0000_0000,
                0x7ff4_0000_0000_0001,
                1,
                0x0010_0000_0000_0000,
                0x3ff0_0000_0000_0000,
                0x43e0_0000_0000_0000,
                0x7fef_ffff_ffff_ffff,
                0xc1e0_0000_0000_0000,
                0x3fb9_9999_9999_999a,
            ];
            const S: [u32; 10] = [
                0,
                0x8000_0000,
                0x7f80_0000,
                0x7fc0_0000,
                0x7fa0_0001,
                1,
                0x0080_0000,
                0x3f80_0000,
                0x4f00_0000,
                0x3dcc_cccd,
            ];
            match rng.below(4) {
                0 => D[rng.below(D.len() as u64) as usize],
                1 => {
                    S[rng.below(S.len() as u64) as usize] as u64
                        | (S[rng.below(S.len() as u64) as usize] as u64) << 32
                }
                2 => {
                    // Normali vicini: somme e prodotti esatti e inesatti.
                    let e = 0x3f0 + rng.below(0x20);
                    e << 52 | rng.next() >> 12 & !((1u64 << rng.below(52)) - 1)
                }
                _ => rng.next(),
            }
        };
        *v = lane(&mut rng) as u128 | (lane(&mut rng) as u128) << 64;
    }
    cpu.fpcr = if rng.below(2) == 0 { 0 } else { (rng.below(32) as u32) << 22 };
    cpu.fpsr = match rng.below(3) {
        0 => 0,
        1 => 0x10,
        _ => rng.next() as u32 & 0x0800_009f,
    };
    (cpu, mem)
}

/// Esito di un'esecuzione: eccezioni con il passo a cui arrivano, stato
/// finale e memoria.
#[derive(Debug, PartialEq)]
struct Trace {
    events: Vec<(u64, Exception)>,
    steps: u64,
    cpu: Cpu,
    code: Vec<u8>,
    data: Vec<u8>,
}

fn snapshot(events: Vec<(u64, Exception)>, steps: u64, cpu: Cpu, mem: &mut UserMemory) -> Trace {
    let mut code = vec![0u8; CODE_LEN];
    mem.read(CODE, &mut code).unwrap();
    let mut data = vec![0u8; DATA_LEN];
    mem.read(DATA, &mut data).unwrap();
    Trace { events, steps, cpu, code, data }
}

/// Le SVC non fermano l'esecuzione (come una syscall che non fa nulla);
/// le altre eccezioni sì.
fn fatal(e: &Exception) -> bool {
    !matches!(e, Exception::Svc(_))
}

fn run_interp(mut cpu: Cpu, mut mem: UserMemory) -> Trace {
    let mut events = Vec::new();
    let mut steps = 0;
    while steps < STEP_LIMIT {
        let r = cpu.step(&mut mem);
        steps += 1;
        if let Err(e) = r {
            events.push((steps, e));
            if fatal(&e) {
                break;
            }
        }
    }
    snapshot(events, steps, cpu, &mut mem)
}

fn run_jit<E: Engine>(jit: &mut JitCpu<E>, mut cpu: Cpu, mut mem: UserMemory, rng: &mut Rng) -> Trace {
    let mut events = Vec::new();
    let mut steps = 0;
    while steps < STEP_LIMIT {
        let budget = match rng.below(4) {
            0 => 1 + rng.below(8),
            _ => 1 + rng.below(300),
        }
        .min(STEP_LIMIT - steps);
        let (n, r) = jit.run(&mut cpu, &mut mem, budget);
        assert!(n >= 1 && n <= budget, "passi {n} fuori dal budget {budget}");
        steps += n;
        if let Err(e) = r {
            events.push((steps, e));
            if fatal(&e) {
                break;
            }
        }
    }
    snapshot(events, steps, cpu, &mut mem)
}

fn describe(seed: u64, simd: bool) -> String {
    let (_, mut mem) = setup_with(seed, simd);
    let mut s = String::new();
    for i in 0..PROG_LEN.min(80) {
        let a = START + 4 * i as u64;
        let w = mem.fetch(a).unwrap();
        s += &format!("  {a:#x}: {w:08x} {:?}\n", decode(w));
    }
    s
}

fn diff(a: &Trace, b: &Trace) -> String {
    let mut s = String::new();
    if a.events != b.events {
        s += &format!("  eventi: interprete={:?}\n          jit       ={:?}\n", a.events, b.events);
    }
    if a.steps != b.steps {
        s += &format!("  passi: {} contro {}\n", a.steps, b.steps);
    }
    for r in 0..31 {
        if a.cpu.x[r] != b.cpu.x[r] {
            s += &format!("  x{r}: {:#x} contro {:#x}\n", a.cpu.x[r], b.cpu.x[r]);
        }
    }
    if a.cpu != b.cpu {
        s += &format!(
            "  sp {:#x}/{:#x} pc {:#x}/{:#x} nzcv {:#x}/{:#x}\n",
            a.cpu.sp, b.cpu.sp, a.cpu.pc, b.cpu.pc, a.cpu.nzcv, b.cpu.nzcv
        );
    }
    for r in 0..32 {
        if a.cpu.v[r] != b.cpu.v[r] {
            s += &format!("  v{r}: {:#034x} contro {:#034x}\n", a.cpu.v[r], b.cpu.v[r]);
        }
    }
    if a.cpu.fpsr != b.cpu.fpsr {
        s += &format!("  fpsr: {:#x} contro {:#x}\n", a.cpu.fpsr, b.cpu.fpsr);
    }
    if a.code != b.code {
        s += "  codice diverso\n";
    }
    if let Some(i) = a.data.iter().zip(&b.data).position(|(x, y)| x != y) {
        s += &format!("  dati diversi da {:#x}\n", DATA + i as u64);
    }
    s
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Confronta i casi `first..first+cases` su una sola istanza del JIT: anche
/// la cache fra spazi diversi (e il riuso dei moduli) passa dal confronto.
fn run_cases<E: Engine>(jit: &mut JitCpu<E>, first: u64, cases: u64, simd: bool) {
    let mut total_steps = 0;
    for seed in first..first + cases {
        let (cpu, mem) = setup_with(seed, simd);
        let want = run_interp(cpu.clone(), mem.clone());
        let mut rng = Rng(seed ^ 0xb0d6e7);
        let got = run_jit(jit, cpu, mem, &mut rng);
        total_steps += want.steps;
        let d = diff(&want, &got);
        assert!(
            d.is_empty(),
            "seme {seed}: interprete e JIT divergono\n{d}programma:\n{}",
            describe(seed, simd)
        );
    }
    eprintln!("{cases} programmi, {total_steps} passi; {:?}", jit.stats);
}

#[test]
fn random_programs_interpreter_equals_jit() {
    let cases = env_u64("VETRO_JIT_PARITY_CASES", 3000);
    let first = env_u64("VETRO_JIT_PARITY_SEED", 0);
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, first, cases, false);
    let s = jit.stats;
    // Il confronto ha senso solo se il JIT ha lavorato davvero, e se tutte le
    // uscite sono state esercitate.
    assert!(s.jit_steps > s.interp_steps, "il JIT ha eseguito troppo poco: {s:?}");
    assert!(s.faults > 0 && s.stops > 0 && s.invalidated_pages > 0, "uscite non esercitate: {s:?}");
}

/// Programmi fatti per tre quarti di istruzioni SIMD/FP (ADR 0026): le
/// forme in linea (SIMD intero con v128, percorsi veloci FP con le loro
/// condizioni) e `env.simd` danno registri V, FPSR e memoria
/// dell'interprete, con valori FP speciali, FPCR e FPSR casuali.
/// `VETRO_JIT_SIMD_CASES` (default 3000) e `VETRO_JIT_PARITY_SEED`.
#[test]
fn random_simd_programs_interpreter_equals_jit() {
    let cases = env_u64("VETRO_JIT_SIMD_CASES", 3000);
    let first = env_u64("VETRO_JIT_PARITY_SEED", 0);
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, first + 1_000_000, cases, true);
    let s = jit.stats;
    assert!(s.jit_steps > s.interp_steps, "il JIT ha eseguito troppo poco: {s:?}");
}

/// Motore che si riempie dopo `cap` moduli (come wasmtime dopo 10000
/// istanze) finché non lo si azzera.
struct Limited {
    inner: NativeEngine,
    cap: u32,
    used: u32,
}

impl Engine for Limited {
    type Module = NativeModule;
    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String> {
        self.inner.runtime(wasm)
    }
    fn compile(&mut self, wasm: &[u8]) -> Result<NativeModule, String> {
        if self.used == self.cap {
            return Err("pieno".into());
        }
        self.used += 1;
        self.inner.compile(wasm)
    }
    fn run(&mut self, m: &NativeModule, index: u32, state: u32, host: &mut dyn Host) -> u32 {
        self.inner.run(m, index, state, host)
    }
    fn memory(&mut self) -> &mut [u8] {
        self.inner.memory()
    }
    fn place(&mut self, m: &NativeModule, count: u32, base: u32) {
        self.inner.place(m, count, base)
    }
    fn reserve(&mut self, bytes: usize) {
        self.inner.reserve(bytes)
    }
    fn reset(&mut self) {
        self.used = 0;
        self.inner.reset();
    }
}

/// Azzeramenti del motore a metà esecuzione (anche con lo stato in
/// JitState): il risultato non cambia.
#[test]
fn engine_reset_keeps_parity() {
    let engine = Limited { inner: NativeEngine::new(), cap: 40, used: 0 };
    let mut jit = JitCpu::new(engine, JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, 100_000, 300, false);
    assert!(jit.stats.resets > 5, "{:?}", jit.stats);
}

/// Blocco con un fault a metà: lo stato è quello di prima dell'istruzione
/// e l'eccezione (con l'indirizzo) quella dell'interprete.
#[test]
fn fault_in_the_middle_of_a_block_is_precise() {
    // Codifiche da tools/a64asm.sh.
    let words = [
        0x91000421u32, // add x1, x1, #1
        0xf9400062,    // ldr x2, [x3]
        0x91000421,    // add x1, x1, #1
        0x14000000,    // b .
    ];
    let mut mem = UserMemory::new();
    let mut code = Vec::new();
    for w in words {
        code.extend_from_slice(&w.to_le_bytes());
    }
    mem.map(CODE, code, Perm::RX).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = CODE;
    cpu.x[3] = 0xdead_0000; // non mappato
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let (n, r) = jit.run(&mut cpu, &mut mem, 100);
    assert_eq!(n, 2);
    assert_eq!(r, Err(Exception::DataAbort { addr: 0xdead_0000, write: false }));
    assert_eq!(cpu.pc, CODE + 4);
    assert_eq!(cpu.x[1], 1);
    assert_eq!(jit.stats.faults, 1);
}

/// Il kernel emulato riscrive il codice (qui con `poke`, come fa read(2) in
/// una pagina RWX): il blocco vecchio non deve più girare.
#[test]
fn kernel_write_invalidates_blocks() {
    // Ciclo infinito che rimette x0 = 1 (codifiche da tools/a64asm.sh).
    let mut mem = UserMemory::new();
    let mut code = Vec::new();
    for w in [
        0xd2800020u32, // movz x0, #1
        0x17ffffff,    // b .-4
    ] {
        code.extend_from_slice(&w.to_le_bytes());
    }
    mem.map(CODE, code, Perm::RWX).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = CODE;
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let (n, r) = jit.run(&mut cpu, &mut mem, 10);
    assert_eq!((n, r), (10, Ok(())));
    assert_eq!(cpu.x[0], 1);
    mem.poke(CODE, &0xd2800040u32.to_le_bytes()).unwrap(); // movz x0, #2
    let (_, r) = jit.run(&mut cpu, &mut mem, 10);
    assert_eq!(r, Ok(()));
    assert_eq!(cpu.x[0], 2);
    assert_eq!(jit.stats.invalidated_pages, 1);
    // mprotect senza esecuzione: l'istruzione successiva è un Instruction
    // Abort, come per l'interprete.
    mem.protect(CODE, CODE + 8, Perm::RW).unwrap();
    let (n, r) = jit.run(&mut cpu, &mut mem, 10);
    assert_eq!(n, 1);
    assert!(matches!(r, Err(Exception::InstructionAbort { .. })), "{r:?}");
}

/// Il budget si rispetta anche a metà di un blocco lungo.
#[test]
fn budget_is_exact() {
    // 8 volte add x0, x0, #1, poi indietro (codifiche da tools/a64asm.sh).
    let mut mem = UserMemory::new();
    let mut code = Vec::new();
    for _ in 0..8 {
        code.extend_from_slice(&0x91000400u32.to_le_bytes()); // add x0, x0, #1
    }
    code.extend_from_slice(&0x17fffff8u32.to_le_bytes()); // b .-32
    mem.map(CODE, code, Perm::RX).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = CODE;
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let mut total = 0;
    for b in [1, 3, 9, 20, 5, 100, 7] {
        let (n, r) = jit.run(&mut cpu, &mut mem, b);
        assert_eq!((n, r), (b, Ok(())));
        total += b;
    }
    // 9 istruzioni per giro, 8 add.
    let adds = (total / 9) * 8 + (total % 9).min(8);
    assert_eq!(cpu.x[0], adds);
}

/// Accessi Q (16 byte) a cavallo di pagina (ADR 0024): l'interprete
/// controlla tutto l'accesso prima di scrivere, il JIT (due metà da 8) deve
/// lasciare la stessa memoria. Senza il controllo di `q_checks` la prima
/// metà di STR Q resta scritta e il test fallisce.
#[test]
fn q_a_cavallo_di_pagina_come_interprete() {
    let words = [
        0x3d800020u32, // str q0, [x1]
        0x3dc00062,    // ldr q2, [x3]
        0xad000480,    // stp q0, q1, [x4]
        0x14000000,    // b .
    ];
    let setup = |x1: u64, x3: u64, x4: u64| {
        let mut mem = UserMemory::new();
        let mut code = Vec::new();
        for w in words {
            code.extend_from_slice(&w.to_le_bytes());
        }
        mem.map(CODE, code, Perm::RX).unwrap();
        // Una pagina scrivibile seguita da una di sola lettura.
        mem.map(DATA, vec![0x11; 0x1000], Perm::RW).unwrap();
        mem.map(DATA + 0x1000, vec![0x22; 0x1000], Perm::R).unwrap();
        let mut cpu = Cpu::new();
        cpu.pc = CODE;
        cpu.v[0] = 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100;
        cpu.v[1] = 0x1f1e_1d1c_1b1a_1918_1716_1514_1312_1110;
        cpu.v[2] = u128::MAX;
        (cpu.x[1], cpu.x[3], cpu.x[4]) = (x1, x3, x4);
        (cpu, mem)
    };
    let dump = |mem: &mut UserMemory| {
        let mut b = vec![0u8; 0x2000];
        mem.read(DATA, &mut b).unwrap();
        b
    };
    // (x1, x3, x4): STR Q che sconfina nella pagina RO; STR riuscito e LDR
    // a cavallo (leggibile); STP Q col secondo Q che sconfina.
    let cases = [
        (DATA + 0xff8, DATA, DATA),
        (DATA + 0x100, DATA + 0xff8, DATA + 0xfe0),
        (DATA + 0x108, DATA + 0x10, DATA + 0xfe8),
    ];
    for (x1, x3, x4) in cases {
        let (mut cpu_i, mut mem_i) = setup(x1, x3, x4);
        let mut ev_i = Vec::new();
        for _ in 0..4 {
            if let Err(e) = cpu_i.step(&mut mem_i) {
                ev_i.push(e);
                break;
            }
        }
        let (mut cpu_j, mut mem_j) = setup(x1, x3, x4);
        let mut jit =
            JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
        let mut ev_j = Vec::new();
        let mut n = 0;
        while n < 4 {
            let (k, r) = jit.run(&mut cpu_j, &mut mem_j, 4 - n);
            n += k;
            if let Err(e) = r {
                ev_j.push(e);
                break;
            }
        }
        assert_eq!(ev_j, ev_i, "caso {x1:#x} {x3:#x} {x4:#x}");
        assert_eq!(cpu_j, cpu_i, "caso {x1:#x} {x3:#x} {x4:#x}");
        assert!(dump(&mut mem_j) == dump(&mut mem_i), "memoria diversa: caso {x1:#x} {x3:#x} {x4:#x}");
        assert!(jit.stats.jit_steps > 0 || jit.stats.faults > 0);
    }
}

/// Concatenamento (ADR 0026): dopo che il kernel emulato riscrive la pagina
/// di una regione, il dispatcher non deve più entrarci da una voce vecchia
/// della cache dei salti (il contesto dello spazio cambia). A salta a B
/// (altra pagina) e B ad A: la voce di A resta nella cache anche quando la
/// corsa riparte da B.
#[test]
fn concatenamento_dopo_invalidazione() {
    // Codifiche da tools/a64asm.sh.
    const MOV1: u32 = 0xd2800020; // movz x0, #1
    const MOV2: u32 = 0xd2800040; // movz x0, #2
    const A_TO_B: u32 = 0x140003ff; // b .+0xffc
    const B_TO_A: u32 = 0x17fffc00; // b .-0x1000
    let mut code = vec![0u8; 0x2000];
    code[0..4].copy_from_slice(&MOV1.to_le_bytes());
    code[4..8].copy_from_slice(&A_TO_B.to_le_bytes());
    code[0x1000..0x1004].copy_from_slice(&B_TO_A.to_le_bytes());
    let mut mem = UserMemory::new();
    mem.map(CODE, code, Perm::RWX).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = CODE + 0x1000;
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    assert_eq!(jit.run(&mut cpu, &mut mem, 100), (100, Ok(())));
    assert_eq!(cpu.x[0], 1);
    assert!(jit.stats.block_runs < 10, "le regioni si concatenano: {:?}", jit.stats);
    mem.poke(CODE, &MOV2.to_le_bytes()).unwrap();
    cpu.pc = CODE + 0x1000;
    cpu.x[0] = 0;
    assert_eq!(jit.run(&mut cpu, &mut mem, 100), (100, Ok(())));
    assert_eq!(cpu.x[0], 2, "la regione vecchia di A non gira più");
}
