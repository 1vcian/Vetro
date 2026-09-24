//! Sonda bare-metal della modalità sistema contro QEMU.
//!
//! `system/probe.bin` (da `system/probe.S`) legge registri ID, maschere di
//! scrittura, sindromi di eccezioni a EL1 e da EL0, AT e fault della MMU, e
//! stampa tutto sulla PL011. `system/probe.expected` è l'uscita registrata
//! da `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53` (versione
//! in `system/probe.qemu`, rigenerabile con `system/build.sh`). Qui la
//! stessa immagine gira su Vetro (CPU in modalità sistema + `vetro-mmu`) e
//! l'uscita deve coincidere riga per riga.

use std::collections::BTreeMap;

use vetro_cpu::sysreg::EnvReg;
use vetro_cpu::{Cpu, CpuEnv, SysConfig, SysEvent};
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
        }
        Ok(())
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

#[test]
fn sonda_di_sistema_uguale_a_qemu() {
    let mut board = Board::default();
    board.write(LOAD, PROBE).unwrap();
    let mut mmu = Mmu::new(Mmu::PA_BITS_CORTEX_A53);
    let mut cpu = Cpu::new();
    cpu.reset_system(SysConfig::default());
    cpu.pc = LOAD;
    let mut done = false;
    for _ in 0..2_000_000 {
        let ev = cpu.step_system(&mut MmuBus::new(&mut mmu, &mut board), &mut NoEnv);
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
    let got = String::from_utf8_lossy(&board.out);
    assert!(done, "la sonda non è arrivata a SYSTEM_OFF; uscita finora:\n{got}");
    let ours: Vec<&str> = got.lines().collect();
    for (i, (g, w)) in ours.iter().zip(EXPECTED.lines()).enumerate() {
        let ctx = ours[i.saturating_sub(3)..(i + 6).min(ours.len())].join("\n");
        assert_eq!(*g, w, "riga {}: Vetro e QEMU divergono; Vetro:\n{ctx}", i + 1);
    }
    assert_eq!(got.lines().count(), EXPECTED.lines().count(), "numero di righe diverso");
}
