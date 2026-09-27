# Spec — vetro-platform

## Scope
Devices of the `virt` platform for M3 (system mode): MMIO bus,
GICv3, generic timer, PL011 UART, PL031 RTC, virtio-mmio transport with
virtio-blk, virtio-net and virtio-console, device tree. For M5: virtio-gpu 2D,
virtio-input (keyboard, tablet, touchscreen), virtio-vsock and PL061 GPIO
with the power key (`gpio-keys`, seen by Android's GKI). The map mirrors `qemu-system-aarch64 -M virt`, so kernel and
device tree can be compared with QEMU without adjustments. A single CPU.

## Memory map (`map.rs`)
| Region | Base | Size | Interrupt (INTID) |
|---|---|---|---|
| GICD (distributor) | `0x0800_0000` | `0x1_0000` | — |
| GICR (redistributor, CPU 0: RD + SGI frames) | `0x080A_0000` | `0x2_0000` | — |
| UART PL011 | `0x0900_0000` | `0x1000` | SPI 1 (33), level |
| RTC PL031 | `0x0901_0000` | `0x1000` | SPI 2 (34), level |
| GPIO PL061 | `0x0903_0000` | `0x1000` | SPI 7 (39), level |
| virtio-mmio, 32 slots | `0x0A00_0000` + k·`0x200` | `0x200` | SPI 16+k (48+k), edge |
| RAM | `0x4000_0000` | configurable | — |

Generic timer: PPI 27 (virtual), PPI 30 (non-secure physical); the device
tree also lists 29 (secure physical) and 26 (hypervisor), never driven.
RAM does not go through the MMIO bus: the CPU/MMU memory handles it.

## Public interface
- `trait MmioDevice: Any { read(&mut self, offset, size) -> u64; write(&mut self, offset, size, value) }`:
  `offset` relative to the region base, `size` 1/2/4/8, values in the low
  bits. `Any` is for downcasting on the host side.
- `Bus`: `map(base, size, name, Box<dyn MmioDevice>) -> Result<DeviceId, BusError>`
  (rejects overlaps and empty ranges), `read(addr, size) -> Option<u64>`,
  `write(addr, size, value) -> bool`. `None`/`false` = no device
  covers the whole access: the CPU in M3 turns it into an external abort.
  `device::<T>(id)` / `device_mut::<T>(id)` for typed access.
- `Pl011`: `push_input(&[u8])`, `output()`, `take_output()`,
  `pending_input()`, `irq_level()`.
- `Pl031`: `new(now_secs)`, `set_time(now_secs)`, `count()`,
  `seconds_to_alarm()`, `irq_level()`.
- `Pl061`: `new()`, `set_input(line, level)` (inputs driven
  by the host), `outputs()`, `irq_level()`; `pl061::POWER_KEY_LINE` = 3.
- `GenericTimer` (`cntfrq`, `cntvoff`, `phys` and `virt` channels):
  `cntp_ctl/cval/tval`, `cntv_ctl/cval/tval` and the matching `set_*`, all
  with the CNTPCT value passed in from outside; `irq_lines(cntpct)` returns
  `[(27, level), (30, level)]`; `next_deadline(cntpct)` the next
  CNTPCT value at which a line goes high.
- `Gic`: MMIO (distributor and redistributor) as an `MmioDevice` on a single
  region `GICD_BASE .. GICR_BASE + 0x2_0000` (`gic::MMIO_SIZE`); the hole
  in between is RAZ/WI. Lines: `set_irq_level(intid, level)`,
  `set_spi_level(spi, level)`, `send_sgi(intid)`. Output: `irq_line()`.
  System registers, to be called from MRS/MSR:
  `read_iar1`, `write_eoir1`, `write_dir`, `read_hppir1`, `read/write_pmr`,
  `read/write_ctlr`, `read/write_igrpen1`, `read/write_sre`,
  `read/write_bpr1`, `read_rpr`, `read/write_ap1r0`, `write_sgi1r`.
- `Virt`: bus already mounted + `timer`; `gic_mut()`, `uart_mut()`, `rtc_mut()`,
  `gpio_mut()`; `update_irqs(cntpct)` brings the timer, UART,
  RTC, GPIO lines and those of the 32
  virtio slots to the GIC; `irq_line()`. Virtio: `attach_virtio(slot, Box<dyn VirtioDevice>)`,
  `attach_virtio_next(dev) -> slot` (highest free slot, like QEMU: the first
  device goes in slot 31), `virtio(slot)` / `virtio_mut(slot)` ->
  `VirtioMmio`, `service_virtio(&mut dyn GuestRam)`. Errors: `VirtioSlotError`
  (`NoSuchSlot`, `Occupied`, `Full`).
- Virtio (`virtio/`):
  - `trait GuestRam { read(&self, addr, &mut [u8]); write(&mut self, addr, &[u8]) }`
    -> `Result<(), RamError>`: the guest RAM for DMA, on physical
    addresses; `GuestRamExt` adds LE reads/writes; `VecRam` is a
    contiguous RAM in a `Vec`.
  - `VirtioMmio`: the transport (`MmioDevice`), `empty()` or `new(dev)`,
    `set_device`, `device_as[_mut]::<T>()`, `service(&mut dyn GuestRam)`,
    `irq_level()`, `signal_config_change()`, `status()`,
    `interrupt_status()`, `last_error()`, `without_features(mask)`.
  - `trait VirtioDevice: Any`: `device_id`, `features` (device bits
    only), `queue_max_sizes`, `read_config`/`write_config`,
    `negotiate(features) -> bool`, `reset`, `service(&mut ServiceCtx)`,
    `save_state`/`restore_state` (snapshots, mandatory: ADR 0015,
    `docs/specs/snapshot.md`).
    `ServiceCtx` exposes queues, RAM, negotiated features and `config_changed()`.
  - `Virtqueue`: `pop` -> `DescChain` (readable and writable buffers, with
    `read`/`write`/`read_to_vec` over contiguous space), `push_used`,
    `available`, `rewind`.
  - `VirtioBlk::new(Box<dyn BlockBackend>, VirtioBlkConfig)`;
    `trait BlockBackend: Any { size, read_only, read_sectors, write_sectors, flush, save_state, restore_state }`
    (the last two empty by default: the backend is a link; `MemBackend`
    and `CowBackend` save the data written by the guest)
    (512-byte sectors, `BlockError::{Io, OutOfRange, ReadOnly, NotReady}`);
    `MemBackend` (in memory, also read-only) and `CowBackend<B>`
    (copy-on-write in 4 KiB clusters over a base used read-only).
  - `VirtioNet::new(Box<dyn NetBackend>, mac)`, `with_mrg_rxbuf`,
    `set_link_up`, `rx_dropped`; `trait NetBackend: Any { send(&[u8]); recv() -> Option<Vec<u8>>; save_state; restore_state }`
    with bare Ethernet frames; `QueueNet` in memory.
  - `VirtioConsole::new(Box<dyn ConsoleBackend>)`;
    `trait ConsoleBackend: Any { write(&[u8]); read(&mut [u8]) -> usize; save_state; restore_state }`;
    `BufferConsole` in memory.
  - `VirtioGpu::new(Box<dyn DisplayBackend>, GpuConfig)` (`gpu.rs`):
    `GpuConfig { scanouts, width, height, edid, monitor: EdidInfo, max_hostmem }`,
    default 1 scanout 1280x800 with EDID and 256 MiB (like QEMU);
    `set_display(scanout, w, h)` (resize requested by the host),
    `frame(scanout) -> Option<Frame>`, `cursor(scanout)`, `set_backend`,
    `resource_count`, `hostmem`, `backend_as[_mut]::<T>()`.
    `trait DisplayBackend: Any { update(scanout, &Frame, dirty: Rect); disable(scanout); cursor(scanout, &Cursor) }`;
    `Frame { width, height, stride, format: PixelFormat, data }` with
    `rgba(x, y)`; `PixelFormat` (the 8 2D formats, `to_rgba`); `MemDisplay`
    (in memory, RGBA, `pixel(scanout, x, y)`).
  - `edid::generate(&EdidInfo, size) -> Vec<u8>` (`edid.rs`): the EDID of the
    virtual monitor, byte for byte QEMU's; `EdidInfo` (manufacturer,
    name, serial, dimensions, preferred mode, limits, refresh).
  - `VirtioInput::new(InputConfig)` (`input.rs`): `inject(&[InputEvent])`,
    `key(code, down)`, `move_abs(x, y)`, `touch(slot, Option<(x, y)>)`,
    `pending`, `dropped`, `leds`, `take_status`, `config`.
    `InputConfig::keyboard()`, `tablet()`, `multitouch()` (QEMU's
    profiles) or built with `new(name)`, `serial`, `devids`, `props`,
    `events(ty, codes, min_len)`, `abs(axis, AbsInfo)`.
    `InputEvent { ty, code, value }`; constants `EV_*`, `BTN_*`, `ABS_*`, `LED_*`.
  - `VirtioVsock::new(guest_cid)` (`vsock.rs`), the host (CID 2) inside the
    device: `listen(port)`, `unlisten`, `accept(port) -> Option<VsockConn>`,
    `connect(guest_port) -> VsockConn`, `send(c, &[u8])`, `recv(c, max)`,
    `available`, `unsent`, `eof`, `shutdown_send`, `close`, `reset`,
    `release`, `state(c) -> Option<VsockState>`, `connections`,
    `transport_reset`, `guest_cid`, `dropped`.
    `VsockConn { host_port, guest_port }`; `VsockState::{Connecting,
    Connected, Closing, Closed}`; `VsockError::{NotFound, Closed, PortInUse}`.
  - Devices are reached with `virtio_mut(slot)?.device_as_mut::<T>()`
    and backends with `backend_as_mut::<T>()`.
- `vetro-machine` mounts the M5 devices: `Devices { gpu: Option<GpuConfig>,
  keyboard, pointer: Option<Pointer>, vsock_cid: Option<u64> }` with
  `Machine::with_devices(&MachineConfig, &Devices)` (`Machine::new` uses
  `Devices::default()`: GPU 1280x800, keyboard, tablet, no vsock;
  `Devices::none()` is the M3 machine). Fixed mounting order
  (GPU, keyboard, pointer, vsock), each in the highest free slot:
  31, 30, 29, 28 like QEMU's `-device`s. `Machine::slots()`,
  `Machine::gpu/keyboard/pointer/vsock(|d| ...)` and `Machine::device::<T>(slot, f)`
  give the host the device and mark it to be serviced before the
  next instruction. From M10 on, host inputs go through
  `Machine::input` (ADR 0019, `docs/specs/replay.md`): closures during
  a recording are opaque events; reads use
  `Machine::device_view`/`gpu_view`/`vsock_view`, the data of an awaited disk
  `Machine::host_link`. `Devices` sits outside `MachineConfig` because
  `vetro-wasm` builds `MachineConfig` by listing its fields.
- `FdtBuilder`: `begin_node`, `end_node`, `prop_u32`, `prop_u64`,
  `prop_u32_list`, `prop_u64_list`, `prop_str`, `prop_strs`, `prop_bytes`,
  `prop_empty`, `reserve_memory`, `boot_cpuid`, `finish() -> Result<Vec<u8>, FdtError>`.
  DTB format v17 (last_comp 16), deduplicated strings.
- `virt_dtb(&VirtDtbConfig) -> Vec<u8>`: memory, cpus (`enable-method =
  "psci"`), psci (`arm,psci-1.0`, `hvc` method by default), timer
  (`arm,armv8-timer`), GIC (`arm,gic-v3`), fixed 24 MHz clock, PL011, PL031,
  PL061 (phandle 3) with `gpio-keys/poweroff` (line 3, KEY_POWER, like
  QEMU), 32 virtio-mmio, `chosen` with `bootargs`, `stdout-path = "/pl011@9000000"`
  and optional initrd.

## Choices and limits
- **GICv3**: a single security state (GICD_CTLR.DS = 1) and ARE = 1, both
  RAO/WI. **Non-secure group 1 only**: IGROUPR is stored but an interrupt
  in group 0 is never signalled (no FIQ); IGRPMODR/NSACR RAZ/WI.
  No LPI/ITS. 256 SPIs (ITLinesNumber = 8, IDbits = 9). CPU interface
  with 5 priority bits (PRIbits = 4) like QEMU: PMR mask `0xF8`, BPR1
  minimum 3. ICC_CTLR_EL1.CBPR is RAZ/WI (BPR0 not modelled); EOImode
  writable (with EOImode = 1, ICC_DIR_EL1 is needed). SGI1R: with one CPU only
  affinity 0.0.0 and bit 0 of the TargetList count. GICR_WAKER does the handshake
  but does not block delivery. Selection: numerically lowest priority,
  ties broken by lowest INTID; delivered if IGRPEN1, priority < PMR and
  group priority < running priority. SPIs routed to CPU 0
  if IROUTER has affinity 0.0.0.0 or IRM = 1.
- **PL011**: instantaneous transmission even with the UART disabled (earlycon), like
  QEMU; reception only with UARTEN and RXE, otherwise bytes stay in the
  host queue. RX threshold 1 (like QEMU), IFLS only stored; TXRIS goes
  high on every write to DR and goes low with ICR. RX FIFO of 16 with FEN,
  of 1 without. Loopback (CR.LBE) supported.
- **PL031**: CR always reads 1; any write to ICR clears
  the interrupt; the alarm fires when DR reaches MR while advancing, or immediately
  if MR = DR after a write to MR or LR (like QEMU).
- **PL061** (like `hw/gpio/pl061.c` in QEMU's virt): 8 lines, those
  not driven read 0 (`pulldowns = 0xff`); DATA with the mask in bits
  9:2 of the offset, writes only the outputs; interrupts like `pl061_update`
  (edge: IBE or IEV on an input change; level: RIS comes back on
  while active; IC clears); line = RIS & IE. Accesses up to 4 bytes (Linux
  uses `readb`/`writeb`); no Luminary registers. The host presses the key with
  `vetro-machine`'s `Machine::gpio_input(3, true/false)` (that is,
  `Machine::input(Input::Gpio { .. })`, recorded for replay), which marks
  the lines to update before the next instruction.
- **Timer**: ISTATUS = ENABLE && counter >= CVAL (unsigned), 0 with
  ENABLE off; TVAL 32-bit signed (like QEMU). Default CNTFRQ
  62.5 MHz.
- **virtio-mmio** (version 2, virtio 1.2 §4.2.2): VendorID `0x554D4551`
  like QEMU; registers below 0x100 only 32-bit aligned, write-only
  registers read as 0; transport features VERSION_1
  (mandatory: without it, FEATURES_OK does not stay in Status), INDIRECT_DESC,
  EVENT_IDX; no shared memory (SHMLen/SHMBase = -1). RAM does not
  go through the bus: QueueNotify does no work, `service` does it, which the engine
  calls after MMIO accesses to virtio slots and periodically (incoming data
  from the backends), before `update_irqs`. No work before
  DRIVER_OK. Interrupt line = `InterruptStatus != 0`, declared as
  rising edge in the DTB like QEMU. An error in the queues (invalid
  chain, access outside RAM, malformed request) leads to
  DEVICE_NEEDS_RESET with a configuration interrupt and stops the device
  until reset.
- **Split virtqueue** (§2.7): QueueNum must be a power of 2 and the areas
  aligned (16/2/4), otherwise QueueReady stays 0. Writable after
  readable; chains at most as long as the table; INDIRECT only
  when negotiated and only on the head descriptor (NEXT on it ignored, like
  QEMU), forbidden inside a table. Full EVENT_IDX: the device
  publishes avail_event after each pop and notifies according to
  `vring_need_event` on used_event; without EVENT_IDX it honours
  VRING_AVAIL_F_NO_INTERRUPT.
- **virtio-blk**: features SIZE_MAX, SEG_MAX, BLK_SIZE, FLUSH and RO if the
  backend or the configuration are read-only; queues of 256, seg_max
  254 (like QEMU), size_max 1 MiB, blk_size 512. IN/OUT/FLUSH/GET_ID, the
  rest UNSUPP; accesses beyond capacity or not multiples of 512 and writes to
  read-only: IOERR. I/O in 64 KiB pieces. `BlockError::NotReady` leaves
  the request pending and retries it at the next `service` (for the disks
  downloaded in pieces in M5). With a pending request `Machine::run`
  returns `Stop::Blocked` without executing instructions until the host
  delivers the data (ADR 0014): guest time does not depend on the network.
- **virtio-net**: MAC, STATUS, MRG_RXBUF (can be disabled); no offload,
  control queue or multiqueue. 12-byte header zeroed except
  num_buffers. The backend is polled only with free buffers and link up;
  with MRG_RXBUF a frame that does not fit waits for more buffers, without it it is
  dropped. TX beyond 64 KiB + header: queue error.
- **virtio-console**: one port, without MULTIPORT (queues 0 rx and 1 tx),
  EMERG_WRITE offered; max_nr_ports = 1.
- **virtio-gpu** (§5.7, like QEMU 10.0 `virtio-gpu-device` without virgl):
  queues 64 (control) and 16 (cursor); EDID feature; config events_read,
  events_clear (write-to-clear), num_scanouts, num_capsets = 0.
  Commands GET_DISPLAY_INFO, GET_EDID, RESOURCE_CREATE_2D/UNREF,
  SET_SCANOUT, RESOURCE_FLUSH, TRANSFER_TO_HOST_2D,
  RESOURCE_ATTACH/DETACH_BACKING, UPDATE/MOVE_CURSOR; QEMU's errors and checks
  (id 0 or duplicate, format, `max_hostmem`, rectangles outside the
  resource, scanout below 16x16, missing or duplicate backing, more than 16384
  entries, entries outside RAM; capset, 3D and UUID ERR_UNSPEC; blob
  ERR_INVALID_PARAMETER). Fence: flag, fence_id and ctx_id echoed back, already
  signalled (synchronous execution). Resources in host memory (stride
  = width x 4, like pixman); TRANSFER like QEMU (one shot if it covers
  the full width, otherwise row by row from offset + stride x row;
  what the backing does not cover stays unchanged). SET_SCANOUT sends the whole
  image to the backend immediately, FLUSH only the intersection with each scanout
  showing the resource, UNREF and SET_SCANOUT 0 turn it off. Cursor:
  image copied only from 64x64 resources. `set_display` = DISPLAY event
  with a configuration interrupt. Differences from QEMU, only on input that
  Linux does not send: command shorter than its structure →
  ERR_INVALID_PARAMETER (QEMU answers OK without executing or stalls the queue).
  Default resolution 1280x800, QEMU's, because the boot test
  compares modes with QEMU; Android will choose its own with `GpuConfig`
  (e.g. 1080x1920 portrait) and `set_display`.
- **virtio-gpu 3D** (ADR 0037, only with `GpuConfig::virgl`, off by default):
  feature VIRGL; `num_capsets` and GET_CAPSET_INFO/GET_CAPSET from the
  `Renderer3d` (index or id/version out of range: ERR_INVALID_PARAMETER);
  CTX_CREATE (id 0 or taken: ERR_INVALID_CONTEXT_ID), CTX_DESTROY,
  CTX_ATTACH/DETACH_RESOURCE (unknown context: ERR_INVALID_CONTEXT_ID,
  unknown or 2D resource: ERR_INVALID_RESOURCE_ID), RESOURCE_CREATE_3D (ids
  shared with 2D resources; size reported by the renderer counted in
  `max_hostmem`), TRANSFER_TO/FROM_HOST_3D (the renderer reads or writes the
  backing through `Backing`; no backing: ERR_UNSPEC), SUBMIT_3D (size beyond
  the command: ERR_INVALID_PARAMETER), 3D resources as scanouts without
  backing (the renderer presents them on RESOURCE_FLUSH; the display
  backend's `update_3d` hears about it), UNREF of a shown 3D resource turns
  the scanout off. With virgl but no renderer the 3D commands answer
  ERR_UNSPEC. `Renderer3d` (`gpu/renderer.rs`): `capsets`,
  `context_create/destroy/attach/detach`, `resource_create/destroy`,
  `transfer_to_host/from_host`, `submit`, `scanout`, `flush`, `reset`,
  `save_state/restore_state`; `VirtioGpu::set_renderer`,
  `renderer_as[_mut]`, `scanout_3d`, `resource_3d_count`, `context_count`.
  Snapshots add the 3D part only with virgl; `GpuConfig`'s `Debug` omits
  `virgl` when off, so 2D machines keep their snapshot key.
- **EDID**: generator equivalent to QEMU's hw/display/edid-generate.c
  (manufacturer RHT, "QEMU Monitor", standard/established/CTA modes, detailed
  descriptor with proportional timings and 75 Hz, DisplayID beyond 4096 pixels);
  verified byte for byte against the EDID read by the guest under QEMU. Name,
  manufacturer and serial can be changed with `EdidInfo` (device profiles, M10).
- **virtio-input** (§5.8): queues 64 (events, status), no features;
  select/subsel windowed configuration (missing entry: all 0, like
  QEMU). Profiles identical to `virtio-keyboard-device` (159 keys, EV_REP,
  num/caps/scroll LEDs), `virtio-tablet-device` (ABS_X/Y 0..32767, buttons,
  wheel) and `virtio-multitouch-device` (MT slots 0..10, INPUT_PROP_DIRECT) of
  QEMU 10.0 and 8.2, verified with EVIOCG* and /proc/bus/input/devices in the
  guest. Events before DRIVER_OK dropped (like QEMU); afterwards delivered as
  whole reports (up to SYN_REPORT) only with buffers for the whole
  report: QEMU drops the report, Vetro keeps it (at most 4096 events,
  then it drops whole reports and counts them). Status queue: EV_LED updates
  `leds`, used length 0 (QEMU puts the bytes read).
- **virtio-vsock** (§5.10): queues 128 (rx, tx, events), STREAM feature,
  config guest_cid (default 3). The host is the device itself (CID 2):
  REQUEST to a listening port → RESPONSE and accept queue, otherwise
  RST; packets without a connection or not stream → RST; wrong CIDs or
  invalid lengths dropped. Credit like Linux: the host does not exceed the guest's
  `buf_alloc - (tx_cnt - fwd_cnt)`, advertises 256 KiB, sends
  CREDIT_UPDATE when it consumes and the guest sees less than 64 KiB free, or on
  CREDIT_REQUEST. Full SHUTDOWN from the guest → RST; host close:
  SHUTDOWN after the last data (even if requested before the RESPONSE).
  Packets up to 64 KiB and to the guest's rx buffer. Deterministic order:
  control packets in order of creation, then data by (host port,
  guest port); local ports from 49152 in sequence. TRANSPORT_RESET on
  request (snapshots, M6). Not compared with QEMU: `vhost-vsock-device`
  needs `/dev/vhost-vsock`, absent in Docker Desktop and on the runners.

## Invariants
- No dependency on `std::fs`, `std::process`, threads, or external
  crates: compiles to `wasm32-unknown-unknown`. Virtio devices
  talk to the outside only through the backend traits, `GuestRam` and
  the devices' host API (input, vsock, `set_display`), which the engine
  calls from the single recordable point.
- **Determinism**: no device reads the host clock; time
  (CNTPCT, RTC seconds) and UART input enter only as
  arguments, from the engine's single recordable point; the same holds for the
  virtio backends (frames, console bytes, disk data), which the engine
  implements on top of that point.
- Bus regions do not overlap; an access straddling the end
  of a region reaches no device.

## Tests
`cargo test -p vetro-platform`: unit tests per module (bus, PL011, PL031, PL061,
timer, GIC, virtio, FDT with a minimal DTB parser, mounted platform).
GPU, input and vsock have tests with the test driver (commands and errors,
formats, backing in pieces, fences, cursor, resizing, reset; input
profiles against the values read under QEMU, whole reports, full queue,
LEDs; handshake, rejections, credit, closes, transport reset,
determinism); the EDID is compared with the 256 bytes read under QEMU.
With the guest kernel (`cargo test --release -p vetro-boot-tests`):
- `vetro.rs`: booting with GPU, keyboard and tablet gives the same log as QEMU
  with the same `-device`s (`QEMU_MACHINE`), including the self-test that runs
  `vetro-dev drm` (modes, dumb buffer, modeset, DIRTYFB, cursor), the EDID from
  sysfs, `/proc/bus/input/devices` and the evdev capabilities, and with virtio-net
  (in QEMU `-netdev user`) DHCP, routes, configured DNS and ping to gateway and
  DNS;
- `net.rs` (Vetro only): the guest network with the `vetro-net` stack and the
  sinkhole (see `docs/specs/net.md`);
- `devices.rs` (Vetro only, with vsock): the host compares every pixel of the
  scanout with the pattern drawn by the guest and the cursor, injects keys and
  movements read by the guest with evdev, sees the LED turned on by the guest, and
  exchanges vsock data in both directions (300 KB towards the guest, beyond its
  credit; 200 KB of echo); two runs give the same log and the same
  instructions;
- `files.rs` (Vetro only, with vsock, M8): the file manager daemon
  `vetro-files`, which `/init` starts when virtio-vsock is present, and the
  host client (`docs/specs/files.md`, ADR 0020).
The virtio tests use a test driver (`virtio/testdrv.rs`) that does what
Linux does on a fake RAM: negotiation, queue setup, direct and
indirect chains, notifications, used ring with used_event updates,
interrupts; one test takes it through the `Virt` bus all the way to the INTID in the GIC.
The produced DTB was also decompiled with `dtc -I dtb -O dts` without
errors (manual check, not in CI). The comparison with QEMU comes in M3,
booting the same kernel on both.
