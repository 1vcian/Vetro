//! Sonda bare-metal della modalità sistema contro QEMU.
//!
//! `system/probe.bin` (da `system/probe.S`) legge registri ID, maschere di
//! scrittura, sindromi di eccezioni a EL1 e da EL0, AT e fault della MMU, e
//! stampa tutto sulla PL011. `system/probe.expected` è l'uscita registrata
//! da `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53` (versione
//! in `system/probe.qemu`, rigenerabile con `system/build.sh`). Qui la
//! stessa immagine gira su Vetro (CPU in modalità sistema + `vetro-mmu`) e
//! l'uscita deve coincidere riga per riga: con l'interprete e col JIT della
//! modalità sistema (`vetro_jit::SysJit` su wasmtime, soglia 0: ogni blocco
//! si traduce subito), che deve dare anche le stesse istruzioni.

use std::collections::BTreeMap;

use vetro_cpu::sysreg::EnvReg;
use vetro_cpu::{Cpu, CpuEnv, SysConfig, SysEvent};
use vetro_jit::{Next, SysJit, SysJitConfig, SysPhys};
use vetro_jit_native::NativeEngine;
use vetro_mmu::{BusError, Mmu, MmuBus, PhysMemory};

const PROBE: &[u8] = include_bytes!("../system/probe.bin");
const EXPECTED: &str = include_str!("../system/probe.expected");

const LOAD: u64 = 0x4008_0000;
const RAM_BASE: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 128 << 20;
const UART: u64 = 0x0900_0000;

/// RAM di `-m 128M` più il solo registro dati della PL011 (le altre
/// periferiche non servono alla sonda). Il resto dà decode error.
#[derive(Default)]
struct Board {
    pages: BTreeMap<u64, Box<[u8; 4096]>>,
    out: Vec<u8>,
    /// Pagine con codice tradotto dal JIT, e quelle scritte da allora.
    watched: std::collections::BTreeSet<u64>,
    dirty: Vec<u64>,
}

impl Board {
    fn ram(p: u64) -> bool {
        (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&p)
    }
}

impl PhysMemory for Board {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if (UART..UART + 0x1000).contains(&pa) {
            buf.fill(0); // FR = 0: FIFO mai piena
            return Ok(());
        }
        for (k, b) in buf.iter_mut().enumerate() {
            let p = pa + k as u64;
            if !Self::ram(p) {
                return Err(BusError::Decode);
            }
            *b = self.pages.get(&(p >> 12)).map_or(0, |pg| pg[(p & 0xfff) as usize]);
        }
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        if pa == UART {
            self.out.push(data[0]);
            return Ok(());
        }
        if (UART..UART + 0x1000).contains(&pa) {
            return Ok(());
        }
        for (k, &b) in data.iter().enumerate() {
            let p = pa + k as u64;
            if !Self::ram(p) {
                return Err(BusError::Decode);
            }
            self.pages.entry(p >> 12).or_insert_with(|| Box::new([0; 4096]))[(p & 0xfff) as usize] = b;
            if self.watched.remove(&(p >> 12)) {
                self.dirty.push(p >> 12);
            }
        }
        Ok(())
    }
}

/// La stessa memoria vista dal JIT: RAM soltanto (la PL011 va
/// all'interprete), senza TLB software (la RAM non è contigua).
impl SysPhys for Board {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool {
        (Self::ram(pa) && Self::ram(pa + buf.len() as u64 - 1)) && self.read(pa, buf).is_ok()
    }
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        if !(Self::ram(pa) && Self::ram(pa + data.len() as u64 - 1)) {
            return None;
        }
        let before = self.dirty.len();
        self.write(pa, data).ok()?;
        Some(self.dirty.len() > before)
    }
    fn watch_code(&mut self, page: u64) -> bool {
        Self::ram(page << 12) && self.watched.insert(page) | true
    }
    fn is_watched(&self, page: u64) -> bool {
        self.watched.contains(&page)
    }
    fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        out.append(&mut self.dirty);
    }
}

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

const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_NOT_SUPPORTED: u64 = -1i64 as u64;

/// Esegue la sonda (col JIT se `jit`); restituisce l'uscita della PL011 e
/// le istruzioni eseguite fino a SYSTEM_OFF.
fn run_probe(jit: bool) -> (String, u64) {
    let mut board = Board::default();
    board.write(LOAD, PROBE).unwrap();
    let mut mmu = Mmu::new(Mmu::PA_BITS_CORTEX_A53);
    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    cpu.pc = LOAD;
    let mut sj = jit
        .then(|| SysJit::new(NativeEngine::new(), SysJitConfig { hot_threshold: 0, ..Default::default() }));
    let mut next = Next::Jit;
    let mut done = false;
    let mut steps = 0u64;
    const LIMIT: u64 = 2_000_000;
    while steps < LIMIT {
        // Come `Machine::run`: blocchi tradotti fra un passo e l'altro
        // (qui nessun interrupt).
        if let Some(j) = sj.as_mut()
            && next == Next::Jit
            && !cpu.sys.il
            && cpu.pc & 3 == 0
        {
            let r = j.run(&mut cpu, &mut mmu, &mut board, LIMIT - steps);
            steps += r.steps;
            next = if r.next == Next::Jit && r.steps == 0 { Next::One } else { r.next };
            if r.steps > 0 {
                continue;
            }
        }
        let old = cpu.pc;
        let ev = cpu.step_system(&mut MmuBus::new(&mut mmu, &mut board), &mut NoEnv);
        steps += 1;
        next = match next {
            Next::Cold if ev == SysEvent::Executed && cpu.pc == old + 4 && cpu.pc >> 12 == old >> 12 => {
                Next::Cold
            }
            _ => Next::Jit,
        };
        match ev {
            SysEvent::Executed | SysEvent::Exception { .. } | SysEvent::WaitForInterrupt => {}
            SysEvent::Hvc(_) if cpu.x[0] == PSCI_SYSTEM_OFF => {
                done = true;
                break;
            }
            // Come il PSCI di QEMU per una funzione sconosciuta.
            SysEvent::Hvc(_) => cpu.x[0] = PSCI_NOT_SUPPORTED,
            other => panic!("evento inatteso {other:?} a PC {:#x}", cpu.pc),
        }
    }
    let got = String::from_utf8_lossy(&board.out).into_owned();
    assert!(done, "la sonda non è arrivata a SYSTEM_OFF (JIT: {jit}); uscita finora:\n{got}");
    if let Some(j) = &sj {
        let s = j.stats();
        assert!(s.jit_steps > steps / 4, "il JIT deve aver eseguito una parte della sonda: {s:?}");
    }
    (got, steps)
}

#[test]
fn sonda_di_sistema_uguale_a_qemu() {
    let (got, steps) = run_probe(false);
    check(&got);
    let (jit, jit_steps) = run_probe(true);
    assert_eq!(jit_steps, steps, "istruzioni diverse col JIT");
    check(&jit);
}

fn check(got: &str) {
    let ours: Vec<&str> = got.lines().collect();
    for (i, (g, w)) in ours.iter().zip(EXPECTED.lines()).enumerate() {
        let ctx = ours[i.saturating_sub(3)..(i + 6).min(ours.len())].join("\n");
        assert_eq!(*g, w, "riga {}: Vetro e QEMU divergono; Vetro:\n{ctx}", i + 1);
    }
    assert_eq!(got.lines().count(), EXPECTED.lines().count(), "numero di righe diverso");
}
