//! Traduttore: da un blocco di istruzioni decodificate a una funzione WASM
//! con l'ABI di docs/specs/jit.md.
//!
//! La semantica di ogni istruzione tradotta è quella di
//! `vetro_cpu::exec` (stessi flag, stessi troncamenti a 32 bit, stessi casi
//! di XZR/SP); la parità si verifica con i test di `vetro-jit-native` e di
//! `tests/diff`.
//!
//! Dentro il blocco i registri stanno in variabili locali: si caricano
//! dall'inizio quelli letti e si riscrivono in `JitState`, a ogni uscita,
//! quelli scritti fino a quel punto. Un'uscita per fault (dopo un `ld`/`st`
//! fallito) riscrive solo i registri delle istruzioni precedenti, `pc` e
//! `steps` dell'istruzione che ha fallito: lo stato è quello che
//! l'interprete avrebbe prima di eseguirla.

use crate::state::off;
use crate::wasm::{BLOCK_EMPTY, Func, MemoryImport, Module, ValType, op};
use crate::{FAULT, NEXT, STOP, SVC};
use vetro_cpu::Insn;
use vetro_cpu::decode::{
    AddrMode, BfOp, BrOp, CcmpOperand, CselOp, Dp1Op, Dp2Op, Dp3Op, Index, LogicOp, MemOp, MovOp, Shift,
    SysReg,
};

/// Istruzioni massime per blocco (ADR 0012).
pub const MAX_BLOCK: usize = 64;

/// Come si comporta un'istruzione per il traduttore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Tradotta, il blocco continua.
    Linear,
    /// Tradotta, e chiude il blocco (salto).
    Branch,
    /// Chiude il blocco con il codice `SVC`, senza essere eseguita:
    /// l'esegue l'interprete.
    Svc,
    /// Non tradotta: il blocco finisce prima e la esegue l'interprete.
    Unsupported,
}

/// Classificazione di un'istruzione.
pub fn kind(insn: &Insn) -> Kind {
    use Kind::*;
    match *insn {
        Insn::AddSubImm { .. }
        | Insn::LogicalImm { .. }
        | Insn::MoveWide { .. }
        | Insn::Adr { .. }
        | Insn::Bitfield { .. }
        | Insn::Extract { .. }
        | Insn::LogicalReg { .. }
        | Insn::AddSubReg { .. }
        | Insn::AddSubExt { .. }
        | Insn::AddSubCarry { .. }
        | Insn::CondCmp { .. }
        | Insn::CondSel { .. }
        | Insn::Dp1 { .. }
        | Insn::Dp3 { .. }
        | Insn::Nop
        | Insn::Barrier
        | Insn::CacheMaint
        | Insn::Wfi
        | Insn::Wfe
        | Insn::LdLiteral { .. }
        | Insn::LdStPair { .. }
        | Insn::LoadAcquire { .. }
        | Insn::StoreRelease { .. } => Linear,
        Insn::Dp2 { op: Dp2Op::Crc32 { .. }, .. } => Unsupported,
        Insn::Dp2 { .. } => Linear,
        Insn::Mrs { reg: SysReg::Nzcv, .. } | Insn::Msr { reg: SysReg::Nzcv, .. } => Linear,
        Insn::LdSt { .. } => Linear,
        Insn::B { .. }
        | Insn::BCond { .. }
        | Insn::Cbz { .. }
        | Insn::Tbz { .. }
        | Insn::BranchReg { .. } => Branch,
        Insn::Svc { .. } => Svc,
        _ => Unsupported,
    }
}

/// Un blocco da tradurre: indirizzo della prima istruzione e istruzioni
/// consecutive (già decodificate).
#[derive(Clone, Debug)]
pub struct Block {
    pub pc: u64,
    pub insns: Vec<Insn>,
}

impl Block {
    /// Istruzioni eseguite al massimo da una corsa del blocco (l'eventuale
    /// SVC finale non si esegue).
    pub fn max_steps(&self) -> u64 {
        let n = self.insns.len();
        match self.insns.last() {
            Some(i) if kind(i) == Kind::Svc => (n - 1) as u64,
            _ => n as u64,
        }
    }
}

/// Indici delle funzioni importate nel modulo.
const F_LD: u32 = 0;
const F_ST: u32 = 1;

/// Variabili locali: 0 = puntatore a `JitState`, 1..=31 x0..x30, 32 SP,
/// 33 NZCV (i32), poi temporanei.
const L_STATE: u32 = 0;
const L_NZCV: u32 = 33;
const L_T64: u32 = 34;
const N_T64: u32 = 11;
/// `steps` all'ingresso del blocco.
const L_STEPS0: u32 = L_T64 + 10;
const L_T32: u32 = L_T64 + N_T64;
const N_T32: u32 = 4;

/// Bit dei registri nelle maschere di lettura/scrittura: 0..=30 x, 31 SP,
/// 32 NZCV.
const B_SP: u32 = 31;
const B_NZCV: u32 = 32;

#[inline]
const fn t64(i: u32) -> u32 {
    L_T64 + i
}
#[inline]
const fn t32(i: u32) -> u32 {
    L_T32 + i
}

/// Genera il modulo WASM con un blocco per elemento di `blocks`: la
/// funzione del blocco `i` si esporta come `b<i>`.
pub fn module(blocks: &[Block], memory: MemoryImport) -> Vec<u8> {
    use ValType::*;
    let mut m = Module::new();
    let t_blk = m.ty(&[I32], &[I32]);
    let t_ld = m.ty(&[I32, I64, I32], &[I64]);
    let t_st = m.ty(&[I32, I64, I32, I64], &[I32]);
    m.import_memory("env", "mem", memory);
    let ld = m.import_func("env", "ld", t_ld);
    let st = m.import_func("env", "st", t_st);
    debug_assert_eq!((ld, st), (F_LD, F_ST));
    for (i, b) in blocks.iter().enumerate() {
        let f = function(b);
        let idx = m.func(t_blk, f);
        m.export_func(&format!("b{i}"), idx);
    }
    m.encode()
}

/// Funzione WASM di un blocco.
pub fn function(b: &Block) -> Func {
    assert!(!b.insns.is_empty() && b.insns.len() <= MAX_BLOCK);
    let mut t = Tx { f: Func::default(), read: 0, written: 0, pc: b.pc, index: 0, ended: false, saved: None };
    for (i, insn) in b.insns.iter().enumerate() {
        t.index = i as u64;
        t.pc = b.pc.wrapping_add(4 * i as u64);
        let k = kind(insn);
        assert!(k != Kind::Unsupported, "istruzione non traducibile nel blocco: {insn:?}");
        if k == Kind::Svc {
            assert_eq!(i + 1, b.insns.len(), "SVC non in fondo al blocco");
            t.exit_const(SVC, t.pc, t.index);
            t.ended = true;
            break;
        }
        t.insn(insn);
        if k == Kind::Branch {
            assert_eq!(i + 1, b.insns.len(), "salto non in fondo al blocco");
            t.ended = true;
        }
    }
    if !t.ended {
        let next = b.pc.wrapping_add(4 * b.insns.len() as u64);
        t.index = b.insns.len() as u64;
        t.exit_const(NEXT, next, t.index);
    }
    // Prologo: `steps` all'ingresso e i registri letti.
    let mut pro = Func::default();
    pro.local_get(L_STATE).i64_load(off::STEPS).local_set(L_STEPS0);
    for r in 0..=B_SP {
        if t.read & (1 << r) != 0 {
            pro.local_get(L_STATE).i64_load(off::X + 8 * r).local_set(1 + r);
        }
    }
    if t.read & (1 << B_NZCV) != 0 {
        pro.local_get(L_STATE).i32_load(off::NZCV).local_set(L_NZCV);
    }
    pro.code.extend_from_slice(&t.f.code);
    pro.op(op::UNREACHABLE);
    pro.locals = vec![(32, ValType::I64), (1, ValType::I32), (N_T64, ValType::I64), (N_T32, ValType::I32)];
    pro
}

/// Traduttore di un blocco.
struct Tx {
    f: Func,
    /// Registri letti dal blocco (da caricare nel prologo).
    read: u64,
    /// Registri scritti finora (da riscrivere a ogni uscita).
    written: u64,
    /// Indirizzo dell'istruzione corrente.
    pc: u64,
    /// Indice dell'istruzione corrente nel blocco.
    index: u64,
    ended: bool,
    /// Istruzione di cui `pc` e `steps` sono già in `JitState`.
    saved: Option<u64>,
}

/// Da dove viene il nuovo `pc` di un'uscita.
enum PcSrc {
    Const(u64),
    /// Valore in cima allo stack (i64), consumato.
    Stack,
}

impl Tx {
    // --- registri -----------------------------------------------------

    /// `xr(r)`: 31 vale XZR.
    fn get_x(&mut self, r: u8) {
        if r == 31 {
            self.f.i64_const(0);
        } else {
            self.read |= 1 << r;
            self.f.local_get(1 + r as u32);
        }
    }

    /// `xsp(r)`: 31 vale SP.
    fn get_xsp(&mut self, r: u8) {
        self.read |= 1 << r;
        self.f.local_get(1 + r as u32);
    }

    /// `set_x(r, top)`: 31 (XZR) scarta.
    fn set_x(&mut self, r: u8) {
        if r == 31 {
            self.f.op(op::DROP);
        } else {
            self.written |= 1 << r;
            self.f.local_set(1 + r as u32);
        }
    }

    /// `set_xsp(r, top)`: 31 vale SP.
    fn set_xsp(&mut self, r: u8) {
        self.written |= 1 << r;
        self.f.local_set(1 + r as u32);
    }

    fn get_nzcv(&mut self) {
        self.read |= 1 << B_NZCV;
        self.f.local_get(L_NZCV);
    }

    fn set_nzcv(&mut self) {
        self.written |= 1 << B_NZCV;
        self.f.local_set(L_NZCV);
    }

    /// Tronca a 32 bit (estensione a zero) se `!sf`.
    fn trunc(&mut self, sf: bool) {
        if !sf {
            self.f.i64_const(0xffff_ffff).op(op::I64_AND);
        }
    }

    // --- uscite -------------------------------------------------------

    /// Riscrive in `JitState` i registri scritti finora.
    fn flush(&mut self) {
        for r in 0..=B_SP {
            if self.written & (1 << r) != 0 {
                self.f.local_get(L_STATE).local_get(1 + r).i64_store(off::X + 8 * r);
            }
        }
        if self.written & (1 << B_NZCV) != 0 {
            self.f.local_get(L_STATE).local_get(L_NZCV).i32_store(off::NZCV);
        }
    }

    /// Uscita con codice `code`, nuovo `pc` e `steps` aumentato di `done`.
    fn exit(&mut self, code: u32, pc: PcSrc, done: u64) {
        if let PcSrc::Stack = pc {
            self.f.local_set(t64(9));
        }
        self.flush();
        self.f.local_get(L_STATE);
        match pc {
            PcSrc::Const(v) => self.f.i64_const(v as i64),
            PcSrc::Stack => self.f.local_get(t64(9)),
        };
        self.f.i64_store(off::PC);
        self.store_steps(done);
        self.f.i32_const(code as i32).op(op::RETURN);
    }

    /// `steps` = valore all'ingresso + `done`.
    fn store_steps(&mut self, done: u64) {
        self.f.local_get(L_STATE).local_get(L_STEPS0).i64_const(done as i64).op(op::I64_ADD);
        self.f.i64_store(off::STEPS);
    }

    fn exit_const(&mut self, code: u32, pc: u64, done: u64) {
        self.exit(code, PcSrc::Const(pc), done);
    }

    /// Prima di un accesso alla memoria: `pc` e `steps` dell'istruzione
    /// corrente in `JitState` (la spec li vuole già salvati durante
    /// `ld`/`st`).
    fn save_pc(&mut self) {
        self.f.local_get(L_STATE).i64_const(self.pc as i64).i64_store(off::PC);
        self.store_steps(self.index);
    }

    /// [`save_pc`](Self::save_pc) una volta per istruzione.
    fn save_once(&mut self) {
        if self.saved != Some(self.index) {
            self.save_pc();
            self.saved = Some(self.index);
        }
    }

    /// Uscita per fault dell'istruzione corrente (registri come prima).
    fn exit_fault(&mut self) {
        self.exit_const(FAULT, self.pc, self.index);
    }

    /// Se `exit_detail` vale FAULT, esce con FAULT (2 = STOP non è un fault).
    fn check_fault(&mut self) {
        self.f.local_get(L_STATE).i32_load(off::EXIT_DETAIL).i32_const(FAULT as i32).op(op::I32_EQ);
        self.f.if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
    }

    /// Chiude il blocco dopo un salto: `pc` in cima allo stack.
    fn exit_branch(&mut self) {
        let done = self.index + 1;
        self.exit(NEXT, PcSrc::Stack, done);
    }

    // --- aritmetica ---------------------------------------------------

    /// `AddWithCarry(x, y, carry)` con x in `t64(0)`, y in `t64(1)`:
    /// risultato (troncato se `!sf`) in `t64(2)`; se `flags`, NZCV nella
    /// variabile dei flag. `carry`: `Some(c)` costante, `None` = flag C.
    fn add_with_carry(&mut self, sf: bool, carry: Option<bool>, flags: bool) {
        let (x, y, r) = (t64(0), t64(1), t64(2));
        let f = &mut self.f;
        if !sf {
            f.local_get(x).i64_const(0xffff_ffff).op(op::I64_AND).local_set(x);
            f.local_get(y).i64_const(0xffff_ffff).op(op::I64_AND).local_set(y);
        }
        f.local_get(x).local_get(y).op(op::I64_ADD);
        match carry {
            Some(false) => {}
            Some(true) => {
                f.i64_const(1).op(op::I64_ADD);
            }
            None => {
                self.read |= 1 << B_NZCV;
                let f = &mut self.f;
                f.local_get(L_NZCV).i32_const(29).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
                f.op(op::I64_EXTEND_I32_U).op(op::I64_ADD);
            }
        }
        let f = &mut self.f;
        if sf {
            f.local_set(r);
        } else {
            // wide in t64(3), r = wide & 0xffffffff
            f.local_tee(t64(3)).i64_const(0xffff_ffff).op(op::I64_AND).local_set(r);
        }
        if !flags {
            return;
        }
        let bits = if sf { 63 } else { 31 };
        // N
        f.local_get(r).i64_const(bits).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
        f.i32_const(31).op(op::I32_SHL);
        // Z
        f.local_get(r).op(op::I64_EQZ).i32_const(30).op(op::I32_SHL).op(op::I32_OR);
        // C
        if sf {
            match carry {
                Some(false) => {
                    f.local_get(r).local_get(x).op(op::I64_LT_U);
                }
                Some(true) => {
                    f.local_get(r).local_get(x).op(op::I64_LE_U);
                }
                None => {
                    f.local_get(r).local_get(x).op(op::I64_LE_U);
                    f.local_get(r).local_get(x).op(op::I64_LT_U);
                    f.local_get(L_NZCV).i32_const(29).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
                    f.op(op::SELECT);
                }
            }
        } else {
            f.local_get(t64(3)).i64_const(32).op(op::I64_SHR_U).op(op::I32_WRAP_I64);
        }
        f.i32_const(29).op(op::I32_SHL).op(op::I32_OR);
        // V = ((x ^ r) & (y ^ r)) >> bits
        f.local_get(x).local_get(r).op(op::I64_XOR);
        f.local_get(y).local_get(r).op(op::I64_XOR).op(op::I64_AND);
        f.i64_const(bits).op(op::I64_SHR_U).op(op::I32_WRAP_I64).i32_const(1).op(op::I32_AND);
        f.i32_const(28).op(op::I32_SHL).op(op::I32_OR);
        self.set_nzcv();
    }

    /// `add_sub(x, y, sub, setflags, sf)` con x in `t64(0)` e y in
    /// `t64(1)`: risultato in `t64(2)`.
    fn add_sub(&mut self, sub: bool, setflags: bool, sf: bool) {
        if sub {
            self.f.local_get(t64(1)).i64_const(-1).op(op::I64_XOR).local_set(t64(1));
        }
        self.add_with_carry(sf, Some(sub), setflags);
    }

    /// Flag di AND/BIC con risultato (troncato) in cima allo stack, che
    /// resta.
    fn logic_flags(&mut self, sf: bool) {
        let bits = if sf { 63 } else { 31 };
        let f = &mut self.f;
        f.local_tee(t64(2));
        f.local_get(t64(2))
            .i64_const(bits)
            .op(op::I64_SHR_U)
            .op(op::I32_WRAP_I64)
            .i32_const(1)
            .op(op::I32_AND);
        f.i32_const(31).op(op::I32_SHL);
        f.local_get(t64(2)).op(op::I64_EQZ).i32_const(30).op(op::I32_SHL).op(op::I32_OR);
        self.set_nzcv();
    }

    /// `shift_reg(top, shift, amount, sf)`: valore in cima allo stack.
    fn shift_reg(&mut self, shift: Shift, amount: u8, sf: bool) {
        let a = amount as i64;
        let f = &mut self.f;
        if sf {
            match shift {
                Shift::Lsl => f.i64_const(a).op(op::I64_SHL),
                Shift::Lsr => f.i64_const(a).op(op::I64_SHR_U),
                Shift::Asr => f.i64_const(a).op(op::I64_SHR_S),
                Shift::Ror => f.i64_const(a).op(op::I64_ROTR),
            };
        } else {
            f.op(op::I32_WRAP_I64);
            match shift {
                Shift::Lsl => f.i32_const(a as i32).op(op::I32_SHL),
                Shift::Lsr => f.i32_const(a as i32).op(op::I32_SHR_U),
                Shift::Asr => f.i32_const(a as i32).op(op::I32_SHR_S),
                Shift::Ror => f.i32_const(a as i32).op(op::I32_ROTR),
            };
            f.op(op::I64_EXTEND_I32_U);
        }
    }

    /// `extend_reg(top, extend, shift)` a 64 bit.
    fn extend_reg(&mut self, extend: u8, shift: u8) {
        let f = &mut self.f;
        match extend {
            0 => f.i64_const(0xff).op(op::I64_AND),
            1 => f.i64_const(0xffff).op(op::I64_AND),
            2 => f.i64_const(0xffff_ffff).op(op::I64_AND),
            3 | 7 => f,
            4 => f.op(op::I64_EXTEND8_S),
            5 => f.op(op::I64_EXTEND16_S),
            _ => f.op(op::I64_EXTEND32_S),
        };
        if shift != 0 {
            f.i64_const(shift as i64).op(op::I64_SHL);
        }
    }

    /// Lascia sullo stack (i32) `ConditionHolds(cond)`.
    fn cond(&mut self, cond: u8) {
        if cond >> 1 == 7 {
            self.f.i32_const(1);
            return;
        }
        self.read |= 1 << B_NZCV;
        let f = &mut self.f;
        let flag = |f: &mut Func, sh: i32| {
            f.local_get(L_NZCV).i32_const(sh).op(op::I32_SHR_U).i32_const(1).op(op::I32_AND);
        };
        match cond >> 1 {
            0 => flag(f, 30),
            1 => flag(f, 29),
            2 => flag(f, 31),
            3 => flag(f, 28),
            4 => {
                flag(f, 29);
                flag(f, 30);
                f.i32_const(1).op(op::I32_XOR).op(op::I32_AND);
            }
            5 => {
                flag(f, 31);
                flag(f, 28);
                f.op(op::I32_EQ);
            }
            _ => {
                flag(f, 31);
                flag(f, 28);
                f.op(op::I32_EQ);
                flag(f, 30);
                f.i32_const(1).op(op::I32_XOR).op(op::I32_AND);
            }
        }
        if cond & 1 == 1 {
            f.op(op::I32_EQZ);
        }
    }

    // --- memoria ------------------------------------------------------

    /// Load di `bytes` byte dall'indirizzo in `t64(4)`: valore esteso a
    /// zero sullo stack; esce con FAULT se l'accesso fallisce.
    fn ld(&mut self, addr: u32, bytes: u32) {
        self.save_once();
        self.f.local_get(L_STATE).local_get(addr).i32_const(bytes as i32).call(F_LD);
        self.f.local_set(t64(5));
        self.check_fault();
        self.f.local_get(t64(5));
    }

    /// Store di `bytes` byte del valore in `val` all'indirizzo in `addr`.
    /// Il risultato di `st` va in `t32(stop)`; se è un fault esce con FAULT.
    fn st(&mut self, addr: u32, bytes: u32, val: u32, stop: u32) {
        self.save_once();
        self.f.local_get(L_STATE).local_get(addr).i32_const(bytes as i32).local_get(val).call(F_ST);
        self.f.local_tee(t32(stop)).if_(BLOCK_EMPTY);
        self.check_fault();
        self.f.end();
    }

    /// Dopo gli store dell'istruzione corrente (e il writeback): se uno ha
    /// chiesto di fermarsi, esce con STOP dopo l'istruzione.
    fn stop_after(&mut self, stops: &[u32]) {
        for (i, &s) in stops.iter().enumerate() {
            self.f.local_get(t32(s));
            if i > 0 {
                self.f.op(op::I32_OR);
            }
        }
        self.f.if_(BLOCK_EMPTY);
        let (next, done) = (self.pc.wrapping_add(4), self.index + 1);
        self.exit_const(STOP, next, done);
        self.f.end();
    }

    /// Estende un valore caricato di `1 << size` byte come chiede `op`.
    fn extend_load(&mut self, size: u8, op_: MemOp) {
        if let MemOp::Load { signed: true, dst64 } = op_ {
            let f = &mut self.f;
            match size {
                0 => f.op(op::I64_EXTEND8_S),
                1 => f.op(op::I64_EXTEND16_S),
                2 => f.op(op::I64_EXTEND32_S),
                _ => f,
            };
            if !dst64 {
                f.i64_const(0xffff_ffff).op(op::I64_AND);
            }
        }
    }

    // --- istruzioni ---------------------------------------------------

    fn insn(&mut self, insn: &Insn) {
        let pc = self.pc;
        match *insn {
            Insn::AddSubImm { sf, sub, setflags, imm, rn, rd } => {
                self.get_xsp(rn);
                self.f.local_set(t64(0));
                self.f.i64_const(imm as i64).local_set(t64(1));
                self.add_sub(sub, setflags, sf);
                self.f.local_get(t64(2));
                if setflags { self.set_x(rd) } else { self.set_xsp(rd) }
            }
            Insn::LogicalImm { sf, op: lop, imm, rn, rd } => {
                self.get_x(rn);
                self.f.i64_const(imm as i64);
                self.f.op(match lop {
                    LogicOp::And | LogicOp::Ands => op::I64_AND,
                    LogicOp::Orr => op::I64_OR,
                    LogicOp::Eor => op::I64_XOR,
                });
                self.trunc(sf);
                if lop == LogicOp::Ands {
                    self.logic_flags(sf);
                    self.set_x(rd);
                } else {
                    self.set_xsp(rd);
                }
            }
            Insn::MoveWide { sf, op: mop, shift, imm16, rd } => {
                let imm = (imm16 as u64) << shift;
                match mop {
                    MovOp::Movz => {
                        self.f.i64_const(imm as i64);
                    }
                    MovOp::Movn => {
                        self.f.i64_const(!imm as i64);
                    }
                    MovOp::Movk => {
                        self.get_x(rd);
                        self.f.i64_const(!(0xffffu64 << shift) as i64).op(op::I64_AND);
                        self.f.i64_const(imm as i64).op(op::I64_OR);
                    }
                }
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Adr { page, imm, rd } => {
                let base = if page { pc & !0xfff } else { pc };
                self.f.i64_const(base.wrapping_add(imm as u64) as i64);
                self.set_x(rd);
            }
            Insn::Bitfield { sf, op: bop, r, s, wmask, tmask, rn, rd } => {
                // src in t64(0), dst in t64(1)
                self.get_x(rn);
                self.f.local_set(t64(0));
                if bop == BfOp::Bfm {
                    self.get_x(rd);
                } else {
                    self.f.i64_const(0);
                }
                self.f.local_set(t64(1));
                let f = &mut self.f;
                // bot = (dst & !wmask) | (ror(src, r, n) & wmask)
                f.local_get(t64(1)).i64_const(!wmask as i64).op(op::I64_AND);
                if sf {
                    f.local_get(t64(0)).i64_const(r as i64).op(op::I64_ROTR);
                } else {
                    f.local_get(t64(0)).op(op::I32_WRAP_I64).i32_const(r as i32).op(op::I32_ROTR);
                    f.op(op::I64_EXTEND_I32_U);
                }
                f.i64_const(wmask as i64).op(op::I64_AND).op(op::I64_OR).local_set(t64(2));
                // top
                if bop == BfOp::Sbfm {
                    f.i64_const(0).local_get(t64(0)).i64_const(s as i64).op(op::I64_SHR_U);
                    f.i64_const(1).op(op::I64_AND).op(op::I64_SUB);
                } else {
                    f.local_get(t64(1));
                }
                f.i64_const(!tmask as i64).op(op::I64_AND);
                f.local_get(t64(2)).i64_const(tmask as i64).op(op::I64_AND).op(op::I64_OR);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Extract { sf, lsb, rm, rn, rd } => {
                if sf {
                    if lsb == 0 {
                        self.get_x(rm);
                    } else {
                        self.get_x(rm);
                        self.f.i64_const(lsb as i64).op(op::I64_SHR_U);
                        self.get_x(rn);
                        self.f.i64_const(64 - lsb as i64).op(op::I64_SHL).op(op::I64_OR);
                    }
                } else {
                    self.get_x(rn);
                    self.f.i64_const(32).op(op::I64_SHL);
                    self.get_x(rm);
                    self.f.i64_const(0xffff_ffff).op(op::I64_AND).op(op::I64_OR);
                    self.f.i64_const(lsb as i64).op(op::I64_SHR_U);
                    self.trunc(false);
                }
                self.set_x(rd);
            }
            Insn::LogicalReg { sf, op: lop, invert, shift, amount, rm, rn, rd } => {
                self.get_x(rn);
                self.get_x(rm);
                if !sf {
                    self.trunc(false);
                }
                self.shift_reg(shift, amount, sf);
                if invert {
                    self.f.i64_const(-1).op(op::I64_XOR);
                }
                self.f.op(match lop {
                    LogicOp::And | LogicOp::Ands => op::I64_AND,
                    LogicOp::Orr => op::I64_OR,
                    LogicOp::Eor => op::I64_XOR,
                });
                self.trunc(sf);
                if lop == LogicOp::Ands {
                    self.logic_flags(sf);
                }
                self.set_x(rd);
            }
            Insn::AddSubReg { sf, sub, setflags, shift, amount, rm, rn, rd } => {
                self.get_x(rn);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.shift_reg(shift, amount, sf);
                self.f.local_set(t64(1));
                self.add_sub(sub, setflags, sf);
                self.f.local_get(t64(2));
                self.set_x(rd);
            }
            Insn::AddSubExt { sf, sub, setflags, extend, amount, rm, rn, rd } => {
                self.get_xsp(rn);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.extend_reg(extend, amount);
                self.trunc(sf);
                self.f.local_set(t64(1));
                self.add_sub(sub, setflags, sf);
                self.f.local_get(t64(2));
                if setflags { self.set_x(rd) } else { self.set_xsp(rd) }
            }
            Insn::AddSubCarry { sf, sub, setflags, rm, rn, rd } => {
                self.get_x(rn);
                self.f.local_set(t64(0));
                self.get_x(rm);
                if sub {
                    self.f.i64_const(-1).op(op::I64_XOR);
                }
                self.f.local_set(t64(1));
                self.add_with_carry(sf, None, setflags);
                self.f.local_get(t64(2));
                self.set_x(rd);
            }
            Insn::CondCmp { sf, sub, operand, cond, nzcv, rn } => {
                self.cond(cond);
                self.f.if_(BLOCK_EMPTY);
                self.get_x(rn);
                self.f.local_set(t64(0));
                match operand {
                    CcmpOperand::Reg(rm) => self.get_x(rm),
                    CcmpOperand::Imm(i) => {
                        self.f.i64_const(i as i64);
                    }
                }
                self.f.local_set(t64(1));
                self.add_sub(sub, true, sf);
                self.f.else_();
                self.f.i32_const(((nzcv as u32) << 28) as i32);
                self.set_nzcv();
                self.f.end();
            }
            Insn::CondSel { sf, op: cop, cond, rm, rn, rd } => {
                self.get_x(rn);
                match cop {
                    CselOp::Csel => self.get_x(rm),
                    CselOp::Csinc => {
                        self.get_x(rm);
                        self.f.i64_const(1).op(op::I64_ADD);
                    }
                    CselOp::Csinv => {
                        self.get_x(rm);
                        self.f.i64_const(-1).op(op::I64_XOR);
                    }
                    CselOp::Csneg => {
                        self.f.i64_const(0);
                        self.get_x(rm);
                        self.f.op(op::I64_SUB);
                    }
                }
                self.cond(cond);
                self.f.op(op::SELECT);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Dp1 { sf, op: dop, rn, rd } => {
                self.get_x(rn);
                self.dp1(sf, dop);
                self.set_x(rd);
            }
            Insn::Dp2 { sf, op: dop, rm, rn, rd } => {
                self.get_x(rn);
                self.trunc(sf);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.trunc(sf);
                self.f.local_set(t64(1));
                self.dp2(sf, dop);
                self.trunc(sf);
                self.set_x(rd);
            }
            Insn::Dp3 { sf, op: dop, ra, rm, rn, rd } => {
                self.get_x(ra);
                self.f.local_set(t64(0));
                self.get_x(rm);
                self.f.local_set(t64(1));
                self.get_x(rn);
                self.f.local_set(t64(2));
                self.dp3(dop);
                self.trunc(sf);
                self.set_x(rd);
            }

            Insn::B { link, offset } => {
                if link {
                    self.f.i64_const(pc.wrapping_add(4) as i64);
                    self.set_x(30);
                }
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64);
                self.exit_branch();
            }
            Insn::BCond { cond, offset } => {
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64);
                self.f.i64_const(pc.wrapping_add(4) as i64);
                self.cond(cond);
                self.f.op(op::SELECT);
                self.exit_branch();
            }
            Insn::Cbz { sf, nonzero, rt, offset } => {
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64);
                self.f.i64_const(pc.wrapping_add(4) as i64);
                // (x != 0) == nonzero
                self.get_x(rt);
                self.trunc(sf);
                self.f.op(op::I64_EQZ);
                if nonzero {
                    self.f.op(op::I32_EQZ);
                }
                self.f.op(op::SELECT);
                self.exit_branch();
            }
            Insn::Tbz { nonzero, bit, rt, offset } => {
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64);
                self.f.i64_const(pc.wrapping_add(4) as i64);
                self.get_x(rt);
                self.f.i64_const(bit as i64).op(op::I64_SHR_U).i64_const(1).op(op::I64_AND);
                self.f.op(op::I32_WRAP_I64);
                if !nonzero {
                    self.f.op(op::I32_EQZ);
                }
                self.f.op(op::SELECT);
                self.exit_branch();
            }
            Insn::BranchReg { op: bop, rn } => {
                self.get_x(rn);
                self.f.local_set(t64(3));
                if bop == BrOp::Blr {
                    self.f.i64_const(pc.wrapping_add(4) as i64);
                    self.set_x(30);
                }
                self.f.local_get(t64(3));
                self.exit_branch();
            }

            Insn::Nop | Insn::Barrier | Insn::CacheMaint | Insn::Wfi | Insn::Wfe => {}
            Insn::Mrs { reg: SysReg::Nzcv, rt } => {
                self.get_nzcv();
                self.f.op(op::I64_EXTEND_I32_U);
                self.set_x(rt);
            }
            Insn::Msr { reg: SysReg::Nzcv, rt } => {
                self.get_x(rt);
                self.f.op(op::I32_WRAP_I64).i32_const(0xf000_0000u32 as i32).op(op::I32_AND);
                self.set_nzcv();
            }

            Insn::LdSt { size, op: mop, addr, rt, rn, unpriv: _ } => {
                if mop == MemOp::Prefetch {
                    return;
                }
                // address in t64(4), writeback in t64(6)
                let writeback = match addr {
                    AddrMode::Imm { offset, index } => {
                        self.get_xsp(rn);
                        self.f.local_set(t64(6));
                        self.f.local_get(t64(6)).i64_const(offset).op(op::I64_ADD);
                        match index {
                            Index::Offset => {
                                self.f.local_set(t64(4));
                                false
                            }
                            Index::Pre => {
                                self.f.local_tee(t64(4)).local_set(t64(6));
                                true
                            }
                            Index::Post => {
                                self.f.local_get(t64(6)).local_set(t64(4)).local_set(t64(6));
                                true
                            }
                        }
                    }
                    AddrMode::Reg { rm, extend, shift } => {
                        self.get_xsp(rn);
                        self.get_x(rm);
                        self.extend_reg(extend, shift);
                        self.f.op(op::I64_ADD).local_set(t64(4));
                        false
                    }
                };
                let bytes = 1u32 << size;
                match mop {
                    MemOp::Store => {
                        self.get_x(rt);
                        self.f.local_set(t64(7));
                        self.st(t64(4), bytes, t64(7), 0);
                        if writeback {
                            self.f.local_get(t64(6));
                            self.set_xsp(rn);
                        }
                        self.stop_after(&[0]);
                    }
                    MemOp::Load { .. } => {
                        self.ld(t64(4), bytes);
                        self.extend_load(size, mop);
                        self.set_x(rt);
                        if writeback {
                            self.f.local_get(t64(6));
                            self.set_xsp(rn);
                        }
                    }
                    MemOp::Prefetch => unreachable!(),
                }
            }
            Insn::LdLiteral { size, op: mop, offset, rt } => {
                if mop == MemOp::Prefetch {
                    return;
                }
                self.f.i64_const(pc.wrapping_add(offset as u64) as i64).local_set(t64(4));
                self.ld(t64(4), 1 << size);
                self.extend_load(size, mop);
                self.set_x(rt);
            }
            Insn::LdStPair { size, load, signed, index, offset, rt, rt2, rn } => {
                // base in t64(6) → moved; address in t64(4), second in t64(8)
                self.get_xsp(rn);
                self.f.local_tee(t64(4));
                self.f.i64_const(offset).op(op::I64_ADD).local_set(t64(6));
                if index != Index::Post {
                    self.f.local_get(t64(6)).local_set(t64(4));
                }
                let bytes = 1u32 << size;
                self.f.local_get(t64(4)).i64_const(bytes as i64).op(op::I64_ADD).local_set(t64(8));
                if load {
                    let lop = MemOp::Load { signed, dst64: true };
                    self.ld(t64(4), bytes);
                    self.extend_load(size, lop);
                    self.f.local_set(t64(7));
                    self.ld(t64(8), bytes);
                    self.extend_load(size, lop);
                    self.f.local_set(t64(3));
                    self.f.local_get(t64(7));
                    self.set_x(rt);
                    self.f.local_get(t64(3));
                    self.set_x(rt2);
                    if index != Index::Offset {
                        self.f.local_get(t64(6));
                        self.set_xsp(rn);
                    }
                } else {
                    self.get_x(rt);
                    self.f.local_set(t64(7));
                    self.get_x(rt2);
                    self.f.local_set(t64(3));
                    self.st(t64(4), bytes, t64(7), 0);
                    self.st(t64(8), bytes, t64(3), 1);
                    if index != Index::Offset {
                        self.f.local_get(t64(6));
                        self.set_xsp(rn);
                    }
                    self.stop_after(&[0, 1]);
                }
            }
            Insn::LoadAcquire { size, rt, rn } => {
                self.get_xsp(rn);
                self.f.local_set(t64(4));
                self.misaligned_fault(size);
                self.ld(t64(4), 1 << size);
                self.set_x(rt);
            }
            Insn::StoreRelease { size, rt, rn } => {
                self.get_xsp(rn);
                self.f.local_set(t64(4));
                self.misaligned_fault(size);
                self.get_x(rt);
                self.f.local_set(t64(7));
                self.st(t64(4), 1 << size, t64(7), 0);
                self.stop_after(&[0]);
            }
            other => unreachable!("istruzione non traducibile: {other:?}"),
        }
    }

    /// Se l'indirizzo in `t64(4)` non è allineato a `1 << size`, esce con
    /// FAULT: l'interprete riesegue e dà l'eccezione di allineamento.
    fn misaligned_fault(&mut self, size: u8) {
        if size == 0 {
            return;
        }
        self.f.local_get(t64(4)).i64_const((1i64 << size) - 1).op(op::I64_AND);
        self.f.op(op::I64_EQZ).op(op::I32_EQZ).if_(BLOCK_EMPTY);
        self.exit_fault();
        self.f.end();
    }

    /// Dp1 sul valore in cima allo stack.
    fn dp1(&mut self, sf: bool, dop: Dp1Op) {
        let f = &mut self.f;
        // passo di scambio: ((v >> s) & m) | ((v & m) << s)
        let swap64 = |f: &mut Func, s: i64, m: u64| {
            f.local_tee(t64(0)).i64_const(s).op(op::I64_SHR_U).i64_const(m as i64).op(op::I64_AND);
            f.local_get(t64(0)).i64_const(m as i64).op(op::I64_AND).i64_const(s).op(op::I64_SHL);
            f.op(op::I64_OR);
        };
        let swap32 = |f: &mut Func, s: i32, m: u32| {
            f.local_tee(t32(0)).i32_const(s).op(op::I32_SHR_U).i32_const(m as i32).op(op::I32_AND);
            f.local_get(t32(0)).i32_const(m as i32).op(op::I32_AND).i32_const(s).op(op::I32_SHL);
            f.op(op::I32_OR);
        };
        if sf {
            match dop {
                Dp1Op::Rbit => {
                    swap64(f, 1, 0x5555_5555_5555_5555);
                    swap64(f, 2, 0x3333_3333_3333_3333);
                    swap64(f, 4, 0x0f0f_0f0f_0f0f_0f0f);
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                    f.i64_const(32).op(op::I64_ROTL);
                }
                Dp1Op::Rev16 => swap64(f, 8, 0x00ff_00ff_00ff_00ff),
                Dp1Op::Rev32 => {
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                }
                Dp1Op::Rev64 => {
                    swap64(f, 8, 0x00ff_00ff_00ff_00ff);
                    swap64(f, 16, 0x0000_ffff_0000_ffff);
                    f.i64_const(32).op(op::I64_ROTL);
                }
                Dp1Op::Clz => {
                    f.op(op::I64_CLZ);
                }
                Dp1Op::Cls => {
                    // clz((x ^ (x >> 1)) & (MAX >> 1)) - 1
                    f.local_tee(t64(0)).local_get(t64(0)).i64_const(1).op(op::I64_SHR_U).op(op::I64_XOR);
                    f.i64_const((u64::MAX >> 1) as i64).op(op::I64_AND).op(op::I64_CLZ);
                    f.i64_const(1).op(op::I64_SUB);
                }
            }
        } else {
            f.op(op::I32_WRAP_I64);
            match dop {
                Dp1Op::Rbit => {
                    swap32(f, 1, 0x5555_5555);
                    swap32(f, 2, 0x3333_3333);
                    swap32(f, 4, 0x0f0f_0f0f);
                    swap32(f, 8, 0x00ff_00ff);
                    f.i32_const(16).op(op::I32_ROTL);
                }
                Dp1Op::Rev16 => swap32(f, 8, 0x00ff_00ff),
                Dp1Op::Rev32 | Dp1Op::Rev64 => {
                    swap32(f, 8, 0x00ff_00ff);
                    f.i32_const(16).op(op::I32_ROTL);
                }
                Dp1Op::Clz => {
                    f.op(op::I32_CLZ);
                }
                Dp1Op::Cls => {
                    f.local_tee(t32(0)).local_get(t32(0)).i32_const(1).op(op::I32_SHR_U).op(op::I32_XOR);
                    f.i32_const((u32::MAX >> 1) as i32).op(op::I32_AND).op(op::I32_CLZ);
                    f.i32_const(1).op(op::I32_SUB);
                }
            }
            f.op(op::I64_EXTEND_I32_U);
        }
    }

    /// Dp2 con x in `t64(0)` e y in `t64(1)` (già troncati): risultato
    /// sullo stack.
    fn dp2(&mut self, sf: bool, dop: Dp2Op) {
        let (x, y) = (t64(0), t64(1));
        let f = &mut self.f;
        match dop {
            Dp2Op::Udiv => {
                f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                f.i64_const(0).else_();
                f.local_get(x).local_get(y).op(op::I64_DIV_U).end();
            }
            Dp2Op::Sdiv => {
                if sf {
                    f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                    f.i64_const(0).else_();
                    // MIN / -1 = MIN (wrapping_div)
                    f.local_get(x).i64_const(i64::MIN).op(op::I64_EQ);
                    f.local_get(y).i64_const(-1).op(op::I64_EQ).op(op::I32_AND);
                    f.if_(ValType::I64 as u8).local_get(x).else_();
                    f.local_get(x).local_get(y).op(op::I64_DIV_S).end();
                    f.end();
                } else {
                    f.local_get(y).op(op::I64_EQZ).if_(ValType::I64 as u8);
                    f.i64_const(0).else_();
                    f.local_get(x).op(op::I64_EXTEND32_S).local_get(y).op(op::I64_EXTEND32_S);
                    f.op(op::I64_DIV_S).end();
                }
            }
            Dp2Op::Lslv | Dp2Op::Lsrv | Dp2Op::Asrv | Dp2Op::Rorv => {
                // Il conteggio modulo la dimensione coincide con la maschera
                // del conteggio delle istruzioni WASM.
                if sf {
                    f.local_get(x).local_get(y);
                    f.op(match dop {
                        Dp2Op::Lslv => op::I64_SHL,
                        Dp2Op::Lsrv => op::I64_SHR_U,
                        Dp2Op::Asrv => op::I64_SHR_S,
                        _ => op::I64_ROTR,
                    });
                } else {
                    f.local_get(x).op(op::I32_WRAP_I64).local_get(y).op(op::I32_WRAP_I64);
                    f.op(match dop {
                        Dp2Op::Lslv => op::I32_SHL,
                        Dp2Op::Lsrv => op::I32_SHR_U,
                        Dp2Op::Asrv => op::I32_SHR_S,
                        _ => op::I32_ROTR,
                    });
                    f.op(op::I64_EXTEND_I32_U);
                }
            }
            Dp2Op::Crc32 { .. } => unreachable!("CRC32 non si traduce"),
        }
    }

    /// Dp3 con a in `t64(0)`, m in `t64(1)`, n in `t64(2)`: risultato
    /// sullo stack (non troncato).
    fn dp3(&mut self, dop: Dp3Op) {
        let (a, m, n) = (t64(0), t64(1), t64(2));
        match dop {
            Dp3Op::Madd | Dp3Op::Msub => {
                let f = &mut self.f;
                f.local_get(a).local_get(n).local_get(m).op(op::I64_MUL);
                f.op(if dop == Dp3Op::Madd { op::I64_ADD } else { op::I64_SUB });
            }
            Dp3Op::Smaddl | Dp3Op::Smsubl | Dp3Op::Umaddl | Dp3Op::Umsubl => {
                let signed = matches!(dop, Dp3Op::Smaddl | Dp3Op::Smsubl);
                let f = &mut self.f;
                f.local_get(a);
                for r in [n, m] {
                    f.local_get(r);
                    if signed {
                        f.op(op::I64_EXTEND32_S);
                    } else {
                        f.i64_const(0xffff_ffff).op(op::I64_AND);
                    }
                }
                f.op(op::I64_MUL);
                f.op(if matches!(dop, Dp3Op::Smaddl | Dp3Op::Umaddl) { op::I64_ADD } else { op::I64_SUB });
            }
            Dp3Op::Umulh => self.umulh(n, m),
            Dp3Op::Smulh => {
                // smulh = umulh - (n < 0 ? m : 0) - (m < 0 ? n : 0)
                self.umulh(n, m);
                let f = &mut self.f;
                f.local_get(m).local_get(n).i64_const(63).op(op::I64_SHR_S).op(op::I64_AND).op(op::I64_SUB);
                f.local_get(n).local_get(m).i64_const(63).op(op::I64_SHR_S).op(op::I64_AND).op(op::I64_SUB);
            }
        }
    }

    /// Parte alta del prodotto senza segno a 128 bit di `a` e `b`, sullo
    /// stack. Usa i temporanei 3..=8.
    fn umulh(&mut self, a: u32, b: u32) {
        let (a0, a1, b0, b1, mid, p) = (t64(3), t64(4), t64(5), t64(6), t64(7), t64(8));
        let f = &mut self.f;
        let lo = |f: &mut Func, v: u32, d: u32| {
            f.local_get(v).i64_const(0xffff_ffff).op(op::I64_AND).local_set(d);
        };
        let hi = |f: &mut Func, v: u32, d: u32| {
            f.local_get(v).i64_const(32).op(op::I64_SHR_U).local_set(d);
        };
        lo(f, a, a0);
        hi(f, a, a1);
        lo(f, b, b0);
        hi(f, b, b1);
        // mid = (a0*b0 >> 32) + lo(a0*b1) + lo(a1*b0)
        f.local_get(a0).local_get(b0).op(op::I64_MUL).i64_const(32).op(op::I64_SHR_U);
        f.local_get(a0).local_get(b1).op(op::I64_MUL).local_tee(p).i64_const(0xffff_ffff).op(op::I64_AND);
        f.op(op::I64_ADD);
        f.local_get(a1).local_get(b0).op(op::I64_MUL).local_tee(mid).i64_const(0xffff_ffff).op(op::I64_AND);
        f.op(op::I64_ADD);
        // stack: mid ; p = a0*b1, mid(local) = a1*b0
        // hi = a1*b1 + (p >> 32) + (a1*b0 >> 32) + (mid >> 32)
        f.i64_const(32).op(op::I64_SHR_U);
        f.local_get(a1).local_get(b1).op(op::I64_MUL).op(op::I64_ADD);
        f.local_get(p).i64_const(32).op(op::I64_SHR_U).op(op::I64_ADD);
        f.local_get(mid).i64_const(32).op(op::I64_SHR_U).op(op::I64_ADD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_cpu::decode;

    fn validate(bytes: &[u8]) {
        wasmparser::Validator::new().validate_all(bytes).expect("modulo WASM non valido");
    }

    /// Ogni istruzione traducibile produce un modulo valido. Le codifiche
    /// coprono tutte le varianti del decoder per le classi tradotte.
    #[test]
    fn every_translatable_encoding_validates() {
        // Scorre codifiche pseudo-casuali: per ognuna traducibile, un blocco
        // con quella sola istruzione (più una di chiusura) deve validare.
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut count = 0;
        let mut blocks = Vec::new();
        while count < 4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = seed as u32;
            let insn = decode(w);
            if kind(&insn) == Kind::Unsupported {
                continue;
            }
            count += 1;
            blocks.push(Block { pc: 0x40_0000 + 4 * count, insns: vec![insn] });
            if blocks.len() == 64 {
                validate(&module(&blocks, MemoryImport { min: 1, shared_max: None }));
                blocks.clear();
            }
        }
    }

    #[test]
    fn max_steps_excludes_final_svc() {
        let b = Block { pc: 0, insns: vec![Insn::Nop, Insn::Svc { imm: 0 }] };
        assert_eq!(b.max_steps(), 1);
        let b = Block { pc: 0, insns: vec![Insn::Nop, Insn::Nop] };
        assert_eq!(b.max_steps(), 2);
    }
}
