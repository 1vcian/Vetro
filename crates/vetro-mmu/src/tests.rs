//! Test della MMU su tabelle costruite in una memoria fisica di prova.
//!
//! I descrittori si scrivono qui con le costanti del formato VMSAv8-64
//! (Arm ARM D8.3), indipendenti dal codice sotto test.

use std::collections::BTreeMap;

use vetro_cpu::{Access, MemFault, Memory};

use crate::*;

/// Sotto questo indirizzo fisico risponde la RAM (pagine assenti = zeri),
/// sopra nessuno (decode error).
const LIMIT: u64 = 1 << 36;

#[derive(Default)]
struct Ram {
    pages: BTreeMap<u64, Box<[u8; 4096]>>,
}

impl PhysMemory for Ram {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        for (k, b) in buf.iter_mut().enumerate() {
            let p = pa + k as u64;
            if p >= LIMIT {
                return Err(BusError::Decode);
            }
            *b = self.pages.get(&(p >> 12)).map_or(0, |pg| pg[(p & 0xfff) as usize]);
        }
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        for (k, &b) in data.iter().enumerate() {
            let p = pa + k as u64;
            if p >= LIMIT {
                return Err(BusError::Decode);
            }
            self.pages.entry(p >> 12).or_insert_with(|| Box::new([0; 4096]))[(p & 0xfff) as usize] = b;
        }
        Ok(())
    }
}

/// Memoria che risponde sempre con uno slave error.
struct Broken;

impl PhysMemory for Broken {
    fn read(&mut self, _: u64, _: &mut [u8]) -> Result<(), BusError> {
        Err(BusError::Slave)
    }
    fn write(&mut self, _: u64, _: &[u8]) -> Result<(), BusError> {
        Err(BusError::Slave)
    }
}

// Formato dei descrittori.
const TABLE: u64 = 0b11;
const PAGE: u64 = 0b11;
const BLOCK: u64 = 0b01;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const AP_RW_EL1: u64 = 0b00 << 6;
const AP_RW_ALL: u64 = 0b01 << 6;
const AP_RO_EL1: u64 = 0b10 << 6;
const AP_RO_ALL: u64 = 0b11 << 6;
const SH_INNER: u64 = 0b11 << 8;
const PXN_TABLE: u64 = 1 << 59;
const UXN_TABLE: u64 = 1 << 60;
const AP_TABLE_NO_EL0: u64 = 1 << 61;
const AP_TABLE_RO: u64 = 1 << 62;
/// Normal Write-Back (AttrIndx 1), Inner Shareable, AF.
const NORMAL: u64 = AF | SH_INNER | 1 << 2;
/// Device-nGnRnE (AttrIndx 0).
const DEVICE: u64 = AF;

/// MAIR: 0 = Device-nGnRnE, 1 = Normal WB, 2 = Normal Non-cacheable.
const MAIR: u64 = 0x44_ff_00;
/// T0SZ = T1SZ = 16, TG0 = 4 KiB, TG1 = 4 KiB, IPS = 40 bit.
const TCR: u64 = 16 | 16 << 16 | 0b10 << 30 | 0b010 << 32;

fn tlbi_xt(va: u64, asid: u16) -> u64 {
    u64::from(asid) << 48 | (va >> 12 & ((1 << 44) - 1))
}

struct Env {
    ram: Ram,
    mmu: Mmu,
    next: u64,
    root0: u64,
    root1: u64,
}

impl Env {
    fn new() -> Env {
        let mut e = Env { ram: Ram::default(), mmu: Mmu::new(40), next: 0x100_0000, root0: 0, root1: 0 };
        e.root0 = e.alloc();
        e.root1 = e.alloc();
        e.mmu.regs = MmuRegs { sctlr: sctlr::M, tcr: TCR, ttbr0: e.root0, ttbr1: e.root1, mair: MAIR };
        e
    }

    fn alloc(&mut self) -> u64 {
        let a = self.next;
        self.next += 4096;
        a
    }

    fn wr(&mut self, pa: u64, v: u64) {
        self.ram.write(pa, &v.to_le_bytes()).unwrap();
    }

    fn rd(&mut self, pa: u64) -> u64 {
        self.ram.read_u64(pa).unwrap()
    }

    /// Dimensione d'ingresso e livello iniziale della metà di `va`.
    fn geometry(&self, va: u64) -> (u32, u32) {
        let hi = va >> 55 & 1 != 0;
        let txsz = (self.mmu.regs.tcr >> if hi { 16 } else { 0 } & 0x3f) as u32;
        let size = 64 - txsz.clamp(16, 39);
        (size, 4 - (size - 12).div_ceil(9))
    }

    /// Indirizzo del descrittore di livello `level` per `va`, creando le
    /// tabelle intermedie che mancano.
    fn slot(&mut self, va: u64, level: u32) -> u64 {
        let (size, start) = self.geometry(va);
        let mut table = if va >> 55 & 1 != 0 { self.root1 } else { self.root0 };
        let mut l = start;
        loop {
            let shift = 39 - 9 * l;
            let width = if l == start { size - shift } else { 9 };
            let slot = table + ((va >> shift) & ((1 << width) - 1)) * 8;
            if l == level {
                return slot;
            }
            let d = self.rd(slot);
            table = if d & 3 == TABLE {
                d & 0xffff_ffff_f000
            } else {
                let t = self.alloc();
                self.wr(slot, t | TABLE);
                t
            };
            l += 1;
        }
    }

    /// Mappa `va` → `pa` con una foglia di livello `level` (1: 1 GiB,
    /// 2: 2 MiB, 3: 4 KiB); restituisce l'indirizzo del descrittore.
    fn map(&mut self, va: u64, pa: u64, level: u32, attrs: u64) -> u64 {
        let slot = self.slot(va, level);
        self.wr(slot, pa | attrs | if level == 3 { PAGE } else { BLOCK });
        slot
    }

    /// Aggiunge bit al descrittore di tabella di livello `level` per `va`.
    fn set_table_bits(&mut self, va: u64, level: u32, bits: u64) {
        let slot = self.slot(va, level);
        let d = self.rd(slot);
        self.wr(slot, d | bits);
    }

    fn tr(&mut self, va: u64, access: Access, el: u8) -> Result<Translation, Fault> {
        self.mmu.translate(&mut self.ram, va, access, el)
    }

    fn walk(&mut self, va: u64, access: Access, el: u8) -> Result<Translation, Fault> {
        self.mmu.walk(&mut self.ram, va, access, el)
    }

    fn pa(&mut self, va: u64) -> u64 {
        self.tr(va, Access::Read, 1).unwrap().pa
    }

    fn kind(&mut self, va: u64, access: Access, el: u8) -> FaultKind {
        self.walk(va, access, el).unwrap_err().kind
    }

    fn vm(&mut self, el: u8) -> VirtMemory<'_, Ram> {
        VirtMemory::new(&mut self.mmu, &mut self.ram, el)
    }
}

// --- Traduzioni riuscite ---

#[test]
fn pagina_4k() {
    let mut e = Env::new();
    e.map(0x40_0000_1000, 0x20_3000, 3, NORMAL | AP_RW_ALL);
    let t = e.tr(0x40_0000_1abc, Access::Read, 0).unwrap();
    assert_eq!(t.pa, 0x20_3abc);
    assert_eq!(t.level, 3);
    assert_eq!(t.block_size, 4096);
    assert_eq!(t.attr_index, Some(1));
    assert_eq!(t.mair_attr, 0xff);
    assert_eq!(t.sh, 0b11);
    assert!(!t.ng);
    assert_eq!(t.perms, Some(Perms { ap: 0b01, uxn: false, pxn: false }));
    assert_eq!(e.walk(0x40_0000_1abc, Access::Write, 0).unwrap(), t);
}

#[test]
fn blocchi_2m_e_1g() {
    let mut e = Env::new();
    e.map(0x8020_0000, 0x4060_0000, 2, NORMAL);
    let t = e.tr(0x8020_0000 + 0x12_3456, Access::Read, 1).unwrap();
    assert_eq!((t.pa, t.level, t.block_size), (0x4060_0000 + 0x12_3456, 2, 2 << 20));

    // I bit 29:12 di un blocco da 1 GiB sono RES0 e vengono ignorati.
    e.map(0x1_4000_0000, 0x8_0000_0000 | 0x20_0000, 1, NORMAL);
    let t = e.tr(0x1_4000_0000 + 0x3456_789a, Access::Write, 1).unwrap();
    assert_eq!((t.pa, t.level, t.block_size), (0x8_3456_789a, 1, 1 << 30));
}

#[test]
fn ttbr1_per_gli_indirizzi_alti() {
    let mut e = Env::new();
    let va = 0xffff_8000_0000_3000;
    e.map(va, 0x7000, 3, NORMAL);
    assert_eq!(e.pa(va + 8), 0x7008);
    // Stessi indici di tabella ma metà bassa: TTBR0 è vuota.
    assert_eq!(e.kind(va & 0xffff_ffff_ffff, Access::Read, 1), FaultKind::Translation(0));
    // Con TTBR1 vuota la metà alta fallisce al livello 0.
    e.mmu.regs.ttbr1 = e.alloc();
    e.mmu.tlb_mut().flush_all();
    assert_eq!(e.kind(va, Access::Read, 1), FaultKind::Translation(0));
}

#[test]
fn indirizzi_fuori_dalle_due_meta() {
    let mut e = Env::new();
    for va in [0x0001_0000_0000_0000, 0x8000_0000_0000_0000, 0xfffe_ffff_ffff_f000, 0x00ff_ffff_ffff_f000] {
        let f = e.tr(va, Access::Read, 1).unwrap_err();
        assert_eq!(f.kind, FaultKind::Translation(0), "{va:#x}");
        assert_eq!(f.far(), va);
    }
}

#[test]
fn txsz_generici_e_livello_iniziale() {
    // T0SZ = 25: 39 bit, si parte dal livello 1.
    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !0x3f | 25;
    e.map(0x7f_ffff_f000, 0x5000, 3, NORMAL);
    assert_eq!(e.rd(e.root0 + 511 * 8) & 3, TABLE, "la radice è una tabella di livello 1");
    assert_eq!(e.pa(0x7f_ffff_f010), 0x5010);
    assert_eq!(e.kind(0x80_0000_0000, Access::Read, 1), FaultKind::Translation(0));
    e.map(0x4000_0000, 0x8000_0000, 1, NORMAL);
    assert_eq!(e.pa(0x4000_1234), 0x8000_1234);

    // T0SZ = 34: 30 bit, si parte dal livello 2 (radice di 8 byte × 512).
    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !0x3f | 34;
    e.map(0x3fe0_0000, 0x60_0000, 2, NORMAL);
    assert_eq!(e.rd(e.root0 + 511 * 8) & 3, BLOCK);
    assert_eq!(e.pa(0x3fe0_0042), 0x60_0042);
    assert_eq!(e.kind(0x4000_0000, Access::Read, 1), FaultKind::Translation(0));

    // T0SZ = 24: 40 bit, livello 0 con due sole voci.
    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !0x3f | 24;
    e.map(0xff_ffff_f000, 0x9000, 3, NORMAL);
    assert_eq!(e.rd(e.root0 + 8) & 3, TABLE);
    assert_eq!(e.pa(0xff_ffff_f001), 0x9001);

    // T1SZ = 39 indipendente da T0SZ.
    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !(0x3f << 16) | 39 << 16;
    e.map(0xffff_ffff_ffe0_0000, 0xa0_0000, 2, NORMAL);
    assert_eq!(e.pa(0xffff_ffff_ffe0_1234), 0xa0_1234);
    assert_eq!(e.kind(0xffff_ffff_fd00_0000, Access::Read, 1), FaultKind::Translation(0));
}

#[test]
fn txsz_fuori_intervallo_si_limita() {
    // T0SZ = 8 vale come 16 (48 bit), T0SZ = 48 come 39.
    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !0x3f | 8;
    e.map(0xffff_ffff_f000, 0x3000, 3, NORMAL);
    assert_eq!(e.pa(0xffff_ffff_f000), 0x3000);
    assert_eq!(e.kind(0x1_0000_0000_0000, Access::Read, 1), FaultKind::Translation(0));

    let mut e = Env::new();
    e.mmu.regs.tcr = TCR & !0x3f | 39; // tabelle per 25 bit
    e.map(0x1ff_f000, 0x5000, 3, NORMAL);
    e.mmu.regs.tcr = TCR & !0x3f | 48;
    assert_eq!(e.pa(0x1ff_f000), 0x5000);
    assert_eq!(e.kind(0x200_0000, Access::Read, 1), FaultKind::Translation(0));
}

#[test]
fn bit_bassi_e_asid_di_ttbr_non_toccano_la_base() {
    let mut e = Env::new();
    e.map(0x1000, 0x2000, 3, NORMAL);
    e.mmu.regs.ttbr0 = e.root0 | 0x7fe | 0x55 << 48;
    assert_eq!(e.walk(0x1000, Access::Read, 1).unwrap().pa, 0x2000);
}

#[test]
fn attributi_mair() {
    let mut e = Env::new();
    e.map(0x1000, 0x1000, 3, DEVICE);
    e.map(0x2000, 0x2000, 3, AF | 2 << 2);
    let t = e.pa(0x1000);
    assert_eq!(t, 0x1000);
    assert_eq!(e.tr(0x1000, Access::Read, 1).unwrap().mair_attr, 0x00);
    assert_eq!(e.tr(0x2000, Access::Read, 1).unwrap().mair_attr, 0x44);
    // MAIR si rilegge anche per le voci già nel TLB.
    e.mmu.regs.mair = 0xbb_00_00;
    assert_eq!(e.tr(0x2000, Access::Read, 1).unwrap().mair_attr, 0xbb);
}

// --- MMU spenta, TBI, granuli ---

#[test]
fn mmu_spenta_identita() {
    let mut e = Env::new();
    e.mmu.regs.sctlr = 0;
    let t = e.tr(0xff_ffff_fff0, Access::Write, 0).unwrap();
    assert_eq!((t.pa, t.perms, t.mair_attr, t.sh), (0xff_ffff_fff0, None, 0x00, 0b10));
    assert_eq!(e.tr(0x1000, Access::Fetch, 1).unwrap().mair_attr, 0x44);
    e.mmu.regs.sctlr = sctlr::I;
    assert_eq!(e.tr(0x1000, Access::Fetch, 1).unwrap().mair_attr, 0xaa);
    // Oltre PARange (40 bit): address size fault di livello 0.
    let f = e.tr(0x100_0000_0000, Access::Read, 1).unwrap_err();
    assert_eq!(f.kind, FaultKind::AddressSize(0));
    assert_eq!(f.kind.fsc(), Some(0));
    assert!(e.mmu.tlb().is_empty());
    // Con TBI il tag non conta.
    assert!(e.tr(0x5a00_0000_0000_1000, Access::Read, 1).is_err());
    e.mmu.regs.tcr |= tcr::TBI0;
    assert_eq!(e.tr(0x5a00_0000_0000_1000, Access::Read, 1).unwrap().pa, 0x1000);
}

#[test]
fn top_byte_ignore() {
    let mut e = Env::new();
    e.map(0x40_0000_1000, 0x20_3000, 3, NORMAL);
    let tagged = 0x5a00_0040_0000_1004;
    assert_eq!(e.kind(tagged, Access::Read, 1), FaultKind::Translation(0));
    e.mmu.regs.tcr |= tcr::TBI0;
    let t = e.tr(tagged, Access::Read, 1).unwrap();
    assert_eq!(t.pa, 0x20_3004);
    // Il FAR conserva il tag.
    e.map(0x40_0000_2000, 0x20_4000, 3, NORMAL | AP_RO_EL1);
    let f = e.tr(0x5a00_0040_0000_2000, Access::Write, 1).unwrap_err();
    assert_eq!((f.kind, f.far()), (FaultKind::Permission(3), 0x5a00_0040_0000_2000));

    // Metà alta: bit 55 = 1 e tag 0x00 vale solo con TBI1.
    let hi = 0xffff_8000_0000_5000;
    e.map(hi, 0x6000, 3, NORMAL);
    let tagged_hi = hi & 0x00ff_ffff_ffff_ffff;
    assert_eq!(e.kind(tagged_hi, Access::Read, 1), FaultKind::Translation(0));
    e.mmu.regs.tcr |= tcr::TBI1;
    assert_eq!(e.pa(tagged_hi), 0x6000);
}

#[test]
fn granuli() {
    let mut e = Env::new();
    e.map(0x1000, 0x2000, 3, NORMAL);
    // TG0 = 16 KiB non esiste sulla A53: vale 4 KiB.
    e.mmu.regs.tcr = TCR | 0b10 << 14;
    assert_eq!(e.walk(0x1000, Access::Read, 1).unwrap().pa, 0x2000);
    // TG0 = 64 KiB: limite di Vetro, niente codifica ESR.
    e.mmu.regs.tcr = TCR | 0b01 << 14;
    let f = e.walk(0x1000, Access::Read, 1).unwrap_err();
    assert!(matches!(f.kind, FaultKind::Unimplemented(_)));
    assert_eq!((f.kind.fsc(), f.esr(1), f.par()), (None, None, None));
    // TG1 = 00 (riservato) vale 4 KiB, TG1 = 11 è 64 KiB.
    let hi = 0xffff_0000_0000_0000;
    e.mmu.regs.tcr = TCR & !(3 << 30);
    e.map(hi, 0x3000, 3, NORMAL);
    assert_eq!(e.walk(hi, Access::Read, 1).unwrap().pa, 0x3000);
    e.mmu.regs.tcr = TCR | 3 << 30;
    assert!(matches!(e.kind(hi, Access::Read, 1), FaultKind::Unimplemented(_)));
    // Descrittori big-endian: limite.
    e.mmu.regs.tcr = TCR;
    e.mmu.regs.sctlr |= sctlr::EE;
    assert!(matches!(e.kind(0x1000, Access::Read, 1), FaultKind::Unimplemented(_)));
}

// --- Fault ---

#[test]
fn translation_fault_ai_vari_livelli() {
    let mut e = Env::new();
    assert_eq!(e.kind(0x1000, Access::Read, 1), FaultKind::Translation(0));
    let va = 0x0000_0080_4020_1000; // indici 1, 1, 1, 1
    e.map(va, 0x5000, 3, NORMAL);
    assert_eq!(e.kind(va + (1 << 30), Access::Read, 1), FaultKind::Translation(1));
    assert_eq!(e.kind(va + (1 << 21), Access::Read, 1), FaultKind::Translation(2));
    assert_eq!(e.kind(va + (1 << 12), Access::Read, 1), FaultKind::Translation(3));
    // "Blocco" al livello 3.
    let slot = e.slot(va, 3);
    e.wr(slot, 0x5000 | NORMAL | BLOCK);
    assert_eq!(e.kind(va, Access::Read, 1), FaultKind::Translation(3));
    // Blocco al livello 0: con 4 KiB non esiste.
    e.wr(e.root0, NORMAL | BLOCK);
    assert_eq!(e.kind(0x10, Access::Read, 1), FaultKind::Translation(0));
    // Descrittore riservato (0b10) al livello 1.
    let slot = e.slot(va, 1);
    e.wr(slot, 0x5000 | NORMAL | 0b10);
    assert_eq!(e.kind(va, Access::Read, 1), FaultKind::Translation(1));
}

#[test]
fn access_flag_fault() {
    let mut e = Env::new();
    e.map(0x1000, 0x2000, 3, NORMAL & !AF);
    e.map(0x40_0000, 0x60_0000, 2, NORMAL & !AF);
    let f = e.tr(0x1008, Access::Read, 1).unwrap_err();
    assert_eq!((f.kind, f.kind.fsc()), (FaultKind::AccessFlag(3), Some(0x0b)));
    let f = e.tr(0x40_0008, Access::Fetch, 0).unwrap_err();
    assert_eq!((f.kind, f.kind.fsc()), (FaultKind::AccessFlag(2), Some(0x0a)));
    // I fault di AF non entrano nel TLB: basta mettere AF.
    let slot = e.slot(0x1000, 3);
    let d = e.rd(slot);
    e.wr(slot, d | AF);
    assert_eq!(e.pa(0x1008), 0x2008);
    // L'AF viene prima dei permessi.
    e.map(0x3000, 0x3000, 3, (NORMAL & !AF) | AP_RO_EL1);
    assert_eq!(e.kind(0x3000, Access::Write, 0), FaultKind::AccessFlag(3));
}

#[test]
fn address_size_fault() {
    let mut e = Env::new();
    // Uscita oltre IPS = 32 bit.
    e.map(0x1000, 0x1_0000_0000, 3, NORMAL);
    e.map(0x40_0000, 0x1000_0000, 2, NORMAL);
    e.mmu.regs.tcr = TCR & !(7 << 32);
    assert_eq!(e.kind(0x1000, Access::Read, 1), FaultKind::AddressSize(3));
    assert_eq!(e.walk(0x40_0000, Access::Read, 1).unwrap().pa, 0x1000_0000);
    // IPS = 48 bit limitato a PARange = 40.
    e.mmu.regs.tcr = TCR | 0b101 << 32;
    e.map(0x2000, 1 << 44, 3, NORMAL);
    assert_eq!(e.kind(0x2000, Access::Read, 1), FaultKind::AddressSize(3));
    assert_eq!(e.walk(0x1000, Access::Read, 1).unwrap().pa, 0x1_0000_0000);
    // Base in TTBR oltre la dimensione in uscita: livello 0.
    e.mmu.regs.tcr = TCR;
    let root = e.root0;
    e.mmu.regs.ttbr0 = root | 1 << 40;
    assert_eq!(e.kind(0x1000, Access::Read, 1), FaultKind::AddressSize(0));
    e.mmu.regs.ttbr0 = root;
    // Descrittore di tabella di livello 1 che punta oltre: livello 1.
    let slot = e.slot(0x1000, 1);
    let d = e.rd(slot);
    e.wr(slot, d | 1 << 41);
    let f = e.walk(0x1000, Access::Read, 1).unwrap_err();
    assert_eq!((f.kind, f.kind.fsc()), (FaultKind::AddressSize(1), Some(1)));
}

#[test]
fn abort_esterno_durante_il_walk() {
    let mut e = Env::new();
    e.mmu.regs.ttbr0 = LIMIT;
    let f = e.walk(0x1000, Access::Read, 1).unwrap_err();
    assert_eq!((f.kind, f.kind.fsc()), (FaultKind::ExternalWalk(0, BusError::Decode), Some(0x14)));
    assert_eq!(f.esr(1), Some(0x9600_0014));
    e.mmu.regs.ttbr0 = e.root0;
    e.map(0x1000, 0x1000, 3, NORMAL);
    // Tabella di livello 2 che punta fuori dalla RAM: fallisce la lettura
    // del descrittore di livello 3.
    let slot = e.slot(0x1000, 2);
    e.wr(slot, LIMIT | TABLE);
    assert_eq!(e.kind(0x1000, Access::Read, 1), FaultKind::ExternalWalk(3, BusError::Decode));
    // Slave error: bit EA nella sindrome.
    let f = e.mmu.walk(&mut Broken, 0x1000, Access::Write, 1).unwrap_err();
    assert_eq!(f.kind, FaultKind::ExternalWalk(0, BusError::Slave));
    assert_eq!(f.esr(0), Some(0x9200_0254));
}

#[test]
fn permessi_ap() {
    // (AP, EL, lettura, scrittura)
    let casi = [
        (AP_RW_EL1, 1, true, true),
        (AP_RW_EL1, 0, false, false),
        (AP_RW_ALL, 1, true, true),
        (AP_RW_ALL, 0, true, true),
        (AP_RO_EL1, 1, true, false),
        (AP_RO_EL1, 0, false, false),
        (AP_RO_ALL, 1, true, false),
        (AP_RO_ALL, 0, true, false),
    ];
    let mut e = Env::new();
    for (i, &(ap, el, r, w)) in casi.iter().enumerate() {
        let va = 0x10_0000 + i as u64 * 0x1000;
        e.map(va, 0x5000, 3, NORMAL | ap);
        let esito = |res: Result<Translation, Fault>| match res {
            Ok(_) => true,
            Err(f) => {
                assert_eq!(f.kind, FaultKind::Permission(3));
                false
            }
        };
        assert_eq!(esito(e.tr(va, Access::Read, el)), r, "lettura, AP {:#b} EL{el}", ap >> 6);
        assert_eq!(esito(e.tr(va, Access::Write, el)), w, "scrittura, AP {:#b} EL{el}", ap >> 6);
    }
}

#[test]
fn permessi_di_esecuzione() {
    let mut e = Env::new();
    let fetch = |e: &mut Env, va, el| e.walk(va, Access::Fetch, el).map(|_| ()).map_err(|f| f.kind);
    let perm = Err(FaultKind::Permission(3));
    e.map(0x1000, 0x1000, 3, NORMAL | AP_RO_ALL);
    assert_eq!(fetch(&mut e, 0x1000, 0), Ok(()));
    assert_eq!(fetch(&mut e, 0x1000, 1), Ok(()));
    // Scrivibile da EL0: mai eseguibile a EL1.
    e.map(0x2000, 0x2000, 3, NORMAL | AP_RW_ALL);
    assert_eq!(fetch(&mut e, 0x2000, 0), Ok(()));
    assert_eq!(fetch(&mut e, 0x2000, 1), perm);
    e.map(0x3000, 0x3000, 3, NORMAL | AP_RO_ALL | UXN);
    assert_eq!(fetch(&mut e, 0x3000, 0), perm);
    assert_eq!(fetch(&mut e, 0x3000, 1), Ok(()));
    e.map(0x4000, 0x4000, 3, NORMAL | AP_RO_ALL | PXN);
    assert_eq!(fetch(&mut e, 0x4000, 0), Ok(()));
    assert_eq!(fetch(&mut e, 0x4000, 1), perm);
    // Solo esecuzione a EL0: AP = 00, UXN = 0.
    e.map(0x5000, 0x5000, 3, NORMAL | AP_RW_EL1 | PXN);
    assert_eq!(fetch(&mut e, 0x5000, 0), Ok(()));
    assert_eq!(e.kind(0x5000, Access::Read, 0), FaultKind::Permission(3));
    // WXN: ciò che è scrivibile non è eseguibile.
    e.mmu.regs.sctlr |= sctlr::WXN;
    e.map(0x6000, 0x6000, 3, NORMAL | AP_RW_EL1);
    e.map(0x7000, 0x7000, 3, NORMAL | AP_RO_EL1);
    assert_eq!(fetch(&mut e, 0x6000, 1), perm);
    assert_eq!(fetch(&mut e, 0x7000, 1), Ok(()));
    assert_eq!(fetch(&mut e, 0x2000, 0), perm);
    assert_eq!(fetch(&mut e, 0x1000, 0), Ok(()));
}

#[test]
fn permessi_ereditati_dalle_tabelle() {
    let base = 0x40_0000_0000;
    let mut e = Env::new();
    for i in 0..4 {
        e.map(base + i * (1 << 30), 0x5000, 3, NORMAL | AP_RW_ALL);
    }
    let fetch_base = base + 4 * (1 << 30);
    e.map(fetch_base, 0x5000, 3, NORMAL | AP_RO_ALL);
    e.map(fetch_base + (1 << 30), 0x5000, 3, NORMAL | AP_RO_ALL);
    // Bit sul descrittore di livello 1 (tabella di livello 2).
    e.set_table_bits(base, 1, AP_TABLE_NO_EL0);
    e.set_table_bits(base + (1 << 30), 1, AP_TABLE_RO);
    e.set_table_bits(base + 2 * (1 << 30), 1, AP_TABLE_RO | AP_TABLE_NO_EL0);
    e.set_table_bits(fetch_base, 1, UXN_TABLE);
    e.set_table_bits(fetch_base + (1 << 30), 1, PXN_TABLE);

    let perm = FaultKind::Permission(3);
    let ok = |e: &mut Env, va, a, el| e.walk(va, a, el).is_ok();
    // APTable[0]: niente EL0, EL1 invariato.
    assert_eq!(e.kind(base, Access::Read, 0), perm);
    assert!(ok(&mut e, base, Access::Write, 1));
    assert_eq!(e.walk(base, Access::Read, 1).unwrap().perms.unwrap().ap, 0b00);
    // APTable[1]: sola lettura per tutti.
    let va = base + (1 << 30);
    assert!(ok(&mut e, va, Access::Read, 0));
    assert_eq!(e.kind(va, Access::Write, 0), perm);
    assert_eq!(e.kind(va, Access::Write, 1), perm);
    // Entrambi.
    let va = base + 2 * (1 << 30);
    assert_eq!(e.kind(va, Access::Read, 0), perm);
    assert!(ok(&mut e, va, Access::Read, 1));
    assert_eq!(e.kind(va, Access::Write, 1), perm);
    // Nessun bit: tutto permesso.
    assert!(ok(&mut e, base + 3 * (1 << 30), Access::Write, 0));
    // UXNTable e PXNTable.
    assert_eq!(e.kind(fetch_base, Access::Fetch, 0), perm);
    assert!(ok(&mut e, fetch_base, Access::Fetch, 1));
    let va = fetch_base + (1 << 30);
    assert!(ok(&mut e, va, Access::Fetch, 0));
    assert_eq!(e.kind(va, Access::Fetch, 1), perm);
    // I bit si accumulano anche dal livello 0.
    e.set_table_bits(base + 3 * (1 << 30), 0, AP_TABLE_RO);
    assert_eq!(e.kind(base + 3 * (1 << 30), Access::Write, 1), perm);
}

#[test]
fn codifiche_esr_far_par() {
    let mut e = Env::new();
    // Scrittura a EL1 su una pagina assente (tabelle presenti): livello 3.
    e.map(0x1000, 0x1000, 3, NORMAL | AP_RO_EL1 | UXN);
    let f = e.tr(0x2008, Access::Write, 1).unwrap_err();
    assert_eq!(f.kind, FaultKind::Translation(3));
    assert_eq!(f.esr(1), Some(0x9600_0047));
    assert_eq!(f.far(), 0x2008);
    assert_eq!(f.par(), Some(1 << 11 | 0x07 << 1 | 1));
    // Lettura da EL0 negata: Data Abort da livello inferiore, permesso L3.
    let f = e.tr(0x1010, Access::Read, 0).unwrap_err();
    assert_eq!(f.esr(0), Some(0x9200_000f));
    // Stesso accesso non privilegiato eseguito a EL1 (LDTR): EC dello stesso livello.
    assert_eq!(f.esr(1), Some(0x9600_000f));
    // Fetch da EL0 senza tabella: Instruction Abort, translation L0.
    let f = e.tr(0x1234_5678_0000, Access::Fetch, 0).unwrap_err();
    assert_eq!(f.esr(0), Some(0x8200_0004));
    let f = e.tr(0x1000, Access::Fetch, 0).unwrap_err();
    assert_eq!(f.esr(0), Some(0x8200_000f));
    assert_eq!(f.esr(1), Some(0x8600_000f));
    // Codici DFSC.
    let fsc = |k: FaultKind| k.fsc().unwrap();
    assert_eq!(
        [fsc(FaultKind::AddressSize(2)), fsc(FaultKind::Translation(1)), fsc(FaultKind::AccessFlag(1))],
        [0x02, 0x05, 0x09]
    );
    assert_eq!(
        [
            fsc(FaultKind::Permission(2)),
            fsc(FaultKind::External(BusError::Decode)),
            fsc(FaultKind::ExternalWalk(3, BusError::Decode))
        ],
        [0x0e, 0x10, 0x17]
    );
    assert_eq!(FaultKind::Permission(2).level(), Some(2));
    assert_eq!(FaultKind::External(BusError::Slave).level(), None);
}

#[test]
fn par_di_una_traduzione() {
    let mut e = Env::new();
    e.map(0x1000, 0x12_3000, 3, NORMAL);
    e.map(0x2000, 0x45_6000, 3, DEVICE);
    let t = e.walk(0x1abc, Access::Read, 1).unwrap();
    assert_eq!(t.par(), 0xff << 56 | 0x12_3000 | 1 << 11 | 1 << 9 | 0b11 << 7);
    // Device: SH riportato come Outer Shareable.
    let t = e.walk(0x2000, Access::Read, 1).unwrap();
    assert_eq!(t.par(), 0x45_6000 | 1 << 11 | 1 << 9 | 0b10 << 7);
}

// --- ASID, TLB e TLBI ---

#[test]
fn asid_da_ttbr_con_a1_e_as() {
    let mut r = MmuRegs { ttbr0: 0x1234 << 48, ttbr1: 0xabcd << 48, ..MmuRegs::default() };
    assert_eq!(r.asid(), 0x34);
    r.tcr |= tcr::AS;
    assert_eq!(r.asid(), 0x1234);
    r.tcr |= tcr::A1;
    assert_eq!(r.asid(), 0xabcd);
    r.tcr &= !tcr::AS;
    assert_eq!(r.asid(), 0xcd);

    let mut e = Env::new();
    e.map(0x1000, 0x1000, 3, NORMAL | NG);
    e.mmu.regs.tcr |= tcr::AS | tcr::A1;
    e.mmu.regs.ttbr1 |= 0x77 << 48;
    e.mmu.regs.ttbr0 |= 0x11 << 48;
    let t = e.tr(0x1000, Access::Read, 1).unwrap();
    assert_eq!((t.ng, t.asid), (true, 0x77));
}

#[test]
fn tlb_non_globale_per_asid() {
    let mut e = Env::new();
    let root = e.root0;
    e.mmu.regs.ttbr0 = root | 1 << 48;
    let slot = e.map(0x1000, 0xa000, 3, NORMAL | NG);
    assert_eq!(e.pa(0x1000), 0xa000);
    // Tabella cambiata senza TLBI: con lo stesso ASID resta la voce vecchia.
    e.wr(slot, 0xb000 | NORMAL | NG | PAGE);
    assert_eq!(e.pa(0x1000), 0xa000);
    assert_eq!(e.walk(0x1000, Access::Read, 1).unwrap().pa, 0xb000);
    // Altro ASID: la voce non vale, nuovo walk.
    e.mmu.regs.ttbr0 = root | 2 << 48;
    assert_eq!(e.pa(0x1000), 0xb000);
    // Di nuovo ASID 1: la voce dell'ASID 2 ha preso lo stesso slot
    // (corrispondenza diretta), quindi un nuovo walk vede la tabella attuale.
    e.mmu.regs.ttbr0 = root | 1 << 48;
    assert_eq!(e.pa(0x1000), 0xb000);
}

#[test]
fn tlb_globale_vale_per_ogni_asid() {
    let mut e = Env::new();
    let root = e.root0;
    e.mmu.regs.ttbr0 = root | 1 << 48;
    let slot = e.map(0x1000, 0xa000, 3, NORMAL);
    assert_eq!(e.pa(0x1000), 0xa000);
    e.wr(slot, 0xb000 | NORMAL | PAGE);
    e.mmu.regs.ttbr0 = root | 2 << 48;
    assert_eq!(e.pa(0x1000), 0xa000);
    // ASIDE1 non tocca le voci globali.
    e.mmu.tlbi(TlbiOp::Aside1, tlbi_xt(0, 2));
    assert_eq!(e.pa(0x1000), 0xa000);
    e.mmu.tlbi(TlbiOp::Aside1, tlbi_xt(0, 1));
    assert_eq!(e.pa(0x1000), 0xa000);
    // VAE1 con un ASID qualunque toglie una voce globale.
    e.mmu.tlbi(TlbiOp::Vae1, tlbi_xt(0x1000, 9));
    assert_eq!(e.pa(0x1000), 0xb000);
}

#[test]
fn tlbi_per_va_e_asid() {
    let mut e = Env::new();
    let root = e.root0;
    e.mmu.regs.ttbr0 = root | 1 << 48;
    let s1 = e.map(0x1000, 0xa000, 3, NORMAL | NG);
    let s2 = e.map(0x2000, 0xc000, 3, NORMAL | NG);
    let rimappa = |e: &mut Env| {
        e.wr(s1, 0xb000 | NORMAL | NG | PAGE);
        e.wr(s2, 0xd000 | NORMAL | NG | PAGE);
    };
    let carica = |e: &mut Env| {
        e.wr(s1, 0xa000 | NORMAL | NG | PAGE);
        e.wr(s2, 0xc000 | NORMAL | NG | PAGE);
        e.mmu.tlbi(TlbiOp::Vmalle1, 0);
        assert_eq!((e.pa(0x1000), e.pa(0x2000)), (0xa000, 0xc000));
    };
    for op in [TlbiOp::Vae1, TlbiOp::Vale1, TlbiOp::Vae1is, TlbiOp::Vale1is] {
        carica(&mut e);
        rimappa(&mut e);
        // ASID diverso: nessun effetto su una voce non globale.
        e.mmu.tlbi(op, tlbi_xt(0x1000, 2));
        assert_eq!(e.pa(0x1000), 0xa000, "{op:?}");
        e.mmu.tlbi(op, tlbi_xt(0x1000, 1));
        assert_eq!((e.pa(0x1000), e.pa(0x2000)), (0xb000, 0xc000), "{op:?}");
    }
    for op in [TlbiOp::Vaae1, TlbiOp::Vaale1, TlbiOp::Vaae1is, TlbiOp::Vaale1is] {
        carica(&mut e);
        rimappa(&mut e);
        e.mmu.tlbi(op, tlbi_xt(0x2000, 7));
        assert_eq!((e.pa(0x1000), e.pa(0x2000)), (0xa000, 0xd000), "{op:?}");
    }
    for op in [TlbiOp::Aside1, TlbiOp::Aside1is] {
        carica(&mut e);
        rimappa(&mut e);
        e.mmu.tlbi(op, tlbi_xt(0, 2));
        assert_eq!(e.pa(0x1000), 0xa000);
        e.mmu.tlbi(op, tlbi_xt(0, 1));
        assert_eq!((e.pa(0x1000), e.pa(0x2000)), (0xb000, 0xd000), "{op:?}");
    }
    for op in [TlbiOp::Vmalle1, TlbiOp::Vmalle1is] {
        carica(&mut e);
        rimappa(&mut e);
        assert_eq!(e.mmu.tlb().len(), 2);
        e.mmu.tlbi(op, 0);
        assert!(e.mmu.tlb().is_empty());
        assert_eq!((e.pa(0x1000), e.pa(0x2000)), (0xb000, 0xd000), "{op:?}");
    }
}

#[test]
fn tlbi_dentro_un_blocco_toglie_tutto_il_blocco() {
    let mut e = Env::new();
    let va = 0x4000_0000;
    let slot = e.map(va, 0x20_0000, 2, NORMAL);
    assert_eq!(e.pa(va + 0x1000), 0x20_1000);
    assert_eq!(e.pa(va + 0x10_0000), 0x30_0000);
    assert_eq!(e.mmu.tlb().len(), 2);
    e.wr(slot, 0x80_0000 | NORMAL | BLOCK);
    // Pagina del blocco mai usata: invalida comunque le voci del blocco.
    e.mmu.tlbi(TlbiOp::Vale1, tlbi_xt(va + 0x5000, 0));
    assert!(e.mmu.tlb().is_empty());
    assert_eq!(e.pa(va + 0x1000), 0x80_1000);
    // Una VA fuori dal blocco non lo tocca.
    e.mmu.tlbi(TlbiOp::Vaae1, tlbi_xt(va + 0x20_0000, 0));
    assert_eq!(e.mmu.tlb().len(), 1);
}

#[test]
fn tlbi_nella_meta_alta() {
    let mut e = Env::new();
    let va = 0xffff_8000_0000_1000;
    let slot = e.map(va, 0xa000, 3, NORMAL);
    assert_eq!(e.pa(va), 0xa000);
    e.wr(slot, 0xb000 | NORMAL | PAGE);
    // Stessi bit 47:12 ma metà bassa (bit 55 = 0): non è la stessa voce.
    e.mmu.tlbi(TlbiOp::Vaae1, tlbi_xt(va & 0xffff_ffff_ffff, 0));
    assert_eq!(e.pa(va), 0xa000);
    e.mmu.tlbi(TlbiOp::Vaae1, tlbi_xt(va, 0));
    assert_eq!(e.pa(va), 0xb000);
}

#[test]
fn epd_blocca_solo_i_walk() {
    let mut e = Env::new();
    e.map(0x1000, 0xa000, 3, NORMAL);
    e.map(0x2000, 0xb000, 3, NORMAL);
    assert_eq!(e.pa(0x1000), 0xa000);
    e.mmu.regs.tcr |= tcr::EPD0;
    assert_eq!(e.pa(0x1000), 0xa000, "voce già nel TLB");
    assert_eq!(e.tr(0x2000, Access::Read, 1).unwrap_err().kind, FaultKind::Translation(0));
    e.mmu.regs.tcr = TCR | tcr::EPD1;
    assert_eq!(e.pa(0x2000), 0xb000);
    assert_eq!(e.kind(0xffff_0000_0000_0000, Access::Read, 1), FaultKind::Translation(0));
}

#[test]
fn permessi_controllati_anche_con_voce_nel_tlb() {
    let mut e = Env::new();
    e.map(0x1000, 0xa000, 3, NORMAL | AP_RO_EL1);
    assert_eq!(e.pa(0x1000), 0xa000);
    assert_eq!(e.tr(0x1000, Access::Read, 0).unwrap_err().kind, FaultKind::Permission(3));
    assert_eq!(e.tr(0x1000, Access::Write, 1).unwrap_err().kind, FaultKind::Permission(3));
    // WXN si legge adesso, non dal momento del walk.
    e.map(0x2000, 0xb000, 3, NORMAL | AP_RW_EL1);
    assert!(e.tr(0x2000, Access::Fetch, 1).is_ok());
    e.mmu.regs.sctlr |= sctlr::WXN;
    assert!(e.tr(0x2000, Access::Fetch, 1).is_err());
}

#[test]
fn tlbi_da_campi_di_sys() {
    // Codifiche prodotte da tools/a64asm.sh (SYS #op1, Cn, Cm, #op2, Xt).
    let casi = [
        (0xd508831f_u32, TlbiOp::Vmalle1is), // tlbi vmalle1is
        (0xd5088320, TlbiOp::Vae1is),        // tlbi vae1is, x0
        (0xd5088340, TlbiOp::Aside1is),      // tlbi aside1is, x0
        (0xd5088360, TlbiOp::Vaae1is),       // tlbi vaae1is, x0
        (0xd50883a0, TlbiOp::Vale1is),       // tlbi vale1is, x0
        (0xd50883e0, TlbiOp::Vaale1is),      // tlbi vaale1is, x0
        (0xd508871f, TlbiOp::Vmalle1),       // tlbi vmalle1
        (0xd5088720, TlbiOp::Vae1),          // tlbi vae1, x0
        (0xd5088740, TlbiOp::Aside1),        // tlbi aside1, x0
        (0xd5088760, TlbiOp::Vaae1),         // tlbi vaae1, x0
        (0xd50887a0, TlbiOp::Vale1),         // tlbi vale1, x0
        (0xd50887e0, TlbiOp::Vaale1),        // tlbi vaale1, x0
    ];
    for (raw, op) in casi {
        let f = |lo: u32, n: u32| raw >> lo & ((1 << n) - 1);
        assert_eq!(TlbiOp::from_sys(f(16, 3), f(12, 4), f(8, 4), f(5, 3)), Some(op), "{raw:#x}");
        assert_eq!(op.is_broadcast(), f(8, 4) == 3);
    }
    assert_eq!(TlbiOp::from_sys(0, 8, 7, 4), None);
    assert_eq!(TlbiOp::from_sys(4, 8, 7, 0), None);
    assert_eq!(TlbiOp::from_sys(0, 7, 7, 0), None);
}

// --- Adattatore Memory ---

#[test]
fn adattatore_lettura_scrittura_fetch() {
    let mut e = Env::new();
    e.map(0x1000_0000, 0x20_0000, 3, NORMAL | AP_RW_ALL);
    e.map(0x1000_1000, 0x50_0000, 3, NORMAL | AP_RW_ALL);
    e.map(0x1000_2000, 0x60_0000, 3, NORMAL | AP_RO_ALL);
    let mut m = e.vm(0);
    m.write(0x1000_0ffc, &0x1122_3344_5566_7788_u64.to_le_bytes()).unwrap();
    let mut b = [0u8; 8];
    m.read(0x1000_0ffc, &mut b).unwrap();
    assert_eq!(u64::from_le_bytes(b), 0x1122_3344_5566_7788);
    assert_eq!(m.fetch(0x1000_0ffc).unwrap(), 0x5566_7788);
    // I due pezzi sono andati in pagine fisiche diverse.
    assert_eq!(e.ram.read_u64(0x20_0ff8).unwrap() >> 32, 0x5566_7788);
    assert_eq!(e.ram.read_u64(0x50_0000).unwrap() & 0xffff_ffff, 0x1122_3344);
    assert_eq!(e.vm(0).last_fault(), None);
}

#[test]
fn adattatore_niente_scritture_parziali() {
    let mut e = Env::new();
    e.map(0x1000_0000, 0x20_0000, 3, NORMAL | AP_RW_ALL);
    e.map(0x1000_1000, 0x50_0000, 3, NORMAL | AP_RO_ALL);
    let mut m = e.vm(0);
    let err = m.write(0x1000_0ffc, &[0xaa; 8]).unwrap_err();
    assert_eq!(err, MemFault { addr: 0x1000_1000, access: Access::Write });
    let f = m.last_fault().unwrap();
    assert_eq!((f.kind, f.far()), (FaultKind::Permission(3), 0x1000_1000));
    assert_eq!(f.esr(0), Some(0x9200_004f));
    assert_eq!(e.ram.read_u64(0x20_0ff8).unwrap(), 0, "primo pezzo non scritto");
    // AP = 11 è sola lettura anche a EL1.
    assert!(e.vm(1).write(0x1000_0ffc, &[0xaa; 8]).is_err());
    // Pagina successiva assente: translation fault sul primo byte.
    let mut b = [0u8; 16];
    let err = e.vm(1).read(0x1000_1ff8, &mut b).unwrap_err();
    assert_eq!(err, MemFault { addr: 0x1000_2000, access: Access::Read });
    assert_eq!(e.mmu.last_fault().unwrap().kind, FaultKind::Translation(3));
}

#[test]
fn adattatore_fetch_e_abort_esterni() {
    let mut e = Env::new();
    e.map(0x1000, 0x3000, 3, NORMAL | AP_RO_ALL);
    e.map(0x2000, 0x4000, 3, NORMAL | AP_RO_ALL | UXN);
    e.map(0x5000, LIMIT, 3, DEVICE | AP_RW_ALL);
    e.ram.write(0x3010, &0xd503201f_u32.to_le_bytes()).unwrap();
    let mut m = e.vm(0);
    assert_eq!(m.fetch(0x1010).unwrap(), 0xd503201f);
    let err = m.fetch(0x2000).unwrap_err();
    assert_eq!(err, MemFault { addr: 0x2000, access: Access::Fetch });
    assert_eq!(m.last_fault().unwrap().esr(0), Some(0x8200_000f));
    // Indirizzo fisico dove non risponde nessuno: abort esterno sincrono.
    let err = m.write(0x5008, &[1]).unwrap_err();
    assert_eq!(err, MemFault { addr: 0x5008, access: Access::Write });
    let f = m.last_fault().unwrap();
    assert_eq!(f.kind, FaultKind::External(BusError::Decode));
    assert_eq!(f.esr(0), Some(0x9200_0050));
}

#[test]
fn adattatore_mmu_spenta() {
    let mut e = Env::new();
    e.mmu.regs.sctlr = 0;
    let mut m = e.vm(1);
    m.write(0x1234, &[1, 2, 3]).unwrap();
    assert_eq!(e.ram.read_u64(0x1230).unwrap() >> 32, 0x03_0201);
    let err = e.vm(1).read(1 << 40, &mut [0u8; 1]).unwrap_err();
    assert_eq!(err.addr, 1 << 40);
    assert_eq!(e.mmu.last_fault().unwrap().kind, FaultKind::AddressSize(0));
}

// --- Cache delle traduzioni recenti (`translate_pa`) ---

impl Env {
    fn fast(&mut self, va: u64, access: Access, el: u8, aligned: bool) -> Result<u64, Fault> {
        self.mmu.translate_pa(&mut self.ram, va, access, el, aligned)
    }
}

#[test]
fn cache_recente_segue_tlbi_ttbr_e_asid() {
    const VA: u64 = 0x40_0000_5000;
    let mut e = Env::new();
    let desc = e.map(VA, 0x20_a000, 3, NORMAL | AP_RW_EL1 | NG);
    assert_eq!(e.fast(VA + 8, Access::Read, 1, true), Ok(0x20_a008));
    assert_eq!(e.fast(VA + 16, Access::Read, 1, true), Ok(0x20_a010));
    assert_eq!(e.mmu.recent_hits, 1);
    // Descrittore cambiato senza TLBI: resta la voce del TLB.
    e.wr(desc, 0x20_b000 | NORMAL | AP_RW_EL1 | NG | PAGE);
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_a000));
    // TLBI VAE1 con l'ASID giusto: si rilegge la tabella.
    e.mmu.tlbi(TlbiOp::Vae1, tlbi_xt(VA, 0));
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_b000));
    // Nuovo ASID in TTBR0: la voce non globale dell'ASID 0 non vale più.
    let root = e.root0;
    e.wr(desc, 0x20_c000 | NORMAL | AP_RW_EL1 | NG | PAGE);
    e.mmu.regs.ttbr0 = root | 1 << 48;
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_c000));
    // Un'altra tabella con lo stesso ASID: con una voce globale nel TLB
    // vale ancora la vecchia traduzione, dopo VMALLE1 la nuova.
    let other = e.alloc();
    e.root0 = other;
    e.map(VA, 0x20_d000, 3, NORMAL | AP_RW_EL1);
    e.root0 = root;
    e.wr(desc, 0x20_e000 | NORMAL | AP_RW_EL1 | PAGE);
    e.mmu.tlb_mut().flush_all();
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_e000));
    e.mmu.regs.ttbr0 = other | 1 << 48;
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_e000), "voce globale nel TLB");
    e.mmu.tlbi(TlbiOp::Vmalle1, 0);
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x20_d000));
}

#[test]
fn cache_recente_rispetta_permessi_device_e_mair() {
    const VA: u64 = 0x40_0000_7000;
    const DEV: u64 = 0x40_0000_8000;
    let mut e = Env::new();
    e.map(VA, 0x30_0000, 3, NORMAL | AP_RW_EL1 | UXN);
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(0x30_0000));
    // Stessa pagina, altro livello o altro accesso: il fault è quello del
    // percorso completo.
    assert_eq!(e.fast(VA, Access::Read, 0, true).unwrap_err().kind, FaultKind::Permission(3));
    assert_eq!(e.fast(VA, Access::Fetch, 0, true).unwrap_err().kind, FaultKind::Permission(3));
    assert_eq!(e.fast(VA, Access::Fetch, 1, true), Ok(0x30_0000));
    // WXN: la pagina scrivibile non è più eseguibile.
    e.mmu.regs.sctlr |= sctlr::WXN;
    assert_eq!(e.fast(VA, Access::Fetch, 1, true).unwrap_err().kind, FaultKind::Permission(3));
    e.mmu.regs.sctlr &= !sctlr::WXN;
    // Device: disallineato fa fault anche dopo un accesso allineato.
    e.map(DEV, 0x30_1000, 3, DEVICE | AP_RW_EL1 | UXN | PXN);
    assert_eq!(e.fast(DEV, Access::Read, 1, true), Ok(0x30_1000));
    assert_eq!(e.fast(DEV + 2, Access::Read, 1, false).unwrap_err().kind, FaultKind::Alignment);
    // MAIR cambiato: la pagina Normal diventa Device.
    assert_eq!(e.fast(VA + 1, Access::Read, 1, false), Ok(0x30_0001));
    e.mmu.regs.mair = MAIR & !0xff00;
    assert_eq!(e.fast(VA + 1, Access::Read, 1, false).unwrap_err().kind, FaultKind::Alignment);
    // MMU spenta: identità.
    e.mmu.regs.sctlr = 0;
    assert_eq!(e.fast(VA, Access::Read, 1, true), Ok(VA));
}

#[test]
fn cache_recente_dopo_lo_sfratto_dal_tlb() {
    // Due pagine sullo stesso slot del TLB (distanza 2 MiB): la seconda
    // sfratta la prima, che poi si rilegge dalla tabella cambiata.
    const A: u64 = 0x40_0000_3000;
    const B: u64 = A + (512 << 12);
    let mut e = Env::new();
    let da = e.map(A, 0x21_0000, 3, NORMAL | AP_RW_EL1);
    e.map(B, 0x22_0000, 3, NORMAL | AP_RW_EL1);
    assert_eq!(e.fast(A, Access::Read, 1, true), Ok(0x21_0000));
    e.wr(da, 0x23_0000 | NORMAL | AP_RW_EL1 | PAGE);
    assert_eq!(e.fast(A, Access::Read, 1, true), Ok(0x21_0000), "ancora nel TLB");
    assert_eq!(e.fast(B, Access::Read, 1, true), Ok(0x22_0000));
    assert_eq!(e.fast(A, Access::Read, 1, true), Ok(0x23_0000), "sfrattata: nuovo walk");
}

/// Generatore deterministico (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

#[test]
fn cache_recente_equivale_al_percorso_completo() {
    let mut hits = 0;
    for seed in [0x5645_5452_4f4d_4d55, 1, 0xdead_beef, 0x0123_4567_89ab_cdef] {
        hits += equivalenza_con_seme(seed, 30_000);
    }
    assert!(hits > 5_000, "la cache recente non è stata usata: {hits} colpi");
}

/// Due MMU sulla stessa memoria e con le stesse operazioni: una traduce con
/// `translate_pa` (cache recente), l'altra con `translate_checked`. Risultati
/// e TLB devono coincidere passo per passo, anche quando le tabelle cambiano
/// senza TLBI. Restituisce i colpi della cache.
fn equivalenza_con_seme(seed: u64, steps: usize) -> u64 {
    const LOW: u64 = 0x40_0000_0000;
    const BLOCK: u64 = LOW + 0x40_0000;
    const HIGH: u64 = 0xffff_8000_0000_0000;
    let mut e = Env::new();
    let root_a = e.root0;
    let root_b = e.alloc();
    let mut pages: Vec<u64> = (0..6).map(|i| LOW + i * 0x1000).collect();
    // Stessi slot del TLB delle prime tre (2 MiB più avanti).
    pages.extend((0..3).map(|i| LOW + 0x20_0000 + i * 0x1000));
    pages.extend((0..3).map(|i| HIGH + i * 0x1000));
    let attrs = [
        NORMAL | AP_RW_ALL,
        NORMAL | AP_RW_EL1 | UXN,
        NORMAL | AP_RO_ALL | PXN,
        NORMAL | AP_RO_EL1 | NG,
        NORMAL | AP_RW_ALL | NG | UXN | PXN,
        DEVICE | AP_RW_EL1 | UXN | PXN,
        NORMAL & !AF | AP_RW_EL1,
    ];
    let mut slots = Vec::new();
    for (i, &va) in pages.iter().enumerate() {
        slots.push(e.map(va, 0x20_0000 + (i as u64) * 0x1000, 3, attrs[i % attrs.len()]));
        e.root0 = root_b;
        e.map(va, 0x28_0000 + (i as u64) * 0x1000, 3, attrs[(i + 3) % attrs.len()]);
        e.root0 = root_a;
    }
    let block_desc = e.map(BLOCK, 0x4000_0000, 2, NORMAL | AP_RW_ALL);
    let mut vas = pages.clone();
    vas.extend([BLOCK, BLOCK + 0x1000, BLOCK + 0x1f_f000, LOW + 0x100_0000]);

    let mut slow = e.mmu.clone();
    let mut rng = Rng(seed);
    let accesses = [Access::Read, Access::Read, Access::Read, Access::Write, Access::Fetch];
    for step in 0..steps {
        match rng.below(100) {
            0..=95 => {
                let mut va = rng.pick(&vas) + rng.below(0x1000);
                if rng.below(8) == 0 && va >> 55 & 1 == 0 {
                    va |= 0x5a << 56; // tag: vale solo con TBI0
                }
                let access = rng.pick(&accesses);
                let el = u8::from(rng.below(4) != 0);
                let aligned = rng.below(8) != 0;
                let got = if rng.below(2) == 0 {
                    e.fast(va, access, el, aligned)
                } else {
                    let regs = e.mmu.regs;
                    e.mmu.translate_pa_with(&regs, &mut e.ram, va, access, el, aligned)
                };
                let want = slow.translate_checked(&mut e.ram, va, access, el, aligned).map(|t| t.pa);
                assert_eq!(
                    got, want,
                    "seme {seed:#x} passo {step}: {va:#x} {access:?} EL{el} allineato {aligned}"
                );
            }
            96..=97 => {
                // Descrittore cambiato senza TLBI (o reso non valido).
                let k = rng.below(slots.len() as u64) as usize;
                let d = if rng.below(6) == 0 {
                    0
                } else {
                    (0x20_0000 + rng.below(32) * 0x1000) | rng.pick(&attrs) | PAGE
                };
                e.wr(slots[k], d);
                if rng.below(4) == 0 {
                    let pa = 0x4000_0000 + rng.below(4) * 0x20_0000;
                    e.wr(block_desc, pa | rng.pick(&attrs) | 0b01);
                }
            }
            98 => {
                use TlbiOp::*;
                let op = rng.pick(&[Vmalle1, Vae1, Aside1, Vaae1, Vale1, Vaale1, Vae1is, Aside1is]);
                let xt = tlbi_xt(rng.pick(&vas), rng.pick(&[0, 1, 0x101]));
                e.mmu.tlbi(op, xt);
                slow.tlbi(op, xt);
            }
            _ => {
                let mut r = e.mmu.regs;
                match rng.below(6) {
                    0 => r.ttbr0 = rng.pick(&[root_a, root_b]) | rng.pick(&[0u64, 1, 0x101]) << 48,
                    1 => r.ttbr1 = (r.ttbr1 & ((1 << 48) - 1)) | rng.pick(&[0u64, 1, 0x101]) << 48,
                    2 => r.tcr ^= rng.pick(&[tcr::TBI0, tcr::TBI1, tcr::AS, tcr::A1]),
                    3 => r.mair ^= 0xfb << 8, // AttrIndx 1: 0xff Normal ↔ 0x04 Device
                    4 => r.sctlr ^= rng.pick(&[sctlr::M, sctlr::WXN, sctlr::WXN]),
                    _ => {
                        e.mmu.tlb_mut().flush_all();
                        slow.tlb_mut().flush_all();
                    }
                }
                e.mmu.regs = r;
                slow.regs = r;
            }
        }
        assert_eq!(e.mmu.tlb().len(), slow.tlb().len(), "seme {seed:#x} passo {step}: TLB diversi");
    }
    e.mmu.recent_hits
}

/// Il contatore delle invalidazioni cresce a ogni TLBI e svuotamento,
/// anche senza voci da togliere: chi copia le traduzioni fuori dal TLB (la
/// TLB software del JIT) lo usa per sapere quando scartarle.
#[test]
fn contatore_delle_invalidazioni() {
    let mut e = Env::new();
    e.map(0x1000, 0xa000, 3, NORMAL);
    let n = e.mmu.tlb().flushes();
    e.mmu.tlbi(TlbiOp::Vae1, tlbi_xt(0x5000, 1));
    assert_eq!(e.mmu.tlb().flushes(), n + 1);
    e.mmu.tlb_mut().flush_all();
    assert_eq!(e.mmu.tlb().flushes(), n + 2);
    assert_eq!(e.pa(0x1000), 0xa000);
    assert_eq!(e.mmu.tlb().flushes(), n + 2, "un walk non è un'invalidazione");
}
