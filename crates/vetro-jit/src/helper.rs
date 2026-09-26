//! `env.simd` (ADR 0026): le istruzioni SIMD/FP senza accessi alla memoria
//! che le regioni non traducono in linea le esegue l'interprete stesso
//! (`vetro_cpu::simd::exec_dp`) sui registri V, FPCR e FPSR di `JitState`,
//! senza uscire dalla regione. La semantica è quindi quella
//! dell'interprete per costruzione (NaN, arrotondamenti, flag cumulativi,
//! denormali, FZ, DN).
//!
//! Registri generali e NZCV restano nelle variabili della regione: chi
//! chiama passa il registro generale letto dall'istruzione (`x`) e NZCV
//! (`nzcv`), e riceve il registro generale scritto o il nuovo NZCV, come dice
//! [`io`].

use std::cell::RefCell;

use vetro_cpu::simd::{CopyOp, FpInsn, IntInsn, SimdInsn};
use vetro_cpu::{Cpu, Insn};

use crate::state::off;

/// Che cosa scrive un'istruzione oltre ai registri V e a FPSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    /// Niente.
    None,
    /// Il registro generale (31 = XZR, scartato): `env.simd` ne
    /// restituisce il valore.
    X(u8),
    /// NZCV: `env.simd` lo restituisce nei bit 31:28.
    Nzcv,
}

/// Registri generali e NZCV di un'istruzione SIMD/FP senza memoria.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Io {
    /// Registro generale letto (31 = XZR).
    pub x_in: Option<u8>,
    /// Legge NZCV (condizione di FCCMP/FCSEL).
    pub nzcv_in: bool,
    pub out: Out,
}

/// Registri generali e NZCV letti e scritti da `i`.
pub fn io(i: &SimdInsn) -> Io {
    let none = Io { x_in: None, nzcv_in: false, out: Out::None };
    match *i {
        SimdInsn::Fp(f) => match f {
            FpInsn::FromInt { rn, .. } | FpInsn::MovFromGp { rn, .. } => Io { x_in: Some(rn), ..none },
            FpInsn::ToInt { rd, .. } | FpInsn::MovToGp { rd, .. } => Io { out: Out::X(rd), ..none },
            FpInsn::Cmp { .. } => Io { out: Out::Nzcv, ..none },
            FpInsn::CondCmp { .. } => Io { nzcv_in: true, out: Out::Nzcv, ..none },
            FpInsn::CondSel { .. } => Io { nzcv_in: true, ..none },
            _ => none,
        },
        SimdInsn::Int(IntInsn::Copy { op, rn, rd, .. }) => match op {
            CopyOp::DupGen | CopyOp::InsGen => Io { x_in: Some(rn), ..none },
            CopyOp::Umov | CopyOp::Smov => Io { out: Out::X(rd), ..none },
            _ => none,
        },
        _ => none,
    }
}

/// V0..V31 dal formato di `JitState` (little-endian, 16 byte ciascuno).
#[inline]
fn copy_v_in(v: &mut [u128; 32], b: &[u8]) {
    assert_eq!(b.len(), 512);
    if cfg!(target_endian = "little") {
        // SAFETY: `v` è di 512 byte; su un host little-endian un u128 ha in
        // memoria gli stessi byte del formato di `JitState`.
        unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr().cast::<u8>(), 512) };
    } else {
        for (r, d) in v.iter_mut().enumerate() {
            *d = u128::from_le_bytes(b[16 * r..16 * r + 16].try_into().unwrap());
        }
    }
}

/// V0..V31 nel formato di `JitState`.
#[inline]
fn copy_v_out(b: &mut [u8], v: &[u128; 32]) {
    assert_eq!(b.len(), 512);
    if cfg!(target_endian = "little") {
        // SAFETY: come in `copy_v_in`.
        unsafe { core::ptr::copy_nonoverlapping(v.as_ptr().cast::<u8>(), b.as_mut_ptr(), 512) };
    } else {
        for (r, s) in v.iter().enumerate() {
            b[16 * r..16 * r + 16].copy_from_slice(&s.to_le_bytes());
        }
    }
}

thread_local! {
    /// CPU di appoggio: si copiano dentro e fuori solo registri V, FPCR,
    /// FPSR, NZCV e il registro generale letto.
    static SCRATCH: RefCell<Cpu> = RefCell::new(Cpu::new());
}

/// `env.simd(state, word, x, nzcv) -> valore`: esegue l'istruzione `word`
/// (SIMD/FP senza memoria) sul `JitState` a `mem[at..]`, i cui registri V
/// devono essere validi (`v_valid`). Restituisce il registro generale
/// scritto o NZCV ([`Out`]), altrimenti 0.
pub fn exec(mem: &mut [u8], at: usize, word: u32, x: u64, nzcv: u32) -> u64 {
    let Insn::Simd(i) = vetro_cpu::decode(word) else {
        panic!("env.simd con un'istruzione non SIMD: {word:#010x}");
    };
    let io = io(&i);
    let st = &mut mem[at..at + off::SIZE];
    let rd32 = |st: &[u8], o: u32| u32::from_le_bytes(st[o as usize..o as usize + 4].try_into().unwrap());
    SCRATCH.with_borrow_mut(|cpu| {
        let v = off::V as usize;
        copy_v_in(&mut cpu.v, &st[v..v + 512]);
        cpu.fpcr = rd32(st, off::FPCR);
        cpu.fpsr = rd32(st, off::FPSR);
        cpu.nzcv = nzcv;
        if let Some(rn) = io.x_in {
            cpu.set_x(rn, x);
        }
        vetro_cpu::simd::exec_dp(cpu, i);
        copy_v_out(&mut st[v..v + 512], &cpu.v);
        let f = off::FPSR as usize;
        st[f..f + 4].copy_from_slice(&cpu.fpsr.to_le_bytes());
        match io.out {
            Out::None => 0,
            Out::X(rd) => cpu.xr(rd),
            Out::Nzcv => cpu.nzcv as u64,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::JitState;

    fn run(cpu: &Cpu, word: u32, x: u64) -> (JitState, u64) {
        let mut s = JitState::from_cpu(cpu);
        for (d, v) in s.v.iter_mut().zip(&cpu.v) {
            *d = [*v as u64, (*v >> 64) as u64];
        }
        let mut mem = vec![0u8; 16 + off::SIZE];
        s.store(&mut mem, 16);
        let r = exec(&mut mem, 16, word, x, cpu.nzcv);
        (JitState::load(&mem, 16), r)
    }

    /// Stessi registri e flag dell'interprete (codifiche da tools/a64asm.sh).
    #[test]
    fn come_interprete() {
        let mut cpu = Cpu::new();
        cpu.v[1] = 0x3ff0_0000_0000_0000; // 1.0
        cpu.v[2] = 0x3fb9_9999_9999_999a; // 0.1
        cpu.x[3] = 7;
        // fadd d0, d1, d2 (inesatta: IXC); fcmp d1, d2; scvtf d4, x3;
        // fcvtzs x5, d1
        for (w, x) in [(0x1e622820u32, 0), (0x1e622020, 0), (0x9e620064, 7), (0x9e780025, 0)] {
            let (s, r) = run(&cpu, w, x);
            let mut want = cpu.clone();
            let Insn::Simd(i) = vetro_cpu::decode(w) else { unreachable!() };
            vetro_cpu::simd::exec_dp(&mut want, i);
            for (a, b) in s.v.iter().zip(&want.v) {
                assert_eq!(a[0] as u128 | (a[1] as u128) << 64, *b, "{w:#x}");
            }
            assert_eq!(s.fpsr, want.fpsr, "{w:#x}");
            match io(&i).out {
                Out::None => assert_eq!(r, 0),
                Out::X(rd) => assert_eq!(r, want.xr(rd)),
                Out::Nzcv => assert_eq!(r as u32, want.nzcv),
            }
        }
    }
}
