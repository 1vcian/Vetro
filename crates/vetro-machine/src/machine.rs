//! La macchina: CPU, MMU e scheda, con il ciclo di esecuzione.

use core::cell::RefCell;

use vetro_cpu::sys::{CpuEnv, SysEvent};
use vetro_cpu::{Cpu, SysConfig};
use vetro_jit::{Next, SysJitDyn, SysJitStats};
use vetro_mmu::{Mmu, MmuBus};
use vetro_platform::virtio::{GpuConfig, InputConfig, MemDisplay, VirtioGpu, VirtioInput, VirtioVsock};
use vetro_platform::{VirtDtbConfig, VirtioDevice, map, virt_dtb};

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

/// Il dispositivo di puntamento assoluto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pointer {
    /// `virtio-tablet-device` di QEMU: puntatore assoluto con pulsanti.
    Tablet,
    /// `virtio-multitouch-device` di QEMU: touchscreen diretto a più
    /// contatti (quello che vuole Android).
    Multitouch,
}

/// Dispositivi virtio-mmio della macchina, oltre a GIC, UART e RTC.
///
/// Si montano in quest'ordine, ciascuno nello slot libero più alto (come i
/// `-device` di QEMU in ordine di riga di comando): GPU nello slot 31,
/// tastiera nel 30, puntatore nel 29, vsock nel successivo libero. Il
/// default è quello del test di avvio confrontato con QEMU
/// (`tests/boot/src/lib.rs`, `QEMU_MACHINE`): GPU 1280x800, tastiera e
/// tablet, senza vsock (QEMU in container non ha vhost-vsock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Devices {
    pub gpu: Option<GpuConfig>,
    pub keyboard: bool,
    pub pointer: Option<Pointer>,
    /// CID del guest, se c'è virtio-vsock.
    pub vsock_cid: Option<u64>,
}

impl Default for Devices {
    fn default() -> Self {
        Devices {
            gpu: Some(GpuConfig::default()),
            keyboard: true,
            pointer: Some(Pointer::Tablet),
            vsock_cid: None,
        }
    }
}

impl Devices {
    /// Nessun dispositivo virtio (la macchina di M3).
    pub fn none() -> Self {
        Devices { gpu: None, keyboard: false, pointer: None, vsock_cid: None }
    }
}

/// Slot virtio-mmio dei dispositivi montati.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    pub gpu: Option<u32>,
    pub keyboard: Option<u32>,
    pub pointer: Option<u32>,
    pub vsock: Option<u32>,
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
    slots: Slots,
    /// Il JIT, se attivo ([`Machine::set_jit`]).
    jit: Option<Box<dyn SysJitDyn>>,
    /// Che cosa fa l'interprete prima di richiamare il JIT: niente
    /// (`Jit`), un'istruzione (`One`), fino al prossimo salto (`Cold`).
    interp: Next,
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
    /// Macchina con i dispositivi di default ([`Devices::default`]).
    pub fn new(cfg: &MachineConfig) -> Self {
        Self::with_devices(cfg, &Devices::default())
    }

    /// Macchina con i dispositivi virtio scelti. La GPU parte con un
    /// [`MemDisplay`]; il browser lo sostituisce con `VirtioGpu::set_backend`
    /// (da [`Machine::gpu`]).
    pub fn with_devices(cfg: &MachineConfig, devices: &Devices) -> Self {
        let mut cpu = Cpu::new();
        cpu.reset_system(SysConfig::default());
        let mut board = Board::new(cfg.ram_size, cfg.now_secs);
        let mut slots = Slots::default();
        let mut attach = |dev: Box<dyn VirtioDevice>| {
            Some(
                board.virt.attach_virtio_next(dev).expect("32 slot bastano per i dispositivi della macchina"),
            )
        };
        if let Some(g) = &devices.gpu {
            slots.gpu = attach(Box::new(VirtioGpu::new(Box::new(MemDisplay::default()), g.clone())));
        }
        if devices.keyboard {
            slots.keyboard = attach(Box::new(VirtioInput::new(InputConfig::keyboard())));
        }
        if let Some(p) = devices.pointer {
            let cfg = match p {
                Pointer::Tablet => InputConfig::tablet(),
                Pointer::Multitouch => InputConfig::multitouch(),
            };
            slots.pointer = attach(Box::new(VirtioInput::new(cfg)));
        }
        if let Some(cid) = devices.vsock_cid {
            slots.vsock = attach(Box::new(VirtioVsock::new(cid)));
        }
        Machine {
            cpu,
            mmu: Mmu::new(PA_BITS),
            board: RefCell::new(board),
            steps: 0,
            timer_deadline: None,
            seed: cfg.seed,
            slots,
            jit: None,
            interp: Next::Jit,
        }
    }

    /// Attiva (o toglie) il JIT della modalità sistema. Il risultato
    /// dell'esecuzione non cambia: stesse istruzioni, stessi interrupt negli
    /// stessi punti, stessa uscita (vedi `vetro_jit::sys`).
    pub fn set_jit(&mut self, jit: Option<Box<dyn SysJitDyn>>) {
        self.jit = jit;
        self.interp = Next::Jit;
    }

    /// Contatori del JIT, se attivo.
    pub fn jit_stats(&self) -> Option<SysJitStats> {
        self.jit.as_ref().map(|j| j.stats())
    }

    /// Slot dei dispositivi virtio montati.
    pub fn slots(&self) -> Slots {
        self.slots
    }

    /// Agisce sul dispositivo virtio dello slot `slot`, di tipo `T`. Il
    /// dispositivo viene servito prima della prossima istruzione (eventi,
    /// dati, cambi di configurazione dell'host arrivano al guest). È il
    /// punto d'ingresso dell'host verso i dispositivi: va registrato per il
    /// replay (M10).
    pub fn device<T: VirtioDevice, R>(
        &mut self,
        slot: Option<u32>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let mut b = self.board.borrow_mut();
        let d = b.virt.virtio_mut(slot?)?.device_as_mut::<T>()?;
        let r = f(d);
        b.virtio_dirty = true;
        Some(r)
    }

    /// La GPU, se c'è.
    pub fn gpu<R>(&mut self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Option<R> {
        self.device(self.slots.gpu, f)
    }

    /// La tastiera, se c'è.
    pub fn keyboard<R>(&mut self, f: impl FnOnce(&mut VirtioInput) -> R) -> Option<R> {
        self.device(self.slots.keyboard, f)
    }

    /// Il tablet o il touchscreen, se c'è.
    pub fn pointer<R>(&mut self, f: impl FnOnce(&mut VirtioInput) -> R) -> Option<R> {
        self.device(self.slots.pointer, f)
    }

    /// virtio-vsock, se c'è.
    pub fn vsock<R>(&mut self, f: impl FnOnce(&mut VirtioVsock) -> R) -> Option<R> {
        self.device(self.slots.vsock, f)
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
        let ram_size = self.board.borrow().ram.size();
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

    /// Passi che il JIT può eseguire adesso senza cambiare nulla rispetto
    /// all'interprete, fino a `end`: nessuno se l'interprete prenderebbe
    /// un interrupt (o PSTATE.IL, o PC non allineato), altrimenti fino alla
    /// prossima scadenza del timer (lì l'interprete aggiorna le linee di
    /// interrupt prima dell'istruzione).
    fn jit_budget(&mut self, end: u64) -> Option<u64> {
        let s = &self.cpu.sys;
        if s.il || self.cpu.pc & 3 != 0 {
            return None;
        }
        // PSTATE.I e PSTATE.A (bit 7 e 8 di DAIF), come `take_interrupt`.
        if s.daif & 1 << 7 == 0 && Env(&self.board).irq_line() {
            return None;
        }
        if s.daif & 1 << 8 == 0 && s.serror_pending.is_some() {
            return None;
        }
        let mut limit = end - self.steps;
        if let Some(d) = self.timer_deadline {
            let at = steps_for(d);
            if at <= self.steps {
                return None;
            }
            limit = limit.min(at - self.steps);
        }
        Some(limit)
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
            if self.interp == Next::Jit
                && self.jit.is_some()
                && let Some(limit) = self.jit_budget(end)
            {
                let jit = self.jit.as_mut().expect("controllato sopra");
                let mut phys = Phys(&self.board);
                let r = jit.run(&mut self.cpu, &mut self.mmu, &mut phys, limit);
                self.steps += r.steps;
                self.interp = if r.next == Next::Jit && r.steps == 0 { Next::One } else { r.next };
                if r.steps > 0 {
                    continue;
                }
            }
            let old_pc = self.cpu.pc;
            let ev = {
                let mut phys = Phys(&self.board);
                let mut bus = MmuBus::new(&mut self.mmu, &mut phys);
                let mut env = Env(&self.board);
                self.cpu.step_system(&mut bus, &mut env)
            };
            self.steps += 1;
            if self.interp == Next::Cold {
                // Fino al prossimo salto (o cambio di pagina, o evento).
                let next = old_pc.wrapping_add(4);
                if !(ev == SysEvent::Executed && self.cpu.pc == next && next >> 12 == old_pc >> 12) {
                    self.interp = Next::Jit;
                }
            } else {
                self.interp = Next::Jit;
            }
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

    /// Dispositivi negli slot dei `-device` di QEMU, nello stesso ordine
    /// (tests/boot/src/lib.rs, `QEMU_MACHINE`); l'host li raggiunge per tipo
    /// e ogni accesso li fa servire.
    #[test]
    fn dispositivi_negli_slot_di_qemu() {
        let cfg = MachineConfig { ram_size: 1 << 20, ..MachineConfig::default() };
        let m = Machine::new(&cfg);
        assert_eq!(m.slots(), Slots { gpu: Some(31), keyboard: Some(30), pointer: Some(29), vsock: None });
        let devices =
            Devices { pointer: Some(Pointer::Multitouch), vsock_cid: Some(5), ..Devices::default() };
        let mut m = Machine::with_devices(&cfg, &devices);
        assert_eq!(m.slots().vsock, Some(28));
        assert_eq!(m.vsock(|v| v.guest_cid()), Some(5));
        assert_eq!(m.gpu(|g| g.resource_count()), Some(0));
        m.board.borrow_mut().virtio_dirty = false;
        m.pointer(|p| p.touch(0, Some((1, 2))));
        assert!(m.board.borrow().virtio_dirty, "l'host ha toccato un dispositivo");
        assert_eq!(m.pointer(|p| p.config().clone()), Some(InputConfig::multitouch()));
        let m = Machine::with_devices(&cfg, &Devices::none());
        assert_eq!(m.slots(), Slots::default());
        assert!(m.board.borrow().virt.virtio(31).unwrap().device().is_none());
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
