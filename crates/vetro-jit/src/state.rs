//! `JitState`: lo stato della CPU nella memoria lineare condivisa con i
//! blocchi tradotti (docs/specs/jit.md).

use vetro_cpu::Cpu;
use vetro_cpu::state::Monitor;
use vetro_cpu::sys::{id, sctlr};

/// Stato letto e scritto dai blocchi tradotti. Il layout è parte dell'ABI:
/// gli offset sono quelli di [`off`] e della spec.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitState {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    /// Istruzioni eseguite (lo stesso orologio dell'interprete).
    pub steps: u64,
    /// Flag N, Z, C, V nei bit 31:28, come `Cpu::nzcv`.
    pub nzcv: u32,
    /// 0 all'ingresso del blocco (lo azzera l'host), [`crate::FAULT`] dopo
    /// un `ld`/`st` fallito, [`crate::STOP`] dopo uno `st` su codice sorvegliato.
    pub exit_detail: u32,
    /// Livello di eccezione (0 in modalità utente).
    pub el: u32,
    /// Contesto del concatenamento (modalità sistema): una voce della
    /// cache dei salti vale solo se ha questo stesso valore.
    pub ctx: u32,
    /// Passi massimi della corsa concatenata: il dispatcher non entra in un
    /// blocco che potrebbe superarli.
    pub limit: u64,
    // Modalità sistema: registri che i blocchi leggono o scrivono con
    // MRS/MSR, e il monitor esclusivo (LDXR/STXR).
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub tpidr_el1: u64,
    /// SP_EL0 quando non è lo SP in uso (EL1 con SPSel = 1).
    pub sp_el0: u64,
    /// TCR_EL1 (solo lettura per i blocchi).
    pub tcr: u64,
    /// DCZID_EL0 per il livello corrente (solo lettura).
    pub dczid: u64,
    pub mon_addr: u64,
    /// Valore del monitor (128 bit: basso e alto).
    pub mon_lo: u64,
    pub mon_hi: u64,
    /// 1 se il monitor è attivo.
    pub mon_valid: u32,
    /// Byte dell'accesso esclusivo che l'ha attivato.
    pub mon_bytes: u32,
    pub _pad: u64,
}

/// Offset dei campi (byte dall'inizio della struttura).
pub mod off {
    pub const X: u32 = 0;
    pub const SP: u32 = 248;
    pub const PC: u32 = 256;
    pub const STEPS: u32 = 264;
    pub const NZCV: u32 = 272;
    pub const EXIT_DETAIL: u32 = 276;
    pub const EL: u32 = 280;
    pub const CTX: u32 = 284;
    pub const LIMIT: u32 = 288;
    pub const TPIDR_EL0: u32 = 296;
    pub const TPIDRRO_EL0: u32 = 304;
    pub const TPIDR_EL1: u32 = 312;
    pub const SP_EL0: u32 = 320;
    pub const TCR: u32 = 328;
    pub const DCZID: u32 = 336;
    pub const MON_ADDR: u32 = 344;
    pub const MON_LO: u32 = 352;
    pub const MON_HI: u32 = 360;
    pub const MON_VALID: u32 = 368;
    pub const MON_BYTES: u32 = 372;
    /// Dimensione totale.
    pub const SIZE: usize = 384;
}

/// Area del JIT in modalità sistema, a partire da `JitState` (offset dal
/// suo inizio): cache dei salti del dispatcher e TLB software dei blocchi.
pub mod area {
    /// Cache dei salti: voci da 16 byte `{pc: u64, ctx: u32, w: u32}` con
    /// `w = slot << 8 | passi massimi del blocco`, indice `(pc >> 2) & (JC_ENTRIES - 1)`.
    pub const JC: u32 = 512;
    pub const JC_ENTRIES: u32 = 8192;
    /// TLB software: 4 tabelle (EL0 lettura, EL0 scrittura, EL1 lettura,
    /// EL1 scrittura) di `TLB_ENTRIES` voci da 16 byte `{tag: u64, addend:
    /// u64}`, indice `(va >> 12) & (TLB_ENTRIES - 1)`. `tag` è la pagina
    /// virtuale (VA con i 12 bit bassi a zero); l'indirizzo nella memoria
    /// del motore è `va + addend` (troncato a 32 bit).
    pub const TLB: u32 = JC + JC_ENTRIES * 16;
    pub const TLB_ENTRIES: u32 = 512;
    pub const TLB_SIZE: u32 = TLB_ENTRIES * 16;
    /// Tag che non corrisponde a nessun accesso allineato (bit 11 a uno).
    pub const TLB_INVALID: u64 = 0x800;
    /// Byte totali dell'area.
    pub const SIZE: u32 = TLB + 4 * TLB_SIZE;

    /// Offset della tabella per il livello `el` e il tipo di accesso.
    pub const fn tlb(el: u8, write: bool) -> u32 {
        TLB + (el as u32 * 2 + write as u32) * TLB_SIZE
    }
}

impl JitState {
    pub fn from_cpu(cpu: &Cpu) -> Self {
        JitState { x: cpu.x, sp: cpu.sp, pc: cpu.pc, nzcv: cpu.nzcv, ..Default::default() }
    }

    /// Ricopia nella `Cpu` i campi che i blocchi possono cambiare.
    pub fn to_cpu(&self, cpu: &mut Cpu) {
        cpu.x = self.x;
        cpu.sp = self.sp;
        cpu.pc = self.pc;
        cpu.nzcv = self.nzcv;
    }

    /// Come [`from_cpu`](Self::from_cpu), con i campi della modalità sistema.
    pub fn from_cpu_sys(cpu: &Cpu) -> Self {
        let s = &cpu.sys;
        let dzp = s.el == 0 && s.sctlr_el1 & sctlr::DZE == 0;
        let (mon_valid, mon_bytes, mon_addr, mon_lo, mon_hi) = match cpu.monitor {
            Some(m) => (1, m.bytes, m.addr, m.value as u64, (m.value >> 64) as u64),
            None => (0, 0, 0, 0, 0),
        };
        JitState {
            el: s.el as u32,
            tpidr_el0: cpu.tpidr_el0,
            tpidrro_el0: cpu.tpidrro_el0,
            tpidr_el1: s.tpidr_el1,
            sp_el0: s.sp_el[0],
            tcr: s.tcr_el1,
            dczid: id::DCZID_BS | (dzp as u64) << 4,
            mon_addr,
            mon_lo,
            mon_hi,
            mon_valid,
            mon_bytes,
            ..Self::from_cpu(cpu)
        }
    }

    /// Come [`to_cpu`](Self::to_cpu), con i campi della modalità sistema
    /// che i blocchi possono cambiare. SP_EL0 torna nella `Cpu` solo se non
    /// è lo SP in uso (che è `sp`).
    pub fn to_cpu_sys(&self, cpu: &mut Cpu) {
        self.to_cpu(cpu);
        cpu.tpidr_el0 = self.tpidr_el0;
        cpu.tpidrro_el0 = self.tpidrro_el0;
        cpu.sys.tpidr_el1 = self.tpidr_el1;
        if cpu.sys.el == 1 && cpu.sys.spsel {
            cpu.sys.sp_el[0] = self.sp_el0;
        }
        cpu.monitor = (self.mon_valid != 0).then_some(Monitor {
            addr: self.mon_addr,
            bytes: self.mon_bytes,
            value: self.mon_lo as u128 | (self.mon_hi as u128) << 64,
        });
    }

    /// Scrive la struttura in `mem` a partire da `at` (little-endian, come
    /// la memoria WASM).
    pub fn store(&self, mem: &mut [u8], at: usize) {
        let m = &mut mem[at..at + off::SIZE];
        for (i, v) in self.x.iter().enumerate() {
            m[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        m[248..256].copy_from_slice(&self.sp.to_le_bytes());
        m[256..264].copy_from_slice(&self.pc.to_le_bytes());
        m[264..272].copy_from_slice(&self.steps.to_le_bytes());
        m[272..276].copy_from_slice(&self.nzcv.to_le_bytes());
        m[276..280].copy_from_slice(&self.exit_detail.to_le_bytes());
        m[280..284].copy_from_slice(&self.el.to_le_bytes());
        m[284..288].copy_from_slice(&self.ctx.to_le_bytes());
        m[288..296].copy_from_slice(&self.limit.to_le_bytes());
        let q = [
            self.tpidr_el0,
            self.tpidrro_el0,
            self.tpidr_el1,
            self.sp_el0,
            self.tcr,
            self.dczid,
            self.mon_addr,
            self.mon_lo,
            self.mon_hi,
        ];
        for (i, v) in q.iter().enumerate() {
            m[296 + 8 * i..304 + 8 * i].copy_from_slice(&v.to_le_bytes());
        }
        m[368..372].copy_from_slice(&self.mon_valid.to_le_bytes());
        m[372..376].copy_from_slice(&self.mon_bytes.to_le_bytes());
        m[376..384].fill(0);
    }

    /// Legge la struttura da `mem` a partire da `at`.
    pub fn load(mem: &[u8], at: usize) -> Self {
        let m = &mem[at..at + off::SIZE];
        let q = |o: usize| u64::from_le_bytes(m[o..o + 8].try_into().unwrap());
        let d = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
        let mut x = [0u64; 31];
        for (i, r) in x.iter_mut().enumerate() {
            *r = q(i * 8);
        }
        JitState {
            x,
            sp: q(248),
            pc: q(256),
            steps: q(264),
            nzcv: d(272),
            exit_detail: d(276),
            el: d(280),
            ctx: d(284),
            limit: q(288),
            tpidr_el0: q(296),
            tpidrro_el0: q(304),
            tpidr_el1: q(312),
            sp_el0: q(320),
            tcr: q(328),
            dczid: q(336),
            mon_addr: q(344),
            mon_lo: q(352),
            mon_hi: q(360),
            mon_valid: d(368),
            mon_bytes: d(372),
            _pad: 0,
        }
    }
}

/// Legge un campo u64 di `JitState` da `mem`.
#[inline]
pub fn read_u64(mem: &[u8], at: usize, field: u32) -> u64 {
    let o = at + field as usize;
    u64::from_le_bytes(mem[o..o + 8].try_into().unwrap())
}

/// Scrive un campo u64 di `JitState` in `mem`.
#[inline]
pub fn write_u64(mem: &mut [u8], at: usize, field: u32, v: u64) {
    let o = at + field as usize;
    mem[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Legge un campo u32 di `JitState` da `mem`.
#[inline]
pub fn read_u32(mem: &[u8], at: usize, field: u32) -> u32 {
    let o = at + field as usize;
    u32::from_le_bytes(mem[o..o + 4].try_into().unwrap())
}

/// Scrive un campo u32 di `JitState` in `mem`.
#[inline]
pub fn write_u32(mem: &mut [u8], at: usize, field: u32, v: u32) {
    let o = at + field as usize;
    mem[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn layout_matches_spec() {
        assert_eq!(offset_of!(JitState, x), off::X as usize);
        assert_eq!(offset_of!(JitState, sp), off::SP as usize);
        assert_eq!(offset_of!(JitState, pc), off::PC as usize);
        assert_eq!(offset_of!(JitState, steps), off::STEPS as usize);
        assert_eq!(offset_of!(JitState, nzcv), off::NZCV as usize);
        assert_eq!(offset_of!(JitState, exit_detail), off::EXIT_DETAIL as usize);
        assert_eq!(offset_of!(JitState, el), off::EL as usize);
        assert_eq!(offset_of!(JitState, ctx), off::CTX as usize);
        assert_eq!(offset_of!(JitState, limit), off::LIMIT as usize);
        assert_eq!(offset_of!(JitState, tpidr_el0), off::TPIDR_EL0 as usize);
        assert_eq!(offset_of!(JitState, tpidrro_el0), off::TPIDRRO_EL0 as usize);
        assert_eq!(offset_of!(JitState, tpidr_el1), off::TPIDR_EL1 as usize);
        assert_eq!(offset_of!(JitState, sp_el0), off::SP_EL0 as usize);
        assert_eq!(offset_of!(JitState, tcr), off::TCR as usize);
        assert_eq!(offset_of!(JitState, dczid), off::DCZID as usize);
        assert_eq!(offset_of!(JitState, mon_addr), off::MON_ADDR as usize);
        assert_eq!(offset_of!(JitState, mon_lo), off::MON_LO as usize);
        assert_eq!(offset_of!(JitState, mon_hi), off::MON_HI as usize);
        assert_eq!(offset_of!(JitState, mon_valid), off::MON_VALID as usize);
        assert_eq!(offset_of!(JitState, mon_bytes), off::MON_BYTES as usize);
        assert_eq!(size_of::<JitState>(), off::SIZE);
        assert_eq!(align_of::<JitState>(), 16);
    }

    #[test]
    fn store_load_roundtrip_and_raw_layout() {
        let mut s = JitState::default();
        for (i, r) in s.x.iter_mut().enumerate() {
            *r = 0x1111_0000_0000_0000 * (i as u64 % 15) + i as u64;
        }
        s.sp = 0xdead_beef;
        s.pc = 0x40_1000;
        s.steps = 7;
        s.nzcv = 0x6000_0000;
        s.exit_detail = 1;
        s.el = 1;
        s.ctx = 0x1234_5679;
        s.limit = 99;
        s.tpidr_el0 = 1;
        s.tpidrro_el0 = 2;
        s.tpidr_el1 = 3;
        s.sp_el0 = 4;
        s.tcr = 5;
        s.dczid = 6;
        s.mon_addr = 7;
        s.mon_lo = 8;
        s.mon_hi = 9;
        s.mon_valid = 1;
        s.mon_bytes = 16;
        let mut mem = vec![0xaau8; 16 + off::SIZE];
        s.store(&mut mem, 16);
        assert_eq!(JitState::load(&mem, 16), s);
        // Il formato in memoria coincide con la struttura #[repr(C)] su un
        // host little-endian.
        let raw: [u8; off::SIZE] = unsafe { core::mem::transmute(s) };
        #[cfg(target_endian = "little")]
        assert_eq!(&mem[16..16 + off::SIZE], &raw[..]);
        assert_eq!(read_u64(&mem, 16, off::PC), 0x40_1000);
        assert_eq!(read_u32(&mem, 16, off::NZCV), 0x6000_0000);
    }

    #[test]
    fn cpu_roundtrip() {
        let mut cpu = Cpu::new();
        cpu.x[3] = 5;
        cpu.sp = 0x100;
        cpu.pc = 0x2000;
        cpu.nzcv = 0x8000_0000;
        cpu.tpidr_el0 = 9;
        let s = JitState::from_cpu(&cpu);
        let mut back = Cpu::new();
        back.tpidr_el0 = 9;
        s.to_cpu(&mut back);
        assert_eq!(back, cpu);
    }
}
