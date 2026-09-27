//! The machine: CPU, MMU and board, with the execution loop.

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
use crate::hooks::{Breakpoint, GuestView, Hooks, Tracer};
use crate::net::{self, NetLink, NetSetup, TappedFrame};
use crate::psci::{self, Call};

mod record;
mod snapshot;

pub use record::RecordOptions;

/// Physical address bits of the Cortex-A53 (ID_AA64MMFR0.PARange = 40 bits).
const PA_BITS: u32 = 40;

/// Machine configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineConfig {
    /// RAM from `0x4000_0000`.
    pub ram_size: u64,
    /// Initial RTC time (seconds since the epoch): external time, fixed.
    pub now_secs: u64,
    /// Seed of the randomness offered to the guest (`rng-seed` in the device tree).
    pub seed: u64,
}

impl Default for MachineConfig {
    fn default() -> Self {
        // 1 GiB like the `-m 1G` of the reference test; fixed time like the
        // virtual time of the user mode layer.
        MachineConfig { ram_size: 1 << 30, now_secs: 1_767_225_600, seed: 0x5645_5452_4f00_0001 }
    }
}

/// The absolute pointing device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pointer {
    /// QEMU's `virtio-tablet-device`: absolute pointer with buttons.
    Tablet,
    /// QEMU's `virtio-multitouch-device`: direct multi-contact touchscreen
    /// (the one Android wants).
    Multitouch,
}

/// virtio-mmio devices of the machine, besides GIC, UART and RTC.
///
/// They are attached in this order, each in the highest free slot (like
/// QEMU's `-device` in command-line order): GPU in slot 31, keyboard in 30,
/// pointer in 29, network in 28, vsock in the next free one. The default is
/// the one of the boot test compared against QEMU
/// (`tests/boot/src/lib.rs`, `QEMU_MACHINE`): 1280x800 GPU, keyboard,
/// tablet and virtio-net with the sinkhole (in QEMU `-netdev user`), without
/// vsock (QEMU in a container has no vhost-vsock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Devices {
    pub gpu: Option<GpuConfig>,
    pub keyboard: bool,
    pub pointer: Option<Pointer>,
    /// virtio-net with the `vetro-net` stack and the sinkhole.
    pub net: Option<NetSetup>,
    /// Guest CID, if there is virtio-vsock.
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
    /// No virtio devices (the M3 machine).
    pub fn none() -> Self {
        Devices { gpu: None, keyboard: false, pointer: None, net: None, vsock_cid: None }
    }
}

/// virtio-mmio slots of the attached devices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    pub gpu: Option<u32>,
    pub keyboard: Option<u32>,
    pub pointer: Option<u32>,
    pub net: Option<u32>,
    pub vsock: Option<u32>,
}

/// Why [`Machine::run`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The instruction quantum ran out: execution can continue.
    Budget,
    /// PSCI SYSTEM_OFF (or CPU_OFF of the only CPU).
    PowerOff,
    /// PSCI SYSTEM_RESET.
    Reset,
    /// WFI with no possible interrupts and no timer deadlines: the guest is
    /// waiting for input (for example from the console).
    Idle,
    /// Instruction or configuration that Vetro does not implement.
    Unimplemented { pc: u64, raw: u32, what: &'static str },
    /// A virtio-blk request is waiting for data from the host
    /// (`BlockError::NotReady`, e.g. a disk downloaded in pieces in the browser).
    /// No instruction executed since the request arrived: guest time is
    /// stopped. The host provides the data to the backend (from
    /// [`Machine::device`], which makes the device be serviced again) and
    /// calls [`Machine::run`] again: the request completes at the same
    /// instruction count as with an always-ready disk.
    Blocked,
}

pub struct Machine {
    pub cpu: Cpu,
    pub mmu: Mmu,
    pub board: RefCell<Board>,
    seed: u64,
    /// Instructions executed (and time steps skipped in WFIs): the clock.
    pub steps: u64,
    /// Next CNTPCT at which something changes by itself: the timer changes
    /// level or a network stack timer expires (cache).
    timer_deadline: Option<u64>,
    /// Next CNTPCT at which to call `poll` on the network stack.
    net_deadline: Option<u64>,
    slots: Slots,
    /// The JIT, if active ([`Machine::set_jit`]).
    jit: Option<Box<dyn SysJitDyn>>,
    /// What the interpreter does before calling the JIT again: nothing
    /// (`Jit`), one instruction (`One`), up to the next branch (`Cold`).
    interp: Next,
    /// A WFI interrupted by [`Stop::Blocked`]: it is resumed before the
    /// next instruction.
    wfi_pending: bool,
    /// Configuration and devices it was built with (for the configuration
    /// hash in snapshots).
    cfg: MachineConfig,
    devices: Devices,
    /// Console output taken from the UART and not yet given to the host, with
    /// the byte count (M10).
    console: record::ConsoleTap,
    /// Recording or replay in progress (M10, ADR 0019).
    rr: record::Rr,
    /// Outcome of the last replay (even a finished one).
    replay_status: Option<crate::record::ReplayStatus>,
    /// Introspection hook points (ADR 0027).
    hooks: Hooks,
    /// Counters for measurements (not in snapshots, no effect on execution).
    perf: Perf,
}

/// Where the machine's steps go, for measurements ([`Machine::perf`]).
/// Not part of the state: not saved, not compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Perf {
    /// Instructions executed by the interpreter.
    pub interp_steps: u64,
    /// Steps skipped by WFI (time jumped to the next deadline) and WFIs.
    pub wfi_steps: u64,
    pub wfis: u64,
    /// Calls to `sync_irqs` and virtio services.
    pub syncs: u64,
    pub services: u64,
}

/// CNTPCT after `steps` instructions: 62.5 MHz over a nominal 100 MHz.
fn counter(steps: u64) -> u64 {
    steps / 8 * 5 + steps % 8 * 5 / 8
}

/// First instruction count at which CNTPCT is at least `c`.
fn steps_for(c: u64) -> u64 {
    let mut s = c / 5 * 8;
    while counter(s) < c {
        s += 1;
    }
    s
}

impl Machine {
    /// Machine with the default devices ([`Devices::default`]).
    pub fn new(cfg: &MachineConfig) -> Self {
        Self::with_devices(cfg, &Devices::default())
    }

    /// Machine with the chosen virtio devices. The GPU starts with a
    /// [`MemDisplay`]; the browser replaces it with `VirtioGpu::set_backend`
    /// (from [`Machine::gpu`]).
    pub fn with_devices(cfg: &MachineConfig, devices: &Devices) -> Self {
        let mut cpu = Cpu::new();
        cpu.reset_system(SysConfig::default());
        let mut board = Board::new(cfg.ram_size, cfg.now_secs);
        let mut slots = Slots::default();
        let mut attach = |dev: Box<dyn VirtioDevice>| {
            Some(board.virt.attach_virtio_next(dev).expect("32 slots are enough for the machine's devices"))
        };
        if let Some(g) = &devices.gpu {
            let mut gpu = VirtioGpu::new(Box::new(MemDisplay::default()), g.clone());
            if g.virgl {
                // gfxstream on 3D (ADR 0037). It draws nothing until the host
                // gives it an executor (WebGL2 in the browser).
                let exec = Box::new(vetro_gfxstream::NullExecutor::default());
                gpu.set_renderer(Box::new(vetro_gfxstream::Gfxstream::new(exec, (g.width, g.height))));
            }
            slots.gpu = attach(Box::new(gpu));
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
            hooks: Hooks::default(),
            perf: Perf::default(),
        }
    }

    /// Turns on (or removes) the system mode JIT. The result of the
    /// execution does not change: same instructions, same interrupts at the
    /// same points, same output (see `vetro_jit::sys`).
    pub fn set_jit(&mut self, mut jit: Option<Box<dyn SysJitDyn>>) {
        if let Some(j) = jit.as_mut() {
            self.rr.note_jit();
            j.set_stops(&self.hooks.stops());
        }
        self.jit = jit;
        self.interp = Next::Jit;
    }

    // ---- Introspection (ADR 0027) ------------------------------------------

    /// Sets (or removes) the introspection event tracer; returns the
    /// previous one. Does not change the execution.
    pub fn set_tracer(&mut self, tracer: Option<Box<dyn Tracer>>) -> Option<Box<dyn Tracer>> {
        core::mem::replace(&mut self.hooks.tracer, tracer)
    }

    /// The tracer, with its type.
    pub fn tracer_mut<T: Tracer>(&mut self) -> Option<&mut T> {
        self.hooks.tracer_any()?.downcast_mut::<T>()
    }

    /// Turns EL0 syscall events on or off
    /// ([`crate::hooks::Event::SyscallEnter`] and `SyscallExit`).
    pub fn trace_syscalls(&mut self, on: bool) {
        self.hooks.syscalls = on;
        if !on {
            self.hooks.forget_pending();
        }
    }

    /// Adds an invisible breakpoint on an EL0 address; returns its number.
    /// The JIT no longer puts the address into its regions.
    pub fn add_breakpoint(&mut self, bp: Breakpoint) -> u32 {
        let id = self.hooks.add(bp);
        self.sync_stops();
        id
    }

    /// Removes a breakpoint: false if it was not there.
    pub fn remove_breakpoint(&mut self, id: u32) -> bool {
        let found = self.hooks.remove(id);
        self.sync_stops();
        found
    }

    /// The breakpoints, with their number.
    pub fn breakpoints(&self) -> Vec<(u32, Breakpoint)> {
        self.hooks.breakpoints()
    }

    fn sync_stops(&mut self) {
        let stops = self.hooks.stops();
        if let Some(j) = self.jit.as_mut() {
            j.set_stops(&stops);
        }
    }

    /// The machine read-only (registers and RAM), to read the guest
    /// between one quantum and the next.
    pub fn with_guest<R>(&self, f: impl FnOnce(&GuestView<'_>) -> R) -> R {
        let b = self.board.borrow();
        f(&GuestView { cpu: &self.cpu, ram: &b.ram, steps: self.steps })
    }

    /// Measurement counters ([`Perf`]).
    pub fn perf(&self) -> Perf {
        self.perf
    }

    /// JIT counters, if active.
    pub fn jit_stats(&self) -> Option<SysJitStats> {
        self.jit.as_ref().map(|j| j.stats())
    }

    /// Interpreter instructions per class, if the JIT counts them
    /// (`SysJitConfig::profile`).
    pub fn jit_profile(&self) -> Option<&vetro_jit::Profile> {
        self.jit.as_ref().and_then(|j| j.profile())
    }

    /// Slots of the attached virtio devices.
    pub fn slots(&self) -> Slots {
        self.slots
    }

    /// Acts on the virtio device in slot `slot`, of type `T`. The device
    /// is serviced before the next instruction (host events, data and
    /// configuration changes reach the guest).
    ///
    /// A closure cannot be recorded: during a recording (M10, ADR 0019)
    /// the access ends up in the log as an opaque event, and the replay
    /// stops there. Inputs go through [`Machine::input`]; reads through
    /// [`Machine::device_view`]; the data of an awaited disk through
    /// [`Machine::host_link`].
    pub fn device<T: VirtioDevice, R>(
        &mut self,
        slot: Option<u32>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        self.note_opaque(slot);
        self.device_raw(slot, f)
    }

    /// Like [`Machine::device`], for the external links of a device that
    /// are not guest inputs: the data of a disk the machine is waiting for
    /// ([`Stop::Blocked`], ADR 0014; guest time is stopped, and in replay
    /// the disk must give the same data). Not recorded.
    pub fn host_link<T: VirtioDevice, R>(
        &mut self,
        slot: Option<u32>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        self.device_raw(slot, f)
    }

    /// The virtio device in slot `slot`, read-only, with no effect on the
    /// machine.
    pub fn device_view<T: VirtioDevice, R>(&self, slot: Option<u32>, f: impl FnOnce(&T) -> R) -> Option<R> {
        let b = self.board.borrow();
        Some(f(b.virt.virtio(slot?)?.device_as::<T>()?))
    }

    /// The GPU, read-only (image, cursor, resources).
    pub fn gpu_view<R>(&self, f: impl FnOnce(&VirtioGpu) -> R) -> Option<R> {
        self.device_view(self.slots.gpu, f)
    }

    /// virtio-vsock, read-only (connection state, ready bytes).
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

    /// The GPU, if present.
    pub fn gpu<R>(&mut self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Option<R> {
        self.device(self.slots.gpu, f)
    }

    /// The keyboard, if present.
    pub fn keyboard<R>(&mut self, f: impl FnOnce(&mut VirtioInput) -> R) -> Option<R> {
        self.device(self.slots.keyboard, f)
    }

    /// The tablet or the touchscreen, if present.
    pub fn pointer<R>(&mut self, f: impl FnOnce(&mut VirtioInput) -> R) -> Option<R> {
        self.device(self.slots.pointer, f)
    }

    /// The network stack (with the sinkhole), if there is virtio-net: event
    /// log, connections and recorded bytes, statistics. After the access the
    /// stack is polled (`poll`) before the next instruction, so whatever
    /// the host changes upstream reaches the guest.
    ///
    /// Like [`Machine::device`], during a recording it is an opaque access:
    /// host connections go through [`Machine::input`] with
    /// [`Input::HostNet`](crate::record::Input::HostNet).
    pub fn net<R>(&mut self, f: impl FnOnce(&mut Stack<Sinkhole>) -> R) -> Option<R> {
        self.note_opaque(self.slots.net);
        self.net_raw(f)
    }

    fn net_raw<R>(&mut self, f: impl FnOnce(&mut Stack<Sinkhole>) -> R) -> Option<R> {
        self.net_input_link(|l| f(&mut l.stack))
    }

    /// The network link (stack and host frames) as an input: the device is
    /// serviced and the stack is polled (`poll`) before the next
    /// instruction.
    fn net_input_link<R>(&mut self, f: impl FnOnce(&mut NetLink) -> R) -> Option<R> {
        let r =
            self.device_raw(self.slots.net, |d: &mut VirtioNet| d.backend_as_mut::<NetLink>().map(f))??;
        self.net_deadline = Some(0);
        self.timer_deadline = Some(0);
        Some(r)
    }

    /// The network stack, read-only (log, connections, sinkhole bytes), with
    /// no effect on the machine: it can be called at any time without
    /// changing the execution.
    pub fn net_view<R>(&self, f: impl FnOnce(&Stack<Sinkhole>) -> R) -> Option<R> {
        let b = self.board.borrow();
        let d = b.virt.virtio(self.slots.net?)?.device_as::<VirtioNet>()?;
        Some(f(&d.backend_as::<NetLink>()?.stack))
    }

    /// Turns on or off the capture of Ethernet frames at the virtio-net
    /// boundary (M7, ADR 0016). Observation only: the execution stays the
    /// same, and the capture does not go into snapshots. `false` if the
    /// machine has no network.
    pub fn net_tap(&mut self, on: bool) -> bool {
        self.net_link(|l| l.set_tap(on)).is_some()
    }

    /// The frames captured by [`Machine::net_tap`] since the last call, in
    /// order, with their instant in virtual time. Does not change the execution.
    pub fn net_tap_take(&mut self) -> Vec<TappedFrame> {
        self.net_link(NetLink::take_tapped).unwrap_or_default()
    }

    /// The virtio-net backend without marking the devices to be serviced.
    fn net_link<R>(&mut self, f: impl FnOnce(&mut NetLink) -> R) -> Option<R> {
        let mut b = self.board.borrow_mut();
        let d = b.virt.virtio_mut(self.slots.net?)?.device_as_mut::<VirtioNet>()?;
        Some(f(d.backend_as_mut::<NetLink>()?))
    }

    /// virtio-vsock, if present.
    pub fn vsock<R>(&mut self, f: impl FnOnce(&mut VirtioVsock) -> R) -> Option<R> {
        self.device(self.slots.vsock, f)
    }

    /// Loads an arm64 Linux kernel (`Image`) with initramfs and command line,
    /// like QEMU's `-kernel/-initrd/-append`: copies the pieces into RAM,
    /// generates the device tree and prepares the entry registers.
    pub fn load_linux(
        &mut self,
        image: &[u8],
        initrd: Option<&[u8]>,
        bootargs: &str,
    ) -> Result<BootPlan, BootError> {
        let ram_size = self.board.borrow().ram.size();
        let ram = RamConfig::virt(ram_size);
        let initrd_len = initrd.map(|i| i.len() as u64);
        // The initramfs position does not depend on the DTB: a first plan
        // fixes it, then the DTB is generated and the plan redone with its length.
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
                assert!(b.ram.write(pa, bytes), "segment outside RAM: the plan rules it out");
            }
        }
        let e = plan.entry;
        self.cpu.reset_system(SysConfig::default());
        self.cpu.pc = e.pc;
        self.cpu.x = [0; 31];
        self.cpu.x[..4].copy_from_slice(&e.x);
        Ok(plan)
    }

    /// Loads kernel, initrd and command line prepared by the Android
    /// bootloader ([`crate::android`]): like [`Machine::load_linux`] with the
    /// pieces taken from `boot.img`, `vendor_boot.img` and `init_boot.img`.
    pub fn load_android(&mut self, boot: &crate::android::AndroidBoot) -> Result<BootPlan, BootError> {
        self.load_linux(&boot.kernel, boot.initrd(), &boot.cmdline)
    }

    /// Queues bytes on the console (PL011) as if they came from the keyboard:
    /// [`Machine::input`] with [`Input::Console`](crate::record::Input::Console).
    pub fn console_input(&mut self, bytes: &[u8]) {
        self.input(crate::record::Input::Console(bytes.to_vec()));
    }

    /// Drives an input line of the PL061 GPIO (line 3 is the power
    /// button): [`Machine::input`] with
    /// [`Input::Gpio`](crate::record::Input::Gpio).
    pub fn gpio_input(&mut self, line: u32, level: bool) {
        self.input(crate::record::Input::Gpio { line, level });
    }

    /// Consumes the console output.
    pub fn console_output(&mut self) -> Vec<u8> {
        self.drain_console();
        core::mem::take(&mut self.console.buf)
    }

    /// A virtio-blk request is waiting for data from the host ([`Stop::Blocked`]).
    pub fn blocked(&self) -> bool {
        self.board.borrow().host_wait
    }

    /// Guest time in nanoseconds (10 ns per instruction).
    pub fn guest_ns(&self) -> u64 {
        self.steps * 10
    }

    fn sync_irqs(&mut self) {
        self.perf.syncs += 1;
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
                .expect("virtio-net with NetLink in the network slot");
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
            self.perf.services += 1;
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
                .expect("virtio-net with NetLink in the network slot");
            // Never in the past: a deadline already reached repeats at the
            // next counter tick.
            self.net_deadline = link.stack.next_deadline().map(|t| net::counter_at(t).max(b.cntpct + 1));
        }
        b.update_irqs();
        let timer = b.virt.timer.next_deadline(b.cntpct);
        self.timer_deadline = match (timer, self.net_deadline) {
            (Some(a), Some(n)) => Some(a.min(n)),
            (a, n) => a.or(n),
        };
    }

    /// Steps the JIT can execute now without changing anything compared to
    /// the interpreter, up to `end`: none if the interpreter would take
    /// an interrupt (or PSTATE.IL, or a misaligned PC), otherwise up to the
    /// next timer deadline (there the interpreter updates the interrupt
    /// lines before the instruction).
    fn jit_budget(&mut self, end: u64) -> Option<u64> {
        let s = &self.cpu.sys;
        if s.il || self.cpu.pc & 3 != 0 {
            return None;
        }
        // PSTATE.I and PSTATE.A (bits 7 and 8 of DAIF), like `take_interrupt`.
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

    /// Executes at most `budget` instructions. During a replay (M10) the
    /// instructions stop at every log event to apply it, and at the end of
    /// the recording ([`Machine::replay_status`]).
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

    /// A quantum of at most `budget` instructions, with no recording or
    /// replay.
    fn run_quantum(&mut self, budget: u64) -> Stop {
        let end = self.steps.saturating_add(budget);
        self.sync_irqs();
        if self.blocked() {
            return Stop::Blocked;
        }
        if core::mem::take(&mut self.wfi_pending)
            && let Some(stop) = self.wait_for_interrupt(end)
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
                let jit = self.jit.as_mut().expect("checked above");
                // The clock for MRS CNTPCT/CNTVCT inside regions.
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
            let old_el = self.cpu.sys.el;
            let hooked = self.hooks.armed();
            let pre = if hooked { self.hooks.pre(&self.cpu) } else { None };
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
            self.perf.interp_steps += 1;
            // Only the steps of interest: a breakpoint at the PC, or an
            // EL change (SVC from EL0, ERET to EL0).
            if hooked && (pre.is_some() || old_el != self.cpu.sys.el) {
                let b = self.board.borrow();
                self.hooks.after(old_el, &ev, pre, &self.cpu, &b.ram, self.steps);
            }
            if self.interp == Next::Cold {
                // Up to the next branch (or page change, or event).
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
                    if let Some(stop) = self.wait_for_interrupt(end) {
                        return stop;
                    }
                }
                SysEvent::Hvc(_) | SysEvent::Smc(_) => {
                    let x = [self.cpu.x[0], self.cpu.x[1], self.cpu.x[2], self.cpu.x[3]];
                    match psci::call(x, self.cpu.sys.cfg.mpidr) {
                        Call::Ret(v) => self.cpu.x[0] = v as u64,
                        Call::Suspend => {
                            self.cpu.x[0] = 0;
                            if let Some(stop) = self.wait_for_interrupt(end) {
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

    /// WFI: if no interrupt is ready, time jumps to the next deadline
    /// (timer or network stack); with no deadlines the machine is
    /// idle. A deadline beyond `end`, the end of the quantum, stops time
    /// there instead (`Stop::Budget`) and the WFI goes on in the next quantum:
    /// an input the host gives in between wakes the guest at that instruction
    /// instead of at the deadline, and the host's clock (the browser's real
    /// time) is never overtaken by more than a quantum. The guest sees the
    /// same thing either way: nothing happens during a WFI but interrupts.
    fn wait_for_interrupt(&mut self, end: u64) -> Option<Stop> {
        self.sync_irqs();
        if self.blocked() {
            self.wfi_pending = true;
            return Some(Stop::Blocked);
        }
        // Like a real CPU, WFI ends only with an interrupt: data arriving
        // on the UART without its interrupt enabled does not wake it.
        if self.board.borrow().virt.irq_line() {
            return None;
        }
        match self.timer_deadline {
            Some(d) if steps_for(d) > end => {
                self.steps = self.steps.max(end);
                self.wfi_pending = true;
                Some(Stop::Budget)
            }
            Some(d) => {
                let to = self.steps.max(steps_for(d));
                self.perf.wfis += 1;
                self.perf.wfi_steps += to - self.steps;
                self.steps = to;
                self.sync_irqs();
                None
            }
            None => Some(Stop::Idle),
        }
    }

    /// Physical start address of RAM.
    pub fn ram_base() -> u64 {
        map::RAM_BASE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::virtio::{self as vio, BlockBackend, BlockError, MemBackend, VirtioBlk};

    /// WFI with bytes arriving on the UART but without its interrupt: no
    /// wakeup, and with no timer deadlines the machine is idle (before, it
    /// spun idly one instruction at a time).
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
        assert!(m.steps < 10, "stops right away, does not use up the quantum");
    }

    /// A WFI whose timer deadline is past the end of the quantum stops time at
    /// the end of the quantum (not at the deadline) and goes on in the next
    /// one; in small quanta or in one large quantum the guest ends up in the
    /// same state at the same instruction.
    #[test]
    fn wfi_stops_at_the_end_of_the_quantum() {
        // printf '...' | tools/a64asm.sh
        let code = [
            0xd51be340u32, // msr CNTV_CVAL_EL0, x0
            0xd2800021,    // mov x1, #0x1
            0xd51be321,    // msr CNTV_CTL_EL0, x1
            0xd503207f,    // wfi
            0x91000442,    // add x2, x2, #0x1
            0x14000000,    // b .
        ];
        let machine = || {
            let mut m = Machine::new(&MachineConfig { ram_size: 1 << 20, ..MachineConfig::default() });
            {
                let mut b = m.board.borrow_mut();
                for (i, w) in code.iter().enumerate() {
                    assert!(b.ram.write(map::RAM_BASE + 4 * i as u64, &w.to_le_bytes()));
                }
            }
            m.cpu.pc = map::RAM_BASE;
            // Deadline at CNTVCT 625 000: 1 000 000 instructions.
            m.cpu.x[0] = 625_000;
            m
        };
        let deadline = steps_for(625_000);
        let mut small = machine();
        assert_eq!(small.run(10_000), Stop::Budget);
        assert_eq!(small.steps, 10_000, "time stops at the end of the quantum, not at the deadline");
        assert_eq!(small.cpu.x[2], 0, "still in the WFI");
        while small.steps < deadline + 100 {
            assert_eq!(small.run(10_000.min(deadline + 100 - small.steps)), Stop::Budget);
        }
        let mut large = machine();
        assert_eq!(large.run(deadline + 100), Stop::Budget);
        assert_eq!(small.steps, large.steps);
        assert_eq!(small.cpu.x[2], 1, "woken at the deadline");
        assert_eq!((small.cpu.pc, small.cpu.x), (large.cpu.pc, large.cpu.x));
    }

    /// Devices in the slots of QEMU's `-device`s, in the same order
    /// (tests/boot/src/lib.rs, `QEMU_MACHINE`); the host reaches them by type
    /// and every access makes them be serviced.
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
        assert!(m.board.borrow().virtio_dirty, "the host touched a device");
        assert_eq!(m.pointer(|p| p.config().clone()), Some(InputConfig::multitouch()));
        let m = Machine::with_devices(&cfg, &Devices::none());
        assert_eq!(m.slots(), Slots::default());
        assert!(m.board.borrow().virt.virtio(31).unwrap().device().is_none());
    }

    /// Disk that answers `NotReady` until the host opens it (like the
    /// browser's HTTP disk before the data arrives).
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

    /// A machine with a virtio-blk already initialized (as the driver would
    /// do) and a read of sector 1 published in the queue; the code
    /// notifies the queue and then counts in x2 forever.
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
            // IN from sector 1: header, 512 bytes of data (writable),
            // status byte (writable). Flags: 1 = NEXT, 2 = WRITE.
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

    /// A disk that is not ready stops the machine right after the
    /// notification, without advancing time; when the host opens the disk
    /// the request completes at the same instruction count as with an
    /// always-ready disk, and the rest of the execution is identical.
    #[test]
    fn disco_non_pronto_ferma_il_tempo_del_guest() {
        let (mut ready, _) = blk_machine(true);
        assert_eq!(ready.run(1000), Stop::Budget);

        let (mut m, slot) = blk_machine(false);
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.steps, 1, "only the notification: no instruction after the request");
        assert!(m.blocked());
        assert_eq!(m.run(1000), Stop::Blocked, "without data it stays stopped");
        assert_eq!(m.steps, 1);
        assert_eq!(ram(&m, USED + 2, 2), [0, 0], "no response to the guest");
        m.device::<VirtioBlk, _>(Some(slot), |b| b.backend_as_mut::<Gate>().unwrap().open = true).unwrap();
        assert_eq!(m.run(999), Stop::Budget);
        assert!(!m.blocked());
        assert_eq!(m.steps, ready.steps);
        assert_eq!(m.cpu.x[2], ready.cpu.x[2]);
        assert_eq!(ram(&m, USED, 16), ram(&ready, USED, 16));
        assert_eq!(ram(&m, DATA, 512), ram(&ready, DATA, 512));
        let expected: Vec<u8> = (512..516u32).map(|i| (i * 7 + 1) as u8).collect();
        assert_eq!(ram(&m, DATA, 4), expected, "sector 1 of the disk");
        assert_eq!(ram(&m, USED + 2, 2), [1, 0], "one response in the used ring");
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
