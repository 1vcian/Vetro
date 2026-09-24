//! Test della modalità sistema su un bus di prova: traduzione identità con
//! fault iniettabili per pagina, RAM fisica da 1 MiB a 0x4000_0000.
//! Codifiche da `tools/a64asm.sh`; i valori attesi di ESR, maschere e
//! registri ID sono quelli letti da `qemu-system-aarch64 -cpu cortex-a53`
//! (vedi `id.rs`), salvo le differenze documentate in `docs/specs/cpu.md`.

use super::*;
use crate::mem::Access;
use crate::state::Cpu;
use crate::sysreg::EnvReg;
use crate::{Exception, UserMemory};

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: usize = 1 << 20;
const VBAR: u64 = RAM + 0x1000;

struct Bus {
    ram: Vec<u8>,
    /// Pagine (VA >> 12) che non si traducono.
    faults: Vec<(u64, BusFault)>,
    /// Pagine di memoria Device.
    device: Vec<u64>,
    /// Privilegio dell'ultima traduzione di dati.
    last_el: Option<u8>,
    tlbi: Vec<(TlbiOp, u64)>,
    at: Vec<(u64, Access, u8)>,
    at_result: AtResult,
    flushes: u32,
}

impl Bus {
    fn new() -> Bus {
        Bus {
            ram: vec![0; RAM_SIZE],
            faults: Vec::new(),
            device: Vec::new(),
            last_el: None,
            tlbi: Vec::new(),
            at: Vec::new(),
            at_result: AtResult::Par(0),
            flushes: 0,
        }
    }

    fn put(&mut self, addr: u64, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            let o = (addr - RAM) as usize + 4 * i;
            self.ram[o..o + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    fn u64_at(&self, addr: u64) -> u64 {
        let o = (addr - RAM) as usize;
        u64::from_le_bytes(self.ram[o..o + 8].try_into().unwrap())
    }

    fn set_u64(&mut self, addr: u64, v: u64) {
        let o = (addr - RAM) as usize;
        self.ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
}

impl SysBus for Bus {
    fn translate(&mut self, _: &TranslationRegs, va: u64, req: AccessReq) -> Result<u64, BusFault> {
        if let Some(&(_, f)) = self.faults.iter().find(|(p, _)| *p == va >> 12) {
            return Err(f);
        }
        if req.access != Access::Fetch {
            if !req.aligned && self.device.contains(&(va >> 12)) {
                return Err(BusFault::Abort { fsc: BusFault::FSC_ALIGNMENT, ea: false });
            }
            self.last_el = Some(req.el);
        }
        Ok(va)
    }

    fn read_phys(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusFault> {
        let o = pa.wrapping_sub(RAM) as usize;
        if o + buf.len() > RAM_SIZE {
            return Err(BusFault::Abort { fsc: BusFault::FSC_EXTERNAL, ea: false });
        }
        buf.copy_from_slice(&self.ram[o..o + buf.len()]);
        Ok(())
    }

    fn write_phys(&mut self, pa: u64, data: &[u8]) -> Result<(), BusFault> {
        let o = pa.wrapping_sub(RAM) as usize;
        if o + data.len() > RAM_SIZE {
            return Err(BusFault::Abort { fsc: BusFault::FSC_EXTERNAL, ea: true });
        }
        self.ram[o..o + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn at(&mut self, _: &TranslationRegs, va: u64, access: Access, el: u8) -> AtResult {
        self.at.push((va, access, el));
        self.at_result
    }

    fn tlbi(&mut self, op: TlbiOp, xt: u64) {
        self.tlbi.push((op, xt));
    }

    fn tlb_flush_all(&mut self) {
        self.flushes += 1;
    }
}

#[derive(Default)]
struct Env {
    irq: bool,
    fiq: bool,
    counter: u64,
    values: Vec<(EnvReg, u64)>,
    writes: Vec<(EnvReg, u64)>,
}

impl CpuEnv for Env {
    fn irq_line(&mut self) -> bool {
        self.irq
    }

    fn fiq_line(&mut self) -> bool {
        self.fiq
    }

    fn read_sysreg(&mut self, reg: EnvReg) -> u64 {
        match reg {
            EnvReg::CntvctEl0 | EnvReg::CntpctEl0 => self.counter,
            _ => self.values.iter().find(|(r, _)| *r == reg).map_or(0, |&(_, v)| v),
        }
    }

    fn write_sysreg(&mut self, reg: EnvReg, value: u64) {
        self.writes.push((reg, value));
    }
}

struct M {
    cpu: Cpu,
    bus: Bus,
    env: Env,
}

impl M {
    /// CPU appena resettata a EL1h, PC = RAM, VBAR = RAM + 0x1000.
    fn new() -> M {
        let mut cpu = Cpu::new();
        cpu.reset_system(SysConfig::default());
        cpu.pc = RAM;
        cpu.sys.vbar_el1 = VBAR;
        M { cpu, bus: Bus::new(), env: Env::default() }
    }

    /// CPU a EL0 (SP_EL0), PC = `pc`.
    fn at_el0(pc: u64) -> M {
        let mut m = M::new();
        m.cpu.set_el_sp(0, false);
        m.cpu.sys.daif = 0;
        m.cpu.pc = pc;
        m
    }

    fn step(&mut self) -> SysEvent {
        self.cpu.step_system(&mut self.bus, &mut self.env)
    }

    /// Esegue `insn` al PC corrente.
    fn one(&mut self, insn: u32) -> SysEvent {
        let pc = self.cpu.pc;
        self.bus.put(pc, &[insn]);
        self.step()
    }

    /// Esegue `insn` e pretende un'eccezione sincrona con ESR `esr`.
    fn expect_sync(&mut self, insn: u32, want: u64) {
        let pc = self.cpu.pc;
        let from_el = self.cpu.sys.el;
        let ev = self.one(insn);
        assert_eq!(
            ev,
            SysEvent::Exception { kind: ExceptionKind::Sync, esr: want, from_el },
            "{insn:#010x}: ESR {:#x}",
            self.cpu.sys.esr_el1
        );
        assert_eq!(self.cpu.sys.esr_el1, want);
        assert_eq!(self.cpu.sys.elr_el1, pc, "{insn:#010x}: ELR");
        let group = if from_el == 0 { 0x400 } else { 0x200 };
        assert_eq!(self.cpu.pc, VBAR + group, "{insn:#010x}: vettore");
        assert_eq!(self.cpu.sys.el, 1);
    }
}

const RESET_READS: &[u32] = &[
    0xd5384240, // mrs x0, CurrentEL
    0xd5380001, // mrs x1, MIDR_EL1
    0xd53800a2, // mrs x2, MPIDR_EL1
    0xd5380403, // mrs x3, ID_AA64PFR0_EL1
    0xd5380604, // mrs x4, ID_AA64ISAR0_EL1
    0xd53b4225, // mrs x5, DAIF
    0xd5384206, // mrs x6, SPSel
    0xd5381007, // mrs x7, SCTLR_EL1
    0xd5380708, // mrs x8, ID_AA64MMFR0_EL1
    0xd53803e9, // mrs x9, S3_0_C0_C3_7
    0xd539002a, // mrs x10, CLIDR_EL1
    0xd53b00eb, // mrs x11, DCZID_EL0
    0xd530118c, // mrs x12, OSLSR_EL1
    0xd53800cd, // mrs x13, REVIDR_EL1
];

#[test]
fn stato_di_reset_e_registri_id() {
    let mut m = M::new();
    m.bus.put(RAM, RESET_READS);
    for _ in RESET_READS {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    let x = m.cpu.x;
    assert_eq!(x[0], 4, "CurrentEL = EL1");
    assert_eq!(x[1], 0x410f_d034);
    assert_eq!(x[2], 0x8000_0000);
    assert_eq!(x[3], 0x0100_0011, "EL0/EL1 solo AArch64, GICv3");
    assert_eq!(x[4], 0x0001_1120);
    assert_eq!(x[5], 0x3c0);
    assert_eq!(x[6], 1);
    assert_eq!(x[7], 0x00c5_0838);
    assert_eq!(x[8], 0x1122);
    assert_eq!(x[9], 0, "codifica riservata dello spazio ID");
    assert_eq!(x[10], 0x0a20_0023);
    assert_eq!(x[11], 4, "DCZID a EL1: DZP = 0");
    assert_eq!(x[12], 0xa, "OS lock attivo al reset");
    assert_eq!(x[13], 0x100);
}

const TO_EL0: &[u32] = &[
    0xd2a80000, // mov x0, #0x40000000         // =1073741824
    0xf2820000, // movk x0, #0x1000
    0xd518c000, // msr VBAR_EL1, x0
    0xd2a80001, // mov x1, #0x40000000         // =1073741824
    0xf2840001, // movk x1, #0x2000
    0xd5184021, // msr ELR_EL1, x1
    0xd518401f, // msr SPSR_EL1, xzr
    0xd2a80002, // mov x2, #0x40000000         // =1073741824
    0xf2900002, // movk x2, #0x8000
    0xd5184102, // msr SP_EL0, x2
    0xd69f03e0, // eret
];
const EL0_SVC: &[u32] = &[
    0x910003e3, // mov x3, sp
    0xd4000841, // svc #0x42
];
const ERET_BACK: &[u32] = &[
    0xd5384025, // mrs x5, ELR_EL1
    0xd5384006, // mrs x6, SPSR_EL1
    0xd5385207, // mrs x7, ESR_EL1
    0xd69f03e0, // eret
];

#[test]
fn svc_da_el0_e_ritorno() {
    let mut m = M::new();
    m.cpu.sp = RAM + 0x9000; // SP_EL1
    m.bus.put(RAM, TO_EL0);
    m.bus.put(RAM + 0x2000, EL0_SVC);
    m.bus.put(VBAR + 0x400, ERET_BACK);
    for _ in TO_EL0 {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    assert_eq!((m.cpu.sys.el, m.cpu.pc, m.cpu.sp), (0, RAM + 0x2000, RAM + 0x8000));
    assert_eq!(m.cpu.sp_el(1), RAM + 0x9000);
    assert_eq!(m.step(), SysEvent::Executed);
    assert_eq!(m.cpu.x[3], RAM + 0x8000, "a EL0 SP è SP_EL0");
    m.cpu.nzcv = 0x6000_0000;
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x5600_0042, from_el: 0 });
    assert_eq!(m.cpu.pc, VBAR + 0x400, "vettore sincrono da EL0");
    assert_eq!(m.cpu.sys.elr_el1, RAM + 0x2008, "ELR dopo la SVC");
    assert_eq!(m.cpu.sys.spsr_el1, 0x6000_0000, "SPSR: NZCV, EL0t");
    assert_eq!((m.cpu.sys.el, m.cpu.sys.spsel, m.cpu.sys.daif), (1, true, 0x3c0));
    assert_eq!(m.cpu.sp, RAM + 0x9000, "a EL1h SP è SP_EL1");
    for _ in ERET_BACK {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    assert_eq!((m.cpu.x[5], m.cpu.x[6], m.cpu.x[7]), (RAM + 0x2008, 0x6000_0000, 0x5600_0042));
    assert_eq!((m.cpu.sys.el, m.cpu.pc, m.cpu.sp), (0, RAM + 0x2008, RAM + 0x8000));
    assert_eq!((m.cpu.nzcv, m.cpu.sys.daif), (0x6000_0000, 0));
}

#[test]
fn vettori_da_el1_con_sp_el0_e_sp_el1() {
    let mut m = M::new();
    // svc #0x7 a EL1h: gruppo 0x200, ELR dopo l'istruzione.
    let ev = m.one(0xd40000e1);
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x5600_0007, from_el: 1 });
    assert_eq!((m.cpu.pc, m.cpu.sys.elr_el1, m.cpu.sys.spsr_el1), (VBAR + 0x200, RAM + 4, 0x3c5));
    // A EL1t: gruppo 0x000 e SPSR con M = 4.
    let mut m = M::new();
    m.cpu.sp = 0x1111;
    m.bus.put(RAM, &[0xd50040bf, 0xd4000121]); // msr SPSel, #0; svc #0x9
    m.cpu.sys.sp_el[0] = 0x2222;
    assert_eq!(m.step(), SysEvent::Executed);
    assert_eq!((m.cpu.sp, m.cpu.sp_el(1)), (0x2222, 0x1111));
    m.step();
    assert_eq!((m.cpu.pc, m.cpu.sys.spsr_el1), (VBAR, 0x3c4));
    assert_eq!((m.cpu.sp, m.cpu.sp_el(0)), (0x1111, 0x2222));
}

#[test]
fn eret_illegale_e_stato_illegale() {
    let mut m = M::new();
    m.bus.put(
        RAM,
        &[
            0xd2807921, // mov x1, #0x3c9              // =969
            0xd5184001, // msr SPSR_EL1, x1
            0xd2a80002, // mov x2, #0x40000000         // =1073741824
            0xf2802002, // movk x2, #0x100
            0xd5184022, // msr ELR_EL1, x2
            0xd69f03e0, // eret
        ],
    );
    m.bus.put(RAM + 0x100, &[0xd503201f]); // nop
    m.cpu.nzcv = 0xf000_0000;
    for _ in 0..6 {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    // SPSR chiede EL2h: EL e SP restano, IL = 1, NZCV e DAIF da SPSR.
    assert_eq!((m.cpu.pc, m.cpu.sys.el, m.cpu.sys.spsel, m.cpu.sys.il), (RAM + 0x100, 1, true, true));
    assert_eq!(m.cpu.nzcv, 0);
    // Come QEMU: ESR 0x3a000000, SPSR con IL.
    m.expect_sync(0xd503201f, 0x3a00_0000);
    assert_eq!(m.cpu.sys.spsr_el1, 0x0010_03c5);
    assert!(!m.cpu.sys.il);
}

#[test]
fn trap_e_undefined_da_el0() {
    let el0 = RAM + 0x3000;
    // Con SCTLR di reset (UCT, DZE, UMA, UCI = 0; nTWI = 1) e CNTKCTL = 0.
    // Sindromi verificate contro QEMU, tranne DAIFSet (vedi sotto).
    let cases: &[(u32, u64)] = &[
        (0xd53b0020, 0x6232_c001), // mrs x0, CTR_EL0
        (0xd50b7422, 0x6212_dc48), // dc zva, x2
        (0xd53b4223, 0x6232_d065), // mrs x3, DAIF
        // msr DAIFSet, #2: sindrome architetturale (Op1 = 3, Op2 = 6).
        // QEMU 10 scambia Op1 e Op2 (0x620793e4).
        (0xd50342df, 0x620c_d3e4),
        (0xd53be044, 0x6234_f881), // mrs x4, CNTVCT_EL0
        (0xd53be009, 0x6230_f921), // mrs x9, CNTFRQ_EL0
        (0xd50b7b26, 0x6212_dcd6), // dc cvau, x6
        (0xd50b7526, 0x6212_dcca), // ic ivau, x6
        (0xd5381005, 0x0200_0000), // mrs x5, SCTLR_EL1
        (0xd51bd067, 0x0200_0000), // msr TPIDRRO_EL0, x7
        (0xd4000002, 0x0200_0000), // hvc #0
        (0xd69f03e0, 0x0200_0000), // eret
        (0xd50041bf, 0x0200_0000), // msr SPSel, #1
        (0xd508871f, 0x0200_0000), // tlbi vmalle1
        (0xd5087800, 0x0200_0000), // at s1e1r, x0
        (0xd538cc0a, 0x0200_0000), // mrs x10, ICC_IAR1_EL1
        (0xd538424b, 0x0200_0000), // mrs x11, CurrentEL
    ];
    for &(insn, want) in cases {
        let mut m = M::at_el0(el0);
        m.expect_sync(insn, want);
        assert_eq!(m.cpu.sys.spsr_el1, 0, "{insn:#010x}: SPSR EL0t");
    }
    // WFI si trappa solo con nTWI = 0 (ESR come QEMU).
    let mut m = M::at_el0(el0);
    assert_eq!(m.one(0xd503207f), SysEvent::WaitForInterrupt);
    let mut m = M::at_el0(el0);
    m.cpu.sys.sctlr_el1 &= !sctlr::NTWI;
    m.expect_sync(0xd503207f, 0x07e0_0000);
    // WFE non si trappa mai (come QEMU).
    let mut m = M::at_el0(el0);
    m.cpu.sys.sctlr_el1 &= !sctlr::NTWE;
    assert_eq!(m.one(0xd503205f), SysEvent::Executed);
    assert_eq!(m.cpu.pc, el0 + 4);
}

#[test]
fn accessi_permessi_da_el0() {
    let el0 = RAM + 0x3000;
    let mut m = M::at_el0(el0);
    m.cpu.sys.sctlr_el1 |= sctlr::UCT | sctlr::DZE | sctlr::UMA | sctlr::UCI;
    m.cpu.sys.cntkctl_el1 = cntkctl::EL0VCTEN;
    m.cpu.tpidrro_el0 = 0x77;
    m.env.counter = 12345;
    m.bus.put(
        el0,
        &[
            0xd53b0020, // mrs x0, CTR_EL0
            0xd53b00e1, // mrs x1, DCZID_EL0
            0xd50b7422, // dc zva, x2
            0xd53b4223, // mrs x3, DAIF
            0xd50342df, // msr DAIFSet, #0x2
            0xd53be044, // mrs x4, CNTVCT_EL0
            0xd50b7b26, // dc cvau, x6
            0xd50b7526, // ic ivau, x6
            0xd53bd068, // mrs x8, TPIDRRO_EL0
            0xd53be009, // mrs x9, CNTFRQ_EL0
        ],
    );
    m.cpu.x[2] = RAM + 0x5010;
    m.bus.ram[0x5000..0x5040].fill(0xaa);
    m.env.values.push((EnvReg::CntfrqEl0, 62_500_000));
    for i in 0..10 {
        assert_eq!(m.step(), SysEvent::Executed, "istruzione {i}");
    }
    assert_eq!(m.cpu.x[0], 0x8444_8004);
    assert_eq!(m.cpu.x[1], 4, "DZE = 1: DZP = 0");
    assert!(m.bus.ram[0x5000..0x5040].iter().all(|&b| b == 0), "DC ZVA azzera il blocco allineato");
    assert_eq!(m.cpu.x[3], 0);
    assert_eq!(m.cpu.sys.daif, 0x80);
    assert_eq!(m.cpu.x[4], 12345);
    assert_eq!(m.cpu.x[8], 0x77);
    assert_eq!(m.cpu.x[9], 62_500_000, "CNTFRQ leggibile con EL0VCTEN");
    assert_eq!(m.cpu.sys.el, 0);
    // DCZID a EL0 con DZE = 0: DZP = 1 (0x14, come QEMU).
    let mut m = M::at_el0(el0);
    m.one(0xd53b00e1);
    assert_eq!(m.cpu.x[1], 0x14);
}

#[test]
fn hvc_smc_brk_e_sp_el0_a_el1() {
    // HVC a EL1 con conduit HVC: evento per il PSCI, PC dopo l'istruzione.
    let mut m = M::new();
    assert_eq!(m.one(0xd40000a2), SysEvent::Hvc(5));
    assert_eq!(m.cpu.pc, RAM + 4);
    // SMC senza EL3: UNDEFINED, ELR sulla SMC (come QEMU).
    m.expect_sync(0xd40000c3, 0x0200_0000);
    // Con conduit SMC si invertono.
    let mut m = M::new();
    m.cpu.sys.cfg.psci = PsciConduit::Smc;
    assert_eq!(m.one(0xd40000c3), SysEvent::Smc(6));
    m.expect_sync(0xd40000a2, 0x0200_0000);
    let mut m = M::new();
    m.expect_sync(0xd4200ee0, 0xf200_0077); // brk #0x77
    // MRS SP_EL0 a EL1h legge la copia; a EL1t è UNDEFINED (come QEMU).
    let mut m = M::new();
    m.cpu.sys.sp_el[0] = 0xabc0;
    m.bus.put(RAM, &[0xd5384100, 0xd50040bf, 0xd5384101]);
    m.step();
    assert_eq!(m.cpu.x[0], 0xabc0);
    m.step();
    let ev = m.step();
    assert!(matches!(ev, SysEvent::Exception { esr: 0x0200_0000, .. }));
    assert_eq!(m.cpu.pc, VBAR, "da EL1t: gruppo 0x000");
}

#[test]
fn trap_fp_simd_da_cpacr() {
    let insns = [
        0x9e670020, // fmov d0, x1
        0xd53b4402, // mrs x2, FPCR
        0x4ea38441, // add v1.4s, v2.4s, v3.4s
    ];
    for insn in insns {
        // CPACR_EL1 = 0 (reset): trap anche a EL1, ESR come QEMU.
        let mut m = M::new();
        m.expect_sync(insn, 0x1fe0_0000);
        // FPEN = 01: EL1 esegue, EL0 trappa.
        let mut m = M::new();
        m.cpu.sys.cpacr_el1 = 1 << 20;
        assert_eq!(m.one(insn), SysEvent::Executed);
        let mut m = M::at_el0(RAM + 0x3000);
        m.cpu.sys.cpacr_el1 = 1 << 20;
        m.expect_sync(insn, 0x1fe0_0000);
        // FPEN = 11: nessuna trap.
        let mut m = M::at_el0(RAM + 0x3000);
        m.cpu.sys.cpacr_el1 = 3 << 20;
        assert_eq!(m.one(insn), SysEvent::Executed);
    }
}

#[test]
fn abort_dei_dati_e_delle_istruzioni() {
    let data = RAM + 0x6000;
    // Translation fault di livello 3 in lettura da EL1 e da EL0.
    let mut m = M::new();
    m.cpu.x[1] = data + 8;
    m.bus.faults.push((data >> 12, BusFault::Abort { fsc: 0b000111, ea: false }));
    m.expect_sync(0xf9400020, 0x9600_0007); // ldr x0, [x1]
    assert_eq!(m.cpu.sys.far_el1, data + 8);
    let mut m = M::at_el0(RAM + 0x3000);
    m.cpu.x[1] = data;
    m.bus.faults.push((data >> 12, BusFault::Abort { fsc: 0b001111, ea: false }));
    m.expect_sync(0xf9000022, 0x9200_004f); // str x2, [x1]: WnR, permesso L3
    // Abort esterno sull'accesso fisico: EA dal bus (slave error in scrittura).
    let mut m = M::new();
    m.cpu.x[1] = 0x1_0000_0000;
    m.expect_sync(0xf9400020, 0x9600_0010);
    assert_eq!(m.cpu.sys.far_el1, 0x1_0000_0000);
    m.cpu.pc = RAM;
    m.expect_sync(0xf9000022, 0x9600_0250);
    // Instruction abort: fetch da una pagina che non si traduce.
    let mut m = M::at_el0(data);
    m.bus.faults.push((data >> 12, BusFault::Abort { fsc: 0b000101, ea: false }));
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x8200_0005, from_el: 0 });
    assert_eq!((m.cpu.sys.far_el1, m.cpu.sys.elr_el1), (data, data));
    // Limite di Vetro nella traduzione: evento, stato invariato.
    let mut m = M::new();
    m.cpu.x[1] = data;
    m.bus.faults.push((data >> 12, BusFault::Unimplemented("granulo 64 KiB")));
    let ev = m.one(0xf9400020);
    assert_eq!(ev, SysEvent::Unimplemented { raw: 0xf9400020, what: "granulo 64 KiB" });
    assert_eq!((m.cpu.pc, m.cpu.sys.el), (RAM, 1));
}

#[test]
fn fault_di_allineamento() {
    let data = RAM + 0x6000;
    // Memoria Device disallineata (a MMU spenta tutti i dati sono Device):
    // ESR e FAR come QEMU.
    let mut m = M::new();
    m.bus.device.push(data >> 12);
    m.cpu.x[1] = data + 1;
    m.expect_sync(0xf9400020, 0x9600_0021);
    assert_eq!(m.cpu.sys.far_el1, data + 1);
    // Allineato su Device: nessun fault.
    let mut m = M::new();
    m.bus.device.push(data >> 12);
    m.cpu.x[1] = data + 8;
    assert_eq!(m.one(0xf9400020), SysEvent::Executed);
    // DC ZVA su Device: sempre fault di allineamento (WnR = 1).
    let mut m = M::new();
    m.bus.device.push(data >> 12);
    m.cpu.x[1] = data;
    m.expect_sync(0xd50b7421, 0x9600_0061);
    // SCTLR.A: disallineato su memoria normale.
    let mut m = M::new();
    m.cpu.sys.sctlr_el1 |= sctlr::A;
    m.cpu.x[1] = data + 4;
    m.expect_sync(0xf9400020, 0x9600_0021);
    // Senza SCTLR.A la memoria normale ammette il disallineato.
    let mut m = M::new();
    m.cpu.x[1] = data + 4;
    assert_eq!(m.one(0xa9402027), SysEvent::Executed); // ldp x7, x8, [x1]
    // Esclusive: sempre allineate; WnR per STXR.
    let mut m = M::new();
    m.cpu.x[1] = data + 4;
    m.expect_sync(0xc85f7c23, 0x9600_0021); // ldxr x3, [x1]
    m.cpu.pc = RAM;
    m.expect_sync(0xc8047c23, 0x9600_0061); // stxr w4, x3, [x1]
}

#[test]
fn ldtr_sttr_usano_il_privilegio_di_el0() {
    let data = RAM + 0x6000;
    let mut m = M::new();
    m.cpu.x[1] = data;
    m.bus.set_u64(data, 0x1234);
    m.one(0xf8400825); // ldtr x5, [x1]
    assert_eq!((m.cpu.x[5], m.bus.last_el), (0x1234, Some(0)));
    m.one(0xf9400020); // ldr x0, [x1]
    assert_eq!(m.bus.last_el, Some(1));
    m.cpu.x[6] = 0x99;
    m.one(0xf8000826); // sttr x6, [x1]
    assert_eq!((m.bus.u64_at(data), m.bus.last_el), (0x99, Some(0)));
}

#[test]
fn pc_disallineato() {
    let mut m = M::new();
    m.cpu.x[1] = RAM + 0x102;
    assert_eq!(m.one(0xd61f0020), SysEvent::Executed); // br x1
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x8a00_0000, from_el: 1 });
    assert_eq!((m.cpu.sys.far_el1, m.cpu.sys.elr_el1), (RAM + 0x102, RAM + 0x102));
}

#[test]
fn top_byte_ignore_sui_salti() {
    let mut m = M::new();
    m.cpu.sys.tcr_el1 = 1 << 37; // TBI0
    m.cpu.x[1] = 0x5a00_0000_4000_0100;
    m.one(0xd61f0020); // br x1
    assert_eq!(m.cpu.pc, RAM + 0x100);
    let mut m = M::new();
    m.cpu.x[1] = 0x5a00_0000_4000_0100;
    m.one(0xd61f0020);
    assert_eq!(m.cpu.pc, 0x5a00_0000_4000_0100, "senza TBI il tag resta");
}

#[test]
fn irq_mascherati_e_smascherati() {
    let mut m = M::new();
    m.env.irq = true;
    m.bus.put(RAM, &[0xd50342ff, 0xd503201f, 0xd503201f]); // msr DAIFClr, #2; nop; nop
    // Mascherato al reset: l'istruzione si esegue.
    assert_eq!(m.step(), SysEvent::Executed);
    assert_eq!(m.cpu.sys.daif, 0x340);
    // Smascherato: si prende prima della prossima istruzione, ELR = PC.
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Irq, esr: 0, from_el: 1 });
    assert_eq!((m.cpu.pc, m.cpu.sys.elr_el1, m.cpu.sys.spsr_el1), (VBAR + 0x280, RAM + 4, 0x345));
    assert_eq!(m.cpu.sys.daif, 0x3c0);
    assert_eq!(m.cpu.sys.esr_el1, 0, "IRQ non scrive ESR");
    // Da EL0: gruppo 0x400 + 0x80.
    let mut m = M::at_el0(RAM + 0x3000);
    m.env.irq = true;
    assert!(matches!(m.step(), SysEvent::Exception { kind: ExceptionKind::Irq, from_el: 0, .. }));
    assert_eq!(m.cpu.pc, VBAR + 0x480);
    // FIQ prima dell'IRQ; SError con PSTATE.A = 0.
    let mut m = M::at_el0(RAM + 0x3000);
    m.env.irq = true;
    m.env.fiq = true;
    m.step();
    assert_eq!(m.cpu.pc, VBAR + 0x500);
    let mut m = M::at_el0(RAM + 0x3000);
    m.cpu.sys.serror_pending = Some(0x12);
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::SError, esr: 0xbe00_0012, from_el: 0 });
    assert_eq!(m.cpu.pc, VBAR + 0x580);
    assert_eq!(m.cpu.sys.serror_pending, None);
}

#[test]
fn wfi_restituisce_l_attesa() {
    let mut m = M::new();
    assert_eq!(m.one(0xd503207f), SysEvent::WaitForInterrupt);
    assert_eq!(m.cpu.pc, RAM + 4);
}

#[test]
fn registri_dell_ambiente() {
    let mut m = M::new();
    m.env.counter = 777;
    m.env.irq = true; // mascherato: si vede solo in ISR_EL1
    m.env.values.push((EnvReg::IccIar1El1, 27));
    m.cpu.x[3] = 1;
    m.bus.put(
        RAM,
        &[
            0xd53be040, // mrs x0, CNTVCT_EL0
            0xd53be021, // mrs x1, CNTPCT_EL0
            0xd538cc02, // mrs x2, ICC_IAR1_EL1
            0xd518cc22, // msr ICC_EOIR1_EL1, x2
            0xd51be323, // msr CNTV_CTL_EL0, x3
            0xd538c104, // mrs x4, ISR_EL1
        ],
    );
    for _ in 0..6 {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    assert_eq!((m.cpu.x[0], m.cpu.x[1], m.cpu.x[2], m.cpu.x[4]), (777, 777, 27, 0x80));
    assert_eq!(m.env.writes, [(EnvReg::IccEoir1El1, 27), (EnvReg::CntvCtlEl0, 1)]);
    // Sola lettura e sola scrittura: UNDEFINED.
    let mut m = M::new();
    m.expect_sync(0xd538cc22, 0x0200_0000); // mrs x2, S3_0_C12_C12_1 (ICC_EOIR1_EL1)
    m.cpu.pc = RAM;
    m.expect_sync(0xd518cc02, 0x0200_0000); // msr S3_0_C12_C12_0 (ICC_IAR1_EL1), x2
    // Senza GICv3 gli ICC_* non esistono; il timer sì.
    let mut m = M::new();
    m.cpu.sys.cfg.gicv3 = false;
    m.expect_sync(0xd538cc02, 0x0200_0000);
    m.cpu.pc = RAM;
    m.one(0xd5380403); // mrs x3, ID_AA64PFR0_EL1
    assert_eq!(m.cpu.x[3], 0x11);
}

#[test]
fn tlbi_at_e_svuotamento_del_tlb() {
    let mut m = M::new();
    m.cpu.x[1] = 0x0005_0000_0000_1234;
    m.cpu.x[2] = 0x4000_5000;
    m.cpu.x[3] = 0x00c5_0839 | 0b1111 << 38; // bit MTE: QEMU li azzera
    m.cpu.x[4] = 0x25;
    m.cpu.x[5] = 0x8000;
    m.bus.at_result = AtResult::Par(0xff00_0000_4000_5b80);
    m.bus.put(
        RAM,
        &[
            0xd5088321, // tlbi vae1is, x1
            0xd5087842, // at s1e0r, x2
            0xd5181003, // msr SCTLR_EL1, x3
            0xd5182044, // msr TCR_EL1, x4
            0xd5182005, // msr TTBR0_EL1, x5
            0xd508751f, // ic iallu
            0xd50b7e22, // dc civac, x2
        ],
    );
    for _ in 0..7 {
        assert_eq!(m.step(), SysEvent::Executed);
    }
    assert_eq!(m.bus.tlbi, [(TlbiOp::Vae1is, 0x0005_0000_0000_1234)]);
    assert_eq!(m.bus.at, [(0x4000_5000, Access::Read, 0)]);
    assert_eq!(m.cpu.sys.par_el1, 0xff00_0000_4000_5b80);
    assert_eq!(m.cpu.sys.sctlr_el1, 0x00c5_0839);
    assert_eq!((m.cpu.sys.tcr_el1, m.cpu.sys.ttbr0_el1), (0x25, 0x8000));
    assert_eq!(m.bus.flushes, 2, "SCTLR e TCR svuotano il TLB, TTBR no");
    // AT con abort esterno sul walk: Data Abort con CM = 1 e WnR = 1.
    let mut m = M::new();
    m.cpu.x[2] = 0x1234;
    m.bus.at_result = AtResult::Abort { fsc: 0b010101, ea: true };
    m.expect_sync(0xd5087842, 0x9600_0355);
    assert_eq!(m.cpu.sys.far_el1, 0x1234);
}

#[test]
fn registri_con_maschere_di_qemu() {
    let mut m = M::new();
    m.cpu.x[0] = u64::MAX;
    m.cpu.x[2] = u64::MAX;
    m.cpu.x[5] = u64::MAX;
    m.cpu.x[7] = u64::MAX;
    m.cpu.x[10] = u64::MAX;
    m.bus.put(
        RAM,
        &[
            0xd51b4220, // msr DAIF, x0
            0xd53b4221, // mrs x1, DAIF
            0xd51b4202, // msr NZCV, x2
            0xd53b4203, // mrs x3, NZCV
            0xd53a0004, // mrs x4, CSSELR_EL1
            0xd51a0005, // msr CSSELR_EL1, x5
            0xd5390006, // mrs x6, CCSIDR_EL1
            0xd518c007, // msr VBAR_EL1, x7
            0xd538c008, // mrs x8, VBAR_EL1
            0xd510109f, // msr OSLAR_EL1, xzr
            0xd5301189, // mrs x9, OSLSR_EL1
            0xd51000ca, // msr DBGWVR0_EL1, x10
            0xd53000cb, // mrs x11, DBGWVR0_EL1
        ],
    );
    for i in 0..13 {
        assert_eq!(m.step(), SysEvent::Executed, "istruzione {i}");
    }
    let x = m.cpu.x;
    assert_eq!((x[1], x[3], x[4], x[6]), (0x3c0, 0xf000_0000, 0, 0));
    assert_eq!(m.cpu.sys.csselr_el1, 0xf);
    assert_eq!(x[8], 0xffff_ffff_ffff_ffe0, "VBAR: solo i 5 bit bassi azzerati");
    assert_eq!(x[9], 0x8, "OS lock tolto");
    assert_eq!(x[11], 0xffff_ffff_ffff_fffc);
    // CCSIDR secondo CSSELR (valori di QEMU).
    for (sel, want) in [(0u64, 0x700f_e01au64), (1, 0x203f_e002), (2, 0x707f_e07a), (3, 0)] {
        let mut m = M::new();
        m.cpu.sys.csselr_el1 = sel;
        m.one(0xd5390006);
        assert_eq!(m.cpu.x[6], want, "CSSELR {sel}");
    }
    // Registro valido ma non modellato: limite di Vetro, stato invariato.
    let mut m = M::new();
    let ev = m.one(0xd538002c); // mrs x12, S3_0_C0_C0_1
    assert_eq!(ev, SysEvent::Unimplemented { raw: 0xd538002c, what: "MRS/MSR registro di sistema" });
    assert_eq!(m.cpu.pc, RAM);
}

#[test]
fn modalita_utente_invariata() {
    // Le istruzioni che la modalità sistema riconosce restano, in modalità
    // utente, quello che erano prima.
    let cases: &[(u32, Result<(), Exception>)] = &[
        (0xd69f03e0, Err(Exception::Undefined(0xd69f03e0))), // eret
        (0xd4000002, Err(Exception::Undefined(0xd4000002))), // hvc #0
        (0xd4000003, Err(Exception::Undefined(0xd4000003))), // smc #0
        (0xd503207f, Ok(())),                                // wfi
        (0xd503205f, Ok(())),                                // wfe
        (0xd51bd060, Err(Exception::Undefined(0xd51bd060))), // msr TPIDRRO_EL0, x0
        (0xd50342df, Err(Exception::Undefined(0xd50342df))), // msr DAIFSet, #2
        (0xd508871f, Err(Exception::Undefined(0xd508871f))), // tlbi vmalle1
        (
            0xd5380001, // mrs x1, MIDR_EL1
            Err(Exception::Unimplemented { raw: 0xd5380001, what: "MRS/MSR registro di sistema" }),
        ),
        (
            0xd53be042, // mrs x2, CNTVCT_EL0
            Err(Exception::Unimplemented { raw: 0xd53be042, what: "MRS/MSR registro di sistema" }),
        ),
    ];
    for &(insn, ref want) in cases {
        let mut mem = UserMemory::new();
        mem.map(0x1000, insn.to_le_bytes().to_vec(), crate::Perm::RX).unwrap();
        let mut cpu = Cpu::new();
        cpu.pc = 0x1000;
        let before = cpu.clone();
        let got = cpu.step(&mut mem);
        assert_eq!(&got, want, "{insn:#010x}");
        if got.is_err() {
            assert_eq!(cpu, before, "{insn:#010x}: stato invariato");
        } else {
            assert_eq!(cpu.pc, 0x1004);
        }
        assert_eq!(cpu.sys, SysState::default());
    }
}
