//! La macchina: CPU, MMU e scheda, con il ciclo di esecuzione.

use core::cell::RefCell;

use vetro_cpu::sys::{CpuEnv, SysEvent};
use vetro_cpu::{Cpu, SysConfig};
use vetro_jit::{Next, SysJitDyn, SysJitStats};
use vetro_mmu::{Mmu, MmuBus};
use vetro_net::{Sinkhole, Stack};
use vetro_platform::virtio::{
    GpuConfig, InputConfig, MemDisplay, VirtioGpu, VirtioInput, VirtioNet, VirtioVsock,
};
use vetro_platform::{VirtDtbConfig, VirtioDevice, map, virt_dtb};

use crate::board::{Board, Env, Phys};
use crate::boot::{self, BootError, BootPlan, RamConfig};
use crate::net::{self, NetLink, NetSetup, TappedFrame};
use crate::psci::{self, Call};

mod record;
mod snapshot;

pub use record::RecordOptions;

/// Bit di indirizzo fisico della Cortex-A53 (ID_AA64MMFR0.PARange = 40 bit).
const PA_BITS: u32 = 40;

/// Configurazione della macchina.
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// tastiera nel 30, puntatore nel 29, rete nel 28, vsock nel successivo
/// libero. Il default è quello del test di avvio confrontato con QEMU
/// (`tests/boot/src/lib.rs`, `QEMU_MACHINE`): GPU 1280x800, tastiera,
/// tablet e virtio-net con il sinkhole (in QEMU `-netdev user`), senza vsock
/// (QEMU in container non ha vhost-vsock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Devices {
    pub gpu: Option<GpuConfig>,
    pub keyboard: bool,
    pub pointer: Option<Pointer>,
    /// virtio-net con lo stack di `vetro-net` e il sinkhole.
    pub net: Option<NetSetup>,
    /// CID del guest, se c'è virtio-vsock.
    pub vsock_cid: Option<u64>,
}

impl Default for Devices {
    fn default() -> Self {
        Devices {
            gpu: Some(GpuConfig::default()),
            keyboard: true,
            pointer: Some(Pointer::Tablet),
            net: Some(NetSetup::default()),
            vsock_cid: None,
        }
    }
}

impl Devices {
    /// Nessun dispositivo virtio (la macchina di M3).
    pub fn none() -> Self {
        Devices { gpu: None, keyboard: false, pointer: None, net: None, vsock_cid: None }
    }
}

/// Slot virtio-mmio dei dispositivi montati.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    pub gpu: Option<u32>,
    pub keyboard: Option<u32>,
    pub pointer: Option<u32>,
    pub net: Option<u32>,
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
    /// Una richiesta di virtio-blk aspetta dati dall'host
    /// (`BlockError::NotReady`, es. un disco scaricato a pezzi nel browser).
    /// Nessuna istruzione eseguita da quando la richiesta è arrivata: il
    /// tempo del guest è fermo. L'host fornisce i dati al backend (da
    /// [`Machine::device`], che fa servire di nuovo il dispositivo) e
    /// richiama [`Machine::run`]: la richiesta si completa allo stesso
    /// numero di istruzioni di un disco sempre pronto.
    Blocked,
}

pub struct Machine {
    pub cpu: Cpu,
    pub mmu: Mmu,
    pub board: RefCell<Board>,
    seed: u64,
    /// Istruzioni eseguite (e passi di tempo saltati nelle WFI): l'orologio.
    pub steps: u64,
    /// Prossimo CNTPCT a cui qualcosa cambia da sé: il timer cambia
    /// livello o scade un timer dello stack di rete (cache).
    timer_deadline: Option<u64>,
    /// Prossimo CNTPCT a cui chiamare `poll` sullo stack di rete.
    net_deadline: Option<u64>,
    slots: Slots,
    /// Il JIT, se attivo ([`Machine::set_jit`]).
    jit: Option<Box<dyn SysJitDyn>>,
    /// Che cosa fa l'interprete prima di richiamare il JIT: niente
    /// (`Jit`), un'istruzione (`One`), fino al prossimo salto (`Cold`).
    interp: Next,
    /// Una WFI interrotta da [`Stop::Blocked`]: la si riprende prima della
    /// prossima istruzione.
    wfi_pending: bool,
    /// Configurazione e dispositivi con cui è stata costruita (per l'hash
    /// della configurazione negli snapshot).
    cfg: MachineConfig,
    devices: Devices,
    /// Uscita della console tolta dalla UART e non ancora data all'host, con
    /// il conto dei byte (M10).
    console: record::ConsoleTap,
    /// Registrazione o replay in corso (M10, ADR 0019).
    rr: record::Rr,
    /// Esito dell'ultimo replay (anche finito).
    replay_status: Option<crate::record::ReplayStatus>,
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
        if let Some(n) = &devices.net {
            slots.net = attach(Box::new(VirtioNet::new(Box::new(NetLink::new(n)), n.mac)));
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
            net_deadline: None,
            seed: cfg.seed,
            slots,
            jit: None,
            interp: Next::Jit,
            wfi_pending: false,
            cfg: cfg.clone(),
            devices: devices.clone(),
            console: record::ConsoleTap::default(),
            rr: record::Rr::Off,
            replay_status: None,
        }
    }

    /// Attiva (o toglie) il JIT della modalità sistema. Il risultato
    /// dell'esecuzione non cambia: stesse istruzioni, stessi interrupt negli
    /// stessi punti, stessa uscita (vedi `vetro_jit::sys`).
    pub fn set_jit(&mut self, jit: Option<Box<dyn SysJitDyn>>) {
        if jit.is_some() {
            self.rr.note_jit();
        }
        self.jit = jit;
        self.interp = Next::Jit;
    }

    /// Contatori del JIT, se attivo.
    pub fn jit_stats(&self) -> Option<SysJitStats> {
        self.jit.as_ref().map(|j| j.stats())
    }

    /// Istruzioni dell'interprete per classe, se il JIT le conta
    /// (`SysJitConfig::profile`).
    pub fn jit_profile(&self) -> Option<&vetro_jit::Profile> {
        self.jit.as_ref().and_then(|j| j.profile())
    }

    /// Slot dei dispositivi virtio montati.
    pub fn slots(&self) -> Slots {
        self.slots
    }

    /// Agisce sul dispositivo virtio dello slot `slot`, di tipo `T`. Il
    /// dispositivo viene servito prima della prossima istruzione (eventi,
    /// dati, cambi di configurazione dell'host arrivano al guest).
    ///
    /// Una chiusura non si può registrare: durante una registrazione (M10,
    /// ADR 0019) l'accesso finisce nel log come evento opaco, e il replay si
    /// ferma lì. Gli ingressi passano da [`Machine::input`]; le letture da
    /// [`Machine::device_view`]; i dati di un disco atteso da
    /// [`Machine::host_link`].
    pub fn device<T: VirtioDevice, R>(
        &mut self,
        slot: Option<u32>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        self.note_opaque(slot);
        self.device_raw(slot, f)
    }

    /// Come [`Machine::device`], per i collegamenti esterni di un
    /// dispositivo che non sono ingressi del guest: i dati di un disco che
    /// la macchina aspetta ([`Stop::Blocked`], ADR 0014; il tempo del guest
    /// è fermo, e in replay il disco deve dare gli stessi dati). Non si
    /// registra.
    pub fn host_link<T: VirtioDevice, R>(
        &mut self,
        slot: Option<u32>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        self.device_raw(slot, f)
    }

    /// Il dispositivo virtio dello slot `slot` in sola lettura, senza
    /// effetti sulla macchina.
    pub fn device_view<T: VirtioDevice, R>(&self, slot: Option<u32>, f: impl FnOnce(&T) -> R) -> Option<R> {
        let b = self.board.borrow();
        Some(f(b.virt.virtio(slot?)?.device_as::<T>()?))
    }

    /// La GPU in sola lettura (immagine, cursore, risorse).
    pub fn gpu_view<R>(&self, f: impl FnOnce(&VirtioGpu) -> R) -> Option<R> {
        self.device_view(self.slots.gpu, f)
    }

    /// virtio-vsock in sola lettura (stato delle connessioni, byte pronti).
    pub fn vsock_view<R>(&self, f: impl FnOnce(&VirtioVsock) -> R) -> Option<R> {
        self.device_view(self.slots.vsock, f)
    }

    fn device_raw<T: VirtioDevice, R>(
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

    /// Lo stack di rete (con il sinkhole), se c'è virtio-net: registro degli
    /// eventi, connessioni e byte registrati, statistiche. Dopo l'accesso lo
    /// stack viene interrogato (`poll`) prima della prossima istruzione, così
    /// ciò che l'host cambia nell'upstream arriva al guest.
    ///
    /// Come [`Machine::device`], durante una registrazione è un accesso
    /// opaco: le connessioni dell'host passano da [`Machine::input`] con
    /// [`Input::HostNet`](crate::record::Input::HostNet).
    pub fn net<R>(&mut self, f: impl FnOnce(&mut Stack<Sinkhole>) -> R) -> Option<R> {
        self.note_opaque(self.slots.net);
        self.net_raw(f)
    }

    fn net_raw<R>(&mut self, f: impl FnOnce(&mut Stack<Sinkhole>) -> R) -> Option<R> {
        self.net_input_link(|l| f(&mut l.stack))
    }

    /// Il collegamento di rete (stack e frame dell'host) come ingresso: il
    /// dispositivo si serve e lo stack si interroga (`poll`) prima della
    /// prossima istruzione.
    fn net_input_link<R>(&mut self, f: impl FnOnce(&mut NetLink) -> R) -> Option<R> {
        let r =
            self.device_raw(self.slots.net, |d: &mut VirtioNet| d.backend_as_mut::<NetLink>().map(f))??;
        self.net_deadline = Some(0);
        self.timer_deadline = Some(0);
        Some(r)
    }

    /// Lo stack di rete in sola lettura (registro, connessioni, byte del
    /// sinkhole), senza effetti sulla macchina: si può chiamare in qualsiasi
    /// momento senza cambiare l'esecuzione.
    pub fn net_view<R>(&self, f: impl FnOnce(&Stack<Sinkhole>) -> R) -> Option<R> {
        let b = self.board.borrow();
        let d = b.virt.virtio(self.slots.net?)?.device_as::<VirtioNet>()?;
        Some(f(&d.backend_as::<NetLink>()?.stack))
    }

    /// Accende o spegne la cattura dei frame Ethernet al confine di
    /// virtio-net (M7, ADR 0016). Solo osservazione: l'esecuzione resta la
    /// stessa, e la cattura non entra negli snapshot. `false` se la
    /// macchina non ha la rete.
    pub fn net_tap(&mut self, on: bool) -> bool {
        self.net_link(|l| l.set_tap(on)).is_some()
    }

    /// I frame catturati da [`Machine::net_tap`] dall'ultima chiamata, in
    /// ordine, con l'istante in tempo virtuale. Non cambia l'esecuzione.
    pub fn net_tap_take(&mut self) -> Vec<TappedFrame> {
        self.net_link(NetLink::take_tapped).unwrap_or_default()
    }

    /// Il backend di virtio-net senza segnare i dispositivi da servire.
    fn net_link<R>(&mut self, f: impl FnOnce(&mut NetLink) -> R) -> Option<R> {
        let mut b = self.board.borrow_mut();
        let d = b.virt.virtio_mut(self.slots.net?)?.device_as_mut::<VirtioNet>()?;
        Some(f(d.backend_as_mut::<NetLink>()?))
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

    /// Carica kernel, initrd e riga di comando preparati dal bootloader
    /// Android ([`crate::android`]): come [`Machine::load_linux`] con i pezzi
    /// ricavati da `boot.img`, `vendor_boot.img` e `init_boot.img`.
    pub fn load_android(&mut self, boot: &crate::android::AndroidBoot) -> Result<BootPlan, BootError> {
        self.load_linux(&boot.kernel, boot.initrd(), &boot.cmdline)
    }

    /// Accoda byte sulla console (PL011) come se arrivassero dalla tastiera:
    /// [`Machine::input`] con [`Input::Console`](crate::record::Input::Console).
    pub fn console_input(&mut self, bytes: &[u8]) {
        self.input(crate::record::Input::Console(bytes.to_vec()));
    }

    /// Pilota una linea d'ingresso del GPIO PL061 (la 3 è il tasto di
    /// spegnimento): [`Machine::input`] con
    /// [`Input::Gpio`](crate::record::Input::Gpio).
    pub fn gpio_input(&mut self, line: u32, level: bool) {
        self.input(crate::record::Input::Gpio { line, level });
    }

    /// Consuma l'uscita della console.
    pub fn console_output(&mut self) -> Vec<u8> {
        self.drain_console();
        core::mem::take(&mut self.console.buf)
    }

    /// Una richiesta di virtio-blk aspetta dati dall'host ([`Stop::Blocked`]).
    pub fn blocked(&self) -> bool {
        self.board.borrow().host_wait
    }

    /// Tempo del guest in nanosecondi (10 ns per istruzione).
    pub fn guest_ns(&self) -> u64 {
        self.steps * 10
    }

    fn sync_irqs(&mut self) {
        let mut b = self.board.borrow_mut();
        b.cntpct = counter(self.steps);
        let net_due = self.net_deadline.is_some_and(|d| b.cntpct >= d);
        if let Some(slot) = self.slots.net
            && (net_due || b.virtio_dirty)
        {
            let now = net::micros(b.cntpct);
            let link = b
                .virt
                .virtio_mut(slot)
                .and_then(|t| t.device_as_mut::<VirtioNet>())
                .and_then(|d| d.backend_as_mut::<NetLink>())
                .expect("virtio-net con NetLink nello slot della rete");
            link.now = now;
            if net_due {
                link.stack.poll(now);
                if link.stack.pending_frames() > 0 {
                    b.virtio_dirty = true;
                }
            }
        }
        let serviced = b.virtio_dirty;
        if serviced {
            b.service_virtio();
        }
        if let Some(slot) = self.slots.net
            && (net_due || serviced)
        {
            let link = b
                .virt
                .virtio_mut(slot)
                .and_then(|t| t.device_as_mut::<VirtioNet>())
                .and_then(|d| d.backend_as_mut::<NetLink>())
                .expect("virtio-net con NetLink nello slot della rete");
            // Mai nel passato: una scadenza già raggiunta si ripete al
            // prossimo tick del contatore.
            self.net_deadline = link.stack.next_deadline().map(|t| net::counter_at(t).max(b.cntpct + 1));
        }
        b.update_irqs();
        let timer = b.virt.timer.next_deadline(b.cntpct);
        self.timer_deadline = match (timer, self.net_deadline) {
            (Some(a), Some(n)) => Some(a.min(n)),
            (a, n) => a.or(n),
        };
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

    /// Esegue al più `budget` istruzioni. Durante un replay (M10) le
    /// istruzioni si fermano a ogni evento del log per applicarlo, e alla
    /// fine della registrazione ([`Machine::replay_status`]).
    pub fn run(&mut self, budget: u64) -> Stop {
        if self.rr.replaying() {
            return self.run_replay(budget);
        }
        let stop = self.run_quantum(budget);
        if self.rr.recording() {
            self.after_quantum();
        }
        stop
    }

    /// Un quanto di al più `budget` istruzioni, senza registrazione né
    /// replay.
    fn run_quantum(&mut self, budget: u64) -> Stop {
        let end = self.steps.saturating_add(budget);
        self.sync_irqs();
        if self.blocked() {
            return Stop::Blocked;
        }
        if core::mem::take(&mut self.wfi_pending)
            && let Some(stop) = self.wait_for_interrupt()
        {
            return stop;
        }
        while self.steps < end {
            let now = counter(self.steps);
            {
                let mut b = self.board.borrow_mut();
                b.cntpct = now;
                let crossed = self.timer_deadline.is_some_and(|d| now >= d);
                if b.irq_dirty || b.virtio_dirty || crossed {
                    drop(b);
                    self.sync_irqs();
                    if self.blocked() {
                        return Stop::Blocked;
                    }
                }
            }
            if self.interp == Next::Jit
                && self.jit.is_some()
                && let Some(limit) = self.jit_budget(end)
            {
                let jit = self.jit.as_mut().expect("controllato sopra");
                // L'orologio per MRS CNTPCT/CNTVCT dentro le regioni.
                let cntvoff = self.board.borrow().virt.timer.cntvoff;
                jit.set_time(vetro_jit::Clock { steps: self.steps, cntvoff });
                let mut phys = Phys(&self.board);
                let r = jit.run(&mut self.cpu, &mut self.mmu, &mut phys, limit);
                self.steps += r.steps;
                self.interp = if r.next == Next::Jit && r.steps == 0 { Next::One } else { r.next };
                if r.steps > 0 {
                    continue;
                }
            }
            let old_pc = self.cpu.pc;
            if let Some(jit) = self.jit.as_mut()
                && jit.profiling()
            {
                jit.profile_step(&self.cpu, &mut self.mmu, &mut Phys(&self.board));
            }
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
    /// scadenza (timer o stack di rete); senza scadenze la macchina è
    /// inattiva.
    fn wait_for_interrupt(&mut self) -> Option<Stop> {
        self.sync_irqs();
        if self.blocked() {
            self.wfi_pending = true;
            return Some(Stop::Blocked);
        }
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
    use vetro_platform::virtio::{self as vio, BlockBackend, BlockError, MemBackend, VirtioBlk};

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
        assert_eq!(
            m.slots(),
            Slots { gpu: Some(31), keyboard: Some(30), pointer: Some(29), net: Some(28), vsock: None }
        );
        assert_eq!(m.net_view(|s| s.config().guest_ip), Some(std::net::Ipv4Addr::new(10, 0, 2, 15)));
        let devices =
            Devices { pointer: Some(Pointer::Multitouch), vsock_cid: Some(5), ..Devices::default() };
        let mut m = Machine::with_devices(&cfg, &devices);
        assert_eq!(m.slots().vsock, Some(27));
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

    /// Disco che risponde `NotReady` finché l'host non lo apre (come il
    /// disco via HTTP del browser prima dell'arrivo dei dati).
    pub(super) struct Gate {
        pub(super) open: bool,
        pub(super) disk: MemBackend,
    }

    impl BlockBackend for Gate {
        fn size(&self) -> u64 {
            self.disk.size()
        }
        fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if !self.open {
                return Err(BlockError::NotReady);
            }
            self.disk.read_sectors(sector, buf)
        }
        fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError> {
            self.disk.write_sectors(sector, data)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            Ok(())
        }
    }

    const R: u64 = map::RAM_BASE;
    pub(super) const DATA: u64 = R + 0x5000;
    pub(super) const USED: u64 = R + 0x3000;

    /// Una macchina con un virtio-blk già inizializzato (come farebbe il
    /// driver) e una lettura del settore 1 pubblicata nella coda; il codice
    /// notifica la coda e poi conta in x2 all'infinito.
    pub(super) fn blk_machine(open: bool) -> (Machine, u32) {
        let cfg = MachineConfig { ram_size: 1 << 20, ..MachineConfig::default() };
        let mut m = Machine::with_devices(&cfg, &Devices::none());
        let disk = MemBackend::from_vec((0..2048u32).map(|i| (i * 7 + i / 512) as u8).collect());
        let blk = VirtioBlk::new(Box::new(Gate { open, disk }), Default::default());
        let slot = m.board.borrow_mut().virt.attach_virtio_next(Box::new(blk)).unwrap();
        let base = map::VIRTIO_BASE + u64::from(slot) * map::VIRTIO_SLOT_SIZE;
        {
            let mut b = m.board.borrow_mut();
            let mut w = |off: u64, v: u64| assert!(b.virt.bus.write(base + off, 4, v));
            let (ack, drv, fok, dok) = (
                u64::from(vio::STATUS_ACKNOWLEDGE),
                u64::from(vio::STATUS_DRIVER),
                u64::from(vio::STATUS_FEATURES_OK),
                u64::from(vio::STATUS_DRIVER_OK),
            );
            w(vio::STATUS, 0);
            w(vio::STATUS, ack | drv);
            w(vio::DRIVER_FEATURES_SEL, 1);
            w(vio::DRIVER_FEATURES, 1); // VIRTIO_F_VERSION_1
            w(vio::STATUS, ack | drv | fok);
            w(vio::QUEUE_SEL, 0);
            w(vio::QUEUE_NUM, 8);
            w(vio::QUEUE_DESC_LOW, R + 0x1000);
            w(vio::QUEUE_DRIVER_LOW, R + 0x2000);
            w(vio::QUEUE_DEVICE_LOW, USED);
            w(vio::QUEUE_READY, 1);
            w(vio::STATUS, ack | drv | fok | dok);
            let ram = &mut b.ram;
            let desc = |i: u64, addr: u64, len: u32, flags: u16, next: u16| {
                let mut d = [0u8; 16];
                d[0..8].copy_from_slice(&addr.to_le_bytes());
                d[8..12].copy_from_slice(&len.to_le_bytes());
                d[12..14].copy_from_slice(&flags.to_le_bytes());
                d[14..16].copy_from_slice(&next.to_le_bytes());
                (R + 0x1000 + 16 * i, d)
            };
            // IN dal settore 1: intestazione, 512 byte di dati (scrivibili),
            // byte di stato (scrivibile). Flag: 1 = NEXT, 2 = WRITE.
            for (a, d) in
                [desc(0, R + 0x4000, 16, 1, 1), desc(1, DATA, 512, 3, 2), desc(2, R + 0x6000, 1, 2, 0)]
            {
                assert!(ram.write(a, &d));
            }
            let mut hdr = [0u8; 16];
            hdr[8] = 1;
            assert!(ram.write(R + 0x4000, &hdr));
            assert!(ram.write(R + 0x2000, &[0, 0, 1, 0, 0, 0])); // avail: idx 1, ring[0] = 0
            let code: [u32; 3] = [
                0xb9000001, // str w1, [x0]
                0x91000442, // add x2, x2, #0x1
                0x17ffffff, // b .-4
            ];
            for (i, c) in code.iter().enumerate() {
                assert!(ram.write(R + 4 * i as u64, &c.to_le_bytes()));
            }
        }
        m.cpu.pc = R;
        m.cpu.x[0] = base + vio::QUEUE_NOTIFY;
        (m, slot)
    }

    fn ram(m: &Machine, pa: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        assert!(m.board.borrow().ram.read(pa, &mut v));
        v
    }

    /// Un disco non pronto ferma la macchina subito dopo la notifica, senza
    /// far avanzare il tempo; quando l'host apre il disco la richiesta si
    /// completa allo stesso numero di istruzioni di un disco sempre pronto,
    /// e il resto dell'esecuzione è identico.
    #[test]
    fn disco_non_pronto_ferma_il_tempo_del_guest() {
        let (mut ready, _) = blk_machine(true);
        assert_eq!(ready.run(1000), Stop::Budget);

        let (mut m, slot) = blk_machine(false);
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.steps, 1, "solo la notifica: nessuna istruzione dopo la richiesta");
        assert!(m.blocked());
        assert_eq!(m.run(1000), Stop::Blocked, "senza dati resta ferma");
        assert_eq!(m.steps, 1);
        assert_eq!(ram(&m, USED + 2, 2), [0, 0], "nessuna risposta al guest");
        m.device::<VirtioBlk, _>(Some(slot), |b| b.backend_as_mut::<Gate>().unwrap().open = true).unwrap();
        assert_eq!(m.run(999), Stop::Budget);
        assert!(!m.blocked());
        assert_eq!(m.steps, ready.steps);
        assert_eq!(m.cpu.x[2], ready.cpu.x[2]);
        assert_eq!(ram(&m, USED, 16), ram(&ready, USED, 16));
        assert_eq!(ram(&m, DATA, 512), ram(&ready, DATA, 512));
        let expected: Vec<u8> = (512..516u32).map(|i| (i * 7 + 1) as u8).collect();
        assert_eq!(ram(&m, DATA, 4), expected, "settore 1 del disco");
        assert_eq!(ram(&m, USED + 2, 2), [1, 0], "una risposta nello used ring");
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
