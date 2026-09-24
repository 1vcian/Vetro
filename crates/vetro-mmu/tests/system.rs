//! CPU in modalità sistema sopra la MMU vera ([`MmuBus`]): programmi
//! bare-metal a EL1 con tabelle delle pagine in una RAM di prova.
//!
//! I descrittori si scrivono con le costanti del formato VMSAv8-64 (Arm ARM
//! D8.3); le codifiche delle istruzioni vengono da `tools/a64asm.sh`.

use std::collections::BTreeMap;

use vetro_cpu::sys::{ExceptionKind, SysEvent};
use vetro_cpu::sysreg::EnvReg;
use vetro_cpu::{Cpu, CpuEnv, SysConfig};
use vetro_mmu::{BusError, Mmu, MmuBus, PhysMemory};

/// RAM fisica da 0x4000_0000 a 0x8000_0000 (pagine assenti = zeri).
#[derive(Default)]
struct Ram {
    pages: BTreeMap<u64, Box<[u8; 4096]>>,
}

const RAM_BASE: u64 = 0x4000_0000;
const RAM_END: u64 = 0x8000_0000;

impl PhysMemory for Ram {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        for (k, b) in buf.iter_mut().enumerate() {
            let p = pa + k as u64;
            if !(RAM_BASE..RAM_END).contains(&p) {
                return Err(BusError::Decode);
            }
            *b = self.pages.get(&(p >> 12)).map_or(0, |pg| pg[(p & 0xfff) as usize]);
        }
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        for (k, &b) in data.iter().enumerate() {
            let p = pa + k as u64;
            if !(RAM_BASE..RAM_END).contains(&p) {
                return Err(BusError::Decode);
            }
            self.pages.entry(p >> 12).or_insert_with(|| Box::new([0; 4096]))[(p & 0xfff) as usize] = b;
        }
        Ok(())
    }
}

impl Ram {
    fn put(&mut self, addr: u64, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            self.write(addr + 4 * i as u64, &w.to_le_bytes()).unwrap();
        }
    }

    fn wr(&mut self, pa: u64, v: u64) {
        self.write(pa, &v.to_le_bytes()).unwrap();
    }

    fn rd(&mut self, pa: u64) -> u64 {
        self.read_u64(pa).unwrap()
    }
}

/// Nessun interrupt; i registri dell'ambiente valgono zero.
struct NoEnv;

impl CpuEnv for NoEnv {
    fn irq_line(&mut self) -> bool {
        false
    }
    fn read_sysreg(&mut self, _: EnvReg) -> u64 {
        0
    }
    fn write_sysreg(&mut self, _: EnvReg, _: u64) {}
}

// Formato dei descrittori.
const TABLE: u64 = 0b11;
const PAGE: u64 = 0b11;
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 0b11 << 8;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const AP_RW_EL1: u64 = 0b00 << 6;
const AP_RW_ALL: u64 = 0b01 << 6;
const AP_RO_ALL: u64 = 0b11 << 6;
/// Normal Write-Back (AttrIndx 1).
const NORMAL: u64 = AF | SH_INNER | 1 << 2;
/// Device-nGnRnE (AttrIndx 0).
const DEVICE: u64 = AF;

/// MAIR: 0 = Device-nGnRnE, 1 = Normal WB, 2 = Normal Non-cacheable.
const MAIR: u64 = 0x44_ff_00;
/// T0SZ = 25 (39 bit, primo livello 1), TG0 = 4 KiB, EPD1, IPS = 40 bit.
const TCR: u64 = 25 | 1 << 23 | 0b010 << 32;

const L1: u64 = 0x4001_0000;
const L2: u64 = 0x4001_1000;
const L3: u64 = 0x4001_2000;

const KCODE: u64 = 0x4000_0000;
const VECTORS: u64 = 0x4000_1000;
const UCODE: u64 = 0x4000_2000;
const UDATA: u64 = 0x4000_3000;
const KDATA: u64 = 0x4000_4000;

struct Machine {
    cpu: Cpu,
    mmu: Mmu,
    ram: Ram,
}

impl Machine {
    /// Tabelle a identità per le pagine del programma: codice e vettori
    /// del kernel (RW a EL1, UXN), codice utente (RO per tutti, PXN), dati
    /// utente (RW per tutti) e dati del kernel (RW solo a EL1).
    fn new() -> Machine {
        let mut m = Machine { cpu: Cpu::new(), mmu: Mmu::new(Mmu::PA_BITS_CORTEX_A53), ram: Ram::default() };
        m.ram.wr(L1 + 8, L2 | TABLE);
        m.ram.wr(L2, L3 | TABLE);
        m.map(KCODE, KCODE, NORMAL | AP_RW_EL1 | UXN);
        m.map(VECTORS, VECTORS, NORMAL | AP_RW_EL1 | UXN);
        m.map(UCODE, UCODE, NORMAL | AP_RO_ALL | PXN);
        m.map(UDATA, UDATA, NORMAL | AP_RW_ALL | UXN | PXN);
        m.map(KDATA, KDATA, NORMAL | AP_RW_EL1 | UXN | PXN);
        m.cpu.reset_system(SysConfig::default());
        m.cpu.pc = KCODE;
        m
    }

    /// Pagina da 4 KiB nella tabella di livello 3 (VA 0x4000_0000..0x401f_ffff).
    fn map(&mut self, va: u64, pa: u64, attrs: u64) {
        self.ram.wr(L3 + ((va >> 12) & 511) * 8, pa | attrs | PAGE);
    }

    /// MMU già accesa, senza passare dal programma.
    fn mmu_on(&mut self) {
        let s = &mut self.cpu.sys;
        s.mair_el1 = MAIR;
        s.tcr_el1 = TCR;
        s.ttbr0_el1 = L1;
        s.sctlr_el1 |= 1;
    }

    fn step(&mut self) -> SysEvent {
        let mut bus = MmuBus::new(&mut self.mmu, &mut self.ram);
        self.cpu.step_system(&mut bus, &mut NoEnv)
    }

    /// Esegue fino a un evento diverso da `Executed` o da un'eccezione.
    fn run_until_event(&mut self, limit: usize) -> SysEvent {
        for _ in 0..limit {
            match self.step() {
                SysEvent::Executed | SysEvent::Exception { .. } => {}
                ev => return ev,
            }
        }
        panic!("nessun evento in {limit} passi, PC {:#x}", self.cpu.pc);
    }
}

const KERNEL: &[u32] = &[
    0xd2a80000, // mov x0, #0x40000000         // =1073741824
    0xf2820000, // movk x0, #0x1000
    0xd518c000, // msr VBAR_EL1, x0
    0xd29fe000, // mov x0, #0xff00             // =65280
    0xf2a00880, // movk x0, #0x44, lsl #16
    0xd518a200, // msr MAIR_EL1, x0
    0xd2800320, // mov x0, #0x19               // =25
    0xf2a01000, // movk x0, #0x80, lsl #16
    0xf2c00040, // movk x0, #0x2, lsl #32
    0xd5182040, // msr TCR_EL1, x0
    0xd2a80020, // mov x0, #0x40010000         // =1073807360
    0xd5182000, // msr TTBR0_EL1, x0
    0xd5033fdf, // isb
    0xd5381000, // mrs x0, SCTLR_EL1
    0xb2400000, // orr x0, x0, #0x1
    0xd5181000, // msr SCTLR_EL1, x0
    0xd5033fdf, // isb
    0xd2a80001, // mov x1, #0x40000000         // =1073741824
    0xf2880001, // movk x1, #0x4000
    0xd5087841, // at s1e0r, x1
    0xd5033fdf, // isb
    0xd538740a, // mrs x10, PAR_EL1
    0xd5087801, // at s1e1r, x1
    0xd5033fdf, // isb
    0xd538740b, // mrs x11, PAR_EL1
    0xf8400822, // ldtr x2, [x1]
    0xf9400029, // ldr x9, [x1]
    0xd2a80000, // mov x0, #0x40000000         // =1073741824
    0xf2840000, // movk x0, #0x2000
    0xd5184020, // msr ELR_EL1, x0
    0xd518401f, // msr SPSR_EL1, xzr
    0xd2a80000, // mov x0, #0x40000000         // =1073741824
    0xf287fe00, // movk x0, #0x3ff0
    0xd5184100, // msr SP_EL0, x0
    0xd69f03e0, // eret
];
/// Vettore sincrono dallo stesso livello con SP_EL1: registra ESR, FAR e
/// salta l'istruzione colpevole.
const SAME_EL_SYNC: &[u32] = &[
    0xd538520c, // mrs x12, ESR_EL1
    0xd538600d, // mrs x13, FAR_EL1
    0xd538402e, // mrs x14, ELR_EL1
    0x910011ce, // add x14, x14, #0x4
    0xd518402e, // msr ELR_EL1, x14
    0xd69f03e0, // eret
];
/// Vettore sincrono da EL0: per una SVC chiude con HVC, per il resto
/// registra ESR e FAR e salta l'istruzione.
const LOWER_EL_SYNC: &[u32] = &[
    0xd5385211, // mrs x17, ESR_EL1
    0xd35afe32, // lsr x18, x17, #26
    0xf100565f, // cmp x18, #0x15
    0x540000e0, // b.eq 0x28 <.text+0x28>
    0xaa1103ef, // mov x15, x17
    0xd5386010, // mrs x16, FAR_EL1
    0xd5384033, // mrs x19, ELR_EL1
    0x91001273, // add x19, x19, #0x4
    0xd5184033, // msr ELR_EL1, x19
    0xd69f03e0, // eret
    0xaa1103f4, // mov x20, x17
    0xd5384015, // mrs x21, SPSR_EL1
    0xd4000002, // hvc #0
];
const USER: &[u32] = &[
    0xd2a80003, // mov x3, #0x40000000         // =1073741824
    0xf2860003, // movk x3, #0x3000
    0xd2800aa4, // mov x4, #0x55               // =85
    0xf9000064, // str x4, [x3]
    0xd2a80005, // mov x5, #0x40000000         // =1073741824
    0xf2880005, // movk x5, #0x4000
    0xf94000a6, // ldr x6, [x5]
    0x910003e7, // mov x7, sp
    0xd4000661, // svc #0x33
];

#[test]
fn programma_bare_metal_mmu_e_svc_da_el0() {
    let mut m = Machine::new();
    m.ram.put(KCODE, KERNEL);
    m.ram.put(VECTORS + 0x200, SAME_EL_SYNC);
    m.ram.put(VECTORS + 0x400, LOWER_EL_SYNC);
    m.ram.put(UCODE, USER);
    m.ram.wr(KDATA, 0xdead);
    m.cpu.sp = KDATA + 0xff0;
    assert_eq!(m.run_until_event(200), SysEvent::Hvc(0));
    let x = m.cpu.x;
    // AT S1E0R su una pagina solo EL1: PAR con F = 1 e permission fault L3.
    assert_eq!(x[10], 1 << 11 | 0b001111 << 1 | 1);
    // AT S1E1R: PA, attributi Normal WB (0xff), Inner Shareable, NS.
    assert_eq!(x[11], 0xff00_0000_4000_4b80);
    // LDTR a EL1 su una pagina solo EL1: Data Abort stesso livello,
    // permission fault L3 in lettura.
    assert_eq!((x[12], x[13], x[14]), (0x9600_000f, KDATA, KCODE + 26 * 4));
    assert_eq!(x[2], 0, "LDTR fallita non scrive");
    assert_eq!(x[9], 0xdead, "LDR a EL1 legge");
    // A EL0: la scrittura sui dati utente riesce, la lettura dei dati del
    // kernel dà un Data Abort da livello inferiore.
    assert_eq!(m.ram.rd(UDATA), 0x55);
    assert_eq!((x[15], x[16]), (0x9200_000f, KDATA));
    assert_eq!(x[6], 0);
    assert_eq!(x[7], UDATA + 0xff0, "a EL0 SP è SP_EL0");
    // SVC da EL0: ESR, SPSR = EL0t, poi HVC a EL1.
    assert_eq!((x[20], x[21]), (0x5600_0033, 0));
    assert_eq!(m.cpu.sys.elr_el1, UCODE + 9 * 4);
    assert_eq!((m.cpu.sys.el, m.cpu.sp), (1, KDATA + 0xff0));
}

#[test]
fn tlb_tlbi_e_svuotamento_su_sctlr() {
    const VA: u64 = 0x4000_5000;
    const A: u64 = 0x4000_6000;
    const B: u64 = 0x4000_7000;
    let mut m = Machine::new();
    m.mmu_on();
    m.map(VA, A, NORMAL | AP_RW_EL1 | UXN | PXN);
    m.ram.wr(A, 0xa);
    m.ram.wr(B, 0xb);
    m.ram.put(
        KCODE,
        &[
            0xf9400020, // ldr x0, [x1]
            0xf9400020, // ldr x0, [x1]
            0xd5088722, // tlbi vae1, x2
            0xf9400020, // ldr x0, [x1]
        ],
    );
    m.cpu.x[1] = VA;
    m.cpu.x[2] = VA >> 12;
    m.step();
    assert_eq!(m.cpu.x[0], 0xa);
    // Il descrittore cambia ma la voce nel TLB resta finché non c'è una TLBI.
    m.map(VA, B, NORMAL | AP_RW_EL1 | UXN | PXN);
    m.step();
    assert_eq!(m.cpu.x[0], 0xa);
    m.step();
    m.step();
    assert_eq!(m.cpu.x[0], 0xb);

    // Una scrittura di SCTLR_EL1 svuota il TLB (come QEMU).
    let mut m = Machine::new();
    m.mmu_on();
    m.map(VA, A, NORMAL | AP_RW_EL1 | UXN | PXN);
    m.ram.wr(A, 0xa);
    m.ram.wr(B, 0xb);
    m.ram.put(
        KCODE,
        &[
            0xf9400020, // ldr x0, [x1]
            0xd5181003, // msr SCTLR_EL1, x3
            0xf9400020, // ldr x0, [x1]
        ],
    );
    m.cpu.x[1] = VA;
    m.cpu.x[3] = m.cpu.sys.sctlr_el1;
    m.step();
    assert_eq!(m.cpu.x[0], 0xa);
    m.map(VA, B, NORMAL | AP_RW_EL1 | UXN | PXN);
    m.step();
    m.step();
    assert_eq!(m.cpu.x[0], 0xb);
}

#[test]
fn abort_delle_istruzioni_e_allineamento_su_device() {
    // EL0 salta nel codice del kernel (UXN): Instruction Abort da EL0,
    // permission fault L3, FAR = destinazione.
    let mut m = Machine::new();
    m.mmu_on();
    m.cpu.sys.vbar_el1 = VECTORS;
    m.ram.put(UCODE, &[0xd63f0020]); // blr x1
    m.cpu.x[1] = KCODE + 0x40;
    m.cpu.pc = UCODE;
    m.cpu.sys.el = 0;
    m.cpu.sys.spsel = false;
    assert_eq!(m.step(), SysEvent::Executed);
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x8200_000f, from_el: 0 });
    assert_eq!((m.cpu.sys.far_el1, m.cpu.sys.elr_el1, m.cpu.pc), (KCODE + 0x40, KCODE + 0x40, VECTORS + 0x400));

    // Accesso disallineato a una pagina Device: fault di allineamento dopo
    // il walk; allineato va.
    const DEV: u64 = 0x4000_8000;
    let mut m = Machine::new();
    m.mmu_on();
    m.cpu.sys.vbar_el1 = VECTORS;
    m.map(DEV, DEV, DEVICE | AP_RW_EL1 | UXN | PXN);
    m.ram.put(KCODE, &[0xf9400020, 0xf9400020]); // ldr x0, [x1] (due volte)
    m.cpu.x[1] = DEV + 8;
    assert_eq!(m.step(), SysEvent::Executed);
    m.cpu.x[1] = DEV + 4;
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x9600_0021, from_el: 1 });
    assert_eq!(m.cpu.sys.far_el1, DEV + 4);
    // La stessa pagina come Normal ammette il disallineato.
    let mut m = Machine::new();
    m.mmu_on();
    m.map(DEV, DEV, NORMAL | AP_RW_EL1 | UXN | PXN);
    m.ram.put(KCODE, &[0xf9400020]);
    m.cpu.x[1] = DEV + 4;
    assert_eq!(m.step(), SysEvent::Executed);

    // MMU spenta: i dati sono Device, il disallineato fa fault (come QEMU).
    let mut m = Machine::new();
    m.cpu.sys.vbar_el1 = VECTORS;
    m.ram.put(KCODE, &[0xf9400020]);
    m.cpu.x[1] = KDATA + 1;
    let ev = m.step();
    assert_eq!(ev, SysEvent::Exception { kind: ExceptionKind::Sync, esr: 0x9600_0021, from_el: 1 });
}

#[test]
fn granulo_64k_e_limite_di_vetro() {
    let mut m = Machine::new();
    m.mmu_on();
    m.cpu.sys.tcr_el1 = TCR | 0b01 << 14; // TG0 = 64 KiB
    let ev = m.step();
    assert!(matches!(ev, SysEvent::Unimplemented { raw: 0, .. }), "{ev:?}");
    assert_eq!((m.cpu.pc, m.cpu.sys.el), (KCODE, 1), "stato invariato");
}
