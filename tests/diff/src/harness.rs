//! Programmi di test con stato iniziale noto e dump dello stato finale.
//!
//! Layout del programma:
//! - **prologo**: SP = [`STACK_TOP`], NZCV, x0–x30 ai valori dati;
//! - **corpo**: le istruzioni sotto test;
//! - **epilogo**: scrive x0–x30, SP e NZCV nel buffer di dump e li manda su
//!   stdout insieme al blocco di memoria, poi `exit(0)`.
//!
//! Convenzioni per il corpo: x28 ([`BASE_REG`]) punta al centro del blocco di
//! memoria e l'epilogo lo usa come base, quindi il corpo non deve lasciarlo
//! modificato. x27 ([`INDEX_REG`]) è un indice piccolo per gli
//! indirizzamenti a registro.

use crate::a64;
use crate::elf::{self, RW_BASE, Rw};

/// Byte del blocco di memoria a [`RW_BASE`], inizializzato dal programma.
pub const MEM_SIZE: usize = 0x1000;
pub const BASE_REG: u32 = 28;
pub const INDEX_REG: u32 = 27;
/// Valore di x28: centro del blocco di memoria.
pub const BASE_PTR: u64 = RW_BASE + 0x800;
const DUMP_OFF: u32 = 0x800; // rispetto a BASE_PTR: RW_BASE + 0x1000
const DUMP_WORDS: usize = 33; // x0..x30, sp, nzcv
pub const STACK_TOP: u64 = RW_BASE + 0x4000;
const RW_MEMSZ: u64 = 0x4000;

#[derive(Clone, Debug)]
pub struct Program {
    pub x: [u64; 31],
    /// Flag nei bit 31:28.
    pub nzcv: u32,
    pub mem: Vec<u8>,
    pub body: Vec<u32>,
}

/// Istruzioni del prologo: il corpo inizia a questo indice.
pub const PROLOGUE_LEN: usize = 4 + 1 + 4 + 1 + 31 * 4;

impl Program {
    pub fn new(body: Vec<u32>) -> Self {
        let mut x = [0u64; 31];
        x[BASE_REG as usize] = BASE_PTR;
        Program { x, nzcv: 0, mem: vec![0; MEM_SIZE], body }
    }

    /// Indice (nell'immagine) dell'istruzione `i` del corpo.
    pub fn body_index(i: usize) -> usize {
        PROLOGUE_LEN + i
    }

    pub fn code(&self) -> Vec<u32> {
        let mut c = Vec::with_capacity(PROLOGUE_LEN + self.body.len() + 64);
        c.extend(a64::mov64(0, STACK_TOP));
        c.push(a64::add_imm(a64::SP, 0, 0));
        c.extend(a64::mov64(0, self.nzcv as u64));
        c.push(a64::msr_nzcv(0));
        for (r, v) in self.x.iter().enumerate() {
            c.extend(a64::mov64(r as u32, *v));
        }
        debug_assert_eq!(c.len(), PROLOGUE_LEN);
        c.extend_from_slice(&self.body);

        let b = BASE_REG;
        for r in 0..31 {
            c.push(a64::str_x(r, b, DUMP_OFF + 8 * r));
        }
        c.push(a64::add_imm(0, a64::SP, 0));
        c.push(a64::str_x(0, b, DUMP_OFF + 8 * 31));
        c.push(a64::mrs_nzcv(0));
        c.push(a64::str_x(0, b, DUMP_OFF + 8 * 32));
        // write(1, dump, 264)
        c.extend([
            a64::movz(0, 1, 0),
            a64::add_imm(1, b, DUMP_OFF),
            a64::movz(2, (DUMP_WORDS * 8) as u16, 0),
            a64::movz(8, a64::sys::WRITE, 0),
            a64::svc(0),
        ]);
        // write(1, mem, MEM_SIZE)
        c.extend([
            a64::movz(0, 1, 0),
            a64::sub_imm(1, b, 0x800),
            a64::movz(2, MEM_SIZE as u16, 0),
            a64::movz(8, a64::sys::WRITE, 0),
            a64::svc(0),
        ]);
        c.extend([a64::movz(0, 0, 0), a64::movz(8, a64::sys::EXIT, 0), a64::svc(0)]);
        c
    }

    pub fn build(&self) -> Vec<u8> {
        assert_eq!(self.mem.len(), MEM_SIZE);
        elf::build_with_rw(&self.code(), &[], Some(Rw { init: &self.mem, memsz: RW_MEMSZ }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dump {
    pub x: [u64; 31],
    pub sp: u64,
    pub nzcv: u32,
    pub mem: Vec<u8>,
}

impl Dump {
    pub fn parse(stdout: &[u8]) -> Option<Dump> {
        if stdout.len() != DUMP_WORDS * 8 + MEM_SIZE {
            return None;
        }
        let w = |i: usize| u64::from_le_bytes(stdout[i * 8..i * 8 + 8].try_into().unwrap());
        let mut x = [0u64; 31];
        for (i, r) in x.iter_mut().enumerate() {
            *r = w(i);
        }
        Some(Dump { x, sp: w(31), nzcv: w(32) as u32, mem: stdout[DUMP_WORDS * 8..].to_vec() })
    }

    /// Differenze leggibili rispetto a `other` (vuoto se identici).
    pub fn diff(&self, other: &Dump) -> String {
        let mut s = String::new();
        for r in 0..31 {
            if self.x[r] != other.x[r] {
                s += &format!("  x{r:<2} vetro={:#018x} qemu={:#018x}\n", self.x[r], other.x[r]);
            }
        }
        if self.sp != other.sp {
            s += &format!("  sp  vetro={:#018x} qemu={:#018x}\n", self.sp, other.sp);
        }
        if self.nzcv != other.nzcv {
            s += &format!("  nzcv vetro={:#x} qemu={:#x}\n", self.nzcv >> 28, other.nzcv >> 28);
        }
        for (i, (a, b)) in self.mem.iter().zip(&other.mem).enumerate() {
            if a != b {
                s += &format!("  mem[{:#x}] vetro={a:#04x} qemu={b:#04x}\n", RW_BASE + i as u64);
            }
        }
        s
    }
}

/// Esito di un'esecuzione, comparabile tra Vetro e QEMU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    Dump(Box<Dump>),
    Signal(i32),
    /// Qualsiasi altro esito (uscita inattesa, syscall mancante, timeout).
    Other(String),
}

pub fn run_vetro(image: &[u8]) -> Run {
    use vetro_cli::user::{Exit, Process};
    let mut p = match Process::load(image, "test") {
        Ok(p) => p,
        Err(e) => return Run::Other(format!("caricamento: {e}")),
    };
    match p.run(1_000_000) {
        Exit::Code(0) => match Dump::parse(&p.stdout) {
            Some(d) => Run::Dump(Box::new(d)),
            None => Run::Other(format!("stdout di {} byte", p.stdout.len())),
        },
        Exit::Signal { signo, .. } => Run::Signal(signo),
        other => Run::Other(format!("{other:?}")),
    }
}

pub fn run_qemu(qemu: &std::path::Path, name: &str, image: &[u8]) -> Run {
    let path = match crate::qemu::write_temp_elf(name, image) {
        Ok(p) => p,
        Err(e) => return Run::Other(format!("scrittura ELF: {e}")),
    };
    let out = match crate::qemu::run(qemu, &path, crate::qemu::DEFAULT_TIMEOUT) {
        Ok(o) => o,
        Err(e) => return Run::Other(format!("qemu: {e}")),
    };
    if let Some(sig) = out.signal() {
        return Run::Signal(sig);
    }
    match (out.exit_code, Dump::parse(&out.stdout)) {
        (Some(0), Some(d)) => Run::Dump(Box::new(d)),
        _ => Run::Other(format!(
            "exit={:?} stdout={}B stderr={}",
            out.exit_code,
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Differenza leggibile tra due esiti (vuota se coincidono).
pub fn compare(vetro: &Run, qemu: &Run) -> String {
    match (vetro, qemu) {
        (Run::Dump(a), Run::Dump(b)) => a.diff(b),
        (a, b) if a == b => String::new(),
        (a, b) => format!("  vetro={}\n  qemu ={}\n", summary(a), summary(b)),
    }
}

fn summary(r: &Run) -> String {
    match r {
        Run::Dump(_) => "uscita regolare con dump".into(),
        Run::Signal(s) => format!("segnale {s}"),
        Run::Other(s) => s.clone(),
    }
}
