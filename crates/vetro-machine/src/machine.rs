//! La macchina: CPU, MMU e scheda, con il ciclo di esecuzione.

use core::cell::RefCell;

use vetro_cpu::sys::SysEvent;
use vetro_cpu::{Cpu, SysConfig};
use vetro_mmu::{Mmu, MmuBus};
use vetro_platform::{VirtDtbConfig, map, virt_dtb};

use crate::board::{Board, Env, Phys};
use crate::boot::{self, BootError, BootPlan, RamConfig};
use crate::psci::{self, Call};

/// Bit di indirizzo fisico della Cortex-A53 (ID_AA64MMFR0.PARange = 40 bit).
const PA_BITS: u32 = 40;

/// Configurazione della macchina.
#[derive(Clone, Debug)]
pub struct MachineConfig {
    /// RAM da `0x4000_0000`.
    pub ram_size: u64,
    /// Ora iniziale dell'RTC (secondi dall'epoca): tempo esterno, fissato.
    pub now_secs: u64,
    /// Seme della casualità offerta al guest (`rng-seed` del device tree).
    pub seed: u64,
}

impl Default for MachineConfig {
    fn default() -> Self {
        // 1 GiB come il `-m 1G` del test di riferimento; ora fissa come il
        // tempo virtuale del livello user mode.
        MachineConfig { ram_size: 1 << 30, now_secs: 1_767_225_600, seed: 0x5645_5452_4f00_0001 }
    }
}

/// Perché [`Machine::run`] si è fermata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Esaurito il quanto di istruzioni: si può continuare.
    Budget,
    /// PSCI SYSTEM_OFF (o CPU_OFF dell'unica CPU).
    PowerOff,
    /// PSCI SYSTEM_RESET.
    Reset,
    /// WFI senza interrupt possibili né scadenze del timer: il guest aspetta
    /// un ingresso (per esempio dalla console).
    Idle,
    /// Istruzione o configurazione che Vetro non implementa.
    Unimplemented { pc: u64, raw: u32, what: &'static str },
}

pub struct Machine {
    pub cpu: Cpu,
    pub mmu: Mmu,
    pub board: RefCell<Board>,
    seed: u64,
    /// Istruzioni eseguite (e passi di tempo saltati nelle WFI): l'orologio.
    pub steps: u64,
    /// Prossimo CNTPCT a cui il timer cambia livello (cache).
    timer_deadline: Option<u64>,
}

/// CNTPCT dopo `steps` istruzioni: 62,5 MHz su 100 MHz nominali.
fn counter(steps: u64) -> u64 {
    steps / 8 * 5 + steps % 8 * 5 / 8
}

/// Primo numero di istruzioni a cui CNTPCT vale almeno `c`.
fn steps_for(c: u64) -> u64 {
    let mut s = c / 5 * 8;
    while counter(s) < c {
        s += 1;
    }
    s
}

impl Machine {
    pub fn new(cfg: &MachineConfig) -> Self {
        let mut cpu = Cpu::new();
        cpu.reset_system(SysConfig::default());
        Machine {
            cpu,
            mmu: Mmu::new(PA_BITS),
            board: RefCell::new(Board::new(cfg.ram_size, cfg.now_secs)),
            steps: 0,
            timer_deadline: None,
            seed: cfg.seed,
        }
    }

    /// Carica un kernel Linux arm64 (`Image`) con initramfs e riga di comando,
    /// come `-kernel/-initrd/-append` di QEMU: copia i pezzi in RAM, genera
    /// il device tree e prepara i registri d'ingresso.
    pub fn load_linux(
        &mut self,
        image: &[u8],
        initrd: Option<&[u8]>,
        bootargs: &str,
    ) -> Result<BootPlan, BootError> {
        let ram_size = self.board.borrow().ram.bytes.len() as u64;
        let ram = RamConfig::virt(ram_size);
        let initrd_len = initrd.map(|i| i.len() as u64);
        // La posizione dell'initramfs non dipende dal DTB: un primo piano la
        // fissa, poi si genera il DTB e si rifà il piano con la sua lunghezza.
        let first = boot::plan(ram, image, initrd_len, 0)?;
        let dtb = virt_dtb(&VirtDtbConfig {
            ram_size,
            bootargs: bootargs.to_string(),
            initrd: first.initrd.map(|r| (r.addr, r.end())),
            seed: Some(self.seed),
            pad_to: vetro_platform::fdt::QEMU_FDT_SIZE,
            ..VirtDtbConfig::default()
        });
        let plan = boot::plan(ram, image, initrd_len, dtb.len() as u64)?;
        {
            let mut b = self.board.borrow_mut();
            for (pa, bytes) in plan.segments(image, initrd, &dtb) {
                assert!(b.ram.write(pa, bytes), "segmento fuori dalla RAM: il piano lo esclude");
            }
        }
        let e = plan.entry;
        self.cpu.reset_system(SysConfig::default());
        self.cpu.pc = e.pc;
        self.cpu.x = [0; 31];
        self.cpu.x[..4].copy_from_slice(&e.x);
        Ok(plan)
    }

    /// Accoda byte sulla console (PL011) come se arrivassero dalla tastiera.
    pub fn console_input(&mut self, bytes: &[u8]) {
        let mut b = self.board.borrow_mut();
        b.virt.uart_mut().push_input(bytes);
        b.irq_dirty = true;
    }

    /// Consuma l'uscita della console.
    pub fn console_output(&mut self) -> Vec<u8> {
        self.board.borrow_mut().virt.uart_mut().take_output()
    }

    /// Tempo del guest in nanosecondi (10 ns per istruzione).
    pub fn guest_ns(&self) -> u64 {
        self.steps * 10
    }

    fn sync_irqs(&mut self) {
        let mut b = self.board.borrow_mut();
        b.cntpct = counter(self.steps);
        if b.virtio_dirty {
            b.service_virtio();
        }
        b.update_irqs();
        self.timer_deadline = b.virt.timer.next_deadline(b.cntpct);
    }

    /// Esegue al più `budget` istruzioni.
    pub fn run(&mut self, budget: u64) -> Stop {
        let end = self.steps.saturating_add(budget);
        self.sync_irqs();
        while self.steps < end {
            let now = counter(self.steps);
            {
                let mut b = self.board.borrow_mut();
                b.cntpct = now;
                let crossed = self.timer_deadline.is_some_and(|d| now >= d);
                if b.irq_dirty || b.virtio_dirty || crossed {
                    drop(b);
                    self.sync_irqs();
                }
            }
            let ev = {
                let mut phys = Phys(&self.board);
                let mut bus = MmuBus::new(&mut self.mmu, &mut phys);
                let mut env = Env(&self.board);
                self.cpu.step_system(&mut bus, &mut env)
            };
            self.steps += 1;
            match ev {
                SysEvent::Executed | SysEvent::Exception { .. } => {}
                SysEvent::WaitForInterrupt => {
                    if let Some(stop) = self.wait_for_interrupt() {
                        return stop;
                    }
                }
                SysEvent::Hvc(_) | SysEvent::Smc(_) => {
                    let x = [self.cpu.x[0], self.cpu.x[1], self.cpu.x[2], self.cpu.x[3]];
                    match psci::call(x, self.cpu.sys.cfg.mpidr) {
                        Call::Ret(v) => self.cpu.x[0] = v as u64,
                        Call::Suspend => {
                            self.cpu.x[0] = 0;
                            if let Some(stop) = self.wait_for_interrupt() {
                                return stop;
                            }
                        }
                        Call::Off => return Stop::PowerOff,
                        Call::SystemReset => return Stop::Reset,
                    }
                }
                SysEvent::Unimplemented { raw, what } => {
                    self.steps -= 1;
                    return Stop::Unimplemented { pc: self.cpu.pc, raw, what };
                }
            }
        }
        Stop::Budget
    }

    /// WFI: se nessun interrupt è pronto il tempo salta alla prossima
    /// scadenza del timer; senza scadenze la macchina è inattiva.
    fn wait_for_interrupt(&mut self) -> Option<Stop> {
        self.sync_irqs();
        // Come una CPU vera, la WFI finisce solo con un interrupt: dati in
        // arrivo sulla UART senza il suo interrupt abilitato non la svegliano.
        if self.board.borrow().virt.irq_line() {
            return None;
        }
        match self.timer_deadline {
            Some(d) => {
                self.steps = self.steps.max(steps_for(d));
                self.sync_irqs();
                None
            }
            None => Some(Stop::Idle),
        }
    }

    /// Indirizzo fisico di inizio della RAM.
    pub fn ram_base() -> u64 {
        map::RAM_BASE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WFI con byte in arrivo sulla UART ma senza il suo interrupt: nessun
    /// risveglio, e senza scadenze del timer la macchina è inattiva (prima
    /// girava a vuoto un'istruzione alla volta).
    #[test]
    fn wfi_non_si_sveglia_senza_interrupt() {
        let mut m = Machine::new(&MachineConfig { ram_size: 1 << 20, ..MachineConfig::default() });
        let code = [0xd503_207fu32, 0x1400_0000]; // wfi; b .
        {
            let mut b = m.board.borrow_mut();
            for (i, w) in code.iter().enumerate() {
                assert!(b.ram.write(map::RAM_BASE + 4 * i as u64, &w.to_le_bytes()));
            }
        }
        m.cpu.pc = map::RAM_BASE;
        m.console_input(b"x");
        assert_eq!(m.run(1_000_000), Stop::Idle);
        assert!(m.steps < 10, "si ferma subito, non esaurisce il quanto");
    }

    #[test]
    fn orologio_a_62_5_mhz() {
        assert_eq!(counter(8), 5);
        assert_eq!(counter(100_000_000), 62_500_000);
        for c in [0, 1, 4, 5, 6, 999, 62_500_000] {
            let s = steps_for(c);
            assert!(counter(s) >= c && (s == 0 || counter(s - 1) < c), "c = {c}");
        }
    }
}
