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
    /// Blocco base d'ingresso della regione chiamata (lo scrive chi la
    /// chiama: il dispatcher dalla cache dei salti, o l'host).
    pub entry: u32,
    /// PSTATE.DAIF (bit 9:6, come `SysState::daif`).
    pub daif: u32,
    /// ELR_EL1 e SPSR_EL1 (MRS/MSR a EL1), ESR_EL1 e FAR_EL1 (solo MRS).
    pub elr_el1: u64,
    pub spsr_el1: u64,
    pub esr_el1: u64,
    pub far_el1: u64,
    /// 1 se `v` contiene i registri SIMD/FP della `Cpu` (li copia l'host
    /// alla prima regione che li usa, `env.vsync`, e li ricopia alla fine).
    pub v_valid: u32,
    /// Flag pigri (ADR 0024): se `fk` non è 0, NZCV è quello di
    /// un'istruzione di tipo `fk` con operandi `fa`, `fb` e risultato `fr`
    /// ([`lazy_nzcv`]), altrimenti è `nzcv`.
    pub fk: u32,
    /// FPCR (letto dalle regioni) e FPSR (flag cumulativi, scritti dalle
    /// regioni e da `env.simd`), come `Cpu::fpcr`/`Cpu::fpsr` (ADR 0026).
    pub fpcr: u32,
    pub fpsr: u32,
    /// V0..V31 (128 bit: metà bassa e alta), validi se `v_valid`.
    pub v: [[u64; 2]; 32],
    pub fa: u64,
    pub fb: u64,
    pub fr: u64,
    /// Modalità sistema (ADR 0026): istruzioni della macchina all'inizio
    /// della corsa del dispatcher (lo scrive l'host prima di ogni corsa):
    /// CNTPCT di un'istruzione è `counter(time_base + steps + indice)`.
    pub time_base: u64,
    /// CNTVOFF: CNTVCT = CNTPCT - `cntvoff`.
    pub cntvoff: u64,
    /// 1 se `time_base` e `cntvoff` valgono per questa corsa; altrimenti
    /// MRS CNTPCT/CNTVCT esce e lo fa l'interprete.
    pub time_ok: u32,
    pub _pad2: u32,
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
    pub const ENTRY: u32 = 376;
    pub const DAIF: u32 = 380;
    pub const ELR_EL1: u32 = 384;
    pub const SPSR_EL1: u32 = 392;
    pub const ESR_EL1: u32 = 400;
    pub const FAR_EL1: u32 = 408;
    pub const V_VALID: u32 = 416;
    pub const FK: u32 = 420;
    pub const FPCR: u32 = 424;
    pub const FPSR: u32 = 428;
    /// V0..V31, 16 byte ciascuno (metà bassa poi alta).
    pub const V: u32 = 432;
    pub const FA: u32 = 944;
    pub const FB: u32 = 952;
    pub const FR: u32 = 960;
    pub const TIME_BASE: u32 = 968;
    pub const CNTVOFF: u32 = 976;
    pub const TIME_OK: u32 = 984;
    /// Dimensione totale.
    pub const SIZE: usize = 992;
}

/// Area del JIT in modalità sistema, a partire da `JitState` (offset dal
/// suo inizio): cache dei salti del dispatcher e TLB software dei blocchi.
pub mod area {
    /// Cache dei salti: voci da 16 byte `{pc: u64, ctx: u32, w: u32}` con
    /// `w = slot << 8 | passi massimi del blocco`, indice `(pc >> 2) & (JC_ENTRIES - 1)`.
    pub const JC: u32 = 1024;
    pub const JC_ENTRIES: u32 = 8192;
    /// TLB software: 4 tabelle (EL0 lettura, EL0 scrittura, EL1 lettura,
    /// EL1 scrittura) di `TLB_ENTRIES` voci da 16 byte `{tag: u64, addend:
    /// u64}`, indice `(va >> 12) & (TLB_ENTRIES - 1)`. `tag` è la pagina
    /// virtuale (VA con i 12 bit bassi a zero); l'indirizzo nella memoria
    /// del motore è `va + addend` (troncato a 32 bit).
    pub const TLB: u32 = JC + JC_ENTRIES * 16;
    pub const TLB_ENTRIES: u32 = 512;
    pub const TLB_SIZE: u32 = TLB_ENTRIES * 16;
    /// Tag che non corrisponde a nessun accesso (bit 11 a uno).
    pub const TLB_INVALID: u64 = 0x800;
    /// Byte totali dell'area: 4 tabelle per gli accessi allineati e 4 per
    /// quelli non allineati ([`tlb_u`]).
    pub const SIZE: u32 = TLB + 8 * TLB_SIZE;

    /// Offset della tabella per il livello `el` e il tipo di accesso.
    pub const fn tlb(el: u8, write: bool) -> u32 {
        TLB + (el as u32 * 2 + write as u32) * TLB_SIZE
    }

    /// Come [`tlb`], per gli accessi non allineati dentro una pagina (ADR
    /// 0024): una voce c'è solo per pagine in cui un accesso non allineato è
    /// riuscito (memoria Normal, SCTLR_EL1.A a 0). Colpo se `tag == va &
    /// !0xfff` e l'accesso non sconfina nella pagina successiva.
    pub const fn tlb_u(el: u8, write: bool) -> u32 {
        TLB + (4 + el as u32 * 2 + write as u32) * TLB_SIZE
    }
}

impl JitState {
    pub fn from_cpu(cpu: &Cpu) -> Self {
        JitState {
            x: cpu.x,
            sp: cpu.sp,
            pc: cpu.pc,
            nzcv: cpu.nzcv,
            fpcr: cpu.fpcr,
            fpsr: cpu.fpsr,
            ..Default::default()
        }
    }

    /// Ricopia nella `Cpu` i campi che i blocchi possono cambiare (i
    /// registri SIMD/FP solo se `v_valid`).
    pub fn to_cpu(&self, cpu: &mut Cpu) {
        cpu.x = self.x;
        cpu.sp = self.sp;
        cpu.pc = self.pc;
        cpu.fpsr = self.fpsr;
        cpu.nzcv = lazy_nzcv(self.fk, self.fa, self.fb, self.fr, self.nzcv);
        if self.v_valid != 0 {
            for (d, s) in cpu.v.iter_mut().zip(&self.v) {
                *d = s[0] as u128 | (s[1] as u128) << 64;
            }
        }
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
            daif: s.daif,
            elr_el1: s.elr_el1,
            spsr_el1: s.spsr_el1,
            esr_el1: s.esr_el1,
            far_el1: s.far_el1,
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
        cpu.sys.daif = self.daif;
        cpu.sys.elr_el1 = self.elr_el1;
        cpu.sys.spsr_el1 = self.spsr_el1;
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
        if cfg!(target_endian = "little") {
            // SAFETY: `JitState` è `repr(C)` senza riempimento implicito (i
            // campi coprono tutti i SIZE byte, vedi `layout_matches_spec`):
            // ogni byte è inizializzato.
            let raw = unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), off::SIZE) };
            m.copy_from_slice(raw);
            return;
        }
        let mut w = |o: usize, b: &[u8]| m[o..o + b.len()].copy_from_slice(b);
        for (i, v) in self.x.iter().enumerate() {
            w(i * 8, &v.to_le_bytes());
        }
        w(248, &self.sp.to_le_bytes());
        w(256, &self.pc.to_le_bytes());
        w(264, &self.steps.to_le_bytes());
        w(272, &self.nzcv.to_le_bytes());
        w(276, &self.exit_detail.to_le_bytes());
        w(280, &self.el.to_le_bytes());
        w(284, &self.ctx.to_le_bytes());
        w(288, &self.limit.to_le_bytes());
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
            w(296 + 8 * i, &v.to_le_bytes());
        }
        w(368, &self.mon_valid.to_le_bytes());
        w(372, &self.mon_bytes.to_le_bytes());
        w(376, &self.entry.to_le_bytes());
        w(380, &self.daif.to_le_bytes());
        for (i, v) in [self.elr_el1, self.spsr_el1, self.esr_el1, self.far_el1].iter().enumerate() {
            w(384 + 8 * i, &v.to_le_bytes());
        }
        w(416, &self.v_valid.to_le_bytes());
        w(420, &self.fk.to_le_bytes());
        w(424, &self.fpcr.to_le_bytes());
        w(428, &self.fpsr.to_le_bytes());
        w(944, &self.fa.to_le_bytes());
        w(952, &self.fb.to_le_bytes());
        w(960, &self.fr.to_le_bytes());
        w(968, &self.time_base.to_le_bytes());
        w(976, &self.cntvoff.to_le_bytes());
        w(984, &self.time_ok.to_le_bytes());
        w(988, &[0; 4]);
        for (i, r) in self.v.iter().enumerate() {
            w(432 + 16 * i, &r[0].to_le_bytes());
            w(440 + 16 * i, &r[1].to_le_bytes());
        }
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
        let mut v = [[0u64; 2]; 32];
        for (i, r) in v.iter_mut().enumerate() {
            *r = [q(432 + 16 * i), q(440 + 16 * i)];
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
            entry: d(376),
            daif: d(380),
            elr_el1: q(384),
            spsr_el1: q(392),
            esr_el1: q(400),
            far_el1: q(408),
            v_valid: d(416),
            fk: d(420),
            fpcr: d(424),
            fpsr: d(428),
            v,
            fa: q(944),
            fb: q(952),
            fr: q(960),
            time_base: q(968),
            cntvoff: q(976),
            time_ok: d(984),
            _pad2: 0,
        }
    }
}

/// Tipi dei flag pigri (`JitState::fk`): somma, differenza, logica, a 64 o
/// 32 bit (operandi e risultato troncati a 32 bit per quelli a 32).
pub mod fk {
    pub const ADD64: u32 = 1;
    pub const SUB64: u32 = 2;
    pub const ADD32: u32 = 3;
    pub const SUB32: u32 = 4;
    pub const LOGIC64: u32 = 5;
    pub const LOGIC32: u32 = 6;
}

/// NZCV (bit 31:28) dei flag pigri: come `AddWithCarry` (somma con carry 0,
/// differenza come somma del complemento con carry 1) e come AND/BIC per la
/// logica (C = V = 0); `old` se `k` = 0. È la funzione `rt.nzcv` dei moduli.
pub fn lazy_nzcv(k: u32, a: u64, b: u64, r: u64, old: u32) -> u32 {
    if k == 0 {
        return old;
    }
    let sh = if matches!(k, fk::ADD32 | fk::SUB32 | fk::LOGIC32) { 31 } else { 63 };
    let n = (r >> sh & 1) as u32;
    let z = (r == 0) as u32;
    let (c, v) = match k {
        fk::ADD64 | fk::ADD32 => ((r < a) as u32, ((!(a ^ b) & (a ^ r)) >> sh & 1) as u32),
        fk::SUB64 | fk::SUB32 => ((a >= b) as u32, (((a ^ b) & (a ^ r)) >> sh & 1) as u32),
        _ => (0, 0),
    };
    n << 31 | z << 30 | c << 29 | v << 28
}

/// Copia i registri SIMD/FP della `Cpu` in `JitState` (`v`, `v_valid` = 1):
/// l'`env.vsync` degli host.
pub fn vsync_in(mem: &mut [u8], at: usize, v: &[u128; 32]) {
    let base = at + off::V as usize;
    for (i, r) in v.iter().enumerate() {
        mem[base + 16 * i..base + 16 * i + 16].copy_from_slice(&r.to_le_bytes());
    }
    write_u32(mem, at, off::V_VALID, 1);
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
        assert_eq!(offset_of!(JitState, entry), off::ENTRY as usize);
        assert_eq!(offset_of!(JitState, daif), off::DAIF as usize);
        assert_eq!(offset_of!(JitState, elr_el1), off::ELR_EL1 as usize);
        assert_eq!(offset_of!(JitState, spsr_el1), off::SPSR_EL1 as usize);
        assert_eq!(offset_of!(JitState, esr_el1), off::ESR_EL1 as usize);
        assert_eq!(offset_of!(JitState, far_el1), off::FAR_EL1 as usize);
        assert_eq!(offset_of!(JitState, v_valid), off::V_VALID as usize);
        assert_eq!(offset_of!(JitState, fk), off::FK as usize);
        assert_eq!(offset_of!(JitState, v), off::V as usize);
        assert_eq!(offset_of!(JitState, fa), off::FA as usize);
        assert_eq!(offset_of!(JitState, fb), off::FB as usize);
        assert_eq!(offset_of!(JitState, fr), off::FR as usize);
        assert_eq!(offset_of!(JitState, fpcr), off::FPCR as usize);
        assert_eq!(offset_of!(JitState, fpsr), off::FPSR as usize);
        assert_eq!(offset_of!(JitState, time_base), off::TIME_BASE as usize);
        assert_eq!(offset_of!(JitState, cntvoff), off::CNTVOFF as usize);
        assert_eq!(offset_of!(JitState, time_ok), off::TIME_OK as usize);
        // Niente riempimento implicito (`store` copia i byte della struttura).
        assert_eq!(offset_of!(JitState, fpsr) + 4, off::V as usize);
        assert_eq!(offset_of!(JitState, _pad2) + 4, off::SIZE);
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
        s.entry = 3;
        s.daif = 0x3c0;
        s.elr_el1 = 10;
        s.spsr_el1 = 11;
        s.esr_el1 = 12;
        s.far_el1 = 13;
        s.v_valid = 1;
        s.v[31] = [0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00];
        s.fk = 2;
        s.fa = 20;
        s.fb = 21;
        s.fr = 22;
        s.fpcr = 0x0300_0000;
        s.fpsr = 0x9f;
        s.time_base = 23;
        s.cntvoff = 24;
        s.time_ok = 1;
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
        cpu.fpcr = 0x0040_0000;
        cpu.fpsr = 0x11;
        let s = JitState::from_cpu(&cpu);
        let mut back = Cpu::new();
        back.tpidr_el0 = 9;
        back.fpcr = 0x0040_0000;
        s.to_cpu(&mut back);
        assert_eq!(back, cpu);
    }
}
