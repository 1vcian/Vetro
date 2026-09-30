//! Browser binding (M4): the `vetro-machine` machine behind
//! a C API, for the WebAssembly module loaded by JavaScript (browser and
//! Node). No external dependencies, no wasm-bindgen: the types that cross
//! the boundary are integers and pointers into the module's linear memory.
//!
//! The contract is in `docs/specs/wasm.md`:
//! - memory: [`vetro_alloc`] and [`vetro_free`] give JS the buffers into which it
//!   copies kernel, initramfs and command line, and from which it reads the console;
//! - machine: [`vetro_machine_new`] (or [`vetro_machine_new_with`] with the
//!   chosen devices), [`vetro_load_linux`], [`vetro_run`] (a quantum of
//!   instructions, with the reason for stopping), console, instruction
//!   counter;
//! - devices (M5): virtio-gpu scanouts in RGBA ([`display`]),
//!   virtio-input events, GPIO lines (power button), virtio-blk
//!   disks with the data provided by JS in blocks ([`disk`]);
//! - network (ABI 5): TCP connections from JS to the guest's services
//!   (port forwarding, [`net`]);
//! - file manager (ABI 7, ADR 0020): the client of the guest's
//!   `vetro-files` daemon over virtio-vsock ([`files`]);
//! - network inspector and input→effects timeline (ABI 8, ADR 0023):
//!   capture, list and detail of the requests in JSON, HAR, pcapng,
//!   user inputs and effects ([`analysis`]);
//! - record & replay (ABI 8, ADR 0019 and 0023): recording, log with the
//!   keyframes that can be moved to OPFS, replay, reading registers and memory
//!   ([`replay`]);
//! - machine snapshots (ABI 4, ADR 0015) and persistent copy-on-write overlay
//!   of the disks (ABI 6, ADR 0017): the guest's writes become
//!   writes to a file that JS keeps in OPFS;
//! - imports from JS: `vetro_host.panic` (message of a panic before the
//!   trap) and the JIT engine of [`jit`] (`vetro_jit.*`).
//!
//! On the native target the same functions are tested as Rust functions (the tests
//! of this crate); the imports from JS don't exist there and have a substitute.

#![allow(clippy::missing_safety_doc)]

pub mod analysis;
pub mod disk;
pub mod display;
pub mod files;
pub mod gl;
pub mod jit;
pub mod net;
pub mod replay;

use std::alloc::Layout;

use vetro_machine::vetro_snapshot::overlay::{self, Overlay, Patches};
use vetro_machine::{Devices, Input, Machine, MachineConfig, NetSetup, Pointer, Reply, Stop};
use vetro_platform::virtio::input::InputEvent;
use vetro_platform::virtio::{
    BlockBackend, CowBackend, GpuConfig, MemBackend, VirtioBlk, VirtioBlkConfig, VirtioGpu,
};

use disk::HostDisk;
use display::WebDisplay;

/// Version of the C API: it changes at every incompatible change of the signatures.
/// 2: system-mode JIT (`vetro_machine_set_jit`, import
/// `vetro_jit.reset` and block table `env.tbl`).
/// 3: devices (`vetro_machine_new_with`, display, input, GPIO, disks)
/// and stop code `BLOCKED`.
/// 4: machine snapshots (`vetro_snapshot_*`, ADR 0015).
/// 5: TCP connections from JS to the guest's services (`vetro_net_*`,
/// port forwarding).
/// 6: persistent copy-on-write overlay of the disks (`vetro_overlay_*`,
/// ADR 0017).
/// 7: virtio-vsock (`VSOCK` bit) and file manager (`vetro_files_*`,
/// ADR 0020).
/// 8: network inspector, timeline, record & replay (`vetro_capture_*`,
/// `vetro_inspect_*`, `vetro_timeline_*`, `vetro_record_*`, `vetro_log_*`,
/// `vetro_replay_*`, `vetro_rr_status`, state reading, result
/// buffer; ADR 0023).
/// 9: file manager SQL and paths as bytes (ADR 0021).
/// 10: region JIT (ADR 0024): import `vetro_jit.runtime` (runtime module
/// `rt.*`), export `vetro_jit_vsync`, counter `yields` at the end of
/// `vetro_jit_stats`.
/// 11: FP/SIMD in regions (ADR 0026): export `vetro_jit_simd` (import
/// `env.simd` of the runtime), `JitState` with FPCR/FPSR and the clock.
/// 12: booting from Android images (`vetro_load_android`, ADR 0018 and
/// 0028), chunked snapshots (`vetro_snapshot_save_stream`,
/// `vetro_snapshot_restore_stream`, imports `vetro_host.snapshot_write/read`).
/// 13: snapshot configuration hash and compression level (ADR 0031).
/// 14: import `vetro_jit.ready` (background JIT compilation, ADR 0038).
/// 15: accelerated graphics (ADR 0037): device bit `GPU_3D`, import
/// `vetro_host.gl_execute`, exports `vetro_gl_*` and `vetro_display_is_3d`,
/// `vetro_display_read_3d`.
pub const ABI_VERSION: u32 = 15;

/// Alignment of the [`vetro_alloc`] buffers (enough for `JitState`).
const ALLOC_ALIGN: usize = 16;

/// Codes of [`vetro_run`].
pub mod stop {
    pub const BUDGET: u32 = 0;
    pub const POWER_OFF: u32 = 1;
    pub const RESET: u32 = 2;
    pub const IDLE: u32 = 3;
    pub const UNIMPLEMENTED: u32 = 4;
    /// A disk is waiting for blocks from JS (`vetro_disk_wanted`): guest time
    /// is stopped until they arrive.
    pub const BLOCKED: u32 = 5;
}

/// Device bits of [`vetro_machine_new_with`].
pub mod dev {
    pub const GPU: u32 = 1;
    pub const KEYBOARD: u32 = 2;
    pub const TABLET: u32 = 4;
    pub const MULTITOUCH: u32 = 8;
    /// virtio-net with the `vetro-net` stack and the sinkhole (`NetSetup::default`).
    pub const NET: u32 = 16;
    /// virtio-vsock (CID 3), for the file manager (`vetro_files_*`).
    pub const VSOCK: u32 = 32;
    /// The GPU offers 3D with the gfxstream decoder (ADR 0037); with
    /// `GPU` only.
    pub const GPU_3D: u32 = 64;
    /// Those of `Devices::default` (the machine of the boot test).
    pub const DEFAULT: u32 = GPU | KEYBOARD | TABLET | NET;
}

/// Bits of `flags` of [`vetro_disk_add`] and [`vetro_disk_add_mem`].
pub mod disk_flags {
    /// The guest sees the disk as read-only (without it, its writes
    /// end up in an in-memory copy-on-write layer).
    pub const READ_ONLY: u32 = 1;
}

/// virtio-input devices for [`vetro_input_events`].
pub mod input_dev {
    pub const KEYBOARD: u32 = 0;
    pub const POINTER: u32 = 1;
}

/// Bits of the `flags` of [`vetro_load_android`].
pub mod android_flags {
    /// Recovery boot: also the vendor ramdisks of type recovery.
    pub const RECOVERY: u32 = 1;
}

/// Codes of [`vetro_load_linux`] and [`vetro_load_android`].
pub mod load {
    pub const OK: u32 = 0;
    /// The loader refused the files: the reason is in `vetro_message_*`.
    pub const BOOT_ERROR: u32 = 1;
    /// The command line (or the bootloader parameters) is not UTF-8.
    pub const BAD_CMDLINE: u32 = 2;
}

/// A machine with the surrounding buffers for JS.
pub struct Vm {
    m: Machine,
    /// Compression level of the snapshots (ABI 13, ADR 0031).
    snapshot_level: vetro_machine::vetro_snapshot::Level,
    /// Console output already taken from the UART and not read by JS yet.
    out: Vec<u8>,
    out_pos: usize,
    /// Last message (load error, unimplemented instruction).
    message: String,
    unimpl: (u64, u32),
    /// Virtio slots of the disks, in order of addition (the index is the
    /// API's).
    disks: Vec<u32>,
    /// Last snapshot of `vetro_snapshot_save`, until JS copies it.
    snapshot: Vec<u8>,
    /// Persistent overlay of every disk (API index), if open.
    overlays: Vec<Option<DiskOverlay>>,
    /// Last writes of `vetro_overlay_take`, until JS applies them.
    patches: Vec<u8>,
    /// File manager client (`vetro_files_open`).
    files: Option<vetro_machine::FilesClient>,
    /// File manager messages not yet taken by JS.
    files_queue: std::collections::VecDeque<Vec<u8>>,
    /// The last message taken (`vetro_files_take`).
    files_msg: Vec<u8>,
    /// Last result (JSON, HAR, pcapng, log, keyframe, registers) for
    /// `vetro_result_ptr`.
    result: Vec<u8>,
    /// Network capture (ABI 8): on, frames, bytes, discarded, generation.
    capture_on: bool,
    capture: vetro_analysis::net::Capture,
    capture_bytes: usize,
    capture_dropped: u64,
    capture_gen: u64,
    /// Analysis of the last capture, with the number of frames it covered.
    analysis: Option<(usize, vetro_analysis::net::NetworkAnalysis)>,
    /// Input→effects timeline and input describer.
    timeline: vetro_analysis::timeline::Timeline,
    describer: analysis::Describer,
    /// Log recorded or loaded, and size of its keyframes (including
    /// those moved out).
    log: Option<vetro_machine::Log>,
    kf_sizes: Vec<u64>,
    /// A replay started from `vetro_replay_start` (and no new recording
    /// has arrived).
    replay_active: bool,
}

/// The persistent overlay of a disk on the Rust side: where every
/// cluster is in the JS file.
struct DiskOverlay {
    file: Overlay,
    /// After a restore the clusters in memory may differ from the file:
    /// the next `take` compares all of them instead of only the written ones.
    full_sync: bool,
}

/// The copy-on-write layer of a disk, whatever the base.
trait CowLayer {
    fn size(&self) -> u64;
    fn take_dirty(&mut self) -> Vec<u64>;
    fn cluster(&self, c: u64) -> Option<&[u8]>;
    fn all(&self) -> Vec<(u64, &[u8])>;
    fn load(&mut self, c: u64, data: &[u8]) -> bool;
}

impl<B: BlockBackend> CowLayer for CowBackend<B> {
    fn size(&self) -> u64 {
        BlockBackend::size(self)
    }
    fn take_dirty(&mut self) -> Vec<u64> {
        CowBackend::take_dirty(self)
    }
    fn cluster(&self, c: u64) -> Option<&[u8]> {
        CowBackend::cluster(self, c)
    }
    fn all(&self) -> Vec<(u64, &[u8])> {
        self.clusters().collect()
    }
    fn load(&mut self, c: u64, data: &[u8]) -> bool {
        self.load_cluster(c, data).is_ok()
    }
}

/// Codes of [`vetro_overlay_open`].
pub mod overlay_open {
    /// Overlay read: its clusters are in the disk.
    pub const LOADED: u32 = 0;
    /// Empty file: new overlay.
    pub const NEW: u32 = 1;
    /// Overlay of another base image (or size): discarded, the file
    /// is rewritten from scratch with the next `vetro_overlay_take`.
    pub const MISMATCH: u32 = 2;
    /// Unreadable file: discarded as above.
    pub const CORRUPT: u32 = 3;
    /// Unknown disk or without copy-on-write (read-only).
    pub const NO_DISK: u32 = 4;
}

impl Vm {
    pub fn new(cfg: &MachineConfig) -> Self {
        Self::with_devices(cfg, &Devices::default())
    }

    /// Machine with the given devices; the GPU shows on a [`WebDisplay`].
    pub fn with_devices(cfg: &MachineConfig, devices: &Devices) -> Self {
        let vm = Vm {
            m: Machine::with_devices(cfg, devices),
            out: Vec::new(),
            out_pos: 0,
            message: String::new(),
            unimpl: (0, 0),
            disks: Vec::new(),
            snapshot: Vec::new(),
            overlays: Vec::new(),
            patches: Vec::new(),
            files: None,
            files_queue: std::collections::VecDeque::new(),
            files_msg: Vec::new(),
            result: Vec::new(),
            capture_on: false,
            capture: Default::default(),
            capture_bytes: 0,
            capture_dropped: 0,
            capture_gen: 0,
            analysis: None,
            timeline: Default::default(),
            describer: Default::default(),
            log: None,
            snapshot_level: vetro_machine::vetro_snapshot::Level::Fast,
            kf_sizes: Vec::new(),
            replay_active: false,
        };
        // Without `Machine::gpu`: changing backend must not make the GPU be serviced.
        vm.with_gpu(|g| g.set_backend(Box::new(WebDisplay::default())));
        vm
    }

    fn with_gpu<R>(&self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Option<R> {
        let slot = self.m.slots().gpu?;
        let mut b = self.m.board.borrow_mut();
        b.virt.virtio_mut(slot)?.device_as_mut::<VirtioGpu>().map(f)
    }

    /// Acts on the GPU display, if any. It doesn't go through
    /// `Machine::device`: reading the image must not make the GPU be serviced
    /// (the guest sees nothing, and the moment of the read is chosen by the
    /// page, not by the guest).
    pub fn with_display<R>(&self, f: impl FnOnce(&mut WebDisplay) -> R) -> Option<R> {
        self.with_gpu(|g| g.backend_as_mut::<WebDisplay>().map(f)).flatten()
    }

    /// Adds a virtio-blk disk in the first free slot (from the top, after
    /// GPU and input); returns its index.
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

    /// Acts on the [`HostDisk`] of disk `index`, if it is one. If the machine
    /// is waiting for data (`Stop::Blocked`) the device is serviced again
    /// before the next instruction, otherwise not (an early
    /// delivery must not change the guest's timing).
    pub fn with_host_disk<R>(&mut self, index: u32, f: impl FnOnce(&mut HostDisk) -> R) -> Option<R> {
        let slot = *self.disks.get(index as usize)?;
        let pick = |b: &mut VirtioBlk| -> Option<R> {
            if let Some(c) = b.backend_as_mut::<CowBackend<HostDisk>>() {
                return Some(f(c.base_mut()));
            }
            b.backend_as_mut::<HostDisk>().map(f)
        };
        if self.m.blocked() {
            self.m.host_link::<VirtioBlk, _>(Some(slot), pick).flatten()
        } else {
            let mut b = self.m.board.borrow_mut();
            pick(b.virt.virtio_mut(slot)?.device_as_mut::<VirtioBlk>()?)
        }
    }

    /// Blocks requested by the disks since the last call: (disk, block).
    pub fn disk_wanted(&mut self) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        for i in 0..self.disks.len() as u32 {
            if let Some(w) = self.with_host_disk(i, |d| d.take_wanted()) {
                out.extend(w.into_iter().map(|b| (i, b)));
            }
        }
        out
    }

    /// Clusters written by the guest in the copy-on-write layer of disk `index`.
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

    /// Acts on the copy-on-write layer of disk `index`, if any. Without
    /// `Machine::device`: reading or loading clusters is not an input that the
    /// guest sees at a precise moment (it is loaded before boot, read
    /// between one quantum and the next).
    fn with_cow<R>(&self, index: u32, f: impl FnOnce(&mut dyn CowLayer) -> R) -> Option<R> {
        let slot = *self.disks.get(index as usize)?;
        let mut b = self.m.board.borrow_mut();
        let blk = b.virt.virtio_mut(slot)?.device_as_mut::<VirtioBlk>()?;
        if blk.backend_as_mut::<CowBackend<HostDisk>>().is_some() {
            return blk.backend_as_mut::<CowBackend<HostDisk>>().map(|c| f(c));
        }
        blk.backend_as_mut::<CowBackend<MemBackend>>().map(|c| f(c))
    }

    /// Opens the persistent overlay of disk `index` from the file contents
    /// (`bytes`, empty if it doesn't exist) for the base `identity`; the clusters read
    /// go into the copy-on-write. To be done before running the guest (and before
    /// restoring a snapshot). Returns a code of
    /// [`overlay_open`]; with `MISMATCH` and `CORRUPT` the reason is in the message.
    pub fn overlay_open(&mut self, index: u32, identity: &[u8], bytes: &[u8]) -> u32 {
        let Some(size) = self.with_cow(index, |c| c.size()) else {
            self.message = format!("disk {index} unknown or without copy-on-write");
            return overlay_open::NO_DISK;
        };
        let (file, code) = match Overlay::load(bytes, identity, size) {
            Ok(l) => {
                let code = if bytes.is_empty() { overlay_open::NEW } else { overlay_open::LOADED };
                let ok = self
                    .with_cow(index, |c| l.clusters.iter().all(|&(k, d)| c.load(k, d)))
                    .expect("disk just found");
                debug_assert!(ok, "overlay clusters checked by Overlay::load");
                (l.overlay, code)
            }
            Err(e) => {
                self.message = e.to_string();
                let code = match e {
                    overlay::LoadError::Mismatch(_) => overlay_open::MISMATCH,
                    overlay::LoadError::Corrupt(_) => overlay_open::CORRUPT,
                };
                (Overlay::new(identity, size), code)
            }
        };
        let i = index as usize;
        if self.overlays.len() <= i {
            self.overlays.resize_with(i + 1, || None);
        }
        self.overlays[i] = Some(DiskOverlay { file, full_sync: false });
        code
    }

    /// The writes to make to the overlay file of disk `index` so that it
    /// contains the clusters written so far (empty if there is nothing new).
    pub fn overlay_take(&mut self, index: u32) -> Option<Patches> {
        let mut ov = self.overlays.get_mut(index as usize)?.take()?;
        let p = self.with_cow(index, |c| {
            if core::mem::take(&mut ov.full_sync) {
                c.take_dirty();
                ov.file.sync(c.all())
            } else {
                let dirty = c.take_dirty();
                ov.file.update(dirty.into_iter().map(|k| (k, c.cluster(k))))
            }
        });
        self.overlays[index as usize] = Some(ov);
        p
    }

    /// (generation, clusters, slots, damaged slots, file length)
    /// of the overlay of disk `index`.
    pub fn overlay_info(&self, index: u32) -> Option<[u64; 5]> {
        let o = &self.overlays.get(index as usize)?.as_ref()?.file;
        Some([o.generation(), o.clusters() as u64, o.slots(), o.damaged(), o.file_len()])
    }

    pub fn machine(&mut self) -> &mut Machine {
        &mut self.m
    }

    /// Turns on the system-mode JIT on the JS engine, with threshold
    /// `hot_threshold` and `batch` blocks per module.
    pub fn set_jit(&mut self, hot_threshold: u32, batch: u32) {
        self.set_jit_with(hot_threshold, batch, false, false);
    }

    /// Like [`Vm::set_jit`]; `profile` also counts the interpreter's
    /// instructions per class (measurements only, slower).
    pub fn set_jit_with(&mut self, hot_threshold: u32, batch: u32, profile: bool, names: bool) {
        let cfg = vetro_jit::SysJitConfig {
            hot_threshold,
            batch: batch.max(1) as usize,
            profile,
            names,
            ..vetro_jit::SysJitConfig::default()
        };
        self.m.set_jit(Some(Box::new(vetro_jit::SysJit::new(jit::JsEngine::default(), cfg))));
    }

    pub fn load_linux(&mut self, image: &[u8], initrd: Option<&[u8]>, cmdline: &[u8]) -> u32 {
        let Ok(cmdline) = core::str::from_utf8(cmdline) else {
            self.message = "non-UTF-8 command line".into();
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

    /// Vetro's Android bootloader (ADR 0018): combines `boot.img`,
    /// `vendor_boot.img` and `init_boot.img` (empty = absent) with the
    /// bootloader parameters `params` and loads the result like [`load_linux`].
    /// On success the message describes kernel, ramdisks and bootconfig.
    ///
    /// [`load_linux`]: Self::load_linux
    pub fn load_android(
        &mut self,
        boot: &[u8],
        vendor: &[u8],
        init: &[u8],
        params: &[u8],
        recovery: bool,
    ) -> u32 {
        let Ok(params) = core::str::from_utf8(params) else {
            self.message = "bootloader parameters are not UTF-8".into();
            return load::BAD_CMDLINE;
        };
        let opts = vetro_machine::android::BootOptions { params: params.to_string(), recovery };
        fn some(b: &[u8]) -> Option<&[u8]> {
            (!b.is_empty()).then_some(b)
        }
        let a = match vetro_machine::android::AndroidBoot::from_images(boot, some(vendor), some(init), &opts)
        {
            Ok(a) => a,
            Err(e) => {
                self.message = e.to_string();
                return load::BOOT_ERROR;
            }
        };
        match self.m.load_android(&a) {
            Ok(_) => {
                self.message = format!(
                    "kernel {} ({} bytes), ramdisks: {}{}; command line: {}",
                    a.kernel_format,
                    a.kernel.len(),
                    if a.ramdisks.is_empty() { "none".to_string() } else { a.ramdisks.join(", ") },
                    if a.bootconfig.is_empty() {
                        String::new()
                    } else {
                        format!(", bootconfig {} bytes", a.bootconfig.len())
                    },
                    a.cmdline
                );
                load::OK
            }
            Err(e) => {
                self.message = e.to_string();
                load::BOOT_ERROR
            }
        }
    }

    pub fn run(&mut self, budget: u64) -> u32 {
        let stop = self.m.run(budget);
        self.collect_capture();
        match stop {
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

    /// Machine snapshot (M6, ADR 0015). The console output already
    /// taken from the UART and not read by JS yet is not part of it: save
    /// after reading the console.
    pub fn save_state(&self) -> Vec<u8> {
        self.m.save_with(self.snapshot_level)
    }

    /// Chunked snapshot ([`Machine::save_stream`]): the content goes to
    /// `sink`, the header is the result. The buffer for the part before the
    /// RAM is sized on the disks' copy-on-write layer.
    pub fn save_state_stream(
        &mut self,
        sink: &mut dyn FnMut(&[u8]),
    ) -> [u8; vetro_machine::vetro_snapshot::HEADER_LEN] {
        let cow: usize = (0..self.disks.len() as u32).map(|i| self.disk_dirty_clusters(i)).sum();
        self.m.save_stream_with(self.snapshot_level, cow * (4096 + 13) + (16 << 20), sink)
    }

    /// Restores a snapshot onto this machine, which must be
    /// configured like the saved one (same devices and disks, already
    /// added with the same parameters). The display immediately receives
    /// the restored image; unread console output is discarded.
    pub fn restore_state(&mut self, bytes: &[u8]) -> Result<(), vetro_machine::vetro_snapshot::Error> {
        self.m.load_state(bytes)?;
        // The restored clusters are those of the snapshot: the overlay
        // file is compared in full at the next `take`.
        self.after_state_change();
        Ok(())
    }

    /// Like [`Vm::restore_state`] with the file read in chunks
    /// ([`vetro_machine::Machine::load_state_stream`]).
    pub fn restore_state_stream(
        &mut self,
        head: &[u8],
        pull: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Result<(), vetro_machine::vetro_snapshot::Error> {
        self.m.load_state_stream(head, pull)?;
        self.after_state_change();
        Ok(())
    }

    /// Copies into `dst` at most `dst.len()` bytes of console output, which
    /// it consumes; the rest waits for the next call.
    pub fn console_read(&mut self, dst: &mut [u8]) -> usize {
        if self.out_pos == self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        }
        let new = self.m.console_output();
        if !new.is_empty() {
            let at = vetro_analysis::timeline::step_us(self.m.steps);
            self.timeline.push_console(at, &new);
            self.out.extend(new);
        }
        let n = dst.len().min(self.out.len() - self.out_pos);
        dst[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        n
    }
}

/// Writes at most `cap` values of `v` into `out`; returns how many.
unsafe fn write_u64s(out: *mut u64, cap: usize, v: &[u64]) -> usize {
    let n = v.len().min(cap);
    if n > 0 && !out.is_null() {
        // SAFETY: `out` is valid for `cap` values (API contract).
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// `&[u8]` from pointer and length passed by JS (null or empty = empty).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(ptr, len) } }
}

/// Installs (once) the hook that sends JS the message of a panic:
/// on wasm32-unknown-unknown a panic is a silent `unreachable` trap.
fn install_panic_hook() {
    #[cfg(target_arch = "wasm32")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                let msg = info.to_string();
                // SAFETY: `vetro_host` import, reads `msg` during the call.
                unsafe { host::panic(msg.as_ptr(), msg.len()) };
            }))
        });
    }
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "vetro_host")]
    unsafe extern "C" {
        /// UTF-8 message of a panic, right before the trap.
        pub fn panic(ptr: *const u8, len: usize);
        /// A chunk of a `vetro_snapshot_save_stream` snapshot.
        pub fn snapshot_write(ptr: *const u8, len: usize);
        /// The next bytes of a snapshot for `vetro_snapshot_restore_stream`.
        pub fn snapshot_read(ptr: *mut u8, cap: usize) -> usize;
    }
}

/// API version ([`ABI_VERSION`]).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_abi_version() -> u32 {
    ABI_VERSION
}

/// Allocates `len` bytes aligned to 16 in the module's memory; null if
/// `len == 0` or if memory is not enough. Beware: the allocation may make
/// memory grow, and the JS views on `memory.buffer` must be recreated.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_alloc(len: usize) -> *mut u8 {
    match Layout::from_size_align(len, ALLOC_ALIGN) {
        // SAFETY: non-zero size.
        Ok(l) if len > 0 => unsafe { std::alloc::alloc(l) },
        _ => core::ptr::null_mut(),
    }
}

/// Frees a [`vetro_alloc`] buffer with the same length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        // SAFETY: `ptr` comes from `vetro_alloc(len)` (API contract).
        unsafe { std::alloc::dealloc(ptr, Layout::from_size_align_unchecked(len, ALLOC_ALIGN)) }
    }
}

/// Creates a machine: `ram_size` in bytes (0 = 1 GiB), RTC time in
/// seconds since the epoch and device tree seed (both 0 = the values of
/// `MachineConfig::default`, those of the native tests).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_machine_new(ram_size: u64, now_secs: u64, seed: u64) -> *mut Vm {
    install_panic_hook();
    Box::into_raw(Box::new(Vm::new(&config(ram_size, now_secs, seed))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_free(vm: *mut Vm) {
    if !vm.is_null() {
        // SAFETY: `vm` comes from `vetro_machine_new` and is no longer used.
        drop(unsafe { Box::from_raw(vm) });
    }
}

/// Loads kernel (`Image`), initramfs (null or length 0 = none) and command
/// line (UTF-8). The buffers can be freed right after. Returns
/// a code of [`load`].
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
    // SAFETY: pointers valid for the given lengths (API contract).
    let vm = unsafe { &mut *vm };
    let (image, initrd, cmdline) =
        unsafe { (bytes(image, image_len), bytes(initrd, initrd_len), bytes(cmdline, cmdline_len)) };
    vm.load_linux(image, (!initrd.is_empty()).then_some(initrd), cmdline)
}

/// Boot from Android images (ABI 12): `boot.img`, `vendor_boot.img` and
/// `init_boot.img` (null or 0 long = absent), bootloader parameters
/// (`params`, UTF-8: `androidboot.*` go into the bootconfig with a v4
/// `vendor_boot`), `flags` from [`android_flags`]. Returns a [`load`] code; on
/// success `vetro_message_*` describes kernel and ramdisks. The buffers can be
/// freed right after.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn vetro_load_android(
    vm: *mut Vm,
    boot: *const u8,
    boot_len: usize,
    vendor_boot: *const u8,
    vendor_boot_len: usize,
    init_boot: *const u8,
    init_boot_len: usize,
    params: *const u8,
    params_len: usize,
    flags: u32,
) -> u32 {
    // SAFETY: pointers valid for the given lengths (API contract).
    let vm = unsafe { &mut *vm };
    let (boot, vendor, init, params) = unsafe {
        (
            bytes(boot, boot_len),
            bytes(vendor_boot, vendor_boot_len),
            bytes(init_boot, init_boot_len),
            bytes(params, params_len),
        )
    };
    vm.load_android(boot, vendor, init, params, flags & android_flags::RECOVERY != 0)
}

/// Runs at most `budget` instructions; returns a code of [`stop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_run(vm: *mut Vm, budget: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &mut *vm }.run(budget)
}

/// Turns on the JIT (ADR 0013) with threshold `hot_threshold` (entries before
/// translating a block) and `batch` blocks per module (0 = 1). The result
/// of execution doesn't change; only the speed changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_set_jit(vm: *mut Vm, hot_threshold: u32, batch: u32) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &mut *vm }.set_jit(hot_threshold, batch);
}

/// JIT counters (`SysJitStats`, in field order) in `out`, at
/// most `cap` values; returns how many it wrote (0 without JIT).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_stats(vm: *const Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
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
        s.yields,
        s.host_lds,
        s.host_sts,
        s.epochs_regs,
        s.epochs_tlbi,
        s.epochs_code,
        s.wasm_bytes,
    ];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// Turns on the JIT like [`vetro_machine_set_jit`]; `flags` bit 0 also
/// counts the interpreter's instructions per class ([`vetro_jit_profile`]).
/// Measurements only: the result of execution doesn't change.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_set_jit_with(vm: *mut Vm, hot_threshold: u32, batch: u32, flags: u32) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &mut *vm }.set_jit_with(hot_threshold, batch, flags & 1 != 0, flags & 2 != 0);
}

/// Report of the `n` most frequent instruction classes executed by the
/// interpreter with the JIT active, then those executed by `env.simd`, as text
/// in the result buffer; 0 without profile.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_profile(vm: *mut Vm, n: u32) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let Some(p) = vm.m.jit_profile() else { return 0 };
    let mut s = p.report(n as usize);
    if let Some(h) = vetro_jit::helper::profile_report(n as usize) {
        s.push_str("env.simd: ");
        s.push_str(&h);
    }
    vm.set_result(s.into_bytes())
}

/// Machine measurement counters (`vetro_machine::Perf`, in field order) in
/// `out`, at most `cap`; returns how many it wrote.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_perf(vm: *const Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
    let p = unsafe { &*vm }.m.perf();
    let v = [p.interp_steps, p.wfi_steps, p.wfis, p.syncs, p.services];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// Instructions executed (the guest clock).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_steps(vm: *const Vm) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.m.steps
}

/// Guest time in nanoseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_guest_ns(vm: *const Vm) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.m.guest_ns()
}

/// Reads and consumes at most `cap` bytes of console output into `dst`;
/// returns how many. 0 = nothing new.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_read(vm: *mut Vm, dst: *mut u8, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `dst` is valid for `cap` bytes.
    let vm = unsafe { &mut *vm };
    if dst.is_null() || cap == 0 {
        return 0;
    }
    vm.console_read(unsafe { core::slice::from_raw_parts_mut(dst, cap) })
}

/// Queues `len` bytes on the console, as from the keyboard.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_write(vm: *mut Vm, src: *const u8, len: usize) {
    // SAFETY: `vm` comes from `vetro_machine_new`, `src` is valid for `len` bytes.
    let vm = unsafe { &mut *vm };
    vm.user_input(Input::Console(unsafe { bytes(src, len) }.to_vec()));
}

/// Last message (UTF-8): load error or unimplemented
/// instruction. Valid until the next call on the machine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.message.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_len(vm: *const Vm) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.message.len()
}

/// PC of the last unimplemented instruction (`stop::UNIMPLEMENTED`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_pc(vm: *const Vm) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.unimpl.0
}

/// Encoding of the last unimplemented instruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_raw(vm: *const Vm) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &*vm }.unimpl.1
}

// ---- Devices (ABI 3) ---------------------------------------------------------

/// `Devices` from the bits of [`dev`] and from the initial GPU resolution (0 =
/// the default one, 1280x800).
pub fn devices_from(bits: u32, width: u32, height: u32) -> Devices {
    let d = GpuConfig::default();
    let gpu = GpuConfig {
        width: if width == 0 { d.width } else { width },
        height: if height == 0 { d.height } else { height },
        virgl: bits & dev::GPU_3D != 0,
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
        vsock_cid: (bits & dev::VSOCK != 0).then_some(3),
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

/// Like [`vetro_machine_new`], with the chosen devices: `devices` are bits
/// of [`dev`] (`MULTITOUCH` wins over `TABLET`), `width`x`height` the
/// initial resolution of scanout 0 (0 = 1280x800).
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

/// Installs the WebGL2 executor (import `vetro_host.gl_execute`) in the
/// GPU's gfxstream renderer: 1 if the machine has one (`GPU_3D`), else 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_gl_enable(vm: *mut Vm) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_gpu(gl::enable).unwrap_or(false) as u32
}

/// gfxstream counters into `out` (u64): GLES calls, batches run, frames
/// presented, bytes read back, unhandled calls. Returns how many were
/// written (0 without a renderer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_gl_stats(vm: *const Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`; `out` holds `cap` values.
    let vm = unsafe { &*vm };
    let Some(Some(s)) = vm.with_gpu(|g| {
        g.renderer_as::<vetro_machine::vetro_gfxstream::Gfxstream>().map(|r| r.gl.stats.clone())
    }) else {
        return 0;
    };
    let v = [s.calls, s.batches, s.presents, s.readback_bytes, s.unhandled];
    let n = v.len().min(cap);
    if n > 0 {
        // SAFETY: see above.
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// Takes the gfxstream decoder's log (unhandled calls…) into the message
/// buffer (`vetro_message_ptr`/`len`), one line per entry; returns its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_gl_take_log(vm: *mut Vm) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let lines = vm
        .with_gpu(|g| g.renderer_as_mut::<vetro_machine::vetro_gfxstream::Gfxstream>().map(|r| r.take_log()))
        .flatten()
        .unwrap_or_default();
    vm.message = lines.join("\n");
    vm.message.len()
}

/// 1 if scanout `scanout` shows a 3D resource (its pixels are drawn by the
/// WebGL2 executor, not in `vetro_display_ptr`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_is_3d(vm: *const Vm, scanout: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_gpu(|g| g.scanout_3d(scanout).is_some()).unwrap_or(false) as u32
}

/// Reads back the 3D scanout into the display's RGBA image (the one of
/// `vetro_display_ptr`, rows from the top) and returns its pointer; null if
/// the scanout is not 3D. A host read (screenshots, home-screen detection):
/// the guest never sees it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_read_3d(vm: *mut Vm, scanout: u32) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    let px = vm.with_gpu(|g| gl::scanout_rgba(g, scanout)).flatten();
    let Some((w, h, rgba)) = px else { return core::ptr::null() };
    vm.with_display(|d| d.set_3d_pixels(scanout, w, h, rgba)).flatten().unwrap_or(core::ptr::null())
}

/// Size of scanout `scanout`: `(width << 32) | height`, 0 if
/// off or if there is no GPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_size(vm: *const Vm, scanout: u32) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.on => u64::from(s.width) << 32 | u64::from(s.height),
        _ => 0,
    })
    .unwrap_or(0)
}

/// RGBA pixels of the scanout (rows of `width * 4` bytes), null if
/// off. Valid until the next call that runs the guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_ptr(vm: *const Vm, scanout: u32) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.on => s.rgba.as_ptr(),
        _ => core::ptr::null(),
    })
    .unwrap_or(core::ptr::null())
}

/// Updates of the scanout (image or turning off): if it doesn't change there
/// is nothing to redraw.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_updates(vm: *const Vm, scanout: u32) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    vm.with_display(|d| d.screen(scanout).map_or(0, |s| s.updates)).unwrap_or(0)
}

/// Rectangle changed since the last call (union): writes `x, y,
/// width, height` into `out` and returns 1, or 0 if nothing changed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_take_dirty(vm: *const Vm, scanout: u32, out: *mut u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for 4 values.
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

/// Resolution requested for the scanout (like resizing the window):
/// the driver sees it with a configuration interrupt. It is a host
/// input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_display_resize(vm: *mut Vm, scanout: u32, width: u32, height: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    (vm.user_input(Input::Display { scanout, width, height }) != Reply::NoDevice) as u32
}

/// Cursor state of the scanout in `out` (6 values): resource (0 =
/// hidden), x, y, hot_x, hot_y, number of changes. Returns 0 without a GPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_cursor_state(vm: *const Vm, scanout: u32, out: *mut u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for 6 values.
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

/// Cursor image, 64x64 RGBA; null if there is none. Valid until the
/// next call that runs the guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_cursor_image(vm: *const Vm, scanout: u32) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    let n = (display::CURSOR_SIZE * display::CURSOR_SIZE * 4) as usize;
    vm.with_display(|d| match d.screen(scanout) {
        Some(s) if s.cursor_rgba.len() == n => s.cursor_rgba.as_ptr(),
        _ => core::ptr::null(),
    })
    .unwrap_or(core::ptr::null())
}

/// Queues `count` evdev events (`type, code, value` as three consecutive
/// `u32`s) on device `device` of [`input_dev`]. The caller puts
/// the SYN_REPORTs. Returns 0 if the device doesn't exist.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_events(
    vm: *mut Vm,
    device: u32,
    events: *const u32,
    count: usize,
) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `events` is valid for 3 * count values.
    let vm = unsafe { &mut *vm };
    let raw = if count == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(events, 3 * count) } };
    let ev: Vec<InputEvent> = raw
        .as_chunks::<3>()
        .0
        .iter()
        .map(|e| InputEvent { ty: e[0] as u16, code: e[1] as u16, value: e[2] })
        .collect();
    let input = match device {
        input_dev::KEYBOARD => Input::Keyboard(ev),
        input_dev::POINTER => Input::Pointer(ev),
        _ => return 0,
    };
    (vm.user_input(input) != Reply::NoDevice) as u32
}

/// A keyboard key (Linux code `KEY_*`) pressed or released, with
/// SYN_REPORT. 0 if there is no keyboard.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_key(vm: *mut Vm, code: u32, down: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    (vm.user_input(Input::Keyboard(Input::key_events(code as u16, down != 0))) != Reply::NoDevice) as u32
}

/// Absolute position of the tablet (0..=32767 per axis), with SYN_REPORT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_abs(vm: *mut Vm, x: u32, y: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    (vm.user_input(Input::Pointer(Input::move_abs_events(x, y))) != Reply::NoDevice) as u32
}

/// Pointer button (`BTN_LEFT` = 0x110, ...), with SYN_REPORT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_button(vm: *mut Vm, code: u32, down: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    (vm.user_input(Input::Pointer(Input::key_events(code as u16, down != 0))) != Reply::NoDevice) as u32
}

/// Touchscreen contact `slot`: `down` != 0 puts it down or moves it to
/// (x, y) (0..=32767), 0 lifts it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_touch(vm: *mut Vm, slot: u32, x: u32, y: u32, down: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let ev = Input::touch_events(slot, (down != 0).then_some((x, y)));
    (vm.user_input(Input::Pointer(ev)) != Reply::NoDevice) as u32
}

/// Keyboard LEDs lit by the guest (`LED_*` bits).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_input_leds(vm: *mut Vm) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let slot = vm.m.slots().keyboard;
    let b = vm.m.board.borrow();
    slot.and_then(|s| b.virt.virtio(s)?.device_as::<vetro_platform::virtio::VirtioInput>().map(|k| k.leds()))
        .unwrap_or(0)
}

/// Drives line `line` of the PL061 GPIO (3 = power button,
/// `gpio-keys` KEY_POWER). It is a host input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_gpio_input(vm: *mut Vm, line: u32, level: u32) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.user_input(Input::Gpio { line, level: level != 0 });
}

/// GPIO line of the power button.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_power_key_line() -> u32 {
    vetro_platform::pl061::POWER_KEY_LINE
}

// ---- Disks (ABI 3) -----------------------------------------------------------

/// Adds a virtio-blk disk of `size` bytes with the data from JS in blocks
/// of `block_size` bytes (a power of two, at least 512), at most `max_blocks`
/// blocks in memory (0 = no limit). `flags`: bits of [`disk_flags`].
/// Returns the disk index, or -1 (reason in the message). To be
/// called before running the guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_add(
    vm: *mut Vm,
    size: u64,
    block_size: u32,
    max_blocks: u32,
    flags: u32,
) -> i32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let d = match HostDisk::new(size, block_size, max_blocks as usize) {
        Ok(d) => d,
        Err(e) => {
            vm.message = format!("disk refused: {e:?}");
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

/// Adds a disk with all its contents already in memory (copied from
/// `data`; length rounded down to 512 as for [`vetro_disk_add`]): always ready, for small
/// files and as a reference in the tests. Same `flags` and result as
/// [`vetro_disk_add`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_add_mem(vm: *mut Vm, data: *const u8, len: usize, flags: u32) -> i32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `data` is valid for `len` bytes.
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

/// Blocks requested by the disks since the last call, as `(disk,
/// block)` pairs of `u64` in `out` (at most `cap` pairs; the rest stays for the
/// next call). Returns how many pairs. Every block appears only
/// once until it arrives ([`vetro_disk_fill`]) or fails
/// ([`vetro_disk_fail`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_wanted(vm: *mut Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for 2 * cap values.
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
    // Those that don't fit in `out` go back to the list.
    for &(d, b) in &all[n..] {
        vm.with_host_disk(d, |h| h.requeue(b));
    }
    n
}

/// Delivers block `block` of disk `disk` (`len` = the block size,
/// or less for the last one). 0 = accepted; 1 = unknown disk; 2 =
/// block outside the disk; 3 = wrong length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_fill(
    vm: *mut Vm,
    disk: u32,
    block: u64,
    data: *const u8,
    len: usize,
) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `data` is valid for `len` bytes.
    let vm = unsafe { &mut *vm };
    let data = unsafe { bytes(data, len) };
    match vm.with_host_disk(disk, |d| d.fill(block, data)) {
        None => 1,
        Some(Ok(())) => 0,
        Some(Err(disk::DiskError::OutOfRange)) => 2,
        Some(Err(_)) => 3,
    }
}

/// JS could not obtain the block: the guest request waiting for it
/// ends with an I/O error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_fail(vm: *mut Vm, disk: u32, block: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.with_host_disk(disk, |d| d.fail(block)).is_some() as u32
}

/// Disk counters in `out` (at most `cap`): size in bytes,
/// block size, blocks in memory, missed reads, blocks
/// delivered, blocks evicted, failed blocks, copy-on-write clusters
/// written by the guest. Returns how many values (0 = unknown disk; for
/// an in-memory disk only size and clusters are meaningful).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_disk_stats(vm: *mut Vm, disk: u32, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
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

/// Snapshot format version (`vetro_snapshot::FORMAT_VERSION`):
/// JS uses it in the cache keys, so a snapshot of another
/// version is not even tried for restore.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_snapshot_version() -> u32 {
    vetro_machine::vetro_snapshot::FORMAT_VERSION
}

/// ABI 13 (ADR 0031): the machine configuration hash that a snapshot of this
/// machine carries in its header (`Machine::config_hash`: RAM, devices, disks
/// and virtio slots). Read after the disks are added; JS puts it in snapshot
/// keys, so that a prebuilt snapshot for another configuration is not even
/// downloaded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_config_hash(vm: *mut Vm) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.machine().config_hash()
}

/// ABI 13 (ADR 0031): how the next snapshots compress RAM and disks: 0 =
/// fast (the default, for snapshots saved while the guest waits), 1 = small
/// (frames with LZ77 and Huffman codes: several times slower to save, about a
/// third smaller; for snapshots that are downloaded). Restoring accepts both.
/// Returns 0, or 1 for an unknown level (nothing changes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_set_level(vm: *mut Vm, level: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.snapshot_level = match level {
        0 => vetro_machine::vetro_snapshot::Level::Fast,
        1 => vetro_machine::vetro_snapshot::Level::Small,
        _ => return 1,
    };
    0
}

/// Saves the machine into an internal buffer and returns its length;
/// the bytes are read from [`vetro_snapshot_ptr`] (valid until the next
/// save, [`vetro_snapshot_clear`] or the destruction of the
/// machine). Read the console first: the output already read from the UART and
/// not yet delivered to JS doesn't go into the snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_save(vm: *mut Vm) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.snapshot = vm.save_state();
    vm.snapshot.len()
}

/// Chunked snapshot (ABI 12, ADR 0028), for snapshots that do not fit whole
/// in the module's memory (Android): the content goes to JS through the import
/// `vetro_host.snapshot_write(ptr, len)`, one chunk at a time and in order (to
/// be written after the header, from offset `vetro_snapshot::HEADER_LEN`); the
/// header stays in the [`vetro_snapshot_ptr`] buffer. Returns the length of the
/// whole file. On the native target (tests) the chunks end up in the buffer
/// after the header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_save_stream(vm: *mut Vm) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let mut total = vetro_machine::vetro_snapshot::HEADER_LEN as u64;
    #[cfg(target_arch = "wasm32")]
    {
        let head = vm.save_state_stream(&mut |c| {
            total += c.len() as u64;
            // SAFETY: `vetro_host` import, reads `c` during the call.
            unsafe { host::snapshot_write(c.as_ptr(), c.len()) };
        });
        vm.snapshot = head.to_vec();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut body = Vec::new();
        let head = vm.save_state_stream(&mut |c| {
            total += c.len() as u64;
            body.extend_from_slice(c);
        });
        vm.snapshot = [head.as_slice(), &body].concat();
    }
    total
}

/// The bytes of the last [`vetro_snapshot_save`] (null if there is none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    if vm.snapshot.is_empty() { core::ptr::null() } else { vm.snapshot.as_ptr() }
}

/// Frees the buffer of the last save.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_clear(vm: *mut Vm) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &mut *vm }.snapshot = Vec::new();
}

/// Codes of [`vetro_snapshot_restore`].
pub mod restore {
    pub const OK: u32 = 0;
    /// Not a Vetro snapshot.
    pub const BAD_MAGIC: u32 = 1;
    /// Format of another version (`vetro_snapshot_version`).
    pub const VERSION: u32 = 2;
    /// Machine configured differently (RAM, devices, disks, seed).
    pub const CONFIG: u32 = 3;
    /// Damaged or inconsistent snapshot: the machine must be discarded.
    pub const CORRUPT: u32 = 4;
}

/// Restores the snapshot of `len` bytes in `data` onto this machine,
/// configured like the saved one (same devices of
/// `vetro_machine_new_with`, same disks added in the same order with
/// the same parameters, before calling it). The buffer can be freed
/// right after. With a code other than 0 the reason is in the message; with
/// `BAD_MAGIC`, `VERSION` and `CONFIG` the machine hasn't changed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_restore(vm: *mut Vm, data: *const u8, len: usize) -> u32 {
    use vetro_machine::vetro_snapshot::Error;
    // SAFETY: `vm` comes from `vetro_machine_new`, `data` is valid for `len` bytes.
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

/// Chunked restore (ABI 12, ADR 0028), without the whole file in the module's
/// memory: `head` is the file's bytes up to and including the header of the
/// `RAM ` section; the rest (the RAM content) is requested from JS through the
/// import `vetro_host.snapshot_read(ptr, cap) -> bytes written` (0 = end).
/// Same codes as [`vetro_snapshot_restore`]; with `CORRUPT` (including a wrong
/// checksum, found at the end) the machine must be discarded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_snapshot_restore_stream(vm: *mut Vm, head: *const u8, head_len: usize) -> u32 {
    use vetro_machine::vetro_snapshot::Error;
    // SAFETY: `vm` comes from `vetro_machine_new`, `head` is valid for `head_len` bytes.
    let vm = unsafe { &mut *vm };
    let head = unsafe { bytes(head, head_len) };
    reserve_heap(head_len);
    #[cfg(target_arch = "wasm32")]
    let mut pull = |buf: &mut [u8]| -> usize {
        // SAFETY: `vetro_host` import: writes at most `buf.len()` bytes into `buf`.
        (unsafe { host::snapshot_read(buf.as_mut_ptr(), buf.len()) }).min(buf.len())
    };
    #[cfg(not(target_arch = "wasm32"))]
    let mut pull = |_: &mut [u8]| -> usize { 0 };
    match vm.restore_state_stream(head, &mut pull) {
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

/// Grows the heap once by about `bytes` before a restore decodes that much
/// state into many small allocations (the disk's copy-on-write clusters:
/// hundreds of MiB for Android, 4 KiB each). Without it the allocator grows
/// the linear memory 64 KiB at a time, and each `memory.grow` of a memory of
/// gigabytes costs V8 about half a millisecond: 16% of an Android restore
/// (M4). The block is freed at once and stays in the allocator's top chunk.
fn reserve_heap(bytes: usize) {
    let v = Vec::<u8>::with_capacity(bytes.saturating_add(bytes / 8));
    // Not optimized away: the allocation is the point.
    core::hint::black_box(v.as_ptr());
    drop(v);
}

// ---- Persistent disk overlay (ABI 6, ADR 0017) -----------------------------

/// Opens the persistent overlay of disk `disk` (added with
/// [`vetro_disk_add`] or [`vetro_disk_add_mem`] without read-only): `data`
/// is the contents of the saved file (length 0 if there is none yet),
/// `identity` the string that identifies the base image (URL, size,
/// ETag). The clusters read go into the disk's copy-on-write. To be called
/// before [`vetro_run`] and before [`vetro_snapshot_restore`]. Codes of
/// [`overlay_open`]; reason for `MISMATCH`/`CORRUPT`/`NO_DISK` in the message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_overlay_open(
    vm: *mut Vm,
    disk: u32,
    identity: *const u8,
    identity_len: usize,
    data: *const u8,
    data_len: usize,
) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, the buffers are valid for the
    // given lengths.
    let vm = unsafe { &mut *vm };
    let (identity, data) = unsafe { (bytes(identity, identity_len), bytes(data, data_len)) };
    vm.overlay_open(disk, identity, data)
}

/// Prepares the writes to make to the overlay file of disk `disk`
/// so that it contains the guest writes made so far, and returns their
/// length (0 = nothing to write, or disk without an overlay). The bytes, in
/// [`vetro_overlay_ptr`]: u64 length to truncate the file to first
/// (`u64::MAX` = don't truncate), u32 number of writes, then for each one u64
/// offset, u32 length and the bytes (little endian). They must be applied in
/// order: the file header is the last one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_overlay_take(vm: *mut Vm, disk: u32) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.patches = vm.overlay_take(disk).filter(|p| !p.is_empty()).map(|p| p.encode()).unwrap_or_default();
    vm.patches.len()
}

/// The bytes of the last [`vetro_overlay_take`] (null if empty), valid until
/// the next `vetro_overlay_take`, [`vetro_overlay_clear`] or the
/// destruction of the machine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_overlay_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    if vm.patches.is_empty() { core::ptr::null() } else { vm.patches.as_ptr() }
}

/// Frees the buffer of the last [`vetro_overlay_take`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_overlay_clear(vm: *mut Vm) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    unsafe { &mut *vm }.patches = Vec::new();
}

/// Overlay counters of disk `disk` in `out` (at most `cap`):
/// generation, clusters in the file, slots in the file, damaged slots found
/// at opening, length of the file after the given writes. Returns
/// how many values (0 = disk without an overlay).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_overlay_info(vm: *const Vm, disk: u32, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
    let vm = unsafe { &*vm };
    let Some(v) = vm.overlay_info(disk) else { return 0 };
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
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
        assert!(msg.contains("truncated"), "{msg}");
        let bad = [0xffu8];
        let code =
            unsafe { vetro_load_linux(vm, junk.as_ptr(), junk.len(), core::ptr::null(), 0, bad.as_ptr(), 1) };
        assert_eq!(code, load::BAD_CMDLINE);
        unsafe { vetro_machine_free(vm) };
    }

    /// ABI 12: images rejected with the reason, parameters not UTF-8.
    #[test]
    fn avvio_android_rifiutato_con_messaggio() {
        let vm = small();
        let msg = |vm| unsafe {
            core::str::from_utf8(bytes(vetro_message_ptr(vm), vetro_message_len(vm))).unwrap().to_string()
        };
        let junk = [0u8; 64];
        let p = b"androidboot.serialno=X";
        let code = unsafe {
            vetro_load_android(
                vm,
                junk.as_ptr(),
                junk.len(),
                core::ptr::null(),
                0,
                core::ptr::null(),
                0,
                p.as_ptr(),
                p.len(),
                0,
            )
        };
        assert_eq!(code, load::BOOT_ERROR);
        assert!(!msg(vm).is_empty());
        let bad = [0xffu8];
        let code = unsafe {
            vetro_load_android(
                vm,
                junk.as_ptr(),
                junk.len(),
                core::ptr::null(),
                0,
                core::ptr::null(),
                0,
                bad.as_ptr(),
                1,
                0,
            )
        };
        assert_eq!(code, load::BAD_CMDLINE);
        assert!(msg(vm).contains("UTF-8"), "{}", msg(vm));
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

    /// Devices chosen by the bits, display and input without a driver, disks with
    /// their rules.
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
            assert_eq!(vetro_display_resize(vm, 0, 800, 600), 0, "without a GPU");
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
            assert_eq!(st[0], 512, "rounded down to 512");
            assert_eq!(vetro_disk_stats(vm, 2, st.as_mut_ptr(), 8), 0);
            let blk = vec![1u8; 65536];
            assert_eq!(vetro_disk_fill(vm, 0, 3, blk.as_ptr(), blk.len()), 0);
            assert_eq!(vetro_disk_fill(vm, 0, 16, blk.as_ptr(), blk.len()), 2);
            assert_eq!(vetro_disk_fill(vm, 0, 2, blk.as_ptr(), 512), 3);
            assert_eq!(vetro_disk_fill(vm, 1, 0, blk.as_ptr(), 512), 1, "in-memory disk");
            assert_eq!(vetro_disk_fill(vm, 5, 0, blk.as_ptr(), 512), 1);
            let mut w = [0u64; 4];
            assert_eq!(vetro_disk_wanted(vm, w.as_mut_ptr(), 2), 0);
            vetro_machine_free(vm);
        }
    }

    /// `vetro_disk_wanted` with little room: the rest stays in the list.
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

    /// A machine without a kernel: the reset PC is not in RAM, and every fetch
    /// is an exception to a vector that is not in RAM. The quantum runs out
    /// and the counter advances by exactly the quantum (10 ns per instruction).
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

    /// Snapshot from the C API (ABI 4): saved into a buffer, copied by JS,
    /// restored onto a new machine with the same devices and the
    /// same disk (the guest's writes in the copy-on-write included);
    /// the two continue identically. A format of another version, another
    /// configuration and random bytes are refused with their code and a
    /// message, without touching the machine.
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
            // A write in the copy-on-write layer, as the guest would do it.
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
            // Chunked (ABI 12): the same file.
            let total = vetro_snapshot_save_stream(a) as usize;
            assert_eq!(total, n);
            assert!(bytes(vetro_snapshot_ptr(a), total) == snap.as_slice(), "chunked snapshot differs");
            vetro_snapshot_clear(a);

            let b = new();
            assert_eq!(vetro_snapshot_restore(b, snap.as_ptr(), snap.len()), restore::OK);
            assert_eq!(vetro_steps(b), 1000);
            assert_eq!(vetro_disk_stats(b, 0, [0u64; 8].as_mut_ptr(), 8), 8);
            let mut st = [0u64; 8];
            vetro_disk_stats(b, 0, st.as_mut_ptr(), 8);
            assert_eq!(st[7], 1, "the written cluster comes back with the restore");
            for vm in [a, b] {
                assert_eq!(vetro_run(vm, 5000), stop::BUDGET);
            }
            assert!((&*a).save_state() == (&*b).save_state());

            // ABI 13: the configuration hash is the one in the header, and a
            // small snapshot restores the same machine.
            assert_eq!(vetro_snapshot_config_hash(b).to_le_bytes(), snap[12..20]);
            assert_eq!(vetro_snapshot_set_level(a, 7), 1, "unknown level");
            assert_eq!(vetro_snapshot_set_level(a, 1), 0);
            let n_small = vetro_snapshot_save(a);
            let small = bytes(vetro_snapshot_ptr(a), n_small).to_vec();
            vetro_snapshot_clear(a);
            assert_eq!(vetro_snapshot_save_stream(a) as usize, n_small);
            assert!(
                bytes(vetro_snapshot_ptr(a), n_small) == small.as_slice(),
                "chunked small snapshot differs"
            );
            vetro_snapshot_clear(a);
            assert_eq!(vetro_snapshot_set_level(a, 0), 0);
            let e = new();
            assert_eq!(vetro_snapshot_restore(e, small.as_ptr(), small.len()), restore::OK);
            assert!((&*a).save_state() == (&*e).save_state(), "restored from the small snapshot");
            vetro_machine_free(e);

            let mut other = snap.clone();
            other[8] ^= 0x7f;
            let c = new();
            let before = (&*c).save_state();
            assert_eq!(vetro_snapshot_restore(c, other.as_ptr(), other.len()), restore::VERSION);
            assert!(message(c).contains("version"), "{}", message(c));
            assert_eq!(vetro_snapshot_restore(c, b"other".as_ptr(), 5), restore::BAD_MAGIC);
            assert!((&*c).save_state() == before, "refused without touching the machine");
            let d = vetro_machine_new_with(64 << 20, 0, 0, dev::DEFAULT, 320, 200);
            assert_eq!(
                vetro_snapshot_restore(d, snap.as_ptr(), snap.len()),
                restore::CONFIG,
                "without the disk"
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

    /// The writes of `vetro_overlay_take` applied to an in-memory file,
    /// as JS does.
    fn take_into(vm: *mut Vm, disk: u32, file: &mut Vec<u8>) -> bool {
        let n = unsafe { vetro_overlay_take(vm, disk) };
        if n == 0 {
            return false;
        }
        let buf = unsafe { bytes(vetro_overlay_ptr(vm), n) };
        let trunc = u64::from_le_bytes(buf[..8].try_into().unwrap());
        if trunc != u64::MAX {
            file.truncate(trunc as usize);
        }
        let count = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let mut at = 12;
        for _ in 0..count {
            let off = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(buf[at + 8..at + 12].try_into().unwrap()) as usize;
            let data = &buf[at + 12..at + 12 + len];
            if file.len() < off + len {
                file.resize(off + len, 0);
            }
            file[off..off + len].copy_from_slice(data);
            at += 12 + len;
        }
        assert_eq!(at, n);
        true
    }

    fn write_disk(vm: *mut Vm, sector: u64, data: &[u8]) {
        let slot = unsafe { &*vm }.disks[0];
        unsafe { &mut *vm }
            .m
            .device::<VirtioBlk, _>(Some(slot), |b| b.backend_mut().write_sectors(sector, data))
            .unwrap()
            .unwrap();
    }

    fn read_disk(vm: *mut Vm, sector: u64, n: usize) -> Vec<u8> {
        let slot = unsafe { &*vm }.disks[0];
        let mut buf = vec![0u8; n];
        unsafe { &mut *vm }
            .m
            .device::<VirtioBlk, _>(Some(slot), |b| b.backend_mut().read_sectors(sector, &mut buf))
            .unwrap()
            .unwrap();
        buf
    }

    fn info(vm: *mut Vm) -> [u64; 5] {
        let mut v = [0u64; 5];
        assert_eq!(unsafe { vetro_overlay_info(vm, 0, v.as_mut_ptr(), 5) }, 5);
        v
    }

    /// Persistent overlay from the C API (ABI 6): the guest's writes go
    /// into the file of one session and come back in the next; an overlay of
    /// another base is discarded (file rewritten from scratch); after restoring
    /// a snapshot the file is realigned to the snapshot's clusters.
    #[test]
    fn overlay_persistente_dall_api() {
        let base: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 7) as u8).collect();
        let id = b"http://x/disco.img|65536|\"e1\"";
        let new = |file: &[u8], identity: &[u8]| {
            let vm = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
            assert_eq!(unsafe { vetro_disk_add_mem(vm, base.as_ptr(), base.len(), 0) }, 0);
            let code = unsafe {
                vetro_overlay_open(vm, 0, identity.as_ptr(), identity.len(), file.as_ptr(), file.len())
            };
            (vm, code)
        };
        let mut file = Vec::new();
        let (a, code) = new(&file, id);
        assert_eq!(code, overlay_open::NEW);
        assert!(take_into(a, 0, &mut file), "new file: the header is written");
        assert_eq!(info(a)[..3], [1, 0, 0]);
        assert!(!take_into(a, 0, &mut file), "nothing new");
        write_disk(a, 9, &[0xab; 512]);
        write_disk(a, 40, &[0xcd; 1024]);
        assert!(take_into(a, 0, &mut file));
        assert_eq!(info(a)[..3], [2, 2, 2]);
        let snap = unsafe { (&*a).save_state() };
        write_disk(a, 9, &[0x11; 512]);
        assert!(take_into(a, 0, &mut file));
        assert_eq!(info(a)[..3], [3, 2, 2], "rewritten in place");

        // Next session: the clusters come back from the file.
        let (b, code) = new(&file, id);
        assert_eq!(code, overlay_open::LOADED);
        assert_eq!(read_disk(b, 9, 512), [0x11; 512]);
        assert_eq!(read_disk(b, 40, 1024), [0xcd; 1024]);
        assert_eq!(read_disk(b, 0, 512), base[..512]);
        assert!(!take_into(b, 0, &mut file), "loaded, not written by the guest");
        assert_eq!(info(b)[0], 3);

        // Snapshot taken before the last write: the disk goes back to the
        // snapshot's and the file is realigned (one cluster rewritten).
        assert_eq!(unsafe { vetro_snapshot_restore(b, snap.as_ptr(), snap.len()) }, restore::OK);
        assert_eq!(read_disk(b, 9, 512), [0xab; 512]);
        assert!(take_into(b, 0, &mut file));
        assert_eq!(info(b)[..3], [4, 2, 2]);
        assert!(!take_into(b, 0, &mut file));
        let (c, _) = new(&file, id);
        assert_eq!(read_disk(c, 9, 512), [0xab; 512]);

        // Another base: discarded, the disk is the base's, the file is
        // rewritten from scratch.
        let (d, code) = new(&file, b"http://x/disco.img|65536|\"e2\"");
        assert_eq!(code, overlay_open::MISMATCH);
        assert!(message(d).contains("another base image"), "{}", message(d));
        assert_eq!(read_disk(d, 9, 512), base[9 * 512..10 * 512]);
        assert!(take_into(d, 0, &mut file));
        assert_eq!(file.len() as u64, overlay::HEADER_LEN);
        let (e, code) = new(b"rovinato", id);
        assert_eq!(code, overlay_open::CORRUPT);
        let ro = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        unsafe {
            assert_eq!(vetro_disk_add_mem(ro, base.as_ptr(), base.len(), disk_flags::READ_ONLY), 0);
            assert_eq!(
                vetro_overlay_open(ro, 0, id.as_ptr(), id.len(), core::ptr::null(), 0),
                overlay_open::NO_DISK
            );
            assert_eq!(vetro_overlay_take(ro, 0), 0);
            assert_eq!(vetro_overlay_info(ro, 0, [0u64; 5].as_mut_ptr(), 5), 0);
            vetro_overlay_clear(a);
            assert!(vetro_overlay_ptr(a).is_null());
            for vm in [a, b, c, d, e, ro] {
                vetro_machine_free(vm);
            }
        }
    }
}
