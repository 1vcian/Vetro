//! Binding verso il browser (M4): la macchina di `vetro-machine` dietro
//! un'API C, per il modulo WebAssembly caricato da JavaScript (browser e
//! Node). Nessuna dipendenza esterna, niente wasm-bindgen: i tipi che passano
//! il confine sono interi e puntatori nella memoria lineare del modulo.
//!
//! Il contratto è in `docs/specs/wasm.md`:
//! - memoria: [`vetro_alloc`] e [`vetro_free`] danno a JS i buffer in cui
//!   copiare kernel, initramfs e riga di comando, e in cui leggere la console;
//! - macchina: [`vetro_machine_new`] (o [`vetro_machine_new_with`] con i
//!   dispositivi scelti), [`vetro_load_linux`], [`vetro_run`] (un quanto di
//!   istruzioni, con il motivo dell'arresto), console, contatore di
//!   istruzioni;
//! - dispositivi (M5): scanout di virtio-gpu in RGBA ([`display`]),
//!   eventi di virtio-input, linee del GPIO (tasto di accensione), dischi
//!   virtio-blk con i dati forniti dal JS a blocchi ([`disk`]);
//! - rete (ABI 5): connessioni TCP dal JS verso i servizi del guest
//!   (inoltro di porte, [`net`]);
//! - import dal JS: `vetro_host.panic` (messaggio di un panic prima della
//!   trappola) e il motore JIT di [`jit`] (`vetro_jit.*`).
//!
//! Sul target nativo le stesse funzioni si provano come funzioni Rust (i test
//! di questo crate); gli import dal JS lì non esistono e hanno un sostituto.

#![allow(clippy::missing_safety_doc)]

pub mod disk;
pub mod display;
pub mod jit;
pub mod net;

use std::alloc::Layout;

use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Pointer, Stop};
use vetro_platform::virtio::input::InputEvent;
use vetro_platform::virtio::{
    BlockBackend, CowBackend, GpuConfig, MemBackend, VirtioBlk, VirtioBlkConfig, VirtioGpu,
};

use disk::HostDisk;
use display::WebDisplay;

/// Versione dell'API C: cambia a ogni modifica incompatibile delle firme.
/// 2: JIT della modalità sistema (`vetro_machine_set_jit`, import
/// `vetro_jit.reset` e tabella `env.tbl` dei blocchi).
/// 3: dispositivi (`vetro_machine_new_with`, display, input, GPIO, dischi)
/// e codice d'arresto `BLOCKED`.
/// 4: snapshot della macchina (`vetro_snapshot_*`, ADR 0015).
/// 5: connessioni TCP dal JS verso i servizi del guest (`vetro_net_*`,
/// inoltro di porte).
pub const ABI_VERSION: u32 = 5;

/// Allineamento dei buffer di [`vetro_alloc`] (basta per `JitState`).
const ALLOC_ALIGN: usize = 16;

/// Codici di [`vetro_run`].
pub mod stop {
    pub const BUDGET: u32 = 0;
    pub const POWER_OFF: u32 = 1;
    pub const RESET: u32 = 2;
    pub const IDLE: u32 = 3;
    pub const UNIMPLEMENTED: u32 = 4;
    /// Un disco aspetta blocchi dal JS (`vetro_disk_wanted`): il tempo del
    /// guest è fermo finché non arrivano.
    pub const BLOCKED: u32 = 5;
}

/// Bit dei dispositivi di [`vetro_machine_new_with`].
pub mod dev {
    pub const GPU: u32 = 1;
    pub const KEYBOARD: u32 = 2;
    pub const TABLET: u32 = 4;
    pub const MULTITOUCH: u32 = 8;
    /// virtio-net con lo stack di `vetro-net` e il sinkhole (`NetSetup::default`).
    pub const NET: u32 = 16;
    /// Quelli di `Devices::default` (la macchina del test di avvio).
    pub const DEFAULT: u32 = GPU | KEYBOARD | TABLET | NET;
}

/// Bit di `flags` di [`vetro_disk_add`] e [`vetro_disk_add_mem`].
pub mod disk_flags {
    /// Il guest vede il disco in sola lettura (senza, le sue scritture
    /// finiscono in un livello copy-on-write in memoria).
    pub const READ_ONLY: u32 = 1;
}

/// Dispositivi di virtio-input per [`vetro_input_events`].
pub mod input_dev {
    pub const KEYBOARD: u32 = 0;
    pub const POINTER: u32 = 1;
}

/// Codici di [`vetro_load_linux`].
pub mod load {
    pub const OK: u32 = 0;
    /// Il caricatore ha rifiutato i file: il motivo è in `vetro_message_*`.
    pub const BOOT_ERROR: u32 = 1;
    /// La riga di comando non è UTF-8.
    pub const BAD_CMDLINE: u32 = 2;
}

/// Una macchina con i buffer di contorno per JS.
pub struct Vm {
    m: Machine,
    /// Uscita della console già tolta alla UART e non ancora letta da JS.
    out: Vec<u8>,
    out_pos: usize,
    /// Ultimo messaggio (errore di caricamento, istruzione non implementata).
    message: String,
    unimpl: (u64, u32),
    /// Slot virtio dei dischi, nell'ordine di aggiunta (l'indice è quello
    /// dell'API).
    disks: Vec<u32>,
    /// Ultimo snapshot di `vetro_snapshot_save`, finché JS non lo copia.
    snapshot: Vec<u8>,
}

impl Vm {
    pub fn new(cfg: &MachineConfig) -> Self {
        Self::with_devices(cfg, &Devices::default())
    }

    /// Macchina con i dispositivi dati; la GPU mostra su un [`WebDisplay`].
    pub fn with_devices(cfg: &MachineConfig, devices: &Devices) -> Self {
        let vm = Vm {
            m: Machine::with_devices(cfg, devices),
            out: Vec::new(),
            out_pos: 0,
            message: String::new(),
            unimpl: (0, 0),
            disks: Vec::new(),
            snapshot: Vec::new(),
        };
        // Senza `Machine::gpu`: cambiare backend non deve far servire la GPU.
        vm.with_gpu(|g| g.set_backend(Box::new(WebDisplay::default())));
        vm
    }

    fn with_gpu<R>(&self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Option<R> {
        let slot = self.m.slots().gpu?;
        let mut b = self.m.board.borrow_mut();
        b.virt.virtio_mut(slot)?.device_as_mut::<VirtioGpu>().map(f)
    }

    /// Agisce sul display della GPU, se c'è. Non passa da
    /// `Machine::device`: leggere l'immagine non deve far servire la GPU
    /// (il guest non vede niente, e il momento della lettura lo sceglie la
    /// pagina, non il guest).
    pub fn with_display<R>(&self, f: impl FnOnce(&mut WebDisplay) -> R) -> Option<R> {
        self.with_gpu(|g| g.backend_as_mut::<WebDisplay>().map(f)).flatten()
    }

    /// Aggiunge un disco virtio-blk nel primo slot libero (dall'alto, dopo
    /// GPU e input); restituisce il suo indice.
    pub fn add_disk(&mut self, backend: Box<dyn BlockBackend>, read_only: bool) -> Result<u32, String> {
        let cfg = VirtioBlkConfig {
            read_only,
            serial: format!("vetro-disk{}", self.disks.len()).into_bytes(),
            ..VirtioBlkConfig::default()
        };
        let blk = VirtioBlk::new(backend, cfg);
        let slot =
            self.m.board.borrow_mut().virt.attach_virtio_next(Box::new(blk)).map_err(|e| format!("{e:?}"))?;
        self.disks.push(slot);
        Ok(self.disks.len() as u32 - 1)
    }

    /// Agisce sul [`HostDisk`] del disco `index`, se lo è. Se la macchina
    /// aspetta dati (`Stop::Blocked`) il dispositivo si fa servire di nuovo
    /// prima della prossima istruzione, altrimenti no (una consegna
    /// anticipata non deve cambiare i tempi del guest).
    pub fn with_host_disk<R>(&mut self, index: u32, f: impl FnOnce(&mut HostDisk) -> R) -> Option<R> {
        let slot = *self.disks.get(index as usize)?;
        let pick = |b: &mut VirtioBlk| -> Option<R> {
            if let Some(c) = b.backend_as_mut::<CowBackend<HostDisk>>() {
                return Some(f(c.base_mut()));
            }
            b.backend_as_mut::<HostDisk>().map(f)
        };
        if self.m.blocked() {
            self.m.device::<VirtioBlk, _>(Some(slot), pick).flatten()
        } else {
            let mut b = self.m.board.borrow_mut();
            pick(b.virt.virtio_mut(slot)?.device_as_mut::<VirtioBlk>()?)
        }
    }

    /// Blocchi chiesti dai dischi dall'ultima chiamata: (disco, blocco).
    pub fn disk_wanted(&mut self) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        for i in 0..self.disks.len() as u32 {
            if let Some(w) = self.with_host_disk(i, |d| d.take_wanted()) {
                out.extend(w.into_iter().map(|b| (i, b)));
            }
        }
        out
    }

    /// Cluster scritti dal guest nel livello copy-on-write del disco `index`.
    fn disk_dirty_clusters(&mut self, index: u32) -> usize {
        let Some(&slot) = self.disks.get(index as usize) else { return 0 };
        let mut b = self.m.board.borrow_mut();
        let Some(blk) = b.virt.virtio_mut(slot).and_then(|t| t.device_as_mut::<VirtioBlk>()) else {
            return 0;
        };
        blk.backend_as_mut::<CowBackend<HostDisk>>()
            .map(|c| c.dirty_clusters())
            .or_else(|| blk.backend_as_mut::<CowBackend<MemBackend>>().map(|c| c.dirty_clusters()))
            .unwrap_or(0)
    }

    pub fn machine(&mut self) -> &mut Machine {
        &mut self.m
    }

    /// Attiva il JIT della modalità sistema sul motore JS, con soglia
    /// `hot_threshold` e `batch` blocchi per modulo.
    pub fn set_jit(&mut self, hot_threshold: u32, batch: u32) {
        let cfg = vetro_jit::SysJitConfig {
            hot_threshold,
            batch: batch.max(1) as usize,
            ..vetro_jit::SysJitConfig::default()
        };
        self.m.set_jit(Some(Box::new(vetro_jit::SysJit::new(jit::JsEngine::default(), cfg))));
    }

    pub fn load_linux(&mut self, image: &[u8], initrd: Option<&[u8]>, cmdline: &[u8]) -> u32 {
        let Ok(cmdline) = core::str::from_utf8(cmdline) else {
            self.message = "riga di comando non UTF-8".into();
            return load::BAD_CMDLINE;
        };
        match self.m.load_linux(image, initrd, cmdline) {
            Ok(_) => load::OK,
            Err(e) => {
                self.message = e.to_string();
                load::BOOT_ERROR
            }
        }
    }

    pub fn run(&mut self, budget: u64) -> u32 {
        match self.m.run(budget) {
            Stop::Budget => stop::BUDGET,
            Stop::PowerOff => stop::POWER_OFF,
            Stop::Reset => stop::RESET,
            Stop::Idle => stop::IDLE,
            Stop::Unimplemented { pc, raw, what } => {
                self.unimpl = (pc, raw);
                self.message = what.into();
                stop::UNIMPLEMENTED
            }
            Stop::Blocked => stop::BLOCKED,
        }
    }

    /// Snapshot della macchina (M6, ADR 0015). L'uscita della console già
    /// tolta alla UART e non ancora letta da JS non ne fa parte: si salva
    /// dopo aver letto la console.
    pub fn save_state(&self) -> Vec<u8> {
        self.m.save()
    }

    /// Ripristina uno snapshot su questa macchina, che dev'essere
    /// configurata come quella salvata (stessi dispositivi e dischi, già
    /// aggiunti con gli stessi parametri). Il display riceve subito
    /// l'immagine ripristinata; l'uscita della console non letta si scarta.
    pub fn restore_state(&mut self, bytes: &[u8]) -> Result<(), vetro_machine::vetro_snapshot::Error> {
        self.m.load_state(bytes)?;
        self.out.clear();
        self.out_pos = 0;
        Ok(())
    }

    /// Copia in `dst` al più `dst.len()` byte dell'uscita della console, che
    /// consuma; il resto aspetta la chiamata successiva.
    pub fn console_read(&mut self, dst: &mut [u8]) -> usize {
        if self.out_pos == self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        }
        self.out.extend(self.m.console_output());
        let n = dst.len().min(self.out.len() - self.out_pos);
        dst[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        n
    }
}

/// `&[u8]` da puntatore e lunghezza passati da JS (nullo o vuoto = vuoto).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(ptr, len) } }
}

/// Installa (una volta) il gancio che manda a JS il messaggio di un panic:
/// su wasm32-unknown-unknown il panic è una trappola `unreachable` muta.
fn install_panic_hook() {
    #[cfg(target_arch = "wasm32")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                let msg = info.to_string();
                // SAFETY: import di `vetro_host`, legge `msg` durante la chiamata.
                unsafe { host::panic(msg.as_ptr(), msg.len()) };
            }))
        });
    }
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "vetro_host")]
    unsafe extern "C" {
        /// Messaggio UTF-8 di un panic, subito prima della trappola.
        pub fn panic(ptr: *const u8, len: usize);
    }
}

/// Versione dell'API ([`ABI_VERSION`]).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_abi_version() -> u32 {
    ABI_VERSION
}

/// Alloca `len` byte allineati a 16 nella memoria del modulo; nullo se
/// `len == 0` o se la memoria non basta. Attenzione: l'allocazione può far
/// crescere la memoria, e le viste JS su `memory.buffer` vanno rifatte.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_alloc(len: usize) -> *mut u8 {
    match Layout::from_size_align(len, ALLOC_ALIGN) {
        // SAFETY: dimensione non nulla.
        Ok(l) if len > 0 => unsafe { std::alloc::alloc(l) },
        _ => core::ptr::null_mut(),
    }
}

/// Libera un buffer di [`vetro_alloc`] con la stessa lunghezza.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        // SAFETY: `ptr` viene da `vetro_alloc(len)` (contratto dell'API).
        unsafe { std::alloc::dealloc(ptr, Layout::from_size_align_unchecked(len, ALLOC_ALIGN)) }
    }
}

/// Crea una macchina: `ram_size` in byte (0 = 1 GiB), ora dell'RTC in
/// secondi dall'epoca e seme del device tree (entrambi 0 = i valori di
/// `MachineConfig::default`, quelli dei test nativi).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_machine_new(ram_size: u64, now_secs: u64, seed: u64) -> *mut Vm {
    install_panic_hook();
    Box::into_raw(Box::new(Vm::new(&config(ram_size, now_secs, seed))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_free(vm: *mut Vm) {
    if !vm.is_null() {
        // SAFETY: `vm` viene da `vetro_machine_new` e non si usa più.
        drop(unsafe { Box::from_raw(vm) });
    }
}

/// Carica kernel (`Image`), initramfs (nullo o lunghezza 0 = nessuno) e riga
/// di comando (UTF-8). I buffer si possono liberare subito dopo. Restituisce
/// un codice di [`load`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_load_linux(
    vm: *mut Vm,
    image: *const u8,
    image_len: usize,
    initrd: *const u8,
    initrd_len: usize,
    cmdline: *const u8,
    cmdline_len: usize,
) -> u32 {
    // SAFETY: puntatori validi per le lunghezze date (contratto dell'API).
    let vm = unsafe { &mut *vm };
    let (image, initrd, cmdline) =
        unsafe { (bytes(image, image_len), bytes(initrd, initrd_len), bytes(cmdline, cmdline_len)) };
    vm.load_linux(image, (!initrd.is_empty()).then_some(initrd), cmdline)
}

/// Esegue al più `budget` istruzioni; restituisce un codice di [`stop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_run(vm: *mut Vm, budget: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &mut *vm }.run(budget)
}

/// Attiva il JIT (ADR 0013) con soglia `hot_threshold` (ingressi prima di
/// tradurre un blocco) e `batch` blocchi per modulo (0 = 1). Il risultato
/// dell'esecuzione non cambia; cambia solo la velocità.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_set_jit(vm: *mut Vm, hot_threshold: u32, batch: u32) {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &mut *vm }.set_jit(hot_threshold, batch);
}

/// Contatori del JIT (`SysJitStats`, nell'ordine dei campi) in `out`, al
/// più `cap` valori; restituisce quanti ne ha scritti (0 senza JIT).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_stats(vm: *const Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per `cap` valori.
    let vm = unsafe { &*vm };
    let Some(s) = vm.m.jit_stats() else { return 0 };
    let v = [
        s.jit_steps,
        s.runs,
        s.resolves,
        s.calls,
        s.blocks,
        s.modules,
        s.reused,
        s.invalidated_pages,
        s.faults,
        s.svcs,
        s.stops,
        s.epochs,
        s.tlb_flushes,
        s.tlb_fills,
        s.resets,
    ];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// Istruzioni eseguite (l'orologio del guest).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_steps(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.m.steps
}

/// Tempo del guest in nanosecondi.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_guest_ns(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.m.guest_ns()
}

/// Legge e consuma al più `cap` byte dell'uscita della console in `dst`;
/// restituisce quanti. 0 = niente di nuovo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_read(vm: *mut Vm, dst: *mut u8, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `dst` vale per `cap` byte.
    let vm = unsafe { &mut *vm };
    if dst.is_null() || cap == 0 {
        return 0;
    }
    vm.console_read(unsafe { core::slice::from_raw_parts_mut(dst, cap) })
}

/// Accoda `len` byte sulla console, come dalla tastiera.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_write(vm: *mut Vm, src: *const u8, len: usize) {
    // SAFETY: `vm` viene da `vetro_machine_new`, `src` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    vm.m.console_input(unsafe { bytes(src, len) });
}

/// Ultimo messaggio (UTF-8): errore di caricamento o istruzione non
/// implementata. Valido fino alla chiamata successiva sulla macchina.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.message.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_len(vm: *const Vm) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.message.len()
}

/// PC dell'ultima istruzione non implementata (`stop::UNIMPLEMENTED`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_pc(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.unimpl.0
}

/// Codifica dell'ultima istruzione non implementata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_raw(vm: *const Vm) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.unimpl.1
}

// ---- Dispositivi (ABI 3) ----------------------------------------------------

/// `Devices` dai bit di [`dev`] e dalla risoluzione iniziale della GPU (0 =
/// quella di default, 1280x800).
pub fn devices_from(bits: u32, width: u32, height: u32) -> Devices {
    let d = GpuConfig::default();
    let gpu = GpuConfig {
        width: if width == 0 { d.width } else { width },
        height: if height == 0 { d.height } else { height },
        ..d
    };
    let pointer = if bits & dev::MULTITOUCH != 0 {
        Some(Pointer::Multitouch)
    } else if bits & dev::TABLET != 0 {
        Some(Pointer::Tablet)
    } else {
        None
    };
    Devices {
        gpu: (bits & dev::GPU != 0).then_some(gpu),
        keyboard: bits & dev::KEYBOARD != 0,
        pointer,
        net: (bits & dev::NET != 0).then(NetSetup::default),
        vsock_cid: None,
    }
}

fn config(ram_size: u64, now_secs: u64, seed: u64) -> MachineConfig {
    let d = MachineConfig::default();
    MachineConfig {
        ram_size: if ram_size == 0 { d.ram_size } else { ram_size },
        now_secs: if now_secs == 0 { d.now_secs } else { now_secs },
        seed: if seed == 0 { d.seed } else { seed },
    }
}

/// Come [`vetro_machine_new`], con i dispositivi scelti: `devices` sono bit
/// di [`dev`] (`MULTITOUCH` vince su `TABLET`), `width`x`height` la
/// risoluzione iniziale dello scanout 0 (0 = 1280x800).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_machine_new_with(
    ram_size: u64,
    now_secs: u64,
    seed: u64,
    devices: u32,
    width: u32,
    height: u32,
) -> *mut Vm {
    install_panic_hook();
    let cfg = config(ram_size, now_secs, seed);
    Box::into_raw(Box::new(Vm::with_devices(&cfg, &devices_from(devices, width, height))))
}

/// Dimensioni dello scanout `scanout`: `(larghezza << 32) | altezza`, 0 se
/// spento o se non c'è la GPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_size(vm: *const Vm, scanout: u32) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.on => u64::from(s.width) << 32 | u64::from(s.height),
        _ => 0,
    })
    .unwrap_or(0)
}

/// Pixel RGBA dello scanout (righe da `larghezza * 4` byte), nullo se
/// spento. Valido fino alla prossima chiamata che esegue il guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_ptr(vm: *const Vm, scanout: u32) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.on => s.rgba.as_ptr(),
        _ => core::ptr::null(),
    })
    .unwrap_or(core::ptr::null())
}

/// Aggiornamenti dello scanout (immagine o spegnimento): se non cambia non
/// c'è niente da ridisegnare.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_updates(vm: *const Vm, scanout: u32) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| d.screen(scanout).map_or(0, |s| s.updates)).unwrap_or(0)
}

/// Rettangolo cambiato dall'ultima chiamata (unione): scrive `x, y,
/// larghezza, altezza` in `out` e restituisce 1, o 0 se niente è cambiato.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_take_dirty(vm: *const Vm, scanout: u32, out: *mut u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per 4 valori.
    let vm = unsafe { &*vm };
    match vm.with_display(|d| d.take_dirty(scanout)).flatten() {
        Some(r) => {
            unsafe { core::slice::from_raw_parts_mut(out, 4) }
                .copy_from_slice(&[r.x, r.y, r.width, r.height]);
            1
        }
        None => 0,
    }
}

/// Risoluzione chiesta per lo scanout (come ridimensionare la finestra):
/// il driver la vede con un interrupt di configurazione. È un ingresso
/// dell'host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_resize(vm: *mut Vm, scanout: u32, width: u32, height: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.gpu(|g| g.set_display(scanout, width, height)).is_some() as u32
}

/// Stato del cursore dello scanout in `out` (6 valori): risorsa (0 =
/// nascosto), x, y, hot_x, hot_y, numero di cambi. Restituisce 0 senza GPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_cursor_state(vm: *const Vm, scanout: u32, out: *mut u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per 6 valori.
    let vm = unsafe { &*vm };
    let Some(v) = vm
        .with_display(|d| {
            d.screen(scanout).map(|s| {
                let c = &s.cursor;
                [c.resource_id, c.x, c.y, c.hot_x, c.hot_y, s.cursor_updates as u32]
            })
        })
        .flatten()
    else {
        return 0;
    };
    unsafe { core::slice::from_raw_parts_mut(out, 6) }.copy_from_slice(&v);
    1
}

/// Immagine del cursore, 64x64 RGBA; nulla se non c'è. Valida fino alla
/// prossima chiamata che esegue il guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_cursor_image(vm: *const Vm, scanout: u32) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    let n = (display::CURSOR_SIZE * display::CURSOR_SIZE * 4) as usize;
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.cursor_rgba.len() == n => s.cursor_rgba.as_ptr(),
        _ => core::ptr::null(),
    })
    .unwrap_or(core::ptr::null())
}

/// Accoda `count` eventi evdev (`tipo, codice, valore` come tre `u32`
/// consecutivi) sul dispositivo `device` di [`input_dev`]. Chi chiama mette
/// i SYN_REPORT. Restituisce 0 se il dispositivo non c'è.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_events(
    vm: *mut Vm,
    device: u32,
    events: *const u32,
    count: usize,
) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `events` vale per 3 * count valori.
    let vm = unsafe { &mut *vm };
    let raw = if count == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(events, 3 * count) } };
    let ev: Vec<InputEvent> = raw
        .as_chunks::<3>()
        .0
        .iter()
        .map(|e| InputEvent { ty: e[0] as u16, code: e[1] as u16, value: e[2] })
        .collect();
    let r = match device {
        input_dev::KEYBOARD => vm.m.keyboard(|k| k.inject(&ev)),
        input_dev::POINTER => vm.m.pointer(|p| p.inject(&ev)),
        _ => None,
    };
    r.is_some() as u32
}

/// Un tasto della tastiera (codice Linux `KEY_*`) premuto o rilasciato, con
/// SYN_REPORT. 0 se non c'è la tastiera.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_key(vm: *mut Vm, code: u32, down: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.keyboard(|k| k.key(code as u16, down != 0)).is_some() as u32
}

/// Posizione assoluta del tablet (0..=32767 per asse), con SYN_REPORT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_abs(vm: *mut Vm, x: u32, y: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.pointer(|p| p.move_abs(x, y)).is_some() as u32
}

/// Pulsante del puntatore (`BTN_LEFT` = 0x110, ...), con SYN_REPORT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_button(vm: *mut Vm, code: u32, down: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.pointer(|p| p.key(code as u16, down != 0)).is_some() as u32
}

/// Contatto `slot` del touchscreen: `down` != 0 lo mette o lo sposta in
/// (x, y) (0..=32767), 0 lo toglie.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_touch(vm: *mut Vm, slot: u32, x: u32, y: u32, down: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.pointer(|p| p.touch(slot, (down != 0).then_some((x, y)))).is_some() as u32
}

/// LED della tastiera accesi dal guest (bit `LED_*`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_leds(vm: *mut Vm) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let slot = vm.m.slots().keyboard;
    let b = vm.m.board.borrow();
    slot.and_then(|s| b.virt.virtio(s)?.device_as::<vetro_platform::virtio::VirtioInput>().map(|k| k.leds()))
        .unwrap_or(0)
}

/// Pilota la linea `line` del GPIO PL061 (3 = tasto di accensione,
/// `gpio-keys` KEY_POWER). È un ingresso dell'host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_gpio_input(vm: *mut Vm, line: u32, level: u32) {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.m.board.borrow_mut().gpio_input(line, level != 0);
}

/// Linea del GPIO del tasto di accensione.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_power_key_line() -> u32 {
    vetro_platform::pl061::POWER_KEY_LINE
}

// ---- Dischi (ABI 3) ---------------------------------------------------------

/// Aggiunge un disco virtio-blk di `size` byte con i dati dal JS a blocchi
/// da `block_size` byte (potenza di due, almeno 512), al più `max_blocks`
/// blocchi in memoria (0 = nessun limite). `flags`: bit di [`disk_flags`].
/// Restituisce l'indice del disco, o -1 (motivo nel messaggio). Da
/// chiamare prima di eseguire il guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_add(
    vm: *mut Vm,
    size: u64,
    block_size: u32,
    max_blocks: u32,
    flags: u32,
) -> i32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let d = match HostDisk::new(size, block_size, max_blocks as usize) {
        Ok(d) => d,
        Err(e) => {
            vm.message = format!("disco rifiutato: {e:?}");
            return -1;
        }
    };
    let ro = flags & disk_flags::READ_ONLY != 0;
    let backend: Box<dyn BlockBackend> = if ro { Box::new(d) } else { Box::new(CowBackend::new(d)) };
    match vm.add_disk(backend, ro) {
        Ok(i) => i as i32,
        Err(e) => {
            vm.message = e;
            -1
        }
    }
}

/// Aggiunge un disco con tutto il contenuto già in memoria (copiato da
/// `data`; lunghezza arrotondata per difetto a 512 come per [`vetro_disk_add`]): sempre pronto, per i file
/// piccoli e come riferimento nei test. Stessi `flags` e risultato di
/// [`vetro_disk_add`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_add_mem(vm: *mut Vm, data: *const u8, len: usize, flags: u32) -> i32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `data` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    let len = len / 512 * 512;
    let mem = MemBackend::from_vec(unsafe { bytes(data, len) }.to_vec()).read_only();
    let ro = flags & disk_flags::READ_ONLY != 0;
    let backend: Box<dyn BlockBackend> = if ro { Box::new(mem) } else { Box::new(CowBackend::new(mem)) };
    match vm.add_disk(backend, ro) {
        Ok(i) => i as i32,
        Err(e) => {
            vm.message = e;
            -1
        }
    }
}

/// Blocchi chiesti dai dischi dall'ultima chiamata, come coppie `(disco,
/// blocco)` di `u64` in `out` (al più `cap` coppie; il resto resta per la
/// chiamata successiva). Restituisce quante coppie. Ogni blocco compare una
/// volta sola finché non arriva ([`vetro_disk_fill`]) o fallisce
/// ([`vetro_disk_fail`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_wanted(vm: *mut Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per 2 * cap valori.
    let vm = unsafe { &mut *vm };
    let all = vm.disk_wanted();
    let n = all.len().min(cap);
    if n > 0 {
        let o = unsafe { core::slice::from_raw_parts_mut(out, 2 * n) };
        for (k, &(d, b)) in all[..n].iter().enumerate() {
            o[2 * k] = u64::from(d);
            o[2 * k + 1] = b;
        }
    }
    // Quelli che non stanno in `out` tornano in lista.
    for &(d, b) in &all[n..] {
        vm.with_host_disk(d, |h| h.requeue(b));
    }
    n
}

/// Consegna il blocco `block` del disco `disk` (`len` = la dimensione del
/// blocco, o meno per l'ultimo). 0 = accettato; 1 = disco sconosciuto; 2 =
/// blocco fuori dal disco; 3 = lunghezza sbagliata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_fill(
    vm: *mut Vm,
    disk: u32,
    block: u64,
    data: *const u8,
    len: usize,
) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `data` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    let data = unsafe { bytes(data, len) };
    match vm.with_host_disk(disk, |d| d.fill(block, data)) {
        None => 1,
        Some(Ok(())) => 0,
        Some(Err(disk::DiskError::OutOfRange)) => 2,
        Some(Err(_)) => 3,
    }
}

/// Il JS non ha potuto procurare il blocco: la richiesta del guest che lo
/// aspetta finisce con un errore di I/O.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_fail(vm: *mut Vm, disk: u32, block: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.with_host_disk(disk, |d| d.fail(block)).is_some() as u32
}

/// Contatori del disco in `out` (al più `cap`): dimensione in byte,
/// dimensione del blocco, blocchi in memoria, letture mancate, blocchi
/// consegnati, blocchi tolti, blocchi falliti, cluster copy-on-write
/// scritti dal guest. Restituisce quanti valori (0 = disco sconosciuto; per
/// un disco in memoria solo dimensione e cluster sono significativi).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_stats(vm: *mut Vm, disk: u32, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per `cap` valori.
    let vm = unsafe { &mut *vm };
    let Some(&slot) = vm.disks.get(disk as usize) else { return 0 };
    let size = {
        let b = vm.m.board.borrow();
        b.virt.virtio(slot).and_then(|t| t.device_as::<VirtioBlk>()).map_or(0, |x| x.backend().size())
    };
    let h = vm
        .with_host_disk(disk, |d| {
            [
                d.block_size(),
                d.cached_blocks() as u64,
                d.stats.misses,
                d.stats.fills,
                d.stats.evictions,
                d.stats.failures,
            ]
        })
        .unwrap_or_default();
    let v = [size, h[0], h[1], h[2], h[3], h[4], h[5], vm.disk_dirty_clusters(disk) as u64];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

// ---- Snapshot (ABI 4, ADR 0015) ---------------------------------------------

/// Versione del formato degli snapshot (`vetro_snapshot::FORMAT_VERSION`):
/// il JS la usa nelle chiavi della cache, così uno snapshot di un'altra
/// versione non si prova nemmeno a ripristinare.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_snapshot_version() -> u32 {
    vetro_machine::vetro_snapshot::FORMAT_VERSION
}

/// Salva la macchina in un buffer interno e ne restituisce la lunghezza;
/// i byte si leggono da [`vetro_snapshot_ptr`] (validi fino al prossimo
/// salvataggio, a [`vetro_snapshot_clear`] o alla distruzione della
/// macchina). Leggere prima la console: l'uscita già letta dalla UART e
/// non ancora consegnata al JS non entra nello snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_save(vm: *mut Vm) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.snapshot = vm.save_state();
    vm.snapshot.len()
}

/// I byte dell'ultimo [`vetro_snapshot_save`] (nullo se non ce n'è).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    if vm.snapshot.is_empty() { core::ptr::null() } else { vm.snapshot.as_ptr() }
}

/// Libera il buffer dell'ultimo salvataggio.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_clear(vm: *mut Vm) {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &mut *vm }.snapshot = Vec::new();
}

/// Codici di [`vetro_snapshot_restore`].
pub mod restore {
    pub const OK: u32 = 0;
    /// Non è uno snapshot di Vetro.
    pub const BAD_MAGIC: u32 = 1;
    /// Formato di un'altra versione (`vetro_snapshot_version`).
    pub const VERSION: u32 = 2;
    /// Macchina configurata diversamente (RAM, dispositivi, dischi, seme).
    pub const CONFIG: u32 = 3;
    /// Snapshot rovinato o incoerente: la macchina va scartata.
    pub const CORRUPT: u32 = 4;
}

/// Ripristina lo snapshot di `len` byte in `data` su questa macchina,
/// configurata come quella salvata (stessi dispositivi di
/// `vetro_machine_new_with`, stessi dischi aggiunti nello stesso ordine con
/// gli stessi parametri, prima di chiamarla). Il buffer si può liberare
/// subito dopo. Con un codice diverso da 0 il motivo è nel messaggio; con
/// `BAD_MAGIC`, `VERSION` e `CONFIG` la macchina non è cambiata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_restore(vm: *mut Vm, data: *const u8, len: usize) -> u32 {
    use vetro_machine::vetro_snapshot::Error;
    // SAFETY: `vm` viene da `vetro_machine_new`, `data` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    match vm.restore_state(unsafe { bytes(data, len) }) {
        Ok(()) => restore::OK,
        Err(e) => {
            vm.message = e.to_string();
            match e {
                Error::BadMagic => restore::BAD_MAGIC,
                Error::Version { .. } => restore::VERSION,
                Error::Config { .. } => restore::CONFIG,
                _ => restore::CORRUPT,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> *mut Vm {
        vetro_machine_new(64 << 20, 0, 0)
    }

    #[test]
    fn alloc_e_free() {
        assert!(vetro_alloc(0).is_null());
        let p = vetro_alloc(100);
        assert!(!p.is_null());
        assert_eq!(p as usize % ALLOC_ALIGN, 0);
        unsafe { vetro_free(p, 100) };
    }

    #[test]
    fn caricamento_rifiutato_con_messaggio() {
        let vm = small();
        let junk = [0u8; 16];
        let code = unsafe {
            vetro_load_linux(vm, junk.as_ptr(), junk.len(), core::ptr::null(), 0, b"x".as_ptr(), 1)
        };
        assert_eq!(code, load::BOOT_ERROR);
        let msg = unsafe {
            core::str::from_utf8(bytes(vetro_message_ptr(vm), vetro_message_len(vm))).unwrap().to_string()
        };
        assert!(msg.contains("troncata"), "{msg}");
        let bad = [0xffu8];
        let code =
            unsafe { vetro_load_linux(vm, junk.as_ptr(), junk.len(), core::ptr::null(), 0, bad.as_ptr(), 1) };
        assert_eq!(code, load::BAD_CMDLINE);
        unsafe { vetro_machine_free(vm) };
    }

    #[test]
    fn lettura_della_console_a_pezzi() {
        let mut vm = Vm::new(&MachineConfig { ram_size: 64 << 20, ..MachineConfig::default() });
        vm.out = b"abcdef".to_vec();
        let mut buf = [0u8; 4];
        assert_eq!(vm.console_read(&mut buf), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(vm.console_read(&mut buf), 2);
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(vm.console_read(&mut buf), 0);
        assert_eq!(vm.out_pos, 0);
    }

    /// Dispositivi scelti dai bit, display e input senza driver, dischi con
    /// le loro regole.
    #[test]
    fn dispositivi_e_dischi_dall_api() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::KEYBOARD | dev::MULTITOUCH, 0, 0);
        let slots = unsafe { &mut *vm }.m.slots();
        assert_eq!((slots.gpu, slots.keyboard, slots.pointer), (None, Some(31), Some(30)));
        assert_eq!(devices_from(dev::DEFAULT, 0, 0), Devices::default());
        let d = devices_from(dev::GPU, 640, 480).gpu.unwrap();
        assert_eq!((d.width, d.height), (640, 480));
        unsafe {
            assert_eq!(vetro_display_size(vm, 0), 0);
            assert!(vetro_display_ptr(vm, 0).is_null());
            assert_eq!(vetro_display_resize(vm, 0, 800, 600), 0, "senza GPU");
            assert_eq!(vetro_input_key(vm, 30, 1), 1);
            assert_eq!(vetro_input_touch(vm, 0, 10, 10, 1), 1);
            assert_eq!(vetro_input_abs(vm, 1, 1), 1);
            let ev = [1u32, 30, 1, 0, 0, 0];
            assert_eq!(vetro_input_events(vm, input_dev::KEYBOARD, ev.as_ptr(), 2), 1);
            assert_eq!(vetro_input_events(vm, 7, ev.as_ptr(), 2), 0);
            assert_eq!(vetro_input_leds(vm), 0);
            vetro_gpio_input(vm, vetro_power_key_line(), 1);

            assert_eq!(vetro_disk_add(vm, 1 << 20, 1000, 0, 0), -1);
            assert_eq!(vetro_disk_add(vm, (1 << 20) + 100, 65536, 0, 0), 0);
            let img = [7u8; 1000];
            assert_eq!(vetro_disk_add_mem(vm, img.as_ptr(), img.len(), disk_flags::READ_ONLY), 1);
            let mut st = [0u64; 8];
            assert_eq!(vetro_disk_stats(vm, 0, st.as_mut_ptr(), 8), 8);
            assert_eq!(st[..2], [1 << 20, 65536]);
            assert_eq!(vetro_disk_stats(vm, 1, st.as_mut_ptr(), 8), 8);
            assert_eq!(st[0], 512, "arrotondato per difetto a 512");
            assert_eq!(vetro_disk_stats(vm, 2, st.as_mut_ptr(), 8), 0);
            let blk = vec![1u8; 65536];
            assert_eq!(vetro_disk_fill(vm, 0, 3, blk.as_ptr(), blk.len()), 0);
            assert_eq!(vetro_disk_fill(vm, 0, 16, blk.as_ptr(), blk.len()), 2);
            assert_eq!(vetro_disk_fill(vm, 0, 2, blk.as_ptr(), 512), 3);
            assert_eq!(vetro_disk_fill(vm, 1, 0, blk.as_ptr(), 512), 1, "disco in memoria");
            assert_eq!(vetro_disk_fill(vm, 5, 0, blk.as_ptr(), 512), 1);
            let mut w = [0u64; 4];
            assert_eq!(vetro_disk_wanted(vm, w.as_mut_ptr(), 2), 0);
            vetro_machine_free(vm);
        }
    }

    /// `vetro_disk_wanted` con poco spazio: il resto resta in lista.
    #[test]
    fn blocchi_chiesti_a_pezzi() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        unsafe {
            assert_eq!(vetro_disk_add(vm, 4 * 4096, 4096, 0, 0), 0);
            let mut buf = vec![0u8; 3 * 4096];
            let r = (*vm).with_host_disk(0, |d| d.read_sectors(0, &mut buf)).unwrap();
            assert_eq!(r, Err(vetro_platform::virtio::BlockError::NotReady));
            let mut w = [0u64; 4];
            assert_eq!(vetro_disk_wanted(vm, w.as_mut_ptr(), 2), 2);
            assert_eq!(w, [0, 0, 0, 1]);
            assert_eq!(vetro_disk_wanted(vm, w.as_mut_ptr(), 2), 1);
            assert_eq!(w[..2], [0, 2]);
            assert_eq!(vetro_disk_fail(vm, 0, 2), 1);
            vetro_machine_free(vm);
        }
    }

    /// Una macchina senza kernel: il PC di reset non è in RAM, e ogni fetch
    /// è un'eccezione verso un vettore che non è in RAM. Il quanto si esaurisce
    /// e il contatore avanza esattamente del quanto (10 ns a istruzione).
    #[test]
    fn quanto_e_contatore() {
        let vm = small();
        assert_eq!(unsafe { vetro_run(vm, 1000) }, stop::BUDGET);
        assert_eq!(unsafe { vetro_steps(vm) }, 1000);
        assert_eq!(unsafe { vetro_guest_ns(vm) }, 10_000);
        unsafe { vetro_machine_free(vm) };
    }

    fn message(vm: *const Vm) -> String {
        unsafe { String::from_utf8_lossy(bytes(vetro_message_ptr(vm), vetro_message_len(vm))).into_owned() }
    }

    /// Snapshot dall'API C (ABI 4): salvato in un buffer, copiato dal JS,
    /// ripristinato su una macchina nuova con gli stessi dispositivi e lo
    /// stesso disco (le scritture del guest nel copy-on-write comprese);
    /// le due proseguono uguali. Formato di un'altra versione, altra
    /// configurazione e byte a caso si rifiutano con il loro codice e un
    /// messaggio, senza toccare la macchina.
    #[test]
    fn snapshot_dall_api() {
        let disk: Vec<u8> = (0..8192u32).map(|i| (i * 13) as u8).collect();
        let new = || {
            let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::DEFAULT, 320, 200);
            assert_eq!(unsafe { vetro_disk_add_mem(vm, disk.as_ptr(), disk.len(), 0) }, 0);
            vm
        };
        let a = new();
        unsafe {
            assert_eq!(vetro_run(a, 1000), stop::BUDGET);
            // Una scrittura nel livello copy-on-write, come la farebbe il guest.
            let slot = (&*a).disks[0];
            (&mut *a)
                .m
                .device::<VirtioBlk, _>(Some(slot), |b| b.backend_mut().write_sectors(3, &[0xab; 512]))
                .unwrap()
                .unwrap();
            let n = vetro_snapshot_save(a);
            assert!(n > vetro_machine::vetro_snapshot::HEADER_LEN);
            let snap = bytes(vetro_snapshot_ptr(a), n).to_vec();
            vetro_snapshot_clear(a);
            assert!(vetro_snapshot_ptr(a).is_null());

            let b = new();
            assert_eq!(vetro_snapshot_restore(b, snap.as_ptr(), snap.len()), restore::OK);
            assert_eq!(vetro_steps(b), 1000);
            assert_eq!(vetro_disk_stats(b, 0, [0u64; 8].as_mut_ptr(), 8), 8);
            let mut st = [0u64; 8];
            vetro_disk_stats(b, 0, st.as_mut_ptr(), 8);
            assert_eq!(st[7], 1, "il cluster scritto torna col ripristino");
            for vm in [a, b] {
                assert_eq!(vetro_run(vm, 5000), stop::BUDGET);
            }
            assert!((&*a).save_state() == (&*b).save_state());

            let mut other = snap.clone();
            other[8] ^= 0x7f;
            let c = new();
            let before = (&*c).save_state();
            assert_eq!(vetro_snapshot_restore(c, other.as_ptr(), other.len()), restore::VERSION);
            assert!(message(c).contains("versione"), "{}", message(c));
            assert_eq!(vetro_snapshot_restore(c, b"altro".as_ptr(), 5), restore::BAD_MAGIC);
            assert!((&*c).save_state() == before, "rifiutati senza toccare la macchina");
            let d = vetro_machine_new_with(64 << 20, 0, 0, dev::DEFAULT, 320, 200);
            assert_eq!(
                vetro_snapshot_restore(d, snap.as_ptr(), snap.len()),
                restore::CONFIG,
                "senza il disco"
            );
            let mut bad = snap.clone();
            let last = bad.len() - 1;
            bad[last] ^= 1;
            assert_eq!(vetro_snapshot_restore(c, bad.as_ptr(), bad.len()), restore::CORRUPT);
            assert_eq!(vetro_snapshot_version(), vetro_machine::vetro_snapshot::FORMAT_VERSION);
            for vm in [a, b, c, d] {
                vetro_machine_free(vm);
            }
        }
    }
}
