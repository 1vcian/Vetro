# ADR 0019 — Machine record & replay: one entry point, one log, jumping to an instruction

- Status: accepted (M10, core, 2026-09-26). Extends ADR 0011
  (`Machine::run`), ADR 0014 (stopped time on disks) and ADR 0015
  (snapshot).

## Context
M10 asks for "identical replay with return to the exact moment of a call".
The machine is already deterministic: time is the number of instructions
(ADR 0011), a disk waiting for data stops the guest's time (ADR 0014), the
randomness offered to the guest comes from the configuration seed, the RTC
starts from a time fixed in the configuration, the network stack and the
sinkhole live inside the machine and use virtual time. From a snapshot we
resume instruction by instruction as if the machine had never stopped
(ADR 0015).

What remains are the **host inputs**, which arrive between one quantum and
the next at moments that depend on the host: console bytes, keyboard and
pointer events, power button (GPIO), display resizing, host operations on
vsock, host connections to the guest (port forwarding, merged into main in
parallel) and, new, network frames delivered by the host. Until now they
went through closures (`Machine::device`, `Machine::net`,
`Board::gpio_input`) that cannot be recorded.

## Decision

### A single entry point: `Machine::input(Input) -> Reply`
- `Input` lists everything the host can do to the guest: `Console(byte)`,
  `Keyboard(events)`, `Pointer(events)`, `Gpio { line, level }`,
  `Display { scanout, width, height }`, `NetFrame(frame)`, `NetLink(bool)`,
  `Vsock(VsockOp)` (listen, unlisten, accept, connect, send, recv,
  shutdown, close, reset, release, transport reset), `HostNet(HostNetOp)`
  (connect, send, recv, shutdown, abort, release: the `Stack::host_*`
  methods). The host's **reads** (`recv` on vsock and on the network) are
  inputs too: they free credit or window, and the guest sees it.
- `Reply` carries the outcome (connection opened, bytes accepted or read,
  vsock errors, `NoDevice`).
- `console_input` and the new `Machine::gpio_input` go through `input`;
  `vetro-cli` (`--hostfwd`) and `vetro-wasm` (keys, pointer, display,
  GPIO, `vetro_net_*`) use `input` instead of the closures. The helpers
  `Input::key_events`, `move_abs_events`, `touch_events` give the same
  events as the `VirtioInput` methods (proven by comparing the saved
  state).
- **Host network frames** (`Input::NetFrame`): the machine's network link
  (`NetLink`) has a queue of host frames that virtio-net delivers before
  the stack's. It is guest state: it goes into the snapshot, and
  `FORMAT_VERSION` goes from 2 to 3.
- The closures remain (`device`, `gpu`, `keyboard`, `pointer`, `vsock`,
  `net`) but during a recording every use of them becomes an **opaque
  event** in the log: the replay stops there (`Divergence::Opaque`). For
  what is not an input there are dedicated accessors that are not
  recorded: `device_view`, `gpu_view`, `vsock_view`, `net_view`
  (read-only) and `host_link` (data for a disk the machine is waiting for:
  ADR 0014, time is stopped and in replay the disk gives the same data).

### What is not recorded, and why
- **Asynchronous disk completions**: with ADR 0014 the machine does not
  execute instructions until the data arrives, and the request completes
  at the same instruction number as with an always-ready disk. The log
  does not need to know when they arrived; only the same content is
  needed (the disk is a link, as in snapshots).
- **Host time and randomness**: there are none. RTC and seed are
  configuration (in the log, and in the configuration hash).
- **Outputs** (console, scanout image, network log): they are
  recomputed; the console is part of the checks.

### Inputs with the machine stopped on a disk
An input that arrives while `run` returns `Stop::Blocked` is **deferred**
(`Reply::Deferred`) to the end of the first quantum after the unblock, and
recorded there. Applying it immediately would put it at a point that does
not exist in replay (with a ready disk): inside the WFI interrupted by the
block, or between the service that found the disk not ready and the one
that completes it. This applies only during a recording: without one, the
behaviour stays that of ADR 0014.

### The log
A `vetro-snapshot` container (same header as snapshots, with magic
`"VETROREC"` and its own version `LOG_VERSION` = 1; new
`encode_container`/`decode_container`), sections:
- `HEAD`: snapshot version of the keyframes, `MachineConfig` (RAM, time,
  seed), JIT used, keyframe interval, starting digest;
- `EVTS`: per event the instruction number, a CPU hash (the snapshot's
  `Cpu` state) and the bytes output by the console up to there, then the
  input (or the opaque event);
- `KEYF`: periodic snapshots (instruction, console count, snapshot);
- `END `: final digest.

The **digest** (`Digest`) is: instructions, hash of the CPU, of the MMU
with the TLB, of the platform (all devices with their internal backends),
of the RAM, and bytes and hash (incremental FNV-1a) of the console from the
start. The TLB is not compared if recording or replay used the JIT (ADR
0013). Console output is counted when the machine takes it out of the
UART; so as not to depend on when the host reads, events use the output
bytes **including** output still in the UART, and the digest first moves
the output into the machine's buffer (which `console_output` returns).

### Replay
- `start_replay(&log)` from the starting state (same kernel loaded or
  same snapshot restored: configuration and digest are checked), or
  `replay_from(&log, n)` from the last keyframe not beyond `n`.
- `run(budget)` in replay cuts the quantum at the instruction of the next
  event: there it compares the CPU hash and console bytes, applies the
  input and continues. The JIT receives the end of the quantum as its
  limit (`jit_budget`), so it **never passes an event**: no change to the
  JIT. At the end of the recording it compares the digest:
  `ReplayStatus::Finished` or `Diverged(...)`. After the end (or a
  difference) the machine continues freely. Host inputs during the replay
  are ignored (`Reply::Ignored`).
- It relies on a property already used by ADRs 0014 and 0015 and now
  proven here too: **quantum boundaries do not change the execution**. An
  input applied between two quanta at instruction N gives the same result
  whatever the host's quantum.
- An event the machine passes without stopping at it, or a machine that
  stops by itself (powered off, idle) before an event, is
  `Divergence::Missed`. Idle on the instruction of the next event is fine:
  the recording had seen the same `Stop::Idle` and given the input there.

### Jumping to an instruction
`goto(&log, n)`: `replay_from` from the nearest keyframe and replay up to
the first boundary with at least `n` instructions (a WFI can jump beyond,
as for `--save-at`). From there `cpu`, `read_phys`, `read_virt`
(translation with the current tables without the TLB, RAM only: no effects
on devices), `translate`, `registers_text`. Keyframes are taken at the end
of quanta, never with the machine stopped on a disk and never after an
event at the same instant (the events of an instant come after its
keyframe); the first at the start, so the log alone is enough to resume.

### CLI
`vetro boot --record=FILE [--keyframes=N]`, `--replay=FILE` (from boot
with `--kernel`, from `--restore`, or from the initial keyframe),
`--goto=N` with `--dump=VA:BYTE`. In replay stdin is not read and
`--hostfwd` is rejected (the host network comes from the log). Exit code 0
if the replay is identical, 1 if it diverges.

## Rejected alternatives
- **Recording at the backend frontier** (frames virtio-net receives,
  upstream responses): more general for a future relay, but the device
  state in replay would not be the recorded one (the stack would not run),
  and the state comparison would fail. For the M7 relay the right frontier
  will remain `vetro-net`'s `Upstream` (responses to record in order,
  because calls happen in virtual time); it is not needed today, the
  machine has only the sinkhole.
- **Injecting inside `run`** at the exact instruction even mid-quantum:
  not needed, because cutting the quantum at the event's instruction gives
  the same point.
- **Incremental keyframes** (only changed pages): less space and less
  time, but it requires tracking pages written by JIT blocks as well.
  Postponed: for now a keyframe is a full snapshot.
- **Hash of the whole machine at every event**: 1 GiB of RAM to scan at
  every keystroke. The CPU hash and the console count are enough to stop
  the replay at the first event after a missed input (proven), and the
  full digest at the end covers the rest.

## Verification
- `vetro-machine`, `record::tests`: log round trip with every input type,
  corrupted logs or logs of another version rejected, virtio-input events
  equal to the device's methods.
- `vetro-machine`, `machine::record::tests` (bare-metal probe with UART
  echo, timer, IRQ, SVC, WFI): replay with quanta of 1, 7919 and 2^40
  instructions and from a keyframe on an empty machine; `goto` to nine
  points (also backwards) with registers and RAM of the recorded run; a
  byte given to the UART without `input` stops the replay at the first
  event after (it fails without the check); an opaque access stops the
  replay; logs of another machine or of another starting state rejected;
  input with the disk waiting deferred and identical replay with a ready
  disk.
- `tests/boot/tests/replay.rs` (guest kernel, release): session with keys
  typed one at a time, DHCP/HTTP/ping to the sinkhole, an ICMP frame from
  the host (the guest answers: `InEchos` from 0 to 1), virtio-input
  keyboard, 20 KB echo from a host connection (`nc -e cat`), `sleep`,
  shutdown. Recording does not change the execution (same session without
  recording: same log and state). Replay from boot with the interpreter at
  different quanta and from the initial keyframe with the JIT: same log,
  instructions, CPU, RAM, devices. `goto` to three points with interpreter
  and JIT: same registers and RAM. Log without a key or without the ICMP
  frame: the replay diverges at the first event after.
- `crates/vetro-cli/tests/boot_replay.rs`: `--record` from stdin,
  `--replay` in other processes (from the keyframe, from boot, with the
  JIT) with the same output and the identical-replay message;
  `--goto`/`--dump` equal to `Machine::goto` in the test process; a log
  with one input fewer gives exit code 1.

## Consequences
- Whoever adds a host input adds it to `Input` (with its encoding in the
  log and `LOG_VERSION` + 1), not as a closure: otherwise recordings that
  use it stop at the opaque event.
- The browser (`vetro-wasm`) already goes through `input`; recording and
  replaying in the browser (exporting the log, keyframes in OPFS) is work
  for the web part.
- Measured costs (`docs/progress/M10.md`): recording without keyframes
  costs one CPU hash per input and two digests (start and end); keyframes
  cost one save each (~10 MiB at the guest kernel shell).
