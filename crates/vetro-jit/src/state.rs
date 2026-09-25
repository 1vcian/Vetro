//! `JitState`: lo stato della CPU nella memoria lineare condivisa con i
//! blocchi tradotti (docs/specs/jit.md).

use vetro_cpu::Cpu;

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
    /// Riservato all'host: vale [`crate::FAULT`] dopo un `ld`/`st` fallito.
    pub exit_detail: u32,
    /// Livello di eccezione (0 in modalità utente).
    pub el: u32,
    pub _pad: u32,
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
    /// Dimensione totale.
    pub const SIZE: usize = 288;
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
        m[284..288].fill(0);
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
