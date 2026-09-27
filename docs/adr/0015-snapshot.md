# ADR 0015 — Machine snapshot: format, contents, determinism

- Status: accepted (M6, first part, 2026-09-26).

## Context
M6 asks for the Android home screen in under 15 s from the second boot: we
start from a snapshot of the already-booted machine. M10 (record & replay,
jump to an event) will need the same thing, with a stronger requirement:
from a snapshot we must resume **exactly** as if the machine had never
stopped, instruction by instruction. The machine is already deterministic
(time = instructions, ADR 0011; stopped time on disks, ADR 0014), so it is
enough for the snapshot to contain all the state that affects the future.

The state is spread across six crates (`vetro-cpu`, `vetro-mmu`,
`vetro-platform`, `vetro-net`, `vetro-machine`, and the host backends in
`vetro-cli` and `vetro-wasm`), with private fields. We need a format with
no external dependencies that also compiles for wasm32.

## Decision

### The `vetro-snapshot` crate
- No dependencies. It provides `Writer`/`Reader` (fixed-width little
  endian integers, strict 0/1 booleans, length-prefixed bytes, options,
  sequences, **sections** with a 4-byte tag and a length), the trait
  `Snapshot { save(&self, &mut Writer); restore(&mut self, &mut Reader) }`,
  the file header and the compression.
- Each crate implements `Snapshot` for its own state, next to the code
  that owns it (fields stay private): `Cpu`, `SysState`, `Mmu`,
  `Tlb`, `GenericTimer`, `Gic`, `Pl011`, `Pl031`, `Pl061`, `Virtqueue`,
  `VirtioMmio`, `Virt`, `Stack<U: Snapshot>`, `Sinkhole`, `Ram`.
  `vetro-machine` assembles the sections (`Machine::save`,
  `Machine::load_state`, `Machine::restore`).
- `restore` always starts from an object **built with the same
  configuration** and brings it to the saved state. Whatever is
  configuration (sizes, CID, MAC, disk capacities, number of queues) is
  written and on restore is **checked** (`Reader::expect_u64`), not
  overwritten.

### Interfaces that change (`docs/specs/platform.md`)
- `VirtioDevice` has two mandatory methods, `save_state` and
  `restore_state`: every device must report its state (a device that
  forgot to would not compile).
- `BlockBackend`, `NetBackend`, `ConsoleBackend` have `save_state` and
  `restore_state` with an empty implementation: as a rule a backend is a
  **link** to the outside that the host recreates before the restore.
  Whoever holds data written by the guest saves it: writable `MemBackend`
  (the whole content; read-only, only the hash, to check it is the same
  image), `CowBackend` (the written clusters, then the base),
  `QueueNet`, `BufferConsole`, `NetLink` from `vetro-machine` (the whole
  network stack). `DisplayBackend` does not change: on restore the GPU
  sends it again the image and cursor of every scanout.

### File format (version 1)
```
"VETROSNP"  u32 version  u64 configuration hash
u64 content length  u64 content checksum
content: sections MACH, CPU , MMU , PLAT, RAM  (in this order)
```
- **Version** (`vetro_snapshot::FORMAT_VERSION`): changes with every
  modification of what is written. A snapshot of another version is
  rejected with `Error::Version` and a clear message; no conversions
  (snapshots are caches, they get redone).
- **Configuration hash**: RAM, initial time and seed (`MachineConfig`),
  `Devices` (the nested GPU and network configurations in their `Debug`
  form, which is stable), and for each of the 32 virtio slots the type,
  offered features and queue sizes (this covers disks mounted by the host
  after construction). Different = `Error::Config`, machine untouched.
- **Checksum** (`hash64`, FNV-1a on words with a final mix,
  not cryptographic): a corrupted file gives `Error::Checksum`.
- **Large data** (RAM, pixels of GPU resources, copy-on-write clusters,
  in-memory disks) in 4 KiB blocks: zero blocks are not written, the
  others with a simple LZ77 (`vetro_snapshot::lz`, LEB128 tokens of
  literals or copies, overlapping too = RLE), or raw if it does not pay
  off. The compressor is greedy and depends only on the block.

### What is in it
| Section | Contents |
|---|---|
| `MACH` | executed instructions (the clock), cached deadlines of the timer and of the network stack, pending WFI (ADR 0014), CNTPCT, lines to update, virtio to serve, waiting disk |
| `CPU ` | X0–X30, SP, PC, NZCV, TPIDR*, V0–V31, FPCR/FPSR, exclusive monitor, PSTATE, SP_EL0/1, all EL1 and debug system registers, pending SError, configuration (PSCI, MPIDR) |
| `MMU ` | PARange (checked), SCTLR/TCR/TTBR0/TTBR1/MAIR, **the TLB entries** |
| `PLAT` | timer (CTL/CVAL of both channels, CNTVOFF), GIC (every INTID, distributor, CPU interface, active priorities), PL011 (registers, receive FIFO, host input not yet in the FIFO, unread output), PL031, PL061, and for each virtio slot the transport (selectors, negotiated features, status, interrupts, generation, last error), the queues (size, ready, addresses, `last_avail`, `used_idx`, `signalled_used`, EVENT_IDX and INDIRECT) and the device |
| `RAM ` | all the RAM, zero pages omitted |

Devices: **virtio-blk** the pending request (descriptor chain) and the
backend; **virtio-net** link, negotiated MRG_RXBUF, frames waiting for a
buffer, and the `vetro-net` stack (guest MAC, outgoing frames, every TCP
connection with state, sequences, windows, congestion control, data in
flight, RTO and timers; UDP flows; indices; counters; event log; the
sinkhole with everything it recorded and the fake names assigned) plus the
current instant; **virtio-gpu** resources (pixels, backing, scanout),
scanouts (requested resolution, resource, rectangle, cursor with image),
events; **virtio-input** configuration window, queued events, LEDs,
state; **virtio-vsock** listening ports, connections with credits and
data, backlog, control packets, next port; **virtio-console** the backend.

Randomness: there is no stateful source of randomness. The machine seed
is configuration (it goes into the device tree, i.e. into RAM); TCP ISNs
derive from the network seed (configuration) and from the connection
counter (saved).

### What is not in it, and why it changes nothing
- **The JIT.** Blocks, branch caches, software TLB and "heat" counters
  are not included: the JIT's result is by construction that of the
  interpreter (ADR 0013). On restore, even on top of a machine that
  already has a JIT with translated blocks: every watched page is marked
  written (the blocks are discarded), the TLB invalidation counter
  (`Tlb::flushes`) grows (new epoch, empty software TLB), and the
  interpreter resumes from `Next::Jit`.
- **Caches without observable effects**: the MMU's recent translations
  (valid only as long as the TLB slot does not change: they start empty,
  and the slot generations are not needed), the cached level of the IRQ
  line, the last fault of `VirtMemory` (diagnostics only in user mode),
  the bitmap of pages watched by the JIT.
- **External backends**: display (`MemDisplay`, `WebDisplay`), disk files
  (`FileBackend`), images over HTTP (`HostDisk` and its block cache), the
  relay. The host reconnects them before the restore; their state is not
  guest state. The base of a copy-on-write disk is a link: its size is
  checked.
- **The quantum in progress**: saving happens only between two
  `Machine::run`. Inside a WFI the machine already jumps to the deadline
  before returning, so the boundary is always clean; the only half-way
  case is the WFI interrupted by `Stop::Blocked`, which is state
  (`wfi_pending`).

### The TLB is included, even though "it could be rebuilt empty"
An empty TLB on restore would change the result for a guest that modifies
the tables without TLBI (the walk would see the new table where the
original machine used the old entry), and more generally for every
sequence the architecture leaves to the TLB. It is small (at most 512
entries), so it is saved: `tlb_nello_snapshot` (vetro-mmu) checks it with
a stale entry. With the JIT the TLB sees fewer accesses than with the
interpreter (a difference already allowed by ADR 0013): the JIT
equivalence tests compare everything except the TLB.

### Guarantees
1. Saving does not change the machine, and two saves at the same point
   give the same bytes (no hash tables, no clocks, no pointers; maps are
   `BTreeMap`).
2. A restored machine re-saves exactly the bytes it read.
3. Save at N instructions, restore into a new machine (or a used one),
   continue: same console log, same final instruction count, same RAM and
   same state of every device as the uninterrupted run, with or without
   the JIT before and after.
4. An incompatible snapshot (version, configuration, corrupted file) is
   rejected with an error stating the reason; with the first three the
   machine does not change. An error further on (content inconsistent
   with a correct checksum) leaves the machine to be discarded.

## Verification
- `vetro-machine`, `salva_e_ripristina_in_molti_punti`: bare-metal probe
  with GICv3, timer, IRQ, SVC, WFI and exclusives; 19 cuts and 5 edge
  cases found one instruction at a time (exclusive monitor armed, inside
  the interrupt handler, active interrupt with IRQs masked, after the SVC,
  after the WFI). The probe accumulates the ELR of every interrupt, so
  every extra or missing instruction after the restore shows up (without
  the monitor in the snapshot the test fails). Plus: restore on top of a
  used machine, incompatible snapshots, in-flight virtio-blk request
  (`Blocked`).
- `tests/boot/tests/snapshot.rs` on the M3 guest kernel: boot up to
  shutdown, network (300 KB to the guest, POST, ping, TIME-WAIT), disk
  (md5sum, write with `dd`, copy-on-write), GPU/input/vsock; cuts at
  fixed instruction numbers and a few quanta after a point in the script,
  with a new machine or going back on the same one, interpreter → JIT →
  interpreter. Log, instructions, CPU, RAM and device state equal to the
  uncut run; network log, sinkhole, scanout and cursor as seen by the
  host equal.
- `crates/vetro-cli/tests/boot_snapshot.rs`: `--save-at` and `--restore`
  in two processes; the continuation matches the original boot.
- `vetro-wasm`, `snapshot_dall_api`: the C API (ABI 4).

## Consequences
- Whoever adds a state field to a device, to the network stack or to the
  CPU also adds it to its `save`/`restore` and increments
  `FORMAT_VERSION`. The equivalence tests find it only if the field
  affects the future in their scenarios: the "re-saves the same bytes"
  check does not (a field forgotten in both passes).
- `vetro-net` keeps the serialization in child files (`stack/snapshot.rs`,
  `tcp/snapshot.rs`, `sinkhole/snapshot.rs`) so as not to get tangled
  with whoever works on the stack.
- Measurements at the guest kernel shell (RAM 1 GiB, release, macOS on
  Apple silicon): 10.1 MiB snapshot (30.6 MiB of non-zero pages), save
  206 ms, restore 172 ms (`docs/progress/M6.md`).
- Second part of M6: snapshot of booted Android and its cache in the
  browser (OPFS), persistent copy-on-write layer across sessions, APK
  drag and drop.
