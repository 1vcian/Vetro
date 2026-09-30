//! In-process interpreter-JIT parity, without ELF or QEMU: random programs
//! of instructions (the translated ones plus a few that stay with the interpreter)
//! run by `Cpu::step` and by `JitCpu::run` starting from the same state.
//! Compared, step by step: the exceptions (and at which step they arrive),
//! the final state and the memory.
//!
//! Unlike the programs in `tests/diff`, here there are also backward
//! branches (loops), mid-block faults, stores to the code (self-modifying
//! code) and randomly split budgets, to exercise the
//! FAULT/STOP/SVC exits and invalidation.
//!
//! `VETRO_JIT_PARITY_CASES` (default 3000) and `VETRO_JIT_PARITY_SEED`.

use vetro_cpu::{Cpu, Exception, Insn, Memory, Perm, UserMemory, decode};
use vetro_jit::translate::{Kind, kind};
use vetro_jit::{Engine, Host, JitConfig, JitCpu};
use vetro_jit_native::{NativeEngine, NativeModule};

const CODE: u64 = 0x40_0000;
const CODE_LEN: usize = 0x3000;
/// Programs start shortly before a page boundary.
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

/// Replaces the field `[lo, lo+width)` of `w` with `v`.
fn with_field(w: u32, lo: u32, width: u32, v: i64) -> u32 {
    let mask = ((1u64 << width) - 1) as u32;
    (w & !(mask << lo)) | (((v as u32) & mask) << lo)
}

/// A random instruction: random bits filtered by the decoder, with the offsets
/// of branches and literal loads brought close.
fn random_insn(rng: &mut Rng, simd: bool) -> u32 {
    if simd && rng.below(4) != 0 {
        // SIMD/FP classes (bits 27:25 = x111) and, once in eight,
        // loads/stores of the V registers (bits 27:25 = 110).
        let class = if rng.below(8) == 0 { 6 } else { 7 };
        loop {
            let w = (rng.next() as u32 & !(7 << 25)) | class << 25;
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
            // Some untranslated instructions (SIMD, exclusives, CRC...) remain,
            // to alternate JIT and interpreter; never UNDEFINED (it would stop everything).
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
            // Register branches: rare (the target is almost always outside).
            Insn::BranchReg { .. } if rng.below(4) != 0 => continue,
            _ => w,
        };
        debug_assert_eq!(kind(&decode(w)), k);
        return w;
    }
}

/// Exclusive pairs (encodings from tools/a64asm.sh): in user mode the
/// JIT translates them with the monitor in `JitState` (ADR 0026).
const EXCLUSIVE_PAIRS: [(u32, u32); 3] = [
    (0xc85f7c20, 0xc8027c20), // ldxr x0, [x1] ; stxr w2, x0, [x1]
    (0xc87f0c20, 0xc8220c20), // ldxp x0, x3, [x1] ; stxp w2, x0, x3, [x1]
    (0x085f7c20, 0x48027c20), // ldxrb w0, [x1] ; stxrh w2, w0, [x1]
];

/// Random program for seed `seed`; with `simd` three instructions out of four
/// are SIMD/FP (ADR 0026).
fn setup_with(seed: u64, simd: bool) -> (Cpu, UserMemory) {
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5eed);
    let mut mem = UserMemory::new();
    let mut code = vec![0u8; CODE_LEN];
    let off = (START - CODE) as usize;
    let mut prog = Vec::with_capacity(PROG_LEN);
    while prog.len() < PROG_LEN {
        if prog.len() + 3 <= PROG_LEN && rng.below(24) == 0 {
            // LDXR, any instruction, STXR on the same base.
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
    // Writable code: stores may land on the program.
    mem.map(CODE, code, Perm::RWX).unwrap();
    let data: Vec<u8> = (0..DATA_LEN).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) as u8 >> 1).collect();
    mem.map(DATA, data, Perm::RW).unwrap();
    let mut cpu = Cpu::new();
    cpu.pc = START;
    cpu.sp = DATA + DATA_LEN as u64 / 2;
    cpu.nzcv = (rng.below(16) as u32) << 28;
    // With SIMD programs, more valid bases: loads/stores should not end the
    // program at the first access.
    let data_ptrs = if simd { 5 } else { 2 };
    for x in cpu.x.iter_mut() {
        *x = match rng.below(8) {
            k if k <= data_ptrs => DATA + rng.below(DATA_LEN as u64 - 0x2000) + 0x1000,
            3 | 6 => START + 4 * rng.below(PROG_LEN as u64),
            4 => rng.below(64),
            5 => (rng.below(64) as i64 - 32) as u64,
            _ => rng.next(),
        };
    }
    // SIMD/FP registers (ADR 0026): special FP values (zeros, denormals,
    // infinities, quiet and signalling NaNs, integer limits) and
    // random ones; FPCR with rounding modes, FZ and DN, FPSR with or without flags.
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
                    // Nearby normals: exact and inexact sums and products.
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

/// Outcome of a run: exceptions with the step at which they arrive, final
/// state and memory.
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

/// SVCs do not stop execution (like a syscall that does nothing);
/// other exceptions do.
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
        assert!(n >= 1 && n <= budget, "steps {n} outside the budget {budget}");
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
        s += &format!("  events: interpreter={:?}\n          jit        ={:?}\n", a.events, b.events);
    }
    if a.steps != b.steps {
        s += &format!("  steps: {} vs {}\n", a.steps, b.steps);
    }
    for r in 0..31 {
        if a.cpu.x[r] != b.cpu.x[r] {
            s += &format!("  x{r}: {:#x} vs {:#x}\n", a.cpu.x[r], b.cpu.x[r]);
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
            s += &format!("  v{r}: {:#034x} vs {:#034x}\n", a.cpu.v[r], b.cpu.v[r]);
        }
    }
    if a.cpu.fpsr != b.cpu.fpsr {
        s += &format!("  fpsr: {:#x} vs {:#x}\n", a.cpu.fpsr, b.cpu.fpsr);
    }
    if a.code != b.code {
        s += "  code differs\n";
    }
    if let Some(i) = a.data.iter().zip(&b.data).position(|(x, y)| x != y) {
        s += &format!("  data differs from {:#x}\n", DATA + i as u64);
    }
    s
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Compares cases `first..first+cases` on a single JIT instance: the
/// cache across different spaces (and module reuse) also goes through the comparison.
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
            "seed {seed}: interpreter and JIT diverge\n{d}program:\n{}",
            describe(seed, simd)
        );
    }
    eprintln!("{cases} programs, {total_steps} steps; {:?}", jit.stats);
}

#[test]
fn random_programs_interpreter_equals_jit() {
    let cases = env_u64("VETRO_JIT_PARITY_CASES", 3000);
    let first = env_u64("VETRO_JIT_PARITY_SEED", 0);
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, first, cases, false);
    let s = jit.stats;
    // The comparison makes sense only if the JIT really did work, and if all the
    // exits were exercised.
    assert!(s.jit_steps > s.interp_steps, "the JIT executed too little: {s:?}");
    assert!(s.faults > 0 && s.stops > 0 && s.invalidated_pages > 0, "exits not exercised: {s:?}");
}

/// Programs made three quarters of SIMD/FP instructions (ADR 0026): the
/// inline forms (integer SIMD with v128, fast FP paths with their
/// conditions) and `env.simd` give the interpreter's V registers, FPSR and
/// memory, with special FP values and random FPCR and FPSR.
/// `VETRO_JIT_SIMD_CASES` (default 3000) and `VETRO_JIT_PARITY_SEED`.
#[test]
fn random_simd_programs_interpreter_equals_jit() {
    let cases = env_u64("VETRO_JIT_SIMD_CASES", 3000);
    let first = env_u64("VETRO_JIT_PARITY_SEED", 0);
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, first + 1_000_000, cases, true);
    let s = jit.stats;
    assert!(s.jit_steps > s.interp_steps, "the JIT executed too little: {s:?}");
}

/// Engine that fills up after `cap` modules (like wasmtime after 10000
/// instances) until it is reset.
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
            return Err("full".into());
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

/// Engine resets mid-run (also with the state in
/// JitState): the result does not change.
#[test]
fn engine_reset_keeps_parity() {
    let engine = Limited { inner: NativeEngine::new(), cap: 40, used: 0 };
    let mut jit = JitCpu::new(engine, JitConfig { hot_threshold: 0, ..JitConfig::default() });
    run_cases(&mut jit, 100_000, 300, false);
    assert!(jit.stats.resets > 5, "{:?}", jit.stats);
}

/// Block with a fault in the middle: the state is the one before the instruction
/// and the exception (with the address) is the interpreter's.
#[test]
fn fault_in_the_middle_of_a_block_is_precise() {
    // Encodings from tools/a64asm.sh.
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
    cpu.x[3] = 0xdead_0000; // unmapped
    let mut jit = JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
    let (n, r) = jit.run(&mut cpu, &mut mem, 100);
    assert_eq!(n, 2);
    assert_eq!(r, Err(Exception::DataAbort { addr: 0xdead_0000, write: false }));
    assert_eq!(cpu.pc, CODE + 4);
    assert_eq!(cpu.x[1], 1);
    assert_eq!(jit.stats.faults, 1);
}

/// The emulated kernel rewrites the code (here with `poke`, as read(2) does in
/// an RWX page): the old block must not run any more.
#[test]
fn kernel_write_invalidates_blocks() {
    // Infinite loop that sets x0 = 1 again (encodings from tools/a64asm.sh).
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
    // mprotect without execute: the next instruction is an Instruction
    // Abort, as for the interpreter.
    mem.protect(CODE, CODE + 8, Perm::RW).unwrap();
    let (n, r) = jit.run(&mut cpu, &mut mem, 10);
    assert_eq!(n, 1);
    assert!(matches!(r, Err(Exception::InstructionAbort { .. })), "{r:?}");
}

/// The budget is honoured even in the middle of a long block.
#[test]
fn budget_is_exact() {
    // 8 times add x0, x0, #1, then back (encodings from tools/a64asm.sh).
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
    // 9 instructions per round, 8 adds.
    let adds = (total / 9) * 8 + (total % 9).min(8);
    assert_eq!(cpu.x[0], adds);
}

/// Q accesses (16 bytes) straddling a page (ADR 0024): the interpreter
/// checks the whole access before writing, the JIT (two 8-byte halves) must
/// leave the same memory. Without the `q_checks` check the first
/// half of STR Q stays written and the test fails.
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
        // A writable page followed by a read-only one.
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
    // (x1, x3, x4): STR Q that spills into the RO page; successful STR and LDR
    // straddling (readable); STP Q with the second Q spilling over.
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
        assert_eq!(ev_j, ev_i, "case {x1:#x} {x3:#x} {x4:#x}");
        assert_eq!(cpu_j, cpu_i, "case {x1:#x} {x3:#x} {x4:#x}");
        assert!(dump(&mut mem_j) == dump(&mut mem_i), "memory differs: case {x1:#x} {x3:#x} {x4:#x}");
        assert!(jit.stats.jit_steps > 0 || jit.stats.faults > 0);
    }
}

/// Chaining (ADR 0026): after the emulated kernel rewrites the page
/// of a region, the dispatcher must not enter it again from a stale entry
/// of the branch cache (the space context changes). A branches to B
/// (another page) and B to A: A's entry stays in the cache even when the
/// run restarts from B.
#[test]
fn concatenamento_dopo_invalidazione() {
    // Encodings from tools/a64asm.sh.
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
    assert!(jit.stats.block_runs < 10, "the regions are chained: {:?}", jit.stats);
    mem.poke(CODE, &MOV2.to_le_bytes()).unwrap();
    cpu.pc = CODE + 0x1000;
    cpu.x[0] = 0;
    assert_eq!(jit.run(&mut cpu, &mut mem, 100), (100, Ok(())));
    assert_eq!(cpu.x[0], 2, "A's old region no longer runs");
}

/// FPSR.QC of the saturating integer SIMD instructions translated inline
/// (ADR 0026): SQXTN(2)/SQXTUN(2) at both sizes and [SU]Q{ADD,SUB} at 8 and 16
/// bits, on lanes at and around the saturation limits, including inputs whose
/// saturated result equals the truncation (0x017f -> 0x7f for SQXTN), where
/// the JIT once missed QC (found by the nightly `tests/diff` run, seed
/// 3000056). V registers and FPSR must equal the interpreter's, and every
/// instruction must set QC in some cases. Fails if QC is computed from the
/// truncation again (tried).
#[test]
fn saturating_simd_sets_qc_like_the_interpreter() {
    // Encodings from tools/a64asm.sh.
    let insns: [(u32, &str); 10] = [
        (0x0e214841, "sqxtn v1.8b, v2.8h"),
        (0x4e214841, "sqxtn2 v1.16b, v2.8h"),
        (0x2e612841, "sqxtun v1.4h, v2.4s"),
        (0x6e612841, "sqxtun2 v1.8h, v2.4s"),
        (0x0e614841, "sqxtn v1.4h, v2.4s"),
        (0x2e212841, "sqxtun v1.8b, v2.8h"),
        (0x6e230c41, "uqadd v1.16b, v2.16b, v3.16b"),
        (0x4e632c41, "sqsub v1.8h, v2.8h, v3.8h"),
        (0x2e232c41, "uqsub v1.8b, v2.8b, v3.8b"),
        (0x0e630c41, "sqadd v1.4h, v2.4h, v3.4h"),
    ];
    const B: u32 = 0x14000000; // b .
    let h16: [u16; 16] = [
        0, 1, 0x7f, 0x80, 0xff, 0x100, 0x17f, 0x1ff, 0x27f, 0x7fff, 0x8000, 0xff80, 0xff7f, 0xfe7f, 0xfeff,
        0xffff,
    ];
    let w32: [u32; 12] = [
        0x7fff,
        0x8000,
        0xffff,
        0x1_0000,
        0x1_7fff,
        0x1_ffff,
        0xffff_8000,
        0xffff_7fff,
        0xfffe_7fff,
        0xfffe_ffff,
        1,
        0,
    ];
    let mut rng = Rng(0x5a7);
    for (w, name) in insns {
        let mut code = Vec::new();
        for x in [w, B] {
            code.extend_from_slice(&x.to_le_bytes());
        }
        let mut jit =
            JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
        let mut with_qc = 0;
        for case in 0..400 {
            let mut v = [0u128; 3];
            for r in v.iter_mut() {
                for k in 0..4 {
                    let word = if rng.below(2) == 0 {
                        let a = h16[rng.below(16) as usize] as u32;
                        let b = h16[rng.below(16) as usize] as u32;
                        a | b << 16
                    } else if rng.below(4) == 0 {
                        rng.next() as u32
                    } else {
                        w32[rng.below(12) as usize]
                    };
                    *r |= (word as u128) << (32 * k);
                }
            }
            let mut cpu = Cpu::new();
            cpu.pc = CODE;
            cpu.v[1] = v[0];
            cpu.v[2] = v[1];
            cpu.v[3] = v[2];
            cpu.fpsr = if case % 7 == 0 { 1 << 27 } else { 0 };
            let mut mem = UserMemory::new();
            mem.map(CODE, code.clone(), Perm::RX).unwrap();
            let mut want = cpu.clone();
            let mut wmem = UserMemory::new();
            wmem.map(CODE, code.clone(), Perm::RX).unwrap();
            for _ in 0..2 {
                want.step(&mut wmem).unwrap();
            }
            let (n, r) = jit.run(&mut cpu, &mut mem, 2);
            assert_eq!((n, r), (2, Ok(())));
            assert_eq!(
                (cpu.v, cpu.fpsr),
                (want.v, want.fpsr),
                "{name}: V2 {:#034x}, V3 {:#034x}, FPSR {:#x} before",
                v[1],
                v[2],
                if case % 7 == 0 { 1u32 << 27 } else { 0 }
            );
            with_qc += u32::from(case % 7 != 0 && want.fpsr & 1 << 27 != 0);
        }
        assert!(jit.stats.jit_steps >= 400, "{name}: not run by the JIT: {:?}", jit.stats);
        assert!(with_qc > 20, "{name}: QC set in only {with_qc} cases: test too weak");
    }
}

/// The cryptographic extension in the regions (ADR 0040): every AES, SHA1,
/// SHA256 and 64-bit PMULL form, with random and special registers (zero,
/// all ones, a register used as both source and destination), must leave the
/// V registers and FPSR exactly as the interpreter does, without calling
/// `env.simd`. Breaking any piece (a S-box slice, a ShiftRows lane, the
/// InvMixColumns pre-multiplication, a carry-less product mask, a SHA round
/// rotation) turns it red.
#[test]
fn crypto_matches_the_interpreter_without_env_simd() {
    // Encodings from tools/a64asm.sh.
    let insns: [(u32, &str); 19] = [
        (0x4e284841, "aese v1.16b, v2.16b"),
        (0x4e285841, "aesd v1.16b, v2.16b"),
        (0x4e286841, "aesmc v1.16b, v2.16b"),
        (0x4e287841, "aesimc v1.16b, v2.16b"),
        (0x0ee3e041, "pmull v1.1q, v2.1d, v3.1d"),
        (0x4ee3e041, "pmull2 v1.1q, v2.2d, v3.2d"),
        (0x5e034041, "sha256h q1, q2, v3.4s"),
        (0x5e035041, "sha256h2 q1, q2, v3.4s"),
        (0x5e282841, "sha256su0 v1.4s, v2.4s"),
        (0x5e036041, "sha256su1 v1.4s, v2.4s, v3.4s"),
        (0x5e030041, "sha1c q1, s2, v3.4s"),
        (0x5e031041, "sha1p q1, s2, v3.4s"),
        (0x5e032041, "sha1m q1, s2, v3.4s"),
        (0x5e033041, "sha1su0 v1.4s, v2.4s, v3.4s"),
        (0x5e281841, "sha1su1 v1.4s, v2.4s"),
        (0x5e280841, "sha1h s1, s2"),
        (0x4e284821, "aese v1.16b, v1.16b"),
        (0x4e286821, "aesmc v1.16b, v1.16b"),
        (0x5e014021, "sha256h q1, q1, v1.4s"),
    ];
    const B: u32 = 0x14000000; // b .
    let mut rng = Rng(0xae5);
    for (w, name) in insns {
        let mut code = Vec::new();
        for x in [w, B] {
            code.extend_from_slice(&x.to_le_bytes());
        }
        let mut jit =
            JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
        let before = vetro_jit::helper::calls();
        for case in 0..300 {
            let mut v = [0u128; 3];
            for r in v.iter_mut() {
                *r = match (case + rng.below(3) as usize) % 8 {
                    0 => 0,
                    1 => u128::MAX,
                    2 => (rng.next() as u128) << 64,
                    _ => (rng.next() as u128) << 64 | rng.next() as u128,
                };
            }
            let mut cpu = Cpu::new();
            cpu.pc = CODE;
            cpu.v[1] = v[0];
            cpu.v[2] = v[1];
            cpu.v[3] = v[2];
            cpu.v[4] = rng.next() as u128;
            cpu.fpsr = if case % 5 == 0 { 1 << 27 } else { 0 };
            let mut mem = UserMemory::new();
            mem.map(CODE, code.clone(), Perm::RX).unwrap();
            let mut want = cpu.clone();
            let mut wmem = UserMemory::new();
            wmem.map(CODE, code.clone(), Perm::RX).unwrap();
            for _ in 0..2 {
                want.step(&mut wmem).unwrap();
            }
            let (n, r) = jit.run(&mut cpu, &mut mem, 2);
            assert_eq!((n, r), (2, Ok(())));
            assert_eq!(
                (cpu.v, cpu.fpsr),
                (want.v, want.fpsr),
                "{name}: V1 {:#034x}, V2 {:#034x}, V3 {:#034x} before",
                v[0],
                v[1],
                v[2]
            );
        }
        assert!(jit.stats.jit_steps >= 300, "{name}: not run by the JIT: {:?}", jit.stats);
        assert_eq!(vetro_jit::helper::calls(), before, "{name}: called env.simd");
    }
}

/// The widening, narrowing, by-element, halving and saturating-doubling
/// integer SIMD forms inline (ADR 0040): V registers and FPSR (QC) exactly as
/// the interpreter, for every size and "2" form, with lanes at the limits
/// (0, 1, the extremes of each width, around the rounding and saturation
/// points) and random ones, without calling `env.simd`. Registers used as
/// source and destination at once too. Breaking a rounding bit, a saturation
/// clamp, a lane selection or the QC condition turns it red.
#[test]
fn more_integer_simd_matches_the_interpreter_without_env_simd() {
    // Encodings from tools/a64asm.sh.
    let insns: [(u32, &str); 66] = [
        (0x0e230041, "saddl v1.8h, v2.8b, v3.8b"),
        (0x6e630041, "uaddl2 v1.4s, v2.8h, v3.8h"),
        (0x0ea31041, "saddw v1.2d, v2.2d, v3.2s"),
        (0x6e231041, "uaddw2 v1.8h, v2.8h, v3.16b"),
        (0x0e632041, "ssubl v1.4s, v2.4h, v3.4h"),
        (0x6ea32041, "usubl2 v1.2d, v2.4s, v3.4s"),
        (0x4e633041, "ssubw2 v1.4s, v2.4s, v3.8h"),
        (0x2e233041, "usubw v1.8h, v2.8h, v3.8b"),
        (0x0e235041, "sabal v1.8h, v2.8b, v3.8b"),
        (0x6e635041, "uabal2 v1.4s, v2.8h, v3.8h"),
        (0x4ea37041, "sabdl2 v1.2d, v2.4s, v3.4s"),
        (0x2e237041, "uabdl v1.8h, v2.8b, v3.8b"),
        (0x0e638041, "smlal v1.4s, v2.4h, v3.4h"),
        (0x6e238041, "umlal2 v1.8h, v2.16b, v3.16b"),
        (0x4ea3a041, "smlsl2 v1.2d, v2.4s, v3.4s"),
        (0x2e63a041, "umlsl v1.4s, v2.4h, v3.4h"),
        (0x0e23c041, "smull v1.8h, v2.8b, v3.8b"),
        (0x6ea3c041, "umull2 v1.2d, v2.4s, v3.4s"),
        (0x0e234041, "addhn v1.8b, v2.8h, v3.8h"),
        (0x6e634041, "raddhn2 v1.8h, v2.4s, v3.4s"),
        (0x4ea36041, "subhn2 v1.4s, v2.2d, v3.2d"),
        (0x2e636041, "rsubhn v1.4h, v2.4s, v3.4s"),
        (0x4f538841, "mul v1.8h, v2.8h, v3.h[5]"),
        (0x2fa30841, "mla v1.2s, v2.2s, v3.s[3]"),
        (0x6fa34041, "mls v1.4s, v2.4s, v3.s[1]"),
        (0x0f73a841, "smull v1.4s, v2.4h, v3.h[7]"),
        (0x6f83a841, "umull2 v1.2d, v2.4s, v3.s[2]"),
        (0x4f432041, "smlal2 v1.4s, v2.8h, v3.h[0]"),
        (0x2fa36041, "umlsl v1.2d, v2.2s, v3.s[1]"),
        (0x4f73c041, "sqdmulh v1.8h, v2.8h, v3.h[3]"),
        (0x0f63d841, "sqrdmulh v1.4h, v2.4h, v3.h[6]"),
        (0x0fa3c041, "sqdmulh v1.2s, v2.2s, v3.s[1]"),
        (0x4f83d041, "sqrdmulh v1.4s, v2.4s, v3.s[0]"),
        (0x4e63b441, "sqdmulh v1.8h, v2.8h, v3.8h"),
        (0x6ea3b441, "sqrdmulh v1.4s, v2.4s, v3.4s"),
        (0x2e63b441, "sqrdmulh v1.4h, v2.4h, v3.4h"),
        (0x0ea3b441, "sqdmulh v1.2s, v2.2s, v3.2s"),
        (0x4e230441, "shadd v1.16b, v2.16b, v3.16b"),
        (0x6ea30441, "uhadd v1.4s, v2.4s, v3.4s"),
        (0x4e631441, "srhadd v1.8h, v2.8h, v3.8h"),
        (0x2ea31441, "urhadd v1.2s, v2.2s, v3.2s"),
        (0x4f0d2441, "srshr v1.16b, v2.16b, #0x3"),
        (0x6f202441, "urshr v1.4s, v2.4s, #0x20"),
        (0x4f402441, "srshr v1.2d, v2.2d, #0x40"),
        (0x6f1f3441, "ursra v1.8h, v2.8h, #0x1"),
        (0x0f393441, "srsra v1.2s, v2.2s, #0x7"),
        (0x6f732441, "urshr v1.2d, v2.2d, #0xd"),
        (0x6f084441, "sri v1.16b, v2.16b, #0x8"),
        (0x6f3b4441, "sri v1.4s, v2.4s, #0x5"),
        (0x6f105441, "sli v1.8h, v2.8h, #0x0"),
        (0x6f7f5441, "sli v1.2d, v2.2d, #0x3f"),
        (0x0f0d8c41, "rshrn v1.8b, v2.8h, #0x3"),
        (0x4f108c41, "rshrn2 v1.8h, v2.4s, #0x10"),
        (0x0f0f9441, "sqshrn v1.8b, v2.8h, #0x1"),
        (0x4f179c41, "sqrshrn2 v1.8h, v2.4s, #0x9"),
        (0x2f109441, "uqshrn v1.4h, v2.4s, #0x10"),
        (0x6f0f9c41, "uqrshrn2 v1.16b, v2.8h, #0x1"),
        (0x2f0b8441, "sqshrun v1.8b, v2.8h, #0x5"),
        (0x6f1f8c41, "sqrshrun2 v1.8h, v2.4s, #0x1"),
        (0x2f108c41, "sqrshrun v1.4h, v2.4s, #0x10"),
        (0x2f089c41, "uqrshrn v1.8b, v2.8h, #0x8"),
        (0x4f52d042, "sqrdmulh v2.8h, v2.8h, v2.h[1]"),
        (0x2e628042, "umlal v2.4s, v2.4h, v2.4h"),
        (0x6f0d8c42, "sqrshrun2 v2.16b, v2.8h, #0x3"),
        (0x6e224042, "raddhn2 v2.16b, v2.8h, v2.8h"),
        (0x6f295442, "sli v2.4s, v2.4s, #0x9"),
    ];
    const B: u32 = 0x14000000; // b .
    const LANES16: [u16; 12] =
        [0, 1, 0x7f, 0x80, 0xff, 0x100, 0x7fff, 0x8000, 0x8001, 0xfffe, 0xffff, 0x4000];
    const LANES32: [u32; 10] = [
        0,
        1,
        0x7fff_ffff,
        0x8000_0000,
        0x8000_0001,
        0xffff_ffff,
        0x0000_8000,
        0x0001_0000,
        0x4000_0000,
        0xffff_7fff,
    ];
    let mut rng = Rng(0xa11);
    for (w, name) in insns {
        let mut code = Vec::new();
        for x in [w, B] {
            code.extend_from_slice(&x.to_le_bytes());
        }
        let mut jit =
            JitCpu::new(NativeEngine::new(), JitConfig { hot_threshold: 0, ..JitConfig::default() });
        let before = vetro_jit::helper::calls();
        let mut with_qc = 0;
        for case in 0..400 {
            let mut v = [0u128; 3];
            for r in v.iter_mut() {
                for k in 0..4 {
                    let word = match rng.below(5) {
                        // The saturating case of SQ(R)DMULH: the most negative values.
                        4 => [0x8000_8000, 0x8000_0000][rng.below(2) as usize],
                        0 => {
                            LANES16[rng.below(12) as usize] as u32
                                | (LANES16[rng.below(12) as usize] as u32) << 16
                        }
                        1 => LANES32[rng.below(10) as usize],
                        2 => ((rng.next() as u32) & 0x8080_8080) | (0x7f7f_7f7f * (rng.below(2) as u32)),
                        _ => rng.next() as u32,
                    };
                    *r |= (word as u128) << (32 * k);
                }
            }
            let mut cpu = Cpu::new();
            cpu.pc = CODE;
            cpu.v[1] = v[0];
            cpu.v[2] = v[1];
            cpu.v[3] = v[2];
            cpu.fpsr = if case % 7 == 0 { 1 << 27 } else { 0 };
            let mut mem = UserMemory::new();
            mem.map(CODE, code.clone(), Perm::RX).unwrap();
            let mut want = cpu.clone();
            let mut wmem = UserMemory::new();
            wmem.map(CODE, code.clone(), Perm::RX).unwrap();
            for _ in 0..2 {
                want.step(&mut wmem).unwrap();
            }
            let (n, r) = jit.run(&mut cpu, &mut mem, 2);
            assert_eq!((n, r), (2, Ok(())));
            assert_eq!(
                (cpu.v, cpu.fpsr),
                (want.v, want.fpsr),
                "{name}: V1 {:#034x}, V2 {:#034x}, V3 {:#034x}, FPSR {:#x} before",
                v[0],
                v[1],
                v[2],
                if case % 7 == 0 { 1u32 << 27 } else { 0 }
            );
            with_qc += u32::from(case % 7 != 0 && want.fpsr & 1 << 27 != 0);
        }
        assert!(jit.stats.jit_steps >= 400, "{name}: not run by the JIT: {:?}", jit.stats);
        assert_eq!(vetro_jit::helper::calls(), before, "{name}: called env.simd");
        // UQSHRN by the full width cannot saturate (the result fits).
        if (name.starts_with("sq") || name.starts_with("uq")) && name != "uqshrn v1.4h, v2.4s, #0x10" {
            assert!(with_qc > 5, "{name}: QC set in only {with_qc} cases: test too weak");
        }
    }
}
