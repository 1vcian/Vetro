//! Test per istruzione.
//!
//! Ogni caso descrive stato iniziale, istruzioni e stato atteso. `run()`
//! esegue il programma su Vetro e verifica le attese; se l'oracolo è
//! disponibile lo esegue anche su QEMU e pretende uno stato finale
//! identico, così anche i valori attesi scritti a mano sono verificati.
//!
//! Le codifiche vengono da `tools/a64asm.sh` (assembler vero); il commento
//! accanto a ogni parola è l'istruzione assemblata.
//!
//! Convenzione: x28 punta al centro del blocco di memoria e il corpo non
//! deve lasciarlo modificato; gli offset di memoria sono relativi a x28.

use vetro_diff::harness::{
    BASE_PTR, Dump, MEM_SIZE, PROLOGUE_LEN, Program, Run, compare, run_qemu, run_vetro,
};
use vetro_diff::qemu;

pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGBUS: i32 = 7;
pub const SIGSEGV: i32 = 11;

/// Indirizzo della prima istruzione del corpo (per ADR, BL, salti).
pub const BODY: u64 = vetro_diff::elf::BASE + vetro_diff::elf::CODE_OFFSET + PROLOGUE_LEN as u64 * 4;

/// Indirizzo corrispondente all'offset `off` rispetto a x28.
pub const fn at(off: i64) -> u64 {
    BASE_PTR.wrapping_add(off as u64)
}

pub struct Case {
    name: String,
    program: Program,
    want_x: Vec<(usize, u64)>,
    want_sp: Option<u64>,
    want_flags: Option<u32>,
    want_mem: Vec<(i64, Vec<u8>)>,
    want_signal: Option<i32>,
    want_v: Vec<(usize, u128)>,
    want_fpsr: Option<u32>,
}

pub fn case(name: &str, body: &[u32]) -> Case {
    Case {
        name: name.into(),
        program: Program::new(body.to_vec()),
        want_x: Vec::new(),
        want_sp: None,
        want_flags: None,
        want_mem: Vec::new(),
        want_signal: None,
        want_v: Vec::new(),
        want_fpsr: None,
    }
}

fn mem_index(off: i64, len: usize) -> usize {
    let i = (MEM_SIZE as i64 / 2 + off) as usize;
    assert!(i + len <= MEM_SIZE, "offset {off} fuori dal blocco di memoria");
    i
}

impl Case {
    pub fn x(mut self, r: usize, v: u64) -> Self {
        assert!(r != 28, "x28 è riservato alla base della memoria");
        self.program.x[r] = v;
        self
    }

    /// Valore iniziale di un registro vettoriale.
    pub fn v(mut self, r: usize, val: u128) -> Self {
        self.program.v[r] = val;
        self
    }

    pub fn fpcr(mut self, val: u32) -> Self {
        self.program.fpcr = val;
        self
    }

    pub fn want_v(mut self, r: usize, val: u128) -> Self {
        self.want_v.push((r, val));
        self
    }

    /// FPSR atteso (flag cumulativi).
    pub fn want_fpsr(mut self, val: u32) -> Self {
        self.want_fpsr = Some(val);
        self
    }

    /// Flag iniziali come NZCV a 4 bit (es. `0b0110` = Z e C).
    pub fn flags(mut self, nzcv: u32) -> Self {
        self.program.nzcv = nzcv << 28;
        self
    }

    pub fn mem(mut self, off: i64, bytes: &[u8]) -> Self {
        let i = mem_index(off, bytes.len());
        self.program.mem[i..i + bytes.len()].copy_from_slice(bytes);
        self
    }

    pub fn want_x(mut self, r: usize, v: u64) -> Self {
        self.want_x.push((r, v));
        self
    }

    pub fn want_sp(mut self, v: u64) -> Self {
        self.want_sp = Some(v);
        self
    }

    pub fn want_flags(mut self, nzcv: u32) -> Self {
        self.want_flags = Some(nzcv);
        self
    }

    pub fn want_mem(mut self, off: i64, bytes: &[u8]) -> Self {
        mem_index(off, bytes.len());
        self.want_mem.push((off, bytes.to_vec()));
        self
    }

    pub fn want_signal(mut self, sig: i32) -> Self {
        self.want_signal = Some(sig);
        self
    }

    pub fn run(self) {
        let image = self.program.build();
        let ours = run_vetro(&image);
        self.check(&ours);
        if let Some(q) = qemu::locate_or_skip(&self.name) {
            let slug: String =
                self.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let theirs = run_qemu(&q, &format!("isa-{slug}"), &image);
            let diff = compare(&ours, &theirs);
            assert!(diff.is_empty(), "{}: Vetro e QEMU divergono\n{diff}", self.name);
        }
    }

    fn check(&self, ours: &Run) {
        let name = &self.name;
        if let Some(sig) = self.want_signal {
            assert_eq!(ours, &Run::Signal(sig), "{name}: atteso segnale {sig}");
            return;
        }
        let d: &Dump = match ours {
            Run::Dump(d) => d,
            other => panic!("{name}: atteso un dump, ottenuto {other:?}"),
        };
        for &(r, v) in &self.want_x {
            assert_eq!(d.x[r], v, "{name}: x{r} = {:#x}, atteso {v:#x}", d.x[r]);
        }
        if let Some(sp) = self.want_sp {
            assert_eq!(d.sp, sp, "{name}: sp = {:#x}, atteso {sp:#x}", d.sp);
        }
        if let Some(f) = self.want_flags {
            assert_eq!(d.nzcv >> 28, f, "{name}: NZCV = {:04b}, atteso {f:04b}", d.nzcv >> 28);
        }
        for &(r, v) in &self.want_v {
            assert_eq!(d.v[r], v, "{name}: v{r} = {:#034x}, atteso {v:#034x}", d.v[r]);
        }
        if let Some(f) = self.want_fpsr {
            assert_eq!(d.fpsr, f, "{name}: FPSR = {:#x}, atteso {f:#x}", d.fpsr);
        }
        for (off, bytes) in &self.want_mem {
            let i = mem_index(*off, bytes.len());
            assert_eq!(&d.mem[i..i + bytes.len()], &bytes[..], "{name}: memoria a x28{off:+}");
        }
        assert_eq!(d.x[28], BASE_PTR, "{name}: il corpo ha lasciato x28 modificato");
    }
}
