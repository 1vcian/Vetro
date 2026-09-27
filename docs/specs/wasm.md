# `vetro-wasm`: the machine in WebAssembly (M4, devices in M5)

`crates/vetro-wasm` compiles `vetro_machine::Machine` for
`wasm32-unknown-unknown` and exposes it to JavaScript through a C API
(`extern "C"`, `#[unsafe(no_mangle)]`). No wasm-bindgen and no other
dependencies: only integers and pointers into the module's linear memory
(export `memory`) cross the boundary. The same module runs in Node and in the
browser.

Build:

```sh
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
# -> target/wasm32-unknown-unknown/release/vetro_wasm.wasm
```

WASM types: `usize` and pointers are `i32` (in JS `number`), `u64` is `i64`
(in JS `BigInt`), `u32` is `i32` (in JS `number`; for values above 2³¹ use
`>>> 0`). Pointers too must be read with `>>> 0`: with more than 2 GiB of
linear memory (two 1 GiB machines) they come out negative.

## Exports

Version: `vetro_abi_version() -> u32`, currently **12**. It changes with every
incompatible change to the signatures or codes below; the JS loader
(`web/node/vetro.mjs`) checks it.

- 2 (M4): system-mode JIT (`vetro_machine_set_jit`, imports
  `vetro_jit.entry/place/reset`).
- 3 (M5): devices (`vetro_machine_new_with`), virtio-gpu display,
  virtio-input, GPIO, virtio-blk disks with data from JS, stop code 5
  `Blocked`. `vetro_machine_new` remains, with the default devices; the
  vetro-wasm GPU shows on `WebDisplay` (RGBA) instead of `MemDisplay`
  (nothing changes for the guest: same instructions).
- 4 (M6): machine snapshots (`vetro_snapshot_*`, ADR 0015).
- 5 (M5): TCP connections from JS to guest services (`vetro_net_*`,
  port forwarding like QEMU's `hostfwd`; the basis of adb in the browser).
- 6 (M6): persistent copy-on-write disk overlay (`vetro_overlay_*`,
  ADR 0017); `vetro_snapshot_restore` marks the overlays for a full
  comparison.
- 7 (M8): virtio-vsock (bit `VSOCK` of `vetro_machine_new_with`) and file
  manager (`vetro_files_*`, ADR 0020, `docs/specs/files.md`).
- 8 (M7, M10): network inspector, input→effects timeline, record & replay
  (`vetro_result_*`, `vetro_capture_*`, `vetro_inspect_*`,
  `vetro_timeline_*`, `vetro_record_*`, `vetro_rr_status`, `vetro_log_*`,
  `vetro_replay_start`, `vetro_registers_text`, `vetro_read_virt`,
  `vetro_translate`, `vetro_read_phys`; ADR 0023). Keyboard, pointer,
  console, GPIO and resolution inputs are annotated in the timeline
  (execution does not change).
- 9 (M8): file manager SQL and paths as bytes (ADR 0021).
- 10 (M4): region JIT (ADR 0024): import `vetro_jit.runtime` (the runtime
  module `rt.*` of the generated modules), export `vetro_jit_vsync`
  (the runtime's `env.vsync`), counter `yields` at the end of
  `vetro_jit_stats`.
- 11 (M4): FP/SIMD in regions (ADR 0026): export `vetro_jit_simd`.
- 12 (M5, M6): booting from Android images (`vetro_load_android`, ADR 0018
  and 0028), chunked snapshots (`vetro_snapshot_save_stream`,
  `vetro_snapshot_restore_stream`, imports `vetro_host.snapshot_write` and
  `snapshot_read`). Existing signatures are unchanged; the RAM may exceed
  2 GiB on wasm32 too (below, "RAM beyond 2 GiB").

### Memory

| Export | Signature | Meaning |
|---|---|---|
| `vetro_alloc` | `(len: usize) -> *mut u8` | buffer of `len` bytes aligned to 16; null if `len == 0` or if memory runs out |
| `vetro_free` | `(ptr: *mut u8, len: usize)` | frees a `vetro_alloc` buffer with the same `len` |

An allocation can grow the memory (`memory.grow`): from that moment the old
`memory.buffer` is detached, and every `Uint8Array` must be recreated. In
practice: take the view after every call that may allocate.

### Machine

| Export | Signature | Meaning |
|---|---|---|
| `vetro_machine_new` | `(ram_size: u64, now_secs: u64, seed: u64) -> *mut Vm` | new machine. 0 in a field = value from `MachineConfig::default` (1 GiB, fixed time and seed of the native tests) |
| `vetro_machine_free` | `(vm)` | destroys it |
| `vetro_load_linux` | `(vm, image, image_len, initrd, initrd_len, cmdline, cmdline_len) -> u32` | like `Machine::load_linux`; `initrd` null or 0 long = none; `cmdline` UTF-8. The buffers can be freed right after |
| `vetro_run` | `(vm, budget: u64) -> u32` | executes at most `budget` instructions (`Machine::run`) |
| `vetro_steps` | `(vm) -> u64` | instructions executed (the guest clock, ADR 0011) |
| `vetro_guest_ns` | `(vm) -> u64` | guest time in ns (10 ns per instruction) |
| `vetro_console_read` | `(vm, dst: *mut u8, cap: usize) -> usize` | copies and consumes at most `cap` bytes of PL011 output; 0 = nothing new. The rest stays for the next call |
| `vetro_console_write` | `(vm, src: *const u8, len: usize)` | queues input bytes, as if from the keyboard |
| `vetro_message_ptr` / `vetro_message_len` | `(vm) -> *const u8` / `usize` | last UTF-8 message: load error or `what` of an unimplemented instruction. Valid until the next call on the machine |
| `vetro_unimplemented_pc` / `vetro_unimplemented_raw` | `(vm) -> u64` / `u32` | PC and encoding of the last unimplemented instruction |
| `vetro_machine_set_jit` | `(vm, hot_threshold: u32, batch: u32)` | enables the system-mode JIT (ADR 0013) on the JS engine: entries before a block is translated, blocks per module (0 = 1). The result does not change, only the speed |
| `vetro_jit_stats` | `(vm, out: *mut u64, cap: usize) -> usize` | JIT counters (`SysJitStats`: `jit_steps`, `runs`, `resolves`, `calls`, `blocks`, `modules`, `reused`, `invalidated_pages`, `faults`, `svcs`, `stops`, `epochs`, `tlb_flushes`, `tlb_fills`, `resets`, `yields`) in `out`; returns how many (0 without JIT) |

`vetro_load_linux` codes: 0 success; 1 the loader rejected the files
(reason in the message); 2 command line not UTF-8.

| Export | Signature | Meaning |
|---|---|---|
| `vetro_load_android` | `(vm, boot, boot_len, vendor_boot, vendor_boot_len, init_boot, init_boot_len, params, params_len, flags: u32) -> u32` | ABI 12: the bootloader in `vetro_machine::android` (ADR 0018) combines `boot.img` (required), `vendor_boot.img` and `init_boot.img` (null or 0 long = absent) with the bootloader parameters `params` (UTF-8; `androidboot.*` go into the bootconfig with a v4 `vendor_boot`, the others at the end of the command line) and loads the result like `vetro_load_linux`. `flags` bit 0 = recovery. Same codes as `vetro_load_linux` (2 = parameters not UTF-8); on success `vetro_message_*` describes kernel, ramdisks, bootconfig and command line. The buffers can be freed right after |

In JS: `Machine.loadAndroid({ boot, vendorBoot, initBoot, params, recovery })`
returns the description. Test: `tests/web/android-boot.mjs`.

#### RAM beyond 2 GiB (ABI 12, ADR 0028)

On wasm32 no Rust allocation can exceed `isize::MAX` (2 GiB - 1): with a
larger `ram_size` the guest RAM is a contiguous region taken with
`memory.grow` outside the allocator (`vetro_machine::board::Ram`), read and
written in pieces; the region of a destroyed machine is reused (zeroed) by
the next one. Linear memory stops at 4 GiB: with 3 GiB of RAM less than
1 GiB is left for everything else (disk copy-on-write, cached blocks, JIT,
snapshot buffers). The guest's behaviour does not change (same instructions
as the native reference, `ram3g`).

`vetro_run` codes (`Stop` of `vetro-machine`):

| Code | `Stop` |
|---|---|
| 0 | `Budget`: quantum exhausted, execution can continue |
| 1 | `PowerOff` |
| 2 | `Reset` |
| 3 | `Idle`: the guest is waiting for input |
| 4 | `Unimplemented` (details in `vetro_unimplemented_*` and in the message) |
| 5 | `Blocked`: a disk is waiting for blocks from JS (`vetro_disk_wanted`); guest time is stopped (ADR 0014) |

The machine is deterministic: the same kernel, initramfs, command line and
input (at the same instruction numbers) give the same output and the same
count as native. `tools/wasm-boot.sh` verifies this.

### Devices (ABI 3)

| Export | Signature | Meaning |
|---|---|---|
| `vetro_machine_new_with` | `(ram_size: u64, now_secs: u64, seed: u64, devices: u32, width: u32, height: u32) -> *mut Vm` | like `vetro_machine_new`, with the chosen devices: bits `GPU` 1, `KEYBOARD` 2, `TABLET` 4, `MULTITOUCH` 8 (wins over `TABLET`), `NET` 16 (virtio-net with `vetro-net` and the sinkhole, `NetSetup::default`), `VSOCK` 32 (virtio-vsock, CID 3, ABI 7); 23 = `Devices::default`. `width`x`height`: initial resolution of scanout 0 (0 = 1280x800). Slots as in `Devices` (GPU 31, keyboard 30, pointer 29, network 28; disks after) |
| `vetro_display_size` | `(vm, scanout: u32) -> u64` | `(width << 32) \| height`; 0 if off or without a GPU |
| `vetro_display_ptr` | `(vm, scanout) -> *const u8` | RGBA pixels (4 bytes, rows of `width * 4`), null if off. Valid until the next `vetro_run` |
| `vetro_display_updates` | `(vm, scanout) -> u64` | update counter (image or power-off): if it does not change, nothing to redraw |
| `vetro_display_take_dirty` | `(vm, scanout, out: *mut u32) -> u32` | union of the rectangles changed since the last call: writes `x, y, w, h` and returns 1; 0 = nothing. After a size change, the whole scanout |
| `vetro_display_resize` | `(vm, scanout, width, height) -> u32` | resolution requested by the host (`VirtioGpu::set_display`, event to the driver); 0 without a GPU. It is an input |
| `vetro_cursor_state` | `(vm, scanout, out: *mut u32) -> u32` | 6 values: resource (0 = hidden), x, y, hot_x, hot_y, number of changes; 0 without a GPU |
| `vetro_cursor_image` | `(vm, scanout) -> *const u8` | 64x64 RGBA cursor, null if there is none |
| `vetro_input_key` | `(vm, code: u32, down: u32) -> u32` | Linux key `KEY_*` with SYN_REPORT; 0 if there is no keyboard |
| `vetro_input_abs` | `(vm, x: u32, y: u32) -> u32` | tablet position (0..=32767 per axis) with SYN_REPORT |
| `vetro_input_button` | `(vm, code, down) -> u32` | pointer button (`BTN_LEFT` 0x110, `BTN_RIGHT` 0x111, `BTN_MIDDLE` 0x112) |
| `vetro_input_touch` | `(vm, slot, x, y, down) -> u32` | touchscreen contact (protocol B, tracking id = slot); `down` 0 removes it |
| `vetro_input_events` | `(vm, device: u32, events: *const u32, count: usize) -> u32` | raw evdev events (`type, code, value` as three `u32`) on keyboard (0) or pointer (1); the caller adds the SYN_REPORTs (e.g. the wheel: `EV_REL REL_WHEEL ±1`) |
| `vetro_input_leds` | `(vm) -> u32` | LEDs lit by the guest (bits `LED_*`) |
| `vetro_gpio_input` | `(vm, line: u32, level: u32)` | PL061 input line (`Board::gpio_input`); line 3 is the power key (`gpio-keys`, KEY_POWER): pressed = 1, released = 0 |
| `vetro_power_key_line` | `() -> u32` | 3 |

Inputs (keys, pointer, touch, console, GPIO, resolution) reach the guest
before the next executed instruction: they are the events to record for
replay (M10). Reads (display, cursor, LEDs, counters) do not touch the
machine: when the page performs them does not change the execution.

### Disks (ABI 3, ADR 0014)

| Export | Signature | Meaning |
|---|---|---|
| `vetro_disk_add` | `(vm, size: u64, block_size: u32, max_blocks: u32, flags: u32) -> i32` | virtio-blk disk of `size` bytes (rounded down to 512, like QEMU for raw images) with data from JS in aligned blocks of `block_size` bytes (power of two, >= 512); at most `max_blocks` blocks in memory (0 = no limit; beyond that, the oldest are evicted). `flags`: 1 = read-only for the guest; without it, guest writes go to an in-memory copy-on-write layer (4 KiB clusters). Returns the disk index or -1 (reason in the message). In the first free slot from the top; to be called before `vetro_run` |
| `vetro_disk_add_mem` | `(vm, data: *const u8, len: usize, flags: u32) -> i32` | disk with its content already in memory (copied), always ready; same rounding, `flags` and copy-on-write |
| `vetro_disk_wanted` | `(vm, out: *mut u64, cap: usize) -> usize` | blocks requested by the guest, `(disk, block)` pairs in `out` (at most `cap`); each block appears only once until it arrives or fails |
| `vetro_disk_fill` | `(vm, disk: u32, block: u64, data: *const u8, len: usize) -> u32` | delivers a block (`len` = `block_size`, or the remainder for the last one); also unrequested (read-ahead). 0 ok, 1 unknown disk (or in-memory), 2 block outside the disk, 3 wrong length |
| `vetro_disk_fail` | `(vm, disk, block: u64) -> u32` | the block cannot be obtained: the request waiting for it ends with IOERR |
| `vetro_disk_stats` | `(vm, disk, out: *mut u64, cap: usize) -> usize` | size, block size, blocks in memory, missed reads, blocks delivered, evicted, failed, copy-on-write clusters written; 0 = unknown disk |

The round trip with a network disk:

1. `vetro_run` returns 5 (`Blocked`): a guest request touches missing
   blocks. No instruction has been executed after the request, and further
   `vetro_run` calls return 5 immediately while the data is missing.
2. JS reads `vetro_disk_wanted`, obtains the blocks (OPFS cache, then
   HTTP Range or `File`) and delivers them with `vetro_disk_fill`.
3. The next `vetro_run` repeats the request and completes it at the same
   instruction number as with a local disk; the rest of the execution is
   identical. To compare executions, the caller continues the quantum up to
   its boundary before looking at the console or giving inputs
   (`tests/web/lib.mjs`, `Session.quantum`).

### Snapshot (ABI 4, ADR 0015, `docs/specs/snapshot.md`)

| Export | Signature | Meaning |
|---|---|---|
| `vetro_snapshot_version` | `() -> u32` | snapshot format version (4 today): it goes into cache keys, so a snapshot of another version is not even tried |
| `vetro_snapshot_config_hash` | `(vm) -> u64` | ABI 13 (ADR 0031): the machine configuration hash its snapshots carry in their header (RAM, devices, disks, virtio slots); read after the disks are added, for snapshot keys (the prebuilt Android snapshot) |
| `vetro_snapshot_set_level` | `(vm, level: u32) -> u32` | ABI 13 (ADR 0031): compression of the next saves, 0 = fast (default), 1 = small (`lzh` frames: several times slower to save, about a third smaller, for downloaded snapshots). Restoring accepts both. Returns 1 for an unknown level (nothing changes) |
| `vetro_snapshot_save` | `(vm) -> usize` | saves the whole machine into an internal buffer and returns its length. Read the console first: output already taken from the UART and not delivered to JS is not included |
| `vetro_snapshot_ptr` | `(vm) -> *const u8` | the bytes of the last save (null if there is none), valid until the next save, `vetro_snapshot_clear` or `vetro_machine_free` |
| `vetro_snapshot_clear` | `(vm)` | frees the buffer |
| `vetro_snapshot_restore_stream` | `(vm, head: *const u8, head_len: usize) -> u32` | ABI 12: restore without the whole file in memory: `head` = the file's bytes up to and including the header of the `RAM ` section; the RAM content is requested from the import `vetro_host.snapshot_read(ptr, cap) -> bytes written` (0 = end). Same codes as `vetro_snapshot_restore`; the checksum is verified at the end (`CORRUPT` = discard the machine). After a restore with a buffer as large as the snapshot, memory stayed high and fragmented and the next save found no contiguous space |
| `vetro_snapshot_save_stream` | `(vm) -> u64` | ABI 12: the same file as `vetro_snapshot_save` in chunks, without holding it whole in memory (Android): the content's chunks go to the import `vetro_host.snapshot_write(ptr, len)` in order (to be written from offset 36 on), the header (36 bytes) stays in the `vetro_snapshot_ptr` buffer. Returns the file length. Besides the chunks (1 MiB) only the part before the RAM is in memory (devices and disk copy-on-write); the RAM is compressed twice (its length enters the hash) |
| `vetro_snapshot_restore` | `(vm, data: *const u8, len: usize) -> u32` | restores; the buffer can be freed right after. Codes: 0 `OK`, 1 `BAD_MAGIC` (not a snapshot), 2 `VERSION` (other format), 3 `CONFIG` (machine configured differently), 4 `CORRUPT` (damaged or inconsistent: the machine must be discarded); reason in the message. With 1, 2 and 3 the machine does not change |

To restore, build the machine with the same parameters of
`vetro_machine_new_with` (RAM, time, seed, devices, resolution), add the
same disks in the same order and with the same parameters
(`vetro_disk_add` / `vetro_disk_add_mem`: size, block, flags), enable the
JIT if desired (the result does not change), then call
`vetro_snapshot_restore`. What is **state** (in the snapshot) and what is
**wiring** (recreated by JS):

| In the snapshot | Wiring |
|---|---|
| CPU, MMU (TLB included), RAM, clock, timer, GIC, UART (FIFO, output not yet read from the UART), RTC, GPIO | the WASM module and the JIT engine (blocks rebuilt from scratch) |
| virtio transports and queues, in-flight requests, GPU state (resources and pixels), input, network (stack and sinkhole), vsock | `WebDisplay`: immediately receives the restored image and cursor (`vetro_display_updates` changes) |
| disk copy-on-write layer (guest writes) | disk data (`HostDisk`, HTTP Range, OPFS): after the restore the blocks are requested again with `BLOCKED` as at boot; the size is checked |
| (nothing else: even the content of `vetro_disk_add_mem` is a read-only base under the copy-on-write) | the content of in-memory disks (`vetro_disk_add_mem`), checked with a hash; console output already delivered to JS; inputs not yet given |

If the disks have a persistent overlay (below), `vetro_overlay_open` must be
called **before** `vetro_snapshot_restore`; after the restore the next
`vetro_overlay_take` compares all clusters with the file and writes only the
ones that differ.

### Persistent disk overlay (ABI 6, ADR 0017)

Guest writes to a copy-on-write disk (`vetro_disk_add` or
`vetro_disk_add_mem` without `READ_ONLY`) are kept in a file held by JS
(OPFS), in the `vetro_snapshot::overlay` format (the same as `vetro
boot --overlay`, `docs/specs/snapshot.md`). Rust decides what to write and
where; JS reads the file when opening it and applies the writes.

| Export | Signature | Meaning |
|---|---|---|
| `vetro_overlay_open` | `(vm, disk: u32, identity: *const u8, identity_len: usize, data: *const u8, data_len: usize) -> u32` | opens the disk's overlay from the file content (`data_len` 0 if there is none) for the base image `identity` (UTF-8: URL, size, ETag); the clusters read go into the copy-on-write. Before `vetro_run` and `vetro_snapshot_restore`. Codes: 0 `LOADED`, 1 `NEW` (empty file), 2 `MISMATCH` (overlay of another base or size: discarded), 3 `CORRUPT` (unreadable: discarded), 4 `NO_DISK` (unknown or read-only disk); reason in the message. With 2 and 3 the next `take` truncates the file |
| `vetro_overlay_take` | `(vm, disk) -> usize` | prepares the writes that bring the file to the copy-on-write state (the clusters written by the guest since last time, or all of them after a restore) and returns their length; 0 = nothing to write. Encoding: u64 length to truncate to first (`u64::MAX` = no), u32 number of writes, then for each u64 offset, u32 length, bytes (LE). In order: the header (offset 0) is last, to be written after a flush of the data. Between any two `vetro_run` calls: it does not touch the guest |
| `vetro_overlay_ptr` | `(vm) -> *const u8` | the bytes of the last `take` (null if empty), valid until the next `take`, `vetro_overlay_clear` or `vetro_machine_free` |
| `vetro_overlay_clear` | `(vm)` | frees the buffer |
| `vetro_overlay_info` | `(vm, disk, out: *mut u64, cap: usize) -> usize` | generation (grows with every `take` that writes something), clusters in the file, slots in the file, damaged slots found when opening, file length; 0 = disk without overlay |

### Network: connections to the guest (ABI 5)

JS opens TCP connections to a guest port (10.0.2.15), which sees them
arrive from the gateway 10.0.2.2 from an ephemeral port (49152, 49153, …),
as with QEMU's `-netdev user,hostfwd=…`. It is the same
`Stack::host_connect` as `vetro boot --hostfwd` (`docs/specs/net.md`). It
needs a machine with networking (bit `NET`) and a guest that has already
done DHCP. The connection id is a `u64` (> 0, in JS `BigInt`).

| Export | Signature | Meaning |
|---|---|---|
| `vetro_net_connect` | `(vm, guest_port: u32) -> u64` | opens a connection to `guest_port`; the SYN leaves before the next instruction. 0 without networking or with port 0 / > 65535 |
| `vetro_net_send` | `(vm, conn: u64, src: *const u8, len: usize) -> usize` | queues bytes for the guest; returns how many it took (at most 256 KiB queued: the rest must be offered again after a `vetro_run`). 0 if closed, unknown or after `vetro_net_shutdown` |
| `vetro_net_recv` | `(vm, conn, dst: *mut u8, cap: usize) -> usize` | copies and consumes at most `cap` bytes that arrived from the guest; 0 = nothing (without touching the machine) |
| `vetro_net_shutdown` | `(vm, conn) -> u32` | closes the JS→guest direction: FIN after the queued bytes. 1 done, 0 unknown |
| `vetro_net_abort` | `(vm, conn) -> u32` | aborts: RST to the guest |
| `vetro_net_release` | `(vm, conn) -> u32` | forgets the connection (if it is alive, aborts it first); to be called after `CLOSED` and the last read |
| `vetro_net_state` | `(vm, conn, out: *mut u32, cap: usize) -> u32` | state: 0 unknown (or without networking), 1 opening, 2 open (also while closing), 3 closed. In `out` (at most `cap`): close reason (0 none, 1 `Normal`, 2 `GuestReset`, 3 `RemoteReset`, 4 `Refused` = nobody listening in the guest, 5 `Timeout`), readable bytes, space for `vetro_net_send`, end of stream from the guest (1 = the guest closed and everything has been read), queued bytes not yet taken by the guest. It does not touch the machine |

Bytes move while the machine executes: the caller alternates `vetro_run`
and `send`/`recv`, as for the console. Opening, writing, reading ready bytes,
closing and aborting are inputs (they reach the guest before the next
instruction, to be recorded for M10 replay); `vetro_net_state` and a
`vetro_net_recv` without ready bytes do not change the execution.

In JS: `Machine.connectGuest(port)` returns a `GuestSocket`
(`web/node/vetro.mjs`) with `send(bytes)`, `recv()`, `shutdown()`,
`abort()`, `release()` and `state()` (`{ state, reason, readable, writable,
guestEof, unsent }`, names in `NET_STATE` and `NET_REASON`). Test:
`tests/web/hostfwd.mjs` (in `tools/web-test.sh`): `nc -l -e cat` in the
guest, echo of 200 KB from JS, close, port without a service, same
instructions with and without JIT and in two executions.

### File manager (ABI 7, ADR 0020; ABI 9, ADR 0021; `docs/specs/files.md`)

The client of `vetro_machine::files` for the guest's `vetro-files` daemon
(vsock port 5200). It needs the `VSOCK` bit.

| Export | Signature | Meaning |
|---|---|---|
| `vetro_files_open` | `(vm, port: u32) -> u32` | creates the client (port 0 = 5200), replacing the existing one; 1 done, 0 without vsock. After `vetro_load_linux` or `vetro_snapshot_restore`: connections to the daemon left in the snapshot are closed at the first `pump` |
| `vetro_files_close` | `(vm)` | closes the connection and removes the client |
| `vetro_files_status` | `(vm, out: *mut u32, cap: usize) -> u32` | 0 no client, 1 connecting (or waiting to retry), 2 connected. In `out`: unfinished operations, hellos received (grows with every reconnection: observations must be redone), maximum chunk and hello flags (bit 0 SELinux). It does not touch the machine |
| `vetro_files_request` | `(vm, op: u32, a: *const u8, a_len, b: *const u8, b_len, x: u64, y: u64) -> u32` | requests an operation on path `a` (guest bytes, possibly not UTF-8, since ABI 9; empty = rejected): 1 `STAT`, 2 `LIST`, 3 `READ` (`x` offset, `y` bytes, `u64::MAX` = to the end, at most 256 MiB), 4 `WRITE` (`b` content, `x` mode of a new file), 5 `MKDIR` (`x` mode), 6 `CREATE` (`x` mode), 7 `DELETE` (`x` 1 = recursive), 8 `RENAME` (`b` destination, bytes), 9 `WATCH`, 10 `UNWATCH` (`x` wd), 11 `SQL` (ABI 9: `b` = `u32` length and UTF-8 SQL, `u16` number of parameters, parameters in the protocol's value format (`proto::encode_sql_args`); `x` expected changed rows, `u64::MAX` = any; `y` bit 0 read-only). Returns the id (> 0) or 0 |
| `vetro_files_pump` | `(vm) -> u32` | advances the client (between one quantum and the next) and returns the ready messages |
| `vetro_files_take` | `(vm) -> usize` | prepares the next message and gives its length (0 = none) |
| `vetro_files_ptr` | `(vm) -> *const u8` | the message bytes, valid until the next `take` |

Message: `u32` JSON length, UTF-8 JSON, then the bytes of a read.
JSON of a reply: `{"kind":"reply","op":N,"ok":true,"type":T,...}` with
`T` = `stat` (`stat`), `list` (`entries: [{name, stat}]`), `data` (`size`,
`length`: the bytes follow), `written` (`stat`), `watch` (`wd`), `sql`
(`changes`, `lastRowid` as a decimal string, `truncated`, `columns`,
`rows`: values `null`, `["i","<integer>"]`, `["f","<real>"]` (`inf`,
`-inf`, `NaN` included), `["t","<text>"]`, `["b","<hex>"]`),
`done`; or
`{"kind":"reply","op":N,"ok":false,"error":"ENOENT (2)","errno":2,"code":"ENOENT"}`
(`code` `PROTOCOL` or `DISCONNECTED` with `errno` null; `SQLITE` with
`sqlite` = SQLite's code if SQLite rejected it). `stat` = `{kind,
mode, uid, gid, size, mtime, mtimeNs, nlink, link, selinux}`. Event:
`{"kind":"event","wd":N,"mask":N,"cookie":N,"name":"..."}`. Names and
link targets are guest bytes in *surrogateescape*: a byte that is not part
of valid UTF-8 is `\udcXX` (lone surrogate U+DC80 + byte − 0x80);
`pathBytes` in `vetro.mjs` does the inverse for the paths sent.

Connecting, sending and reading are inputs (`Machine::input`, recorded
for replay); `status`, `take` and `ptr` are not. In JS: `Machine.files(port)`
→ `GuestFiles` (a Promise per operation, `sql(path, sql, params,
{ expect, readonly })`, `onEvent`, `status()`, `pump()`, `close()`),
constants `FILES_OP`, `FILES_STATUS`, `INOTIFY`; `pathBytes`,
`pathString`, `displayName`, `encodeSqlArgs`, `sqlValue`.

### Result buffer (ABI 8)

Functions that produce bytes (JSON, HAR, pcapng, logs, keyframes, registers)
put them in the machine's result buffer and return their length
(0 = nothing).

| Export | Signature | Meaning |
|---|---|---|
| `vetro_result_ptr` | `(vm) -> *const u8` | the bytes of the last result (null if empty), valid until the next result |
| `vetro_result_clear` | `(vm)` | frees the buffer |

### Network inspector and timeline (ABI 8, ADR 0023)

The virtio-net frame capture (`Machine::net_tap`, ADR 0016) is collected in
the vetro-wasm machine at every `vetro_run` (at most 64 MiB); list and
detail are the JSON of `vetro_analysis::net::view`, the timeline is that of
`Timeline::to_json` (`docs/specs/analysis.md`). None of this changes the
execution.

| Export | Signature | Meaning |
|---|---|---|
| `vetro_capture_set` | `(vm, on: u32) -> u32` | turns the capture on (1) or off; 1 done, 0 without networking |
| `vetro_capture_clear` | `(vm)` | empties frames and analysis |
| `vetro_capture_stats` | `(vm, out: *mut u64, cap) -> usize` | on, frames, bytes, frames dropped beyond the limit |
| `vetro_inspect_requests` | `(vm) -> usize` | the list in JSON (`requests_json`) |
| `vetro_inspect_request` | `(vm, index: u32) -> usize` | the detail of request `index` in JSON (`exchange_json`); 0 if there is none |
| `vetro_inspect_har` | `(vm, epoch_us: u64) -> usize` | the HAR 1.2 (`epoch_us`: Unix µs of guest time 0) |
| `vetro_inspect_pcapng` | `(vm, epoch_us: u64) -> usize` | the pcapng of the frames |
| `vetro_timeline_input` | `(vm, kind: u32, weak: u32, text, len)` | annotates a user input that the machine does not recognise by itself (file manager command: `kind` 4) at the current instruction |
| `vetro_timeline_effect` | `(vm, kind: u32, text, len) -> u32` | annotates an effect (file changed: `kind` 3) at the current instruction; 0 if the kind does not exist |
| `vetro_timeline_json` | `(vm, window_us: u64) -> usize` | the timeline in JSON with the network effects of the capture; attribution window `window_us` (0 = 3 s) |
| `vetro_timeline_version` | `(vm) -> u64` | changes when inputs, effects or frames change: if it is the same, nothing to redraw |
| `vetro_timeline_clear` | `(vm)` | empties the timeline |

Kinds: inputs `InputKind` (0 key, 1 pointer, 2 touch, 3 console, 4
file, 5 power, 6 screen, 7 other), effects `EffectKind` (0 http, 1
dns, 2 tls, 3 file, 4 console). Inputs that go through
`vetro_console_write`, `vetro_input_*`, `vetro_gpio_input` and
`vetro_display_resize` are annotated automatically (`analysis::Describer`:
keys and buttons pressed, new touches, console lines, power key,
resolution; not movements, releases, terminal replies); console output is
annotated when `vetro_console_read` reads it.

### Record & replay (ABI 8, ADR 0019 and 0023, `docs/specs/replay.md`)

| Export | Signature | Meaning |
|---|---|---|
| `vetro_record_start` | `(vm, keyframe_every: u64)` | records from here, a keyframe every so many instructions (the first immediately; 0 = none) |
| `vetro_record_stop` | `(vm) -> u32` | stops; the log stays in the machine. 1 done, 0 was not recording |
| `vetro_rr_status` | `(vm, out: *mut u64, cap) -> u32` | 0 idle, 1 recording, 2 replaying, 3 replay finished identical, 4 replay diverged (reason in the message). In `out`: events recorded or next event, log events, keyframes, start and end instruction, 1 if there is a log |
| `vetro_log_encode` | `(vm) -> usize` | the log file with the keyframes present |
| `vetro_log_load` | `(vm, data, len) -> u32` | loads a log file; 0 done, 1 invalid (reason in the message) |
| `vetro_log_info` | `(vm, out: *mut u64, cap) -> usize` | start, end, events, keyframes, interval, JIT, 1 if from the same configuration, event bytes; 0 without a log |
| `vetro_log_events` | `(vm) -> usize` | the events in JSON: `[{i, step, kind, label, weak, user}]` |
| `vetro_log_keyframe` | `(vm, index, out: *mut u64, cap) -> usize` | instruction, console bytes and hash, size, 1 if present |
| `vetro_log_keyframe_take` | `(vm, index) -> usize` | moves the keyframe bytes into the result buffer (the position stays in the log) |
| `vetro_log_keyframe_put` | `(vm, index, data, len) -> u32` | puts them back; 1 done, 0 wrong index or length |
| `vetro_log_keyframe_for` | `(vm, step: u64) -> i32` | the keyframe the replay towards `step` starts from, -1 none |
| `vetro_replay_start` | `(vm, step: u64) -> u32` | replay from the last keyframe not beyond `step` (0 = from the beginning). Codes: 0 done, 1 no log, 2 keyframe not present, 3 rejected (reason in the message). Capture and timeline restart (timeline with the log's inputs), the file manager client is closed |
| `vetro_registers_text` | `(vm) -> usize` | the registers (`Machine::registers_text`) |
| `vetro_read_virt` | `(vm, va: u64, dst, len, fault: *mut u64) -> u32` | virtual memory (current tables, RAM only); 1 done, 0 with the first unreadable address in `fault` |
| `vetro_translate` | `(vm, va: u64) -> u64` | physical address, `u64::MAX` if not mapped |
| `vetro_read_phys` | `(vm, pa: u64, dst, len) -> u32` | RAM at the physical address; 0 outside the RAM |

During replay `vetro_run` stops at the log's events and at the end compares
the fingerprint; inputs from JS are ignored. To jump to an instruction:
`vetro_replay_start(step)`, then `vetro_run` with budget
`min(quantum, step - instructions)` until it is reached (disks are served
as always).

In JS: `Machine.capture`, `captureStats`, `inspectRequests`,
`inspectRequest`, `inspectHar`, `inspectPcapng`, `timelineInput`,
`timelineEffect`, `timeline`, `timelineVersion`, `recordStart`,
`recordStop`, `rrStatus`, `logEncode`, `logLoad`, `logInfo`, `logEvents`,
`logKeyframe`, `logKeyframeTake`, `logKeyframePut`, `logKeyframeFor`,
`replayStart`, `registersText`, `readVirt`, `translate`, `readPhys`;
constants `TIMELINE_INPUT`, `TIMELINE_EFFECT`, `RR_STATE`, `REPLAY_START`;
`Recording` (`web/node/recording.mjs`) for keyframes in an archive.

### JIT bridge

| Export | Signature | Meaning |
|---|---|---|
| `vetro_jit_ld` | `(state: usize, va: u64, size: u32) -> u64` | `env.ld` of the generated modules (spec `jit.md`) |
| `vetro_jit_st` | `(state: usize, va: u64, size: u32, value: u64) -> u32` | `env.st` of the generated modules |
| `vetro_jit_resolve` | `(state: usize) -> u32` | `env.resolve` of the dispatcher |
| `vetro_jit_vsync` | `(state: usize)` | the runtime's `env.vsync`: V0..V31 of the `Cpu` in the `JitState` (ABI 10) |
| `vetro_jit_simd` | `(state: usize, word: u32, x: u64, nzcv: u32) -> u64` | the runtime's `env.simd`: SIMD/FP instruction without memory access executed by the interpreter on the `JitState` (ABI 11, ADR 0026) |
| `__indirect_function_table` | table | the vetro-wasm function table, exported and growable (`build.rs`): JS puts the dispatcher in it, which Rust calls as a function pointer |
| `vetro_jit_selftest` | `(wasm: *const u8, len: usize) -> u64` | full round-trip test with a test module (below) |

## Imports

JS provides them at instantiation (`web/node/vetro.mjs`):

| Import | Signature | Meaning |
|---|---|---|
| `vetro_host.panic` | `(ptr: *const u8, len: usize)` | UTF-8 message of a panic, right before the `unreachable` trap |
| `vetro_host.snapshot_write` | `(ptr: *const u8, len: usize)` | ABI 12: a chunk of `vetro_snapshot_save_stream` (the view is valid only during the call) |
| `vetro_host.snapshot_read` | `(ptr: *mut u8, cap: usize) -> usize` | ABI 12: the next bytes (at most `cap`) for `vetro_snapshot_restore_stream`, 0 at the end |
| `vetro_jit.compile` | `(ptr: *const u8, len: usize) -> i32` | compiles and instantiates a generated module; index ≥ 0, or < 0 if rejected |
| `vetro_jit.runtime` | `(ptr: *const u8, len: usize) -> i32` | compiles and instantiates the runtime module (with `env.mem`, `env.ld`, `env.st`, `env.vsync`, `env.simd` since ABI 11); its exports are the `rt.*` imports of the modules compiled afterwards, also after `reset`; 0, or < 0 if rejected (ABI 10) |
| `vetro_jit.entry` | `(module: i32, index: u32) -> u32` | puts the module's export `b<index>` in a new entry of `__indirect_function_table` and returns it: `JsEngine::run` calls it as a function pointer, without going through JS |
| `vetro_jit.place` | `(module: i32, count: u32, base: u32)` | puts the module's `b0..b<count-1>` in the block table (the dispatcher's `env.tbl`) starting at entry `base` |
| `vetro_jit.reset` | `()` | discards all instances and recreates the block table |
| `vetro_jit.ready` | `(module: i32) -> u32` | ABI 14: 1 if the module can run, 0 while a Worker still compiles it (ADR 0038: `JitEngine.startBackground()`; the regions of a module that is not ready run in the interpreter and the module is placed in the block table when it arrives) |
| `vetro_jit.drop` | `(module: i32)` | frees the module |

## The JIT engine in JavaScript

ADR 0012: in the browser the generated code is compiled and executed by the
JS `WebAssembly` API. The pieces:

- `web/node/jit-engine.mjs`, class `JitEngine`, the JS equivalent of the
  `vetro_jit::Engine` trait in `jit.md`:
  - `runtime(bytes)`: instantiates the runtime module with `env.mem`,
    `env.ld`/`env.st`/`env.vsync` = `vetro_jit_ld`/`vetro_jit_st`/
    `vetro_jit_vsync`, and keeps its exports for the `rt.*` imports;
  - `compile(bytes)`: `new WebAssembly.Module(bytes)` and immediately
    `new WebAssembly.Instance(module, { env: { mem, tbl, ld, st, resolve }, rt })`,
    with `env.mem` = vetro-wasm's `memory`, `env.tbl` = the block table
    (only the dispatcher imports it), `env.ld`/`env.st`/`env.resolve`
    = `vetro_jit_ld`/`vetro_jit_st`/`vetro_jit_resolve` and `rt` = the
    runtime's exports. An export of one instance passed as an import of
    another is called by V8 directly, without going through JS;
  - `place`, `entry`, `reset` like the imports above;
  - the shared memory is vetro-wasm's linear memory;
  - `imports()`: the `vetro_jit.*` imports; `attach(exports)` after
    instantiation.
- `crates/vetro-wasm/src/jit.rs`, Rust side: `impl vetro_jit::Engine for
  JsEngine`.
  - The shared memory is a 256 KiB buffer aligned to 16 inside
    vetro-wasm (`JitState` and the system-mode area): `state` is an
    offset into that buffer, and the block receives the absolute address
    (buffer + `state`), because for the block `env.mem` is the whole linear
    memory. For the same reason `host_address` is the address itself: the
    blocks' software TLB points straight at the guest RAM.
  - `run` calls the function through `__indirect_function_table` (entry
    given by `vetro_jit.entry` and cached): no trip through JS.
  - During `run` the `Host` and the shared memory are reachable from
    `vetro_jit_ld`/`vetro_jit_st`/`vetro_jit_resolve` (per-thread cells,
    set and restored by `run`, hence also reentrant).
  - Faults: `vetro_jit_ld` writes `FAULT` (1) in `exit_detail` and returns
    0; `vetro_jit_st` writes `FAULT` or `STOP` (2) and returns 1.
- `vetro_jit_selftest` and `web/node/jit-selftest.mjs`: JS encodes a
  module with a block `b0` that does `x2 = ld(x0) + x1; st(x0 + 8, x2);
  pc += 12; steps += 3`; Rust compiles it with `JsEngine`, runs it on a
  `JitState` with `x0 = 0x1000`, `x1 = 5` over a test RAM containing
  37, and returns the value written (42). It tests the round trip Rust → JS →
  generated module → `ld`/`st` in Rust.

The execution loop with blocks lives in `vetro-machine` and in
`vetro_jit::sys` (ADR 0013).

## Disks in JavaScript (`web/node/disk.mjs`)

No Node APIs (it runs in the app's Worker and in the tests):

- `RangeSource(url)`: `open()` reads the size from the `Content-Range` of
  a `Range: bytes=0-0` request (a 206 response is required) and builds the
  cache key from URL, size and `ETag`/`Last-Modified`; `read(offset,
  length)` with Range, 3 attempts on network errors and on 5xx.
- `BlobSource(file)`: a `File` chosen by the user (`Blob.slice`).
- `MemoryCache` and `OpfsCache.open(key, blockSize, blocks)`: the OPFS cache
  uses `FileSystemSyncAccessHandle` (only in a dedicated Worker): an `.img`
  file with the blocks in place and a `.map` file with one bit per block
  present, written after the data. On restart the blocks present do not go
  back to the network.
- `DiskFeeder(machine)`: `add(source, { cache, blockSize, maxBlocks,
  readOnly, readahead })` adds the disk (`vetro_disk_add`); `serve()`
  after `Blocked` delivers the requested blocks, first from the cache, then
  from the source, merging contiguous blocks (plus `readahead` blocks after
  each one) into a request of up to 8 MiB. A source error after the
  attempts becomes `vetro_disk_fail`.

## Persistence in JavaScript (`web/node/persist.mjs`, M6, ADR 0017)

No Node APIs. Files have the `FileSystemSyncAccessHandle` interface
(`getSize`, `read`, `write`, `truncate`, `flush`, `close`): the OPFS ones
in the Worker (`opfsFile(folder, name)`), `MemFile` in the tests.

- `DiskOverlay.open(machine, disk, file, identity)`: reads the file and calls
  `vetro_overlay_open` (`opened.code`: `Loaded`, `New`, `Mismatch`,
  `Corrupt`); `persist()` applies the writes of `vetro_overlay_take`
  (truncation, data, flush, header, flush) and says whether it wrote;
  `generation`, `info`.
- `SnapshotStore.opfs()` / `.memory()`: `loadMeta(key)` (metadata of a
  complete snapshot without reading its bytes), `readInto(key, view)`,
  `openReader(key)` (`{ size, readAt, close }`), `saveStream(key, meta,
  produce)` (chunks written as they come into a new file, which replaces the
  old one only when the save succeeded);
  `save(key, metadata, bytes)`
  writes `<key>.snap` and then `<key>.json` (with `size`), `load(key)`
  returns `{ meta, bytes }` only if the metadata is there and the length
  matches; `remove`.
- `snapshotKey(parts)`: SHA-256 (32 hex digits) of the JSON with sorted
  keys; `sha256Hex`; `staleReason(meta, overlays)`: null if every overlay
  is at the generation saved in the metadata, otherwise the reason;
  `toBase64`/`fromBase64` for the console tail in the metadata.

The `Machine` class of `vetro.mjs` has `snapshotVersion`, `snapshotSave()`
(copy of the bytes), `snapshotSaveTo(write)` (chunked: synchronous
`write(bytes, offset)`, then the header at offset 0),
`snapshotRestoreStream(size, readAt)` (chunked: only the part before the
RAM goes into the module's memory), `snapshotRestoreWith(n, fill)` (the
bytes read straight into a buffer in the module's memory), `memoryBytes`,
`snapshotRestore(bytes)` (throws an `Error` with `code`
`BadMagic`/`Version`/`Config`/`Corrupt`), `overlayOpen(disk, identity,
bytes)`, `overlayTake(disk)` (`{ truncate, writes: [{ at, bytes }] }` or null),
`overlayInfo(disk)`.

## The web app (`web/app`)

HTML, CSS and ES modules served as they are: no bundler, no npm
dependencies.

```sh
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
node tools/web-serve.mjs            # http://127.0.0.1:8080/app/
```

`tools/web-serve.mjs` serves `web/` (the app at `/app/`, the shared modules at
`/node/`), the `.wasm` at `/wasm/vetro_wasm.wasm`, `target/guest-kernel` at
`/guest/` (M3 kernel and initramfs, already selected in the page) and
`target/web-disks` at `/disks/`, with Range, `ETag` and the headers
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` (plus
`Cross-Origin-Resource-Policy: same-origin`). They are not needed today (a
single thread, no `SharedArrayBuffer`), but the page is already
`crossOriginIsolated`: when WASM threads arrive (ADR 0002) every server
hosting the app will have to send them, and every resource from another
origin (e.g. a disk on a CDN) will need CORS or `Cross-Origin-Resource-Policy:
cross-origin`. URL parameters:
`?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1`,
plus `snapshot=0` (no snapshot cache), `persist=0` (non-persistent
disks), `files=/a,/b` (file manager roots), `nofiles=1`
(no file manager and no vsock), plus `ram=MiB` and, for Vetro's AOSP
image, `os=android`, `manifest=URL` (default: the version published on
R2; `tools/web-serve.mjs` also serves `target/aosp/out` at `/aosp/`) and
`catalog=URL` (the app catalog, default catalog/v1.json on R2, ADR 0033).

- `main.mjs` (page thread): chooses kernel, initramfs and disk (URL
  or local file), options (RAM, resolution, tablet or touchscreen, 64 KiB
  or 1 MiB blocks, JIT, real time, OPFS cache, persistent disks,
  cached snapshots, WebGPU), starts the
  Worker; draws the changed rectangles of the scanout (`display.mjs`:
  Canvas2D with `putImageData`, or WebGPU with `writeTexture` and a
  full-screen triangle, if chosen and available), the cursor in a second
  canvas on top, the console (`terminal.mjs`: CR/LF/BS/TAB, CSI K/J/C/D/G/H,
  reply to `ESC[6n`, UTF-8), the status bar. Buttons "Save state"
  (snapshot now) and, before boot, "Delete saved data"
  (OPFS folders `vetro-snapshots`, `vetro-overlays`, `vetro-disks`). After
  a restore it shows the snapshot's console tail again without
  replying to the terminal's queries. `window.vetroState` (`boot`:
  `{ mode: 'cold' | 'snapshot', ms, times }`, `snapshots`, `disks`) for the
  tests.
- File manager (M8, ADR 0020, `docs/specs/files.md`): option on by
  default (virtio-vsock in the machine and in the snapshot key); the
  Worker holds `GuestFiles` and advances it between one slice and the next,
  the page's requests are inputs recorded in `inputLog`; the
  `files.mjs` panel next to the screen shows the tree of the roots
  (`window.vetroFiles.setRoots`), updated by inotify events, with the
  viewers (text, JSON, XML/SharedPreferences, hex,
  images, SQLite with `sqlite.mjs`) and saving into the guest.
- Analysis (M7, M10, ADR 0023; `analysis.mjs`): three panels below the
  screen. **Network**: list of requests (method, host, path, status,
  sizes, type, duration, waterfall), filters (text, method, status, body
  type), detail with per-phase timings, headers and decoded bodies
  (JSON, form, multipart, schema-less protobuf, text, hex),
  HAR and pcapng export (Blob + `<a download>`). **Timeline**: inputs
  in guest time with their effects (http, dns, tls, file, console),
  time axis, attribution window, filters, click on a request →
  detail, "go here" → replay up to that instruction. **Recording**:
  record/stop (keyframe every N M instructions), replay (verdict "identical
  replay" or the difference), download and load the log, go to instruction,
  continue, registers and hex dump of a virtual address, recorded
  inputs. `window.vetroAnalysis.state()` for the tests. The Worker sends
  list and timeline at most every 0.7 s if they have changed; with networking
  the capture is on from boot; file manager commands (save, create,
  delete, rename) are timeline inputs and inotify events
  (created, written, moved, deleted) are effects. Recordings in OPFS
  (`vetro-recordings/`, "Delete saved data" removes them too); during a
  replay the page's inputs are discarded, the file manager is
  closed, no real time and no cached snapshots.
- Inputs: keyboard on the canvas with `KeyboardEvent.code` → Linux code
  (`keymap.mjs`; browser repeats are discarded, the guest does autorepeat
  with EV_REP; on blur the pressed keys are released); mouse with
  absolute tablet coordinates 0..32767 and `BTN_*` buttons, wheel as
  `REL_WHEEL`; with the touchscreen each `pointerId` takes a slot (0..9);
  console with a terminal's bytes (Enter = CR, Backspace = DEL, arrows
  CSI, Ctrl+letter, paste); power key button (GPIO 3
  pressed and released).
- `worker.mjs`: instantiates vetro-wasm and the JIT engine, creates the machine
  (`vetro_machine_new_with`, network with the sinkhole optionally), the disks (`DiskFeeder`, OPFS cache for
  URLs), loads the kernel; runs 1 M-instruction quanta for at most 12 ms
  per slice, then sends console, changed rectangle (transferred
  ArrayBuffer), cursor, statistics; applies inputs between one slice and
  the next (recording them with the instruction number, for M10); on
  `Blocked` it waits for `DiskFeeder.serve()`; on `Idle` it waits for a message.
  With "real time" guest time does not run ahead of the real clock
  (time spent waiting for disks does not count).
- Persistence in the Worker (ADR 0017): for every writable disk an overlay
  in `vetro-overlays/<sha256(identity)>.cow`, saved between one slice and
  the next (at most every second, and at every stop); the snapshot in
  `vetro-snapshots/`, with a key from the format version, hash of kernel and
  initramfs, command line, RAM, resolution, devices, identity and
  size of the disks. At boot, if there is a snapshot for the key and the
  overlays are at the generation of its metadata, it restores instead of
  loading the kernel (message `restored` with the timings); otherwise it
  boots from scratch (`cold`). The snapshot is saved (after the overlays,
  message `snapshot`) the first time the guest is idle, again when idle if
  the overlays have changed, and on request. Idle: `Idle`, or 1.5 s of
  guest time without console, scanout, inputs or disks.

### The AOSP image in the app (M5/M6, ADR 0028)

- `web/node/android.mjs`: `PHASES` and `BootProgress` (boot phases from the
  console: kernel, init first and second stage, zygote, surfaceflinger,
  system_server, boot finished = `sys-boot-completed-set`; the home screen
  is marked from outside with `mark`), `HOME_QUERY`/`isHome` (focused
  window via adb), `gridColors`/`HOME_MIN_COLORS` (home actually drawn),
  `ANDROID_PARAMS`, `colorSeen` (an app colour in RGB order, ADR 0032),
  `ANDROID_VERSIONS` (the published image versions, newest first: the app's
  version selector; `DEFAULT_MANIFEST` is the first).
- `web/node/adb.mjs`: ADB client over a transport with `send`/`recv`/
  `state` (the `GuestSocket` to port 5555 of the guest): `connect` (CNXN;
  AUTH with an RSA `AdbKey` if the device asks), `open`, `shell` (shell v2
  with stdout, stderr and exit code), `push` (sync; optional
  `onProgress(sent, total)` every 1 MiB), `install` (push to
  /data/local/tmp + `pm install -r`), `devices`; `pump()` between quanta.
- `web/node/apk.mjs`: `apkInfo(bytes)` = package, version, label, main
  activity, SDK levels, icon resource and native ABIs from the ZIP's binary
  manifest; `parseArsc`/`resolveResource` (resources.arsc) and `apkIcon`
  (densest raster icon, or an SVG made from an adaptive icon with a raster
  foreground) for the catalog tool.
- `web/node/catalog.mjs` (ADR 0033): the app catalog without the DOM:
  `parseCatalog`/`parseEntry` (catalog/v1.json, format 1), `loadCatalog`,
  `imageRelease`/`imageSatisfies` (minimum image version by AOSP release),
  `verifyApk` and `downloadApk` (size and SHA-256, progress), `nextState`
  (card states absent/downloading/installing/installed/failed),
  `parsePackages` (`pm list packages --show-versioncode`).
- `web/node/disk.mjs`: `LayoutSource(url)`, the disk rebuilt from a map
  (`tools/aosp/web-disk.mjs`): extents into the published sparse files
  (`super.img`, `userdata.img`) read with HTTP Range, fills and holes;
  `parseLayout`, `composePlan`, `composeRead`.
- Worker: with `config.android` it reads the manifest (image hashes for the
  snapshot key), downloads the boot images only for a cold boot (sha256
  checked, copy in OPFS `vetro-images/`), disk from the map with 64 1 MiB
  blocks in memory and the rest in OPFS, machine with touchscreen, network
  and (if chosen) vsock; it sends the phases (`progress`, `booted`), after
  `sys.boot_completed` it connects the ADB client (`adb-status`), keeps the
  screen on, watches for the home screen, and serves the page's `adb`
  requests (`shell`, `devices`, `install` opening the app with
  `am start -W -n` unless `open: false`, with `adb-progress` messages carrying
  `fraction` during the push, `open` of an installed package by its launcher
  activity); the snapshot is saved 5 s of guest time after the home
  screen is drawn, after an install and on request; no separate overlay (the
  snapshot already contains the copy-on-write layer). Snapshots are written
  to and read from OPFS in chunks.
- Page: "System" selector, a panel with the phases and their times, adb
  status, APK dropped on the panel or on the screen (or chosen), an
  `adb shell` line; `window.vetroAndroid` (`state`, `install`, `shell`,
  `devices`) for tests. The app catalog panel (`web/app/catalog.mjs`,
  ADR 0033): cards from catalog/v1.json on R2 (`&catalog=URL` for another),
  Install -> verified download -> the same `install` request (`open: false`)
  -> Open; hidden if the catalog can't be loaded; `window.vetroCatalog`
  (`state`, `install`, `open`, `refresh`) for tests.
- Device profiles (M10, ADR 0035, `docs/specs/device-profiles.md`,
  `web/node/profiles.mjs`, starters in `web/app/profiles/`): a "Device
  profile" menu, a profile file, or `profile=<id>` fill screen and RAM; the
  Worker gets `config.android.params` (`profileBootParams`) and
  `config.android.setup` (adb commands run after `ANDROID_WAKE` on every
  connection). The default profile is `ANDROID_MACHINE` with
  `ANDROID_PARAMS`. A portrait scanout is fitted to the window height.
- Shared with the tools (`web/node/android.mjs`): `ANDROID_MACHINE` (2048 MiB,
  1280x800, touchscreen, network, vsock), `ANDROID_DISK`, `machineDevices`,
  `ANDROID_WAKE`, `ANDROID_COMPACT`, the home screen timings,
  `DEFAULT_MANIFEST`, `androidKeyParts`.

### The prebuilt Android snapshot (M6, ADR 0031)

- Key (`androidKeyParts`, `web/node/prebuilt.mjs` `androidSnapshotKey`):
  snapshot format and configuration hash of the running vetro-wasm (ABI 13),
  RAM, screen, devices, image version, sha256 of the three boot images,
  bootloader parameters, sha256 and size of the disk map (not its URL). The
  app's own Android snapshots in OPFS use the same key.
- On R2, next to the image: `aosp/<version>/snapshots/<key>.snap` (the
  snapshot, small level) and `<key>.json`: `{ format:
  'vetro-prebuilt-snapshot', version: 1, key, parts, size, sha256, chunk:
  16 MiB, chunks: [sha256 of every chunk], meta: { steps, console, progress,
  savedAt, why, prebuilt }, measures }` (`prebuiltProblem` checks it).
- `findPrebuilt(manifestUrl, key)` → `{ info, url }` or `{ missing }` (404 =
  none for this vetro-wasm). `downloadPrebuilt(info, url, file, { resume,
  saveResume, onProgress, fetch, retries })`: streaming download, each chunk
  verified before it is written, retries with a Range from the first
  unverified chunk, `{ bytes, ms, resumedFrom, retries }`.
  `SnapshotStore.downloadTarget(key)` → `{ file, resume, saveResume, finish,
  close }`: the file is the snapshot itself, the resume state in
  `<key>.part.json`, the metadata written by `finish`.
- Worker: with no snapshot of its own for the key and `config.android.prebuilt`
  not false, it looks up and downloads the prebuilt snapshot (messages
  `prebuilt`: `missing`, `downloading` with `loaded`/`total`/`ms`, `done`,
  `failed`), then restores it as usual (`restored` with `prebuilt: true`). A
  failed download stops the start (reloading resumes it); none for the key =
  cold boot.
- Page: the expected size from `app/android-prebuilt.json` (written by the
  site build, `tools/aosp/prebuilt-key.mjs`), progress bar with MiB, rate and
  time left, a "cold boot" box (`&cold=1`); `window.vetroAndroid.state().prebuilt`.
- Tools: `tools/aosp/prebuilt-snapshot.mjs` (makes it: the app's machine in
  Node up to the home screen, compaction, small level), `upload-snapshot.sh`,
  `prebuilt-key.mjs` (key of a vetro-wasm build, lookup on R2, the site's
  hint and guard).

## Web tests

`tools/web-test.sh [--no-jit]` (CI `boot` job, after
`tools/wasm-boot.sh`):

- `tests/boot/tests/web.rs` (native, release): the same scripts as
  `boot-disk.mjs` and `devices.mjs` with the vetro-wasm API compiled for
  the host, the interpreter and a local disk; writes instructions and raw log to
  `target/web-test/native-*`. With `VETRO_WEB_NATIVE=1` (set by
  `tools/web-test.sh`) the Node tests must give the same instructions and the
  same log byte for byte (JIT in V8, disk over HTTP);
- `tests/web/unit.mjs`: server (Range, suffixes, 416, HEAD, COOP/COEP,
  paths outside the root), `RangeSource`, `BlobSource`, `DiskFeeder` (cache,
  contiguous blocks, read-ahead, errors), key map, terminal,
  `MemFile`, `SnapshotStore`, `snapshotKey`, `staleReason`;
- `tests/web/boot-disk.mjs`: the M3 kernel reads and writes a raw test disk
  (`md5sum /dev/vda`, 13 bytes written, cache dropped, read back,
  `md5sum`): local disk, over HTTP with an empty cache, from the full cache
  (no network reads), with 4 KiB blocks and read-ahead.
  Same instructions and same log in all cases; sums equal to those of the
  file; file on the server intact;
- `tests/web/devices.mjs`: `vetro-dev drm-hold` and the framebuffer read with
  `vetro_display_ptr` equal pixel for pixel to the guest's pattern (the same
  check as `tests/boot/tests/devices.rs`), changed rectangle,
  cursor, scanout power-off; keyboard and tablet via API read by the
  guest with evdev; LEDs; two identical executions;
- `tests/web/snapshot.mjs` (M6): M3 kernel with a disk over HTTP and overlay on
  `MemFile`; snapshot at 40 M instructions and at the prompt, then a write (dd +
  sync, 75 clusters), read back with the cache dropped, `md5sum`, power-off;
  restore on new machines with the JIT and with the interpreter (and from mid-boot):
  continuation of the log byte for byte, final instructions and overlay file
  equal to the uncut execution; boot from scratch with the overlay (the
  write is there); base changed (overlay discarded). Prints save and
  restore times and sizes in V8;
- `tests/web/files.mjs` (M8): the file manager via API on the M3 kernel
  with vsock: list, read, ENOENT, a write that preserves mode and
  owner read by the guest, event from a guest process within 1 s
  of guest time, 1.2 MB written and read back in pieces (`cmp` in the guest),
  recursive delete; two equal executions;
- `tests/web/browser.mjs`: the app in headless Chrome driven with the
  DevTools protocol (Node 22 WebSocket): boot with the disk over HTTP, `md5sum` and
  guest pattern on the canvas pixel for pixel with the cursor visible, a real
  key from the canvas to the guest; snapshot saved when idle after boot,
  write to the disk and snapshot saved again; second session with
  `snapshot=0` (boot from scratch, write found again from the overlay in OPFS,
  blocks from the OPFS cache); third session restored from the snapshot
  (time measured, console that responds, write present); file manager
  panel (tree updated live when the guest creates a
  file, file opened, edited and saved, read back by the guest with `cat`,
  reconnection after the restore). Without Chrome
  (`VETRO_CHROME`) it prints SKIP; `VETRO_REQUIRE_BROWSER=1` makes it an
  error;
- `tests/web/inspector.mjs` (M7, ABI 8): M3 kernel with networking, JSON POST and
  form POST from wget to the sinkhole; list, detail with decoded bodies,
  HAR, pcapng; requests and DNS attributed to the command line, a
  single character that causes no network traffic; two equal executions (log, HAR,
  pcapng, timeline);
- `tests/web/replay.mjs` (M10, ABI 8): recording with a keyframe every 10 M
  instructions, log from file, keyframes in an in-memory archive (`Recording`),
  identical replay with JIT and interpreter (console byte for byte, inspector and
  timeline equal), jump to an instruction with the same registers and the
  same memory at VBAR_EL1, recomposed log equal to the file, altered keyframe
  rejected;
- `tests/web/android-boot.mjs` (M5, ABI 12): v4 `boot.img` and
  `init_boot.img` from `mkbootimg.py` around the M3 kernel,
  `vetro_load_android` on a 3 GiB machine: instructions and log equal to the
  native reference `ram3g` (direct `load_linux`); chunked snapshot equal to
  the whole one; chunked restore on a new 3 GiB machine (region reused) with
  the same continuation; a tiny JIT code limit (260 resets) with the same
  execution;
- `tests/web/adb.mjs`: the ADB client against a fake adbd
  (`tests/web/fake-adbd.mjs`: one WRTE in flight per stream, sync, shell v2
  and raw, AUTH with a signature and with the public key, push progress);
  `tests/web/catalog.mjs`: the app catalog over HTTP against the same fake
  adbd (verified download, install, installed packages, tampered APKs never
  pushed); `tests/web/browser-catalog.mjs`: the catalog panel in Chrome with
  fake adb requests; `tests/web/adb-tcp.mjs HOST:PORT [APK]` against a real
  adbd (manual test);
- long, only with `VETRO_ANDROID=1`: `tests/web/android.mjs` (Android in
  Node from a cold boot or a snapshot, phases, memory, snapshot, adb over
  GuestSocket, the test APK installed and opened, a touch) and
  `tests/web/android-chrome.mjs` (the app in Chrome: first boot to the home
  screen, snapshot, second start from the snapshot measured, APK installed
  from the page, a click on the canvas; with `VETRO_ANDROID_PREBUILT=1` the
  first start downloads and restores the prebuilt snapshot instead, ADR 0031;
  measurements in `target/aosp/chrome-measurements.json`) and
  `tests/web/android-catalog.mjs` (a catalog app installed on the prebuilt
  snapshot through the card, opened, found installed again after a restore
  from OPFS; `target/aosp/catalog-measurements.json`). In CI: the
  prebuilt form every night, the cold boot weekly
  (`.github/workflows/nightly.yml`);
- `tests/web/unit.mjs` also covers the prebuilt snapshot download (lookup,
  404, chunks verified, a break retried with a Range, an interrupted download
  resumed in a new session, a damaged chunk never written);
- `tests/web/browser-analysis.mjs` (Chrome, like `browser.mjs`): wget
  in the inspector with the JSON decoded and tied to the command in the
  timeline, a file write tied to its command, real downloads of
  log, HAR and pcapng, identical "Replay", "go here" from the timeline with
  registers and memory dump, identical "Continue", log reloaded with "Load
  log" and replayed.

## Node

- `web/node/vetro.mjs`: `instantiate(bytes)` and the `Machine` class, which wraps
  the API (buffers, console, counts). It uses no Node APIs.
- `web/node/boot.mjs`: the script of `tests/boot/tests/vetro.rs` (`/init`
  marker, self-test ok, `echo VETRO-SHELL-$((6*7))` at the full prompt,
  `poweroff -f` until `PowerOff`), with real timings, `--expect-steps N` and the
  log in `target/guest-kernel/node-boot.log`. With `--jit` (`--jit-threshold
  N`, `--jit-batch N`) it runs with the JIT, prints the counters and writes
  `node-boot-jit.log`.
- `tools/wasm-boot.sh [--jit]`: builds the .wasm, runs
  `jit-selftest.mjs`, the native boot and the boot in Node (and with the JIT);
  instructions and logs must match. With `--jit` it fails if the JIT in V8 is
  slower than the native interpreter (M4 threshold). It runs in the CI `boot`
  job (Node 22).
