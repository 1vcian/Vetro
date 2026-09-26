//! Parità interprete-JIT in modalità sistema (ADR 0013): programmi
//! bare-metal casuali con la MMU accesa, eseguiti da `Cpu::step_system` e
//! dal ciclo della macchina con `SysJit` (blocchi tradotti fra un passo
//! dell'interprete e l'altro, come `Machine::run`), dallo stesso stato.
//! Si confrontano le eccezioni (e a che passo arrivano), lo stato finale
//! della CPU (registri di sistema e monitor esclusivo compresi) e tutta la
//! RAM.
//!
//! Nel ciclo col JIT la RAM sta dentro la memoria di wasmtime, così i
//! blocchi la raggiungono con la TLB software (il percorso veloce di
//! `ld`/`st`, lo stesso del browser).
//!
//! I programmi mescolano istruzioni casuali (filtrate dal decoder) e
//! istruzioni di sistema prese da una tabella (MRS/MSR, esclusive, DC ZVA,
//! CRC32, LDTR/STTR, SVC, TLBI...), con registri casuali; le pagine dei dati
//! hanno permessi e attributi diversi (non mappate, sola lettura, solo EL1,
//! Device), SCTLR_EL1.A/SA/SA0/DZE, TBI0 e SPSel sono casuali, e una parte
//! dei programmi parte a EL0. Ogni eccezione va a un gestore che salta
//! l'istruzione (ELR += 4) e torna con ERET.
//!
//! `VETRO_JIT_SYS_PARITY_CASES` (default 400) e `VETRO_JIT_SYS_PARITY_SEED`.

use vetro_cpu::sys::{CpuEnv, SysEvent, sctlr};
use vetro_cpu::sysreg::EnvReg;
use vetro_cpu::{Cpu, Insn, SysConfig, decode};
use vetro_jit::translate::{Kind, SysTarget, kind_in};
use vetro_jit::{Next, SysJit, SysJitConfig, SysPhys};
use vetro_jit_native::NativeEngine;
use vetro_mmu::{BusError, Mmu, MmuBus, PhysMemory};

const RAM_BASE: u64 = 0x4000_0000;
const RAM_LEN: usize = 8 << 20;
/// Vettori delle eccezioni (VBAR_EL1), in un blocco di 2 MiB solo per EL1.
const VBAR: u64 = 0x4000_0800;
/// Il programma, in un altro blocco di 2 MiB: scrivibile ed eseguibile dal
/// livello a cui gira (la memoria scrivibile da EL0 non è mai eseguibile a
/// EL1).
const CODE: u64 = 0x4060_0000;
const START: u64 = CODE + 0x1_0e00;
const PROG_LEN: usize = 256;
/// Dati: 2 MiB mappati a pagine con permessi diversi.
const DATA: u64 = 0x4020_0000;
const DATA_LEN: u64 = 0x20_0000;
/// Tabelle delle pagine (fuori dallo spazio virtuale: il programma non le
/// può scrivere).
const TABLES: u64 = 0x4050_0000;
const STEP_LIMIT: u64 = 3000;

/// Gestore di ogni eccezione: salta l'istruzione e torna.
const HANDLER: [u32; 4] = [
    0xd538403c, // mrs x28, ELR_EL1
    0x9100139c, // add x28, x28, #0x4
    0xd518403c, // msr ELR_EL1, x28
    0xd69f03e0, // eret
];

/// Istruzioni di sistema (registri Rt = x0, Rn = x1, Rs = w2, Rt2 = x3:
/// il generatore li cambia a caso).
const SYSTEM: [u32; 42] = [
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
    // ADR 0024: DAIF, ELR/SPSR/ESR/FAR a EL1 nei blocchi.
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
];

/// Coppie esclusive con lo stesso indirizzo in x1 (perché lo store possa
/// riuscire): load, poi store.
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

/// Sostituisce il campo `[lo, lo+width)` di `w` con `v`.
fn with_field(w: u32, lo: u32, width: u32, v: i64) -> u32 {
    let mask = ((1u64 << width) - 1) as u32;
    (w & !(mask << lo)) | (((v as u32) & mask) << lo)
}

/// Registri a caso in una istruzione della tabella (x28 resta al gestore).
fn random_regs(rng: &mut Rng, w: u32) -> u32 {
    // SVC, TLBI, MSR DAIFSet: nessun registro da cambiare.
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
        // MRS/MSR hanno solo Rt; le altre hanno anche Rn, Rs, Rt2.
        w = with_field(w, 5, 5, rn);
        if w & 0x3f00_0000 == 0x0800_0000 {
            w = with_field(with_field(w, 16, 5, rs), 10, 5, rt2);
        }
    }
    w
}

/// Un'istruzione casuale per un blocco della modalità sistema: bit casuali
/// filtrati dal decoder, con gli offset dei salti riportati vicino.
fn random_insn(rng: &mut Rng, sys: SysTarget) -> u32 {
    loop {
        let mut w = rng.next() as u32;
        let insn = decode(w);
        match insn {
            Insn::Unimplemented(_) | Insn::Eret => continue,
            // Poche UNDEFINED (il gestore le salta) e qualche altra
            // istruzione non tradotta, per alternare JIT e interprete.
            Insn::Undefined if rng.below(64) != 0 => continue,
            _ if kind_in(&insn, Some(sys)) == Kind::Unsupported && rng.below(16) != 0 => continue,
            _ => {}
        }
        // x28 è del gestore.
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

// Descrittori VMSAv8-64 (granulo 4 KiB).
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

/// La memoria fisica di prova: RAM a `RAM_BASE` dietro un puntatore (un
/// `Vec` per l'interprete, la memoria di wasmtime per il JIT), il resto
/// decode error. Sorveglia le pagine di codice come `vetro_machine::Board`.
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
        // SAFETY: `ram` vale per RAM_LEN byte per tutta la vita di `self`, e
        // nessun blocco WASM gira mentre questa fetta esiste.
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

/// CNTVOFF delle prove.
const CNTVOFF: u64 = 12345;

/// CNTPCT dopo `steps` istruzioni, come `vetro_machine` (62,5 MHz su 100).
fn counter(steps: u64) -> u64 {
    steps / 8 * 5 + steps % 8 * 5 / 8
}

/// Nessun interrupt; CNTPCT e CNTVCT dal numero di istruzioni già fatte.
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

/// RAM iniziale e CPU di un caso.
fn setup(seed: u64) -> (Cpu, Vec<u8>) {
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5eed_5eed);
    let mut ram = vec![0u8; RAM_LEN];
    let put = |ram: &mut [u8], pa: u64, bytes: &[u8]| {
        let o = (pa - RAM_BASE) as usize;
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    };
    // Dati riconoscibili.
    for (i, b) in
        ram[(DATA - RAM_BASE) as usize..(DATA - RAM_BASE + DATA_LEN) as usize].iter_mut().enumerate()
    {
        *b = (i as u64).wrapping_mul(0x9E37_79B9) as u8 >> 1;
    }
    // Vettori: lo stesso gestore per sincrone e IRQ di ogni provenienza.
    let handler: Vec<u8> = HANDLER.iter().flat_map(|w| w.to_le_bytes()).collect();
    for off in (0..0x800).step_by(0x80) {
        put(&mut ram, VBAR + off, &handler);
    }
    let el0 = rng.below(4) == 0;
    // Tabelle: L1[1] -> L2; L2[0] blocco dei vettori (EL1); L2[1] -> L3
    // (dati); L2[3] blocco del programma.
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
    // Programma: istruzioni casuali, di sistema, coppie esclusive.
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
    // T0SZ = 25 (VA a 39 bit, si parte da L1), granulo 4 KiB, IPS 40 bit,
    // niente walk da TTBR1.
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
        s.cpacr_el1 = 0b11 << 20; // FP/SIMD senza trap
    }
    s.tpidr_el1 = rng.next();
    // CNTKCTL_EL1.EL0PCTEN/EL0VCTEN: MRS del contatore a EL0 permesso o no.
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

/// Esito: eventi (eccezioni, HVC, ...) col passo a cui arrivano, passi,
/// CPU finale e RAM.
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

/// Un passo dell'interprete; `None` se l'esecuzione si ferma.
fn step(
    cpu: &mut Cpu,
    mmu: &mut Mmu,
    phys: &mut TestPhys,
    events: &mut Vec<(u64, SysEvent)>,
    n: u64,
) -> bool {
    // `n` conta anche questo passo: prima ne erano stati fatti n - 1.
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

/// Come [`run_interp`]; conta anche i passi fatti con il PC in `stops`.
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

/// Offset della RAM del guest nella memoria di wasmtime (dopo l'area del
/// JIT).
const RAM_IN_ENGINE: usize = 1 << 20;

fn run_jit(cpu: Cpu, ram: &[u8], seed: u64) -> (Trace, vetro_jit::SysJitStats) {
    let (t, s, _) = run_jit_stops(cpu, ram, seed, &[]);
    (t, s)
}

/// Come [`run_jit`] con `SysJit::set_stops(stops)`; conta anche i passi
/// dell'interprete fatti con il PC in `stops`.
fn run_jit_stops(mut cpu: Cpu, ram: &[u8], seed: u64, stops: &[u64]) -> (Trace, vetro_jit::SysJitStats, u64) {
    let mut rng = Rng(seed ^ 0xb0d9e7);
    let mut engine = NativeEngine::new();
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
        // Come `Machine::jit_budget`: niente interrupt in questa prova.
        if interp == Next::Jit && !cpu.sys.il && cpu.pc & 3 == 0 {
            let budget = (STEP_LIMIT - n).min(1 + rng.below(300));
            // L'orologio (a volte no: MRS del contatore esce all'interprete).
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
    assert_eq!(stats.resets, 0, "la RAM sta nella memoria del motore: niente azzeramenti");
    let h = hash(phys.bytes());
    (Trace { events, steps: n, cpu, ram_hash: h }, stats, hits)
}

/// Punti di fermata del JIT (ADR 0027, punti di aggancio
/// dell'introspezione): con indirizzi del programma in `set_stops`, le
/// regioni non li contengono. L'esecuzione resta identica all'interprete,
/// e ogni passo con il PC su un punto di fermata lo fa l'interprete (lo
/// stesso numero di volte che nell'esecuzione tutta interpretata), così
/// la macchina vi può controllare i punti di aggancio. Senza le fermate
/// nelle regioni il conto scende (provato togliendo il controllo in
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
        assert_eq!(got, want, "seme {seed}: interprete e JIT con fermate {stops:x?}");
        assert_eq!(got_hits, want_hits, "seme {seed}: il JIT ha eseguito un punto di fermata {stops:x?}");
        total_hits += want_hits;
        jit_steps += s.jit_steps;
    }
    assert!(
        total_hits > 100 && jit_steps > 10_000,
        "prova troppo debole: {total_hits} fermate, {jit_steps} passi nel JIT"
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
            eprintln!("seme {seed}: {:?}", &want.events[..want.events.len().min(8)]);
        }
        let (got, s) = run_jit(cpu, &ram, seed);
        if got != want {
            let first_diff = got.events.iter().zip(&want.events).position(|(a, b)| a != b);
            let around = |e: &[(u64, SysEvent)]| {
                let i = first_diff.unwrap_or(e.len()).saturating_sub(1);
                format!("{:?}", &e[i.min(e.len())..(i + 3).min(e.len())])
            };
            eprintln!("eventi vicini: interprete {}\nJIT {}", around(&want.events), around(&got.events));
            panic!(
                "seme {seed}: interprete e JIT diversi (VETRO_JIT_SYS_PARITY_SEED={seed} \
                 VETRO_JIT_SYS_PARITY_CASES=1)\nprimo evento diverso: {first_diff:?}\n\
                 passi {} / {}, eventi {} / {}, RAM uguale: {}\ninterprete: {:?}\nJIT:        {:?}",
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
    eprintln!("{cases} programmi, {exceptions} eccezioni; JIT: {total:?}");
    // La prova vale solo se il JIT ha lavorato davvero, anche nei casi
    // difficili.
    assert!(total.jit_steps > cases * STEP_LIMIT / 10, "troppo pochi passi nei blocchi: {total:?}");
    assert!(total.faults > 0 && total.stops > 0 && total.invalidated_pages > 0, "{total:?}");
    assert!(total.tlb_fills > 0 && total.svcs > 0 && total.resolves > 0, "{total:?}");
    // MSR DAIF/DAIFClr che smascherano (ADR 0024).
    assert!(total.yields > 0, "{total:?}");
}

/// TLB degli accessi non allineati (ADR 0024): una pagina Normal riempita da
/// un accesso non allineato riuscito non deve far passare un accesso non
/// allineato che sconfina nella pagina successiva, non mappata: lì
/// l'interprete dà il fault. Anche un Q allineato a 8 ma non a 16 su
/// memoria Device va all'interprete (fault di allineamento).
#[test]
fn tlb_non_allineata_non_sconfina() {
    // (pagina corrente, pagina successiva) cercate nelle tabelle di un caso.
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
        // Normal RW seguita da una pagina non mappata.
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
        assert!(!want.events.is_empty(), "l'accesso a cavallo deve dare un fault");
        assert_eq!(got, want, "seme {seed}");
        checked += 1;
        if checked == 8 {
            break;
        }
    }
    assert!(checked > 0, "nessun caso adatto");
}

/// MSR DAIFClr che smaschera un interrupt (ADR 0024): il blocco esce con
/// YIELD subito dopo l'istruzione, così la macchina ricontrolla gli
/// interrupt al confine giusto (lo stesso dell'interprete). DAIFSet, e
/// DAIFClr di un bit già a zero, non escono.
#[test]
fn daifclr_esce_dopo_l_istruzione() {
    let seed = (0..100).find(|&s| setup(s).0.sys.el == 1).expect("un caso a EL1");
    let (mut cpu, mut ram) = setup(seed);
    let prog = [
        0xd5034fdfu32, // msr DAIFSet, #0xf
        0xd50342ff,    // msr DAIFClr, #0x2
        0x91000400,    // add x0, x0, #1
        0xd50342ff,    // msr DAIFClr, #0x2 (I già a 0)
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
    assert_eq!((r.steps, r.next), (2, Next::Jit), "YIELD dopo DAIFClr");
    assert_eq!(cpu.pc, START + 8);
    assert_eq!(cpu.sys.daif, 0x340, "D, A, F mascherati, I no");
    assert_eq!(jit.stats().yields, 1);
    // Il resto fino al limite: nessun'altra uscita.
    let r = jit.run(&mut cpu, &mut mmu, &mut phys, 10);
    assert_eq!((r.steps, r.next), (10, Next::Jit));
    assert_eq!(cpu.x[0], x0.wrapping_add(2));
    assert_eq!(jit.stats().yields, 1);
}
