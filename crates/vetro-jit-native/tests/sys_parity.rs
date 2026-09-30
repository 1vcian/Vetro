//! Interpreter-JIT parity in system mode (ADR 0013): random bare-metal
//! programs with the MMU on, run by `Cpu::step_system` and
//! by the machine loop with `SysJit` (translated blocks between one interpreter
//! step and the next, like `Machine::run`), from the same state.
//! Compared: the exceptions (and at which step they arrive), the final state
//! of the CPU (system registers and exclusive monitor included) and all of
//! RAM.
//!
//! In the JIT loop RAM lives inside wasmtime's memory, so the
//! blocks reach it through the software TLB (the fast path of
//! `ld`/`st`, the same as in the browser).
//!
//! The programs mix random instructions (filtered by the decoder) and
//! system instructions taken from a table (MRS/MSR, exclusives, DC ZVA,
//! CRC32, LDTR/STTR, SVC, TLBI...), with random registers; the data pages
//! have different permissions and attributes (unmapped, read-only, EL1 only,
//! Device), SCTLR_EL1.A/SA/SA0/DZE, TBI0 and SPSel are random, and some
//! of the programs start at EL0. Every exception goes to a handler that skips
//! the instruction (ELR += 4) and returns with ERET.
//!
//! `VETRO_JIT_SYS_PARITY_CASES` (default 400) and `VETRO_JIT_SYS_PARITY_SEED`.

use vetro_cpu::sys::{CpuEnv, SysEvent, sctlr};
use vetro_cpu::sysreg::EnvReg;
use vetro_cpu::{Cpu, Insn, SysConfig, decode};
use vetro_jit::translate::{Kind, SysTarget, kind_in};
use vetro_jit::{Engine, Host, Next, SysJit, SysJitConfig, SysPhys};
use vetro_jit_native::NativeEngine;
use vetro_mmu::{BusError, Mmu, MmuBus, PhysMemory};

const RAM_BASE: u64 = 0x4000_0000;
const RAM_LEN: usize = 8 << 20;
/// Exception vectors (VBAR_EL1), in a 2 MiB block for EL1 only.
const VBAR: u64 = 0x4000_0800;
/// The program, in another 2 MiB block: writable and executable at the
/// level it runs at (memory writable from EL0 is never executable at
/// EL1).
const CODE: u64 = 0x4060_0000;
const START: u64 = CODE + 0x1_0e00;
const PROG_LEN: usize = 256;
/// Data: 2 MiB mapped in pages with different permissions.
const DATA: u64 = 0x4020_0000;
const DATA_LEN: u64 = 0x20_0000;
/// Page tables (outside the virtual space: the program cannot
/// write them).
const TABLES: u64 = 0x4050_0000;
const STEP_LIMIT: u64 = 3000;

/// Handler for every exception: skips the instruction and returns.
const HANDLER: [u32; 4] = [
    0xd538403c, // mrs x28, ELR_EL1
    0x9100139c, // add x28, x28, #0x4
    0xd518403c, // msr ELR_EL1, x28
    0xd69f03e0, // eret
];

/// System instructions (registers Rt = x0, Rn = x1, Rs = w2, Rt2 = x3:
/// the generator changes them at random).
const SYSTEM: [u32; 50] = [
    0xd53be040, // mrs x0, CNTVCT_EL0 (ADR 0026)
    0xd53be020, // mrs x0, CNTPCT_EL0
    0xd53bd040, // mrs x0, TPIDR_EL0
    0xd51bd040, // msr TPIDR_EL0, x0
    0xd53bd060, // mrs x0, TPIDRRO_EL0
    0xd51bd060, // msr TPIDRRO_EL0, x0
    0xd538d080, // mrs x0, TPIDR_EL1
    0xd518d080, // msr TPIDR_EL1, x0
    0xd5384100, // mrs x0, SP_EL0
    0xd5184100, // msr SP_EL0, x0
    0xd5382040, // mrs x0, TCR_EL1
    0xd53b00e0, // mrs x0, DCZID_EL0
    0xd5384240, // mrs x0, CurrentEL
    0xd50b7420, // dc zva, x0
    0xc85f7c20, // ldxr x0, [x1]
    0x885ffc20, // ldaxr w0, [x1]
    0xc8027c20, // stxr w2, x0, [x1]
    0x8802fc20, // stlxr w2, w0, [x1]
    0xc87f0c20, // ldxp x0, x3, [x1]
    0xc8220c20, // stxp w2, x0, x3, [x1]
    0x887f0c20, // ldxp w0, w3, [x1]
    0x88220c20, // stxp w2, w0, w3, [x1]
    0x085f7c20, // ldxrb w0, [x1]
    0x48027c20, // stxrh w2, w0, [x1]
    0x9ac24c20, // crc32x w0, w1, x2
    0x1ac25020, // crc32cb w0, w1, w2
    0xf8400820, // ldtr x0, [x1]
    0xf8000820, // sttr x0, [x1]
    0xd4000001, // svc #0
    0xd508871f, // tlbi vmalle1
    0xd50342df, // msr DAIFSet, #0x2
    0xd50b7e20, // dc civac, x0
    // ADR 0024: DAIF, ELR/SPSR/ESR/FAR at EL1 in the blocks.
    0xd53b4220, // mrs x0, DAIF
    0xd51b4220, // msr DAIF, x0
    0xd50342ff, // msr DAIFClr, #0x2
    0xd5034fdf, // msr DAIFSet, #0xf
    0xd5384020, // mrs x0, ELR_EL1
    0xd5184020, // msr ELR_EL1, x0
    0xd5384000, // mrs x0, SPSR_EL1
    0xd5184000, // msr SPSR_EL1, x0
    0xd5385200, // mrs x0, ESR_EL1
    0xd5386000, // mrs x0, FAR_EL1
    // M4: FPCR/FPSR, TTBRs (read), CONTEXTIDR, MIDR in the regions.
    0xd53b4400, // mrs x0, FPCR
    0xd53b4420, // mrs x0, FPSR
    0xd51b4420, // msr FPSR, x0
    0xd5382020, // mrs x0, TTBR1_EL1
    0xd5382000, // mrs x0, TTBR0_EL1
    0xd538d020, // mrs x0, CONTEXTIDR_EL1
    0xd518d020, // msr CONTEXTIDR_EL1, x0
    0xd5380000, // mrs x0, MIDR_EL1
];

/// Exclusive pairs with the same address in x1 (so that the store can
/// succeed): load, then store.
const EXCLUSIVE_PAIRS: [(u32, u32); 3] = [
    (0xc85f7c20, 0xc8027c20), // ldxr x0, [x1] ; stxr w2, x0, [x1]
    (0xc87f0c20, 0xc8220c20), // ldxp x0, x3, [x1] ; stxp w2, x0, x3, [x1]
    (0x085f7c20, 0x48027c20), // ldxrb w0, [x1] ; stxrh w2, w0, [x1]
];

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

/// Random registers in an instruction from the table (x28 stays with the handler).
fn random_regs(rng: &mut Rng, w: u32) -> u32 {
    // SVC, TLBI, MSR DAIFSet: no register to change.
    if w >> 24 == 0xd4 || (w & 0x1f == 0x1f && w >> 22 == 0x354) {
        return w;
    }
    let mut r = || {
        let v = rng.below(31) as i64;
        if v == 28 { 1 } else { v }
    };
    let (rt, rn, rs, rt2) = (r(), r(), r(), r());
    let mut w = with_field(w, 0, 5, rt);
    if w >> 22 != 0x354 {
        // MRS/MSR have only Rt; the others also have Rn, Rs, Rt2.
        w = with_field(w, 5, 5, rn);
        if w & 0x3f00_0000 == 0x0800_0000 {
            w = with_field(with_field(w, 16, 5, rs), 10, 5, rt2);
        }
    }
    w
}

/// A random instruction for a system-mode block: random bits
/// filtered by the decoder, with branch offsets brought close.
fn random_insn(rng: &mut Rng, sys: SysTarget) -> u32 {
    loop {
        let mut w = rng.next() as u32;
        let insn = decode(w);
        match insn {
            Insn::Unimplemented(_) | Insn::Eret => continue,
            // A few UNDEFINED (the handler skips them) and some other
            // untranslated instructions, to alternate JIT and interpreter.
            Insn::Undefined if rng.below(64) != 0 => continue,
            _ if kind_in(&insn, Some(sys)) == Kind::Unsupported && rng.below(16) != 0 => continue,
            _ => {}
        }
        // x28 belongs to the handler.
        if w & 0x1f == 28 {
            continue;
        }
        let near = rng.below(40) as i64 - 20;
        w = match insn {
            Insn::B { .. } => with_field(w, 0, 26, near),
            Insn::BCond { .. } | Insn::Cbz { .. } | Insn::LdLiteral { .. } => with_field(w, 5, 19, near),
            Insn::Tbz { .. } => with_field(w, 5, 14, near),
            Insn::BranchReg { .. } if rng.below(4) != 0 => continue,
            _ => w,
        };
        return w;
    }
}

// VMSAv8-64 descriptors (4 KiB granule).
const VALID_TABLE: u64 = 0b11;
const VALID_BLOCK: u64 = 0b01;
const VALID_PAGE: u64 = 0b11;
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 0b11 << 8;
const AP_RW_ALL: u64 = 0b01 << 6;
const AP_RO_ALL: u64 = 0b11 << 6;
const AP_RW_EL1: u64 = 0b00 << 6;
const ATTR_NORMAL: u64 = 0 << 2;
const ATTR_DEVICE: u64 = 1 << 2;

/// The test physical memory: RAM at `RAM_BASE` behind a pointer (a
/// `Vec` for the interpreter, wasmtime's memory for the JIT), the rest
/// decode error. Watches the code pages like `vetro_machine::Board`.
struct TestPhys {
    ram: *mut u8,
    watched: Vec<bool>,
    dirty: Vec<u64>,
}

impl TestPhys {
    fn range(&self, pa: u64, len: usize) -> Option<usize> {
        let o = pa.checked_sub(RAM_BASE)?;
        (o + len as u64 <= RAM_LEN as u64).then_some(o as usize)
    }
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `ram` is valid for RAM_LEN bytes for the whole life of `self`, and
        // no WASM block runs while this slice exists.
        unsafe { std::slice::from_raw_parts_mut(self.ram, RAM_LEN) }
    }
}

impl PhysMemory for TestPhys {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.ram_read(pa, buf) { Ok(()) } else { Err(BusError::Decode) }
    }
    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        self.ram_write(pa, data).map(|_| ()).ok_or(BusError::Decode)
    }
}

impl SysPhys for TestPhys {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool {
        match self.range(pa, buf.len()) {
            Some(o) => {
                buf.copy_from_slice(&self.bytes()[o..o + buf.len()]);
                true
            }
            None => false,
        }
    }
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        let o = self.range(pa, data.len())?;
        self.bytes()[o..o + data.len()].copy_from_slice(data);
        let mut hit = false;
        for p in o >> 12..=(o + data.len().max(1) - 1) >> 12 {
            if self.watched[p] {
                self.watched[p] = false;
                self.dirty.push((RAM_BASE >> 12) + p as u64);
                hit = true;
            }
        }
        Some(hit)
    }
    fn watch_code(&mut self, page: u64) -> bool {
        match (page << 12).checked_sub(RAM_BASE).map(|o| (o >> 12) as usize) {
            Some(i) if i < self.watched.len() => {
                self.watched[i] = true;
                true
            }
            _ => false,
        }
    }
    fn is_watched(&self, page: u64) -> bool {
        (page << 12)
            .checked_sub(RAM_BASE)
            .is_some_and(|o| self.watched.get((o >> 12) as usize) == Some(&true))
    }
    fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        out.append(&mut self.dirty);
    }
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        Some((RAM_BASE, self.ram, RAM_LEN))
    }
}

/// CNTVOFF of the tests.
const CNTVOFF: u64 = 12345;

/// CNTPCT after `steps` instructions, like `vetro_machine` (62.5 MHz over 100).
fn counter(steps: u64) -> u64 {
    steps / 8 * 5 + steps % 8 * 5 / 8
}

/// No interrupts; CNTPCT and CNTVCT from the number of instructions already executed.
struct NoEnv {
    steps: u64,
}

impl CpuEnv for NoEnv {
    fn irq_line(&mut self) -> bool {
        false
    }
    fn read_sysreg(&mut self, r: EnvReg) -> u64 {
        match r {
            EnvReg::CntpctEl0 => counter(self.steps),
            EnvReg::CntvctEl0 => counter(self.steps).wrapping_sub(CNTVOFF),
            _ => 0,
        }
    }
    fn write_sysreg(&mut self, _: EnvReg, _: u64) {}
}

/// Initial RAM and CPU of a case.
fn setup(seed: u64) -> (Cpu, Vec<u8>) {
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5eed_5eed);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    // Recognisable data.
    for (i, b) in
        ram[(DATA - RAM_BASE) as usize..(DATA - RAM_BASE + DATA_LEN) as usize].iter_mut().enumerate()
    {
        *b = (i as u64).wrapping_mul(0x9E37_79B9) as u8 >> 1;
    }
    // Vectors: the same handler for synchronous and IRQ from every source.
    let handler: Vec<u8> = HANDLER.iter().flat_map(|w| w.to_le_bytes()).collect();
    for off in (0..0x800).step_by(0x80) {
        put(&mut ram, VBAR + off, &handler);
    }
    let el0 = rng.below(4) == 0;
    // Tables: L1[1] -> L2; L2[0] vector block (EL1); L2[1] -> L3
    // (data); L2[3] program block.
    let (l1, l2, l3) = (TABLES, TABLES + 0x1000, TABLES + 0x2000);
    put(&mut ram, l1 + 8, &(l2 | VALID_TABLE).to_le_bytes());
    let block = |pa: u64, ap: u64| pa | VALID_BLOCK | AF | SH_INNER | ap | ATTR_NORMAL;
    put(&mut ram, l2, &block(RAM_BASE, AP_RW_EL1).to_le_bytes());
    put(&mut ram, l2 + 8, &(l3 | VALID_TABLE).to_le_bytes());
    let code_ap = if el0 { AP_RW_ALL } else { AP_RW_EL1 };
    put(&mut ram, l2 + 8 * 3, &block(CODE, code_ap).to_le_bytes());
    for i in 0..512u64 {
        let pa = DATA + i * 0x1000;
        let attrs = match rng.below(100) {
            0..=4 => None,
            5..=8 => Some(AP_RO_ALL | ATTR_NORMAL),
            9..=11 => Some(AP_RW_EL1 | ATTR_NORMAL),
            12..=14 => Some(AP_RW_ALL | ATTR_DEVICE),
            _ => Some(AP_RW_ALL | ATTR_NORMAL),
        };
        if let Some(a) = attrs {
            put(&mut ram, l3 + 8 * i, &(pa | VALID_PAGE | AF | SH_INNER | a).to_le_bytes());
        }
    }
    let sys = SysTarget {
        el: if el0 { 0 } else { 1 },
        tbi0: rng.below(2) == 0,
        tbi1: false,
        spsel: !el0,
        fp: true,
        cntk: 0,
    };
    // Program: random instructions, system instructions, exclusive pairs.
    let mut prog = Vec::with_capacity(PROG_LEN);
    while prog.len() < PROG_LEN {
        match rng.below(10) {
            0 => {
                let w = SYSTEM[rng.below(SYSTEM.len() as u64) as usize];
                prog.push(random_regs(&mut rng, w));
            }
            1 if prog.len() + 3 < PROG_LEN => {
                let (ld, st) = EXCLUSIVE_PAIRS[rng.below(EXCLUSIVE_PAIRS.len() as u64) as usize];
                let rn = [1, 5, 9][rng.below(3) as usize];
                prog.push(with_field(ld, 5, 5, rn));
                prog.push(random_insn(&mut rng, sys));
                prog.push(with_field(st, 5, 5, rn));
            }
            _ => prog.push(random_insn(&mut rng, sys)),
        }
    }
    let code: Vec<u8> = prog.iter().flat_map(|w| w.to_le_bytes()).collect();
    put(&mut ram, START, &code);

    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    let s = &mut cpu.sys;
    s.vbar_el1 = VBAR;
    s.mair_el1 = 0x00ff; // attr0 Normal WB, attr1 Device-nGnRnE
    // T0SZ = 25 (39-bit VA, starting from L1), 4 KiB granule, 40-bit IPS,
    // no walk from TTBR1.
    s.tcr_el1 = 25 | 0b01 << 8 | 0b01 << 10 | 0b11 << 12 | 1 << 23 | 0b010 << 32;
    if sys.tbi0 {
        s.tcr_el1 |= 1 << 37;
    }
    s.ttbr0_el1 = l1;
    let mut sctlr_v = s.sctlr_el1 | sctlr::M | sctlr::DZE | sctlr::UCI;
    for bit in [sctlr::A, sctlr::SA, sctlr::SA0, sctlr::UMA] {
        match rng.below(3) {
            0 => sctlr_v |= bit,
            1 => sctlr_v &= !bit,
            _ => {}
        }
    }
    if rng.below(4) == 0 {
        sctlr_v &= !sctlr::DZE;
    }
    s.sctlr_el1 = sctlr_v;
    s.daif = 0;
    if rng.below(2) == 0 {
        s.cpacr_el1 = 0b11 << 20; // FP/SIMD without trap
    }
    s.tpidr_el1 = rng.next();
    // CNTKCTL_EL1.EL0PCTEN/EL0VCTEN: MRS of the counter at EL0 allowed or not.
    s.cntkctl_el1 = rng.below(4);
    cpu.tpidr_el0 = rng.next();
    cpu.tpidrro_el0 = rng.next();
    cpu.nzcv = (rng.below(16) as u32) << 28;
    let stack = DATA + 0x8_0000 + rng.below(0x1000) * 16 + if rng.below(8) == 0 { 8 } else { 0 };
    for x in cpu.x.iter_mut() {
        *x = match rng.below(8) {
            0..=2 => DATA + rng.below(DATA_LEN - 0x2000) + 0x1000,
            3 => START + 4 * rng.below(PROG_LEN as u64),
            4 => rng.below(64),
            5 => (rng.below(64) as i64 - 32) as u64,
            _ => rng.next(),
        };
    }
    cpu.pc = START;
    if el0 {
        cpu.sys.el = 0;
        cpu.sys.spsel = false;
        cpu.sp = stack;
        cpu.sys.sp_el[1] = DATA + 0x10_0000;
    } else {
        cpu.sys.spsel = rng.below(4) != 0;
        cpu.sp = stack;
        cpu.sys.sp_el[0] = DATA + 0x10_0000 + rng.below(0x100) * 8;
    }
    (cpu, ram)
}

/// Outcome: events (exceptions, HVC, ...) with the step at which they arrive, steps,
/// final CPU and RAM.
#[derive(Debug, PartialEq)]
struct Trace {
    events: Vec<(u64, SysEvent)>,
    steps: u64,
    cpu: Cpu,
    ram_hash: u64,
}

fn hash(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100_0000_01b3))
}

/// One interpreter step; `None` if execution stops.
fn step(
    cpu: &mut Cpu,
    mmu: &mut Mmu,
    phys: &mut TestPhys,
    events: &mut Vec<(u64, SysEvent)>,
    n: u64,
) -> bool {
    // `n` also counts this step: n - 1 had been done before.
    let ev = cpu.step_system(&mut MmuBus::new(mmu, phys), &mut NoEnv { steps: n - 1 });
    match ev {
        SysEvent::Executed | SysEvent::WaitForInterrupt => true,
        SysEvent::Unimplemented { .. } => {
            events.push((n, ev));
            false
        }
        _ => {
            events.push((n, ev));
            true
        }
    }
}

fn run_interp(cpu: Cpu, ram: Vec<u8>) -> Trace {
    run_interp_stops(cpu, ram, &[]).0
}

/// Like [`run_interp`]; also counts the steps done with the PC in `stops`.
fn run_interp_stops(mut cpu: Cpu, mut ram: Vec<u8>, stops: &[u64]) -> (Trace, u64) {
    let mut phys = TestPhys { ram: ram.as_mut_ptr(), watched: vec![false; RAM_LEN >> 12], dirty: Vec::new() };
    let mut mmu = Mmu::new(Mmu::PA_BITS_CORTEX_A53);
    let mut events = Vec::new();
    let mut n = 0;
    let mut hits = 0;
    while n < STEP_LIMIT {
        n += 1;
        hits += u64::from(stops.contains(&cpu.pc));
        if !step(&mut cpu, &mut mmu, &mut phys, &mut events, n) {
            break;
        }
    }
    (Trace { events, steps: n, cpu, ram_hash: hash(&ram) }, hits)
}

/// Offset of guest RAM in wasmtime's memory (after the
/// JIT area).
const RAM_IN_ENGINE: usize = 1 << 20;

fn run_jit(cpu: Cpu, ram: &[u8], seed: u64) -> (Trace, vetro_jit::SysJitStats) {
    let (t, s, _) = run_jit_stops(cpu, ram, seed, &[]);
    (t, s)
}

/// Like [`run_jit`] with `SysJit::set_stops(stops)`; also counts the
/// interpreter steps done with the PC in `stops`.
fn run_jit_stops(cpu: Cpu, ram: &[u8], seed: u64, stops: &[u64]) -> (Trace, vetro_jit::SysJitStats, u64) {
    run_jit_on(NativeEngine::new(), cpu, ram, seed, stops)
}

/// Like [`run_jit_stops`] on any engine.
fn run_jit_on<E: Engine>(
    mut engine: E,
    mut cpu: Cpu,
    ram: &[u8],
    seed: u64,
    stops: &[u64],
) -> (Trace, vetro_jit::SysJitStats, u64) {
    let mut rng = Rng(seed ^ 0xb0d9e7);
    vetro_jit::Engine::reserve(&mut engine, RAM_IN_ENGINE + RAM_LEN);
    let cfg = SysJitConfig {
        hot_threshold: rng.below(3) as u32,
        batch: 1 + rng.below(4) as usize,
        ..Default::default()
    };
    let mut jit = SysJit::new(engine, cfg);
    jit.set_stops(stops);
    let base = vetro_jit::Engine::memory(jit.engine())[RAM_IN_ENGINE..].as_mut_ptr();
    let mut phys = TestPhys { ram: base, watched: vec![false; RAM_LEN >> 12], dirty: Vec::new() };
    phys.bytes().copy_from_slice(ram);
    let mut mmu = Mmu::new(Mmu::PA_BITS_CORTEX_A53);
    let mut events = Vec::new();
    let mut n = 0;
    let mut hits = 0;
    let mut interp = Next::Jit;
    loop {
        if n >= STEP_LIMIT {
            break;
        }
        // Like `Machine::jit_budget`: no interrupts in this test.
        if interp == Next::Jit && !cpu.sys.il && cpu.pc & 3 == 0 {
            let budget = (STEP_LIMIT - n).min(1 + rng.below(300));
            // The clock (sometimes not: an MRS of the counter exits to the interpreter).
            if rng.below(8) != 0 {
                jit.set_time(vetro_jit::Clock { steps: n, cntvoff: CNTVOFF });
            }
            let r = jit.run(&mut cpu, &mut mmu, &mut phys, budget);
            n += r.steps;
            interp = if r.next == Next::Jit && r.steps == 0 { Next::One } else { r.next };
            if r.steps > 0 {
                continue;
            }
        }
        let old = cpu.pc;
        n += 1;
        hits += u64::from(stops.contains(&cpu.pc));
        let before = events.len();
        if !step(&mut cpu, &mut mmu, &mut phys, &mut events, n) {
            break;
        }
        interp = match interp {
            Next::Cold
                if events.len() == before && cpu.pc == old.wrapping_add(4) && cpu.pc >> 12 == old >> 12 =>
            {
                Next::Cold
            }
            _ => Next::Jit,
        };
    }
    let stats = jit.stats();
    assert_eq!(stats.resets, 0, "RAM lives in the engine memory: no resets");
    let h = hash(phys.bytes());
    (Trace { events, steps: n, cpu, ram_hash: h }, stats, hits)
}

/// JIT stop points (ADR 0027, introspection hook
/// points): with program addresses in `set_stops`, the
/// regions do not contain them. Execution stays identical to the interpreter,
/// and every step with the PC on a stop point is done by the interpreter (the
/// same number of times as in the fully interpreted run), so
/// the machine can check the hook points there. Without the stops
/// in the regions the count drops (tested by removing the check in
/// `install`).
#[test]
fn punti_di_fermata_restano_all_interprete() {
    let mut total_hits = 0;
    let mut jit_steps = 0;
    for seed in 0..150u64 {
        let (cpu, ram) = setup(seed);
        let mut rng = Rng(seed ^ 0x5709);
        let stops: Vec<u64> = (0..3).map(|_| START + 4 * rng.below(PROG_LEN as u64 / 2)).collect();
        let (want, want_hits) = run_interp_stops(cpu.clone(), ram.clone(), &stops);
        let (got, s, got_hits) = run_jit_stops(cpu, &ram, seed, &stops);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT with stops {stops:x?}");
        assert_eq!(got_hits, want_hits, "seed {seed}: the JIT executed a stop point {stops:x?}");
        total_hits += want_hits;
        jit_steps += s.jit_steps;
    }
    assert!(
        total_hits > 100 && jit_steps > 10_000,
        "test too weak: {total_hits} stops, {jit_steps} steps in the JIT"
    );
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn sistema_interprete_e_jit_identici() {
    let cases = env_u64("VETRO_JIT_SYS_PARITY_CASES", 400);
    let first = env_u64("VETRO_JIT_SYS_PARITY_SEED", 0);
    let mut total = vetro_jit::SysJitStats::default();
    let mut exceptions = 0;
    for seed in first..first + cases {
        let (cpu, ram) = setup(seed);
        let want = run_interp(cpu.clone(), ram.clone());
        if std::env::var_os("VETRO_JIT_SYS_PARITY_DEBUG").is_some() {
            eprintln!("seed {seed}: {:?}", &want.events[..want.events.len().min(8)]);
        }
        let (got, s) = run_jit(cpu, &ram, seed);
        if got != want {
            let first_diff = got.events.iter().zip(&want.events).position(|(a, b)| a != b);
            let around = |e: &[(u64, SysEvent)]| {
                let i = first_diff.unwrap_or(e.len()).saturating_sub(1);
                format!("{:?}", &e[i.min(e.len())..(i + 3).min(e.len())])
            };
            eprintln!("nearby events: interpreter {}\nJIT {}", around(&want.events), around(&got.events));
            panic!(
                "seed {seed}: interpreter and JIT differ (VETRO_JIT_SYS_PARITY_SEED={seed} \
                 VETRO_JIT_SYS_PARITY_CASES=1)\nfirst differing event: {first_diff:?}\n\
                 steps {} / {}, events {} / {}, RAM equal: {}\ninterpreter: {:?}\nJIT:         {:?}",
                want.steps,
                got.steps,
                want.events.len(),
                got.events.len(),
                want.ram_hash == got.ram_hash,
                want.cpu,
                got.cpu
            );
        }
        exceptions += want.events.len();
        total.jit_steps += s.jit_steps;
        total.blocks += s.blocks;
        total.faults += s.faults;
        total.stops += s.stops;
        total.invalidated_pages += s.invalidated_pages;
        total.resolves += s.resolves;
        total.runs += s.runs;
        total.calls += s.calls;
        total.modules += s.modules;
        total.reused += s.reused;
        total.svcs += s.svcs;
        total.epochs += s.epochs;
        total.tlb_flushes += s.tlb_flushes;
        total.tlb_fills += s.tlb_fills;
        total.yields += s.yields;
    }
    eprintln!("{cases} programs, {exceptions} exceptions; JIT: {total:?}");
    // The test is valid only if the JIT really did work, even in the
    // hard cases.
    assert!(total.jit_steps > cases * STEP_LIMIT / 10, "too few steps in the blocks: {total:?}");
    assert!(total.faults > 0 && total.stops > 0 && total.invalidated_pages > 0, "{total:?}");
    assert!(total.tlb_fills > 0 && total.svcs > 0 && total.resolves > 0, "{total:?}");
    // MSR DAIF/DAIFClr that unmask (ADR 0024).
    assert!(total.yields > 0, "{total:?}");
}

/// Unaligned-access TLB (ADR 0024): a Normal page filled by
/// a successful unaligned access must not let through an unaligned
/// access that spills into the next, unmapped page: there
/// the interpreter gives the fault. Also a Q aligned to 8 but not to 16 on
/// Device memory goes to the interpreter (alignment fault).
#[test]
fn tlb_non_allineata_non_sconfina() {
    // (current page, next page) looked up in the tables of a case.
    let l3 = TABLES + 0x2000;
    let pte = |ram: &[u8], i: u64| {
        let o = (l3 + 8 * i - RAM_BASE) as usize;
        u64::from_le_bytes(ram[o..o + 8].try_into().unwrap())
    };
    let mut checked = 0;
    for seed in 0..400 {
        let (mut cpu, mut ram) = setup(seed);
        if cpu.sys.el != 1 {
            continue;
        }
        // Normal RW followed by an unmapped page.
        let normal = |d: u64| d & 3 == VALID_PAGE && d & (1 << 2) == ATTR_NORMAL && d & (0b10 << 6) == 0;
        let Some(i) = (1..511).find(|&i| normal(pte(&ram, i)) && pte(&ram, i + 1) == 0) else { continue };
        let page = DATA + i * 0x1000;
        // ldr x0, [x1]; ldr x0, [x2]; b . (tools/a64asm.sh)
        let prog = [0xf9400020u32, 0xf9400040, 0x14000000];
        let o = (START - RAM_BASE) as usize;
        for (k, w) in prog.iter().enumerate() {
            ram[o + 4 * k..o + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
        cpu.sys.sctlr_el1 &= !sctlr::A;
        cpu.x[1] = page + 1;
        cpu.x[2] = page + 0xffd;
        let want = run_interp(cpu.clone(), ram.clone());
        let (got, _) = run_jit(cpu, &ram, seed);
        assert!(!want.events.is_empty(), "the straddling access must fault");
        assert_eq!(got, want, "seed {seed}");
        checked += 1;
        if checked == 8 {
            break;
        }
    }
    assert!(checked > 0, "no suitable case");
}

/// MSR DAIFClr that unmasks an interrupt (ADR 0024): the block exits with
/// YIELD right after the instruction, so the machine checks the
/// interrupts again at the right boundary (the same as the interpreter). DAIFSet, and
/// DAIFClr of a bit already zero, do not exit.
#[test]
fn daifclr_esce_dopo_l_istruzione() {
    let seed = (0..100).find(|&s| setup(s).0.sys.el == 1).expect("a case at EL1");
    let (mut cpu, mut ram) = setup(seed);
    let prog = [
        0xd5034fdfu32, // msr DAIFSet, #0xf
        0xd50342ff,    // msr DAIFClr, #0x2
        0x91000400,    // add x0, x0, #1
        0xd50342ff,    // msr DAIFClr, #0x2 (I already 0)
        0x91000400,    // add x0, x0, #1
        0x14000000,    // b .
    ];
    let o = (START - RAM_BASE) as usize;
    for (k, w) in prog.iter().enumerate() {
        ram[o + 4 * k..o + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
    }
    let mut engine = NativeEngine::new();
    vetro_jit::Engine::reserve(&mut engine, RAM_IN_ENGINE + RAM_LEN);
    let cfg = SysJitConfig { hot_threshold: 0, batch: 1, ..Default::default() };
    let mut jit = SysJit::new(engine, cfg);
    let base = vetro_jit::Engine::memory(jit.engine())[RAM_IN_ENGINE..].as_mut_ptr();
    let mut phys = TestPhys { ram: base, watched: vec![false; RAM_LEN >> 12], dirty: Vec::new() };
    phys.bytes().copy_from_slice(&ram);
    let mut mmu = Mmu::new(Mmu::PA_BITS_CORTEX_A53);
    let x0 = cpu.x[0];
    let r = jit.run(&mut cpu, &mut mmu, &mut phys, 100);
    assert_eq!((r.steps, r.next), (2, Next::Jit), "YIELD after DAIFClr");
    assert_eq!(cpu.pc, START + 8);
    assert_eq!(cpu.sys.daif, 0x340, "D, A, F masked, I not");
    assert_eq!(jit.stats().yields, 1);
    // The rest up to the limit: no other exit.
    let r = jit.run(&mut cpu, &mut mmu, &mut phys, 10);
    assert_eq!((r.steps, r.next), (10, Next::Jit));
    assert_eq!(cpu.x[0], x0.wrapping_add(2));
    assert_eq!(jit.stats().yields, 1);
}

/// Table bases per half of the address space (ADR 0036): kernel code in
/// TTBR1, and two TTBR0 tables (different ASIDs) that map the same user
/// addresses to different code and data. The kernel switches TTBR0 at
/// every SVC, like Linux's software PAN switches it at every entry and
/// exit. Jump cache entries and software TLB entries made under one table
/// must not be used under the other (the user region at `x24` is reached
/// by chaining from the dispatcher; the loads go through the TLB at EL0 and
/// at EL1), and the kernel's entries survive the switches. Fails if the
/// context ignores a table base (TTBR0 for EL0 or for EL1 code in TTBR0),
/// or if the TLB groups are not emptied on a base change (all tried).
#[test]
fn ttbr0_switches_keep_code_and_data_apart() {
    const K: u64 = 0xffff_ff80_0000_0000;
    const U: u64 = 0x20_0000;
    const NG: u64 = 1 << 11;
    let (l1a, l2a, l3a) = (TABLES, TABLES + 0x1000, TABLES + 0x2000);
    let (l1b, l2b, l3b) = (TABLES + 0x3000, TABLES + 0x4000, TABLES + 0x5000);
    let (l1k, l2k) = (TABLES + 0x6000, TABLES + 0x7000);
    let (ua, da, ub, db) = (DATA, DATA + 0x1000, DATA + 0x2000, DATA + 0x3000);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    let words = |ws: &[u32]| ws.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    let handler = words(&HANDLER);
    for off in (0..0x800).step_by(0x80) {
        put(&mut ram, VBAR + off, &handler);
    }
    // Synchronous exception from EL0 (the SVC): switch table, back to EL0.
    put(
        &mut ram,
        VBAR + 0x400,
        &words(&[
            0x10000059, // adr x25, 0x8
            0xd61f0340, // br x26 (EL1 code in TTBR0, different per table)
            0xf94002e4, // ldr x4, [x23]
            0x8b0400a5, // add x5, x5, x4
            0xd1000673, // sub x19, x19, #0x1
            0xb4000133, // cbz x19, 0x30
            0x36000073, // tbz w19, #0x0, 0x1c
            0xd5182015, // msr TTBR0_EL1, x21
            0x14000002, // b 0x20
            0xd5182014, // msr TTBR0_EL1, x20
            0xd5033fdf, // isb
            0xd5184036, // msr ELR_EL1, x22
            0xd518401f, // msr SPSR_EL1, xzr
            0xd69f03e0, // eret
            0x14000000, // b 0x30
        ]),
    );
    // Kernel entry: table A, to EL0.
    put(
        &mut ram,
        START,
        &words(&[
            0xd5182014, // msr TTBR0_EL1, x20
            0xd5033fdf, // isb
            0xd5184036, // msr ELR_EL1, x22
            0xd518401f, // msr SPSR_EL1, xzr
            0xd69f03e0, // eret
        ]),
    );
    // User code: the same first part, a second part (at U + 0x40) that
    // differs between the two tables.
    let first = words(&[
        0xf94002e1, // ldr x1, [x23]
        0xd61f0300, // br x24
    ]);
    put(&mut ram, ua, &first);
    put(&mut ram, ub, &first);
    put(
        &mut ram,
        ua + 0x40,
        &words(&[
            0x8b010042, // add x2, x2, x1
            0x91000442, // add x2, x2, #0x1
            0xf9000ae2, // str x2, [x23, #0x10]
            0xd4000001, // svc #0
        ]),
    );
    put(
        &mut ram,
        ub + 0x40,
        &words(&[
            0xca010042, // eor x2, x2, x1
            0x91000c42, // add x2, x2, #0x3
            0xf9000ee2, // str x2, [x23, #0x18]
            0xd4000001, // svc #0
        ]),
    );
    // EL1 code in TTBR0 (like the identity map): it must follow the table too.
    let (ka, kb) = (DATA + 0x4000, DATA + 0x5000);
    put(&mut ram, ka, &words(&[0x910004c6, 0xd61f0320])); // add x6, x6, #0x1; br x25
    put(&mut ram, kb, &words(&[0x910014c6, 0xd61f0320])); // add x6, x6, #0x5; br x25
    put(&mut ram, da, &0x1111u64.to_le_bytes());
    put(&mut ram, db, &0x2_2222_0000u64.to_le_bytes());
    let block = |pa: u64, ap: u64| pa | VALID_BLOCK | AF | SH_INNER | ap | ATTR_NORMAL;
    let page = |pa: u64| pa | VALID_PAGE | AF | SH_INNER | AP_RW_ALL | ATTR_NORMAL | NG;
    let kpage = |pa: u64| pa | VALID_PAGE | AF | SH_INNER | AP_RW_EL1 | ATTR_NORMAL | NG;
    for (l1, l2, l3, code, data, kcode) in [(l1a, l2a, l3a, ua, da, ka), (l1b, l2b, l3b, ub, db, kb)] {
        put(&mut ram, l3 + 16, &kpage(kcode).to_le_bytes());
        put(&mut ram, l1, &(l2 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l2 + 8, &(l3 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l3, &page(code).to_le_bytes());
        put(&mut ram, l3 + 8, &page(data).to_le_bytes());
    }
    put(&mut ram, l1k, &(l2k | VALID_TABLE).to_le_bytes());
    put(&mut ram, l2k, &block(RAM_BASE, AP_RW_EL1).to_le_bytes());
    put(&mut ram, l2k + 8 * 3, &block(CODE, AP_RW_EL1).to_le_bytes());

    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    let s = &mut cpu.sys;
    s.vbar_el1 = K + (VBAR - RAM_BASE);
    s.mair_el1 = 0x00ff;
    // T0SZ = T1SZ = 25, 4 KiB granules, 40-bit IPS, ASID from TTBR0 (A1 = 0).
    s.tcr_el1 = 25
        | 0b01 << 8
        | 0b01 << 10
        | 0b11 << 12
        | 25 << 16
        | 0b01 << 24
        | 0b01 << 26
        | 0b11 << 28
        | 0b10 << 30
        | 0b010 << 32;
    s.ttbr0_el1 = l1a | 1 << 48;
    s.ttbr1_el1 = l1k;
    s.sctlr_el1 |= sctlr::M;
    s.daif = 0;
    s.cpacr_el1 = 0b11 << 20;
    cpu.sys.el = 1;
    cpu.sys.spsel = true;
    cpu.pc = K + (START - RAM_BASE);
    cpu.x[19] = 1000;
    cpu.x[20] = l1a | 1 << 48;
    cpu.x[21] = l1b | 2 << 48;
    cpu.x[22] = U;
    cpu.x[23] = U + 0x1000;
    cpu.x[24] = U + 0x40;
    cpu.x[26] = U + 0x2000;

    let want = run_interp(cpu.clone(), ram.clone());
    assert!(want.events.len() > 100, "too few SVCs: {:?}", &want.events[..want.events.len().min(4)]);
    for seed in 0..24 {
        let (got, s) = run_jit(cpu.clone(), &ram, seed);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT differ ({s:?})");
        assert!(
            s.jit_steps > 500 && s.base_switches > 50 && s.resolves > 0,
            "seed {seed}: test too weak: {s:?}"
        );
    }
}

/// TLBI by VA (ADR 0036): the JIT forgets only the software TLB and jump
/// cache entries within the 1 GiB around the address, not everything. Here
/// the kernel swaps a 2 MiB block descriptor between two physical blocks
/// (different data and code) and invalidates only the block's first page:
/// an entry covers the whole block, so the other pages of the block (the
/// data at +0x5000, the code at +0x6000 reached by chaining) must follow.
/// Fails if only the invalidated page is forgotten, or nothing (both tried).
#[test]
fn tlbi_by_va_forgets_the_whole_block() {
    const K: u64 = 0xffff_ff80_0000_0000;
    const D: u64 = 0x4000_0000;
    let (l1, l2d, l1k, l2k) = (TABLES, TABLES + 0x1000, TABLES + 0x2000, TABLES + 0x3000);
    let (x1, x2) = (DATA, DATA + 0x20_0000);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    let words = |ws: &[u32]| ws.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    let handler = words(&HANDLER);
    for off in (0..0x800).step_by(0x80) {
        put(&mut ram, VBAR + off, &handler);
    }
    put(
        &mut ram,
        START,
        &words(&[
            0xf94002e1, // ldr x1, [x23]
            0x8b010042, // add x2, x2, x1
            0x10000059, // adr x25, 0x10
            0xd61f0300, // br x24
            0x36000073, // tbz w19, #0x0, 0x1c
            0xf9000354, // str x20, [x26]
            0x14000002, // b 0x20
            0xf9000355, // str x21, [x26]
            0xd5033b9f, // dsb ish
            0xd5088736, // tlbi vae1, x22
            0xd5033b9f, // dsb ish
            0xd5033fdf, // isb
            0xd1000673, // sub x19, x19, #0x1
            0xb5fffe73, // cbnz x19, 0x0
            0x14000000, // b .
        ]),
    );
    put(&mut ram, x1 + 0x5000, &0x1111u64.to_le_bytes());
    put(&mut ram, x2 + 0x5000, &0x2_2222_0000u64.to_le_bytes());
    put(&mut ram, x1 + 0x6000, &words(&[0x91000463, 0xd61f0320])); // add x3, x3, #0x1; br x25
    put(&mut ram, x2 + 0x6000, &words(&[0x91001c63, 0xd61f0320])); // add x3, x3, #0x7; br x25
    let block = |pa: u64| pa | VALID_BLOCK | AF | SH_INNER | AP_RW_EL1 | ATTR_NORMAL;
    put(&mut ram, l1 + 8, &(l2d | VALID_TABLE).to_le_bytes());
    put(&mut ram, l2d, &block(x1).to_le_bytes());
    put(&mut ram, l1k, &(l2k | VALID_TABLE).to_le_bytes());
    put(&mut ram, l2k, &block(RAM_BASE).to_le_bytes());
    put(&mut ram, l2k + 8 * 2, &block(RAM_BASE + 0x40_0000).to_le_bytes());
    put(&mut ram, l2k + 8 * 3, &block(CODE).to_le_bytes());

    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    let s = &mut cpu.sys;
    s.vbar_el1 = K + (VBAR - RAM_BASE);
    s.mair_el1 = 0x00ff;
    s.tcr_el1 = 25
        | 0b01 << 8
        | 0b01 << 10
        | 0b11 << 12
        | 25 << 16
        | 0b01 << 24
        | 0b01 << 26
        | 0b11 << 28
        | 0b10 << 30
        | 0b010 << 32;
    s.ttbr0_el1 = l1;
    s.ttbr1_el1 = l1k;
    s.sctlr_el1 |= sctlr::M;
    s.daif = 0;
    cpu.sys.el = 1;
    cpu.sys.spsel = true;
    cpu.pc = K + (START - RAM_BASE);
    cpu.x[19] = 1000;
    cpu.x[20] = block(x1);
    cpu.x[21] = block(x2);
    cpu.x[22] = D >> 12;
    cpu.x[23] = D + 0x5000;
    cpu.x[24] = D + 0x6000;
    cpu.x[26] = K + (l2d - RAM_BASE);

    let want = run_interp(cpu.clone(), ram.clone());
    assert!(want.events.is_empty(), "no exception expected: {:?}", &want.events[..want.events.len().min(4)]);
    assert!(want.cpu.x[19] < 900, "too few iterations: {}", want.cpu.x[19]);
    for seed in 0..24 {
        let (got, s) = run_jit(cpu.clone(), &ram, seed);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT differ ({s:?})");
        assert!(s.jit_steps > 500 && s.tlbi_partial > 50, "seed {seed}: test too weak: {s:?}");
    }
}

/// LDTR/STTR at EL1 in regions (M4): through the host with the permissions
/// of EL0, never through the software TLB. On a page EL0 may use they
/// succeed; on an EL1-only page they fault (a permission fault the
/// interpreter takes), a store to a read-only page too. Same exceptions,
/// state and RAM as the interpreter, with the instructions really run in
/// regions. Fails if the host checks them with the permissions of EL1
/// (tried).
#[test]
fn ldtr_sttr_at_el1_use_el0_permissions() {
    let l3 = TABLES + 0x2000;
    let pte = |ram: &[u8], i: u64| {
        let o = (l3 + 8 * i - RAM_BASE) as usize;
        u64::from_le_bytes(ram[o..o + 8].try_into().unwrap())
    };
    let kind = |d: u64, ap: u64| d & 3 == VALID_PAGE && d & (1 << 2) == ATTR_NORMAL && d & (0b11 << 6) == ap;
    let mut checked = 0;
    for seed in 0..400 {
        let (mut cpu, mut ram) = setup(seed);
        if cpu.sys.el != 1 {
            continue;
        }
        let find = |ap: u64| (0..512).find(|&i| kind(pte(&ram, i), ap)).map(|i| DATA + i * 0x1000);
        let (Some(all), Some(el1), Some(ro)) = (find(AP_RW_ALL), find(AP_RW_EL1), find(AP_RO_ALL)) else {
            continue;
        };
        let prog = [
            0xf8400820u32, // ldtr x0, [x1]
            0xf8400862,    // ldtr x2, [x3]
            0xf80008a4,    // sttr x4, [x5]
            0xf8000864,    // sttr x4, [x3]
            0x38403826,    // ldtrb w6, [x1, #0x3]
            0x91000484,    // add x4, x4, #0x1
            0x17fffffa,    // b 0x0
        ];
        let o = (START - RAM_BASE) as usize;
        for (k, w) in prog.iter().enumerate() {
            ram[o + 4 * k..o + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
        cpu.sys.sctlr_el1 &= !(sctlr::A | sctlr::SA);
        cpu.x[1] = all + 0x40;
        // EL1-only, then read-only for the store to x5.
        cpu.x[3] = el1 + 0x80;
        cpu.x[5] = if seed % 2 == 0 { ro + 0x10 } else { all + 0x10 };
        let want = run_interp(cpu.clone(), ram.clone());
        assert!(want.events.len() > 100, "seed {seed}: the EL1-only accesses must fault");
        let (got, s) = run_jit(cpu, &ram, seed);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT differ ({s:?})");
        assert!(s.jit_steps > 500 && s.faults > 50, "seed {seed}: not run in regions: {s:?}");
        checked += 1;
        if checked == 8 {
            break;
        }
    }
    assert!(checked >= 4, "only {checked} suitable cases");
}

/// MSR TTBR0_EL1 inside a region (M4): the region exits right after it
/// (YIELD, `exit_detail` 3) and the host goes on in the new regime within the
/// same run (ADR 0040; before, the run ended). Here EL1 code in TTBR1 loads
/// the same TTBR0 address under two tables in one loop: a software TLB entry
/// filled under the first table must not serve the load after the switch.
/// Fails if the MSR does not end the region, or if the host does not resync
/// the regime (contexts, TLB groups, translation registers) before going on
/// (all tried).
#[test]
fn msr_ttbr0_in_a_region_switches_the_regime() {
    const K: u64 = 0xffff_ff80_0000_0000;
    const U: u64 = 0x20_0000;
    let (l1a, l2a, l3a) = (TABLES, TABLES + 0x1000, TABLES + 0x2000);
    let (l1b, l2b, l3b) = (TABLES + 0x3000, TABLES + 0x4000, TABLES + 0x5000);
    let (l1k, l2k) = (TABLES + 0x6000, TABLES + 0x7000);
    let (da, db) = (DATA, DATA + 0x1000);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    let words = |ws: &[u32]| ws.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    let handler = words(&HANDLER);
    for off in (0..0x800).step_by(0x80) {
        put(&mut ram, VBAR + off, &handler);
    }
    put(
        &mut ram,
        START,
        &words(&[
            0xf94002e1, // ldr x1, [x23]
            0x8b0100a5, // add x5, x5, x1
            0xd5182015, // msr TTBR0_EL1, x21
            0xf94002e2, // ldr x2, [x23]
            0x8b0200c6, // add x6, x6, x2
            0xd5182014, // msr TTBR0_EL1, x20
            0xd1000673, // sub x19, x19, #0x1
            0xb5ffff33, // cbnz x19, 0x0
            0x14000000, // b .
        ]),
    );
    put(&mut ram, da, &0x1111u64.to_le_bytes());
    put(&mut ram, db, &0x2_2222_0000u64.to_le_bytes());
    let block = |pa: u64| pa | VALID_BLOCK | AF | SH_INNER | AP_RW_EL1 | ATTR_NORMAL;
    let page = |pa: u64| pa | VALID_PAGE | AF | SH_INNER | AP_RW_EL1 | ATTR_NORMAL | (1 << 11);
    for (l1, l2, l3, data) in [(l1a, l2a, l3a, da), (l1b, l2b, l3b, db)] {
        put(&mut ram, l1, &(l2 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l2 + 8, &(l3 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l3, &page(data).to_le_bytes());
    }
    put(&mut ram, l1k, &(l2k | VALID_TABLE).to_le_bytes());
    put(&mut ram, l2k, &block(RAM_BASE).to_le_bytes());
    put(&mut ram, l2k + 8 * 3, &block(CODE).to_le_bytes());

    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    let s = &mut cpu.sys;
    s.vbar_el1 = K + (VBAR - RAM_BASE);
    s.mair_el1 = 0x00ff;
    s.tcr_el1 = 25
        | 0b01 << 8
        | 0b01 << 10
        | 0b11 << 12
        | 25 << 16
        | 0b01 << 24
        | 0b01 << 26
        | 0b11 << 28
        | 0b10 << 30
        | 0b010 << 32;
    s.ttbr0_el1 = l1a | 1 << 48;
    s.ttbr1_el1 = l1k;
    s.sctlr_el1 |= sctlr::M;
    s.daif = 0;
    cpu.sys.el = 1;
    cpu.sys.spsel = true;
    cpu.pc = K + (START - RAM_BASE);
    cpu.x[19] = 300;
    cpu.x[20] = l1a | 1 << 48;
    cpu.x[21] = l1b | 2 << 48;
    cpu.x[23] = U;

    let want = run_interp(cpu.clone(), ram.clone());
    assert!(want.events.is_empty() && want.cpu.x[19] == 0, "the loop must finish: {}", want.cpu.x[19]);
    assert_eq!(want.cpu.x[6], 300 * 0x2_2222_0000, "the second load reads the second table");
    for seed in 0..24 {
        let (got, s) = run_jit(cpu.clone(), &ram, seed);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT differ ({s:?})");
        assert!(s.jit_steps > 500 && s.yields > 50, "seed {seed}: not run in regions: {s:?}");
        assert!(s.regime_switches > 50, "seed {seed}: the run ended at every MSR TTBR0: {s:?}");
    }
}

/// LDTR/STTR at EL1 through the EL0 software TLB (ADR 0040), as in Linux's
/// uaccess with software PAN: EL0 code loads and stores a user page (filling
/// the EL0 tables under table A), then an SVC; the EL1 handler reads and
/// writes the same page with LDTR/STTR (hits in the EL0 tables), loads an
/// EL1-only page with LDR (an EL1 entry) and then with LDTR (must fault: EL0
/// permissions), switches TTBR0 to table B, where the same VA maps another
/// page (the EL0 entries filled under A must not serve it), and back. Same
/// trace as the interpreter, and the user page accesses mostly without the
/// host. Fails if the EL0 tables are used under another base, or if LDTR
/// takes an EL1 entry (both tried).
#[test]
fn ldtr_sttr_at_el1_use_the_el0_tlb_of_the_same_tables() {
    const K: u64 = 0xffff_ff80_0000_0000;
    const U: u64 = 0x20_0000;
    const UCODE: u64 = 0x40_0000 + 0x8000;
    let (l1a, l2a, l3a) = (TABLES, TABLES + 0x1000, TABLES + 0x2000);
    let (l1b, l2b, l3b) = (TABLES + 0x3000, TABLES + 0x4000, TABLES + 0x5000);
    let (l1k, l2k) = (TABLES + 0x6000, TABLES + 0x7000);
    let (da, db, dk) = (DATA, DATA + 0x1000, DATA + 0x2000);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    let words = |ws: &[u32]| ws.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    // Current EL, SPx, synchronous: skip the instruction (the faulting LDTR).
    put(&mut ram, VBAR + 0x200, &words(&HANDLER));
    // Lower EL, AArch64, synchronous: the SVC handler.
    put(
        &mut ram,
        VBAR + 0x400,
        &words(&[
            0xd5384039, // mrs x25, ELR_EL1
            0xd538401a, // mrs x26, SPSR_EL1
            0xf8400ae2, // ldtr x2, [x23]
            0x8b0200c6, // add x6, x6, x2
            0xf8010ae5, // sttr x5, [x23, #0x10]
            0xf9400309, // ldr x9, [x24]
            0xf8400b07, // ldtr x7, [x24]
            0x8b070108, // add x8, x8, x7
            0x38403aeb, // ldtrb w11, [x23, #0x3]
            0x8b0b018c, // add x12, x12, x11
            0xd5182015, // msr TTBR0_EL1, x21
            0xf8400ae3, // ldtr x3, [x23]
            0x8b03014a, // add x10, x10, x3
            0xf8018ae5, // sttr x5, [x23, #0x18]
            0xd5182014, // msr TTBR0_EL1, x20
            0xd5184039, // msr ELR_EL1, x25
            0xd518401a, // msr SPSR_EL1, x26
            0xd69f03e0, // eret
        ]),
    );
    put(
        &mut ram,
        CODE + (UCODE - 0x40_0000),
        &words(&[
            0xf94002e1, // ldr x1, [x23]
            0xf90006e5, // str x5, [x23, #0x8]
            0x910004a5, // add x5, x5, #0x1
            0xd4000001, // svc #0
            0xd1000673, // sub x19, x19, #0x1
            0xb5ffff73, // cbnz x19, 0x0
            0x14000000, // b .
        ]),
    );
    put(&mut ram, da, &0x1111u64.to_le_bytes());
    put(&mut ram, db, &0x2_2222_0000u64.to_le_bytes());
    put(&mut ram, dk, &0x3_0000_0003u64.to_le_bytes());
    let block = |pa: u64, ap: u64| pa | VALID_BLOCK | AF | SH_INNER | ap | ATTR_NORMAL;
    let page = |pa: u64, ap: u64| pa | VALID_PAGE | AF | SH_INNER | ap | ATTR_NORMAL | (1 << 11);
    for (l1, l2, l3, data) in [(l1a, l2a, l3a, da), (l1b, l2b, l3b, db)] {
        put(&mut ram, l1, &(l2 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l2 + 8, &(l3 | VALID_TABLE).to_le_bytes());
        put(&mut ram, l2 + 16, &(block(CODE, AP_RW_ALL) | 1 << 11).to_le_bytes());
        put(&mut ram, l3, &page(data, AP_RW_ALL).to_le_bytes());
        put(&mut ram, l3 + 8, &page(dk, AP_RW_EL1).to_le_bytes());
    }
    put(&mut ram, l1k, &(l2k | VALID_TABLE).to_le_bytes());
    put(&mut ram, l2k, &block(RAM_BASE, AP_RW_EL1).to_le_bytes());

    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    let s = &mut cpu.sys;
    s.vbar_el1 = K + (VBAR - RAM_BASE);
    s.mair_el1 = 0x00ff;
    s.tcr_el1 = 25
        | 0b01 << 8
        | 0b01 << 10
        | 0b11 << 12
        | 25 << 16
        | 0b01 << 24
        | 0b01 << 26
        | 0b11 << 28
        | 0b10 << 30
        | 0b010 << 32;
    s.ttbr0_el1 = l1a | 1 << 48;
    s.ttbr1_el1 = l1k;
    s.sctlr_el1 |= sctlr::M;
    s.daif = 0;
    cpu.sys.el = 0;
    cpu.sys.spsel = false;
    cpu.pc = UCODE;
    cpu.x[19] = 1000;
    cpu.x[20] = l1a | 1 << 48;
    cpu.x[21] = l1b | 2 << 48;
    cpu.x[23] = U;
    cpu.x[24] = U + 0x1000;

    let want = run_interp(cpu.clone(), ram.clone());
    let rounds = 1000 - want.cpu.x[19];
    assert!(rounds > 50, "only {rounds} rounds");
    assert_eq!(want.cpu.x[6], rounds * 0x1111, "LDTR reads the user page under table A");
    assert_eq!(want.cpu.x[10], rounds * 0x2_2222_0000, "under table B the other page");
    assert_eq!(want.cpu.x[8], 0, "LDTR of the EL1-only page faults");
    for seed in 0..24 {
        let (got, s) = run_jit(cpu.clone(), &ram, seed);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT differ ({s:?})");
        assert!(s.jit_steps > 1000, "seed {seed}: not run in regions: {s:?}");
        // The runs after each MSR TTBR0 find their regions again without the
        // MMU (remembered fetch translations, ADR 0040).
        assert!(s.memo_hits > rounds, "seed {seed}: fetch translations not remembered: {s:?}");
        // Per round: the faulting LDTR and the LDTR/STTR under table B go to
        // the host; those under table A are TLB hits.
        assert!(
            s.host_lds + s.host_sts < 5 * rounds,
            "seed {seed}: LDTR/STTR through the host: {} + {} for {rounds} rounds",
            s.host_lds,
            s.host_sts
        );
    }
}

/// An engine that compiles "in the background" (ADR 0038): every region
/// module becomes ready only after a random number of `ready` polls, and
/// its regions reach the block table only then (as in the browser, where
/// the Worker's module is placed when it arrives). A region called before
/// that would hit an empty table entry and trap.
struct Delayed {
    inner: NativeEngine,
    rng: Rng,
    /// Module id -> (polls left, placement).
    waiting: std::collections::HashMap<u64, (u64, u32, u32)>,
    next: u64,
    not_ready: std::rc::Rc<std::cell::Cell<u64>>,
}

struct DelayedModule {
    id: u64,
    m: <NativeEngine as Engine>::Module,
}

impl Engine for Delayed {
    type Module = DelayedModule;
    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String> {
        self.inner.runtime(wasm)
    }
    fn compile(&mut self, wasm: &[u8]) -> Result<DelayedModule, String> {
        self.next += 1;
        Ok(DelayedModule { id: self.next, m: self.inner.compile(wasm)? })
    }
    fn ready(&mut self, m: &DelayedModule) -> bool {
        let Some(w) = self.waiting.get_mut(&m.id) else { return true };
        if w.0 > 0 {
            w.0 -= 1;
            self.not_ready.set(self.not_ready.get() + 1);
            return false;
        }
        let (_, count, base) = self.waiting.remove(&m.id).expect("waiting");
        self.inner.place(&m.m, count, base);
        true
    }
    fn run(&mut self, m: &DelayedModule, index: u32, state: u32, host: &mut dyn Host) -> u32 {
        self.inner.run(&m.m, index, state, host)
    }
    fn memory(&mut self) -> &mut [u8] {
        self.inner.memory()
    }
    fn place(&mut self, m: &DelayedModule, count: u32, base: u32) {
        let polls = self.rng.below(4);
        if polls == 0 {
            self.inner.place(&m.m, count, base);
        } else {
            self.waiting.insert(m.id, (polls, count, base));
        }
    }
    fn reset(&mut self) {
        self.waiting.clear();
        self.inner.reset();
    }
    fn reserve(&mut self, bytes: usize) {
        self.inner.reserve(bytes);
    }
    fn host_address(&mut self, p: *const u8, len: usize) -> Option<u32> {
        self.inner.host_address(p, len)
    }
}

/// Background compilation (ADR 0038): with modules that become ready late,
/// execution stays identical to the interpreter (the regions of a module
/// still compiling run in the interpreter). Red if `Cache::lookup` or
/// `SysJit::run` hand out a region that is not ready (its table entry is
/// empty: wasmtime traps).
#[test]
fn background_compilation_keeps_the_execution() {
    let not_ready = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut jit_steps = 0;
    for seed in 0..200u64 {
        let (cpu, ram) = setup(seed);
        let want = run_interp(cpu.clone(), ram.clone());
        let engine = Delayed {
            inner: NativeEngine::new(),
            rng: Rng(seed ^ 0xa51c),
            waiting: Default::default(),
            next: 0,
            not_ready: not_ready.clone(),
        };
        let (got, s, _) = run_jit_on(engine, cpu, &ram, seed, &[]);
        assert_eq!(got, want, "seed {seed}: interpreter and JIT with background compilation");
        jit_steps += s.jit_steps;
    }
    assert!(
        not_ready.get() > 100 && jit_steps > 10_000,
        "test too weak: {} not-ready polls, {jit_steps} steps in the JIT",
        not_ready.get()
    );
}
