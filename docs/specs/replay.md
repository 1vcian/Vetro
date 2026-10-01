# Machine record & replay (M10)

Decisions and rationale in ADR 0019. Here: the interface and the format. Code:
`crates/vetro-machine/src/record.rs` (types and log format) and
`crates/vetro-machine/src/machine/record.rs` (recording, replay,
jumping, state reading).

## Host inputs

`Machine::input(Input) -> Reply` is the only point through which the host
changes what the guest sees. The input reaches the guest before the next
instruction.

| `Input` | Effect | `Reply` |
|---|---|---|
| `Console(Vec<u8>)` | bytes into the PL011 input queue | `Done` |
| `Keyboard(Vec<InputEvent>)`, `Pointer(Vec<InputEvent>)` | `VirtioInput::inject` (the caller adds the SYN_REPORTs; helpers `Input::key_events`, `move_abs_events`, `touch_events`) | `Done` / `NoDevice` |
| `Gpio { line, level }` | PL061 input line (3 = power button) | `Done` |
| `Display { scanout, width, height }` | `VirtioGpu::set_display` | `Done` / `NoDevice` |
| `NetFrame(Vec<u8>)` | Ethernet frame for the guest, delivered before those of the stack | `Done` / `NoDevice` |
| `NetLink(bool)` | `VirtioNet::set_link_up` | `Done` / `NoDevice` |
| `Vsock(VsockOp)` | `Listen(p)`, `Unlisten(p)`, `Accept(p)`, `Connect(p)`, `Send(c, bytes)`, `Recv(c, max)`, `ShutdownSend(c)`, `Close(c)`, `Reset(c)`, `Release(c)`, `TransportReset` | `Vsock(Result)`, `Conn(Option)`, `Data`, `Done` |
| `HostNet(HostNetOp)` | `Connect(port)`, `Send(id, bytes)`, `Recv(id, max)`, `Shutdown(id)`, `Abort(id)`, `Release(id)` on the stack (`Stack::host_*`), with a forced `poll` | `HostConn(Option<ConnId>)`, `Accepted(n)`, `Data`, `Done` |

In addition: `Reply::Deferred` (recording in progress and machine stalled on
a disk: the input is applied and recorded at the end of the first quantum after
the unblock) and `Reply::Ignored` (replay in progress).

`console_input(&[u8])` and `gpio_input(line, level)` are shortcuts for
`input`.

Accesses that are **not** inputs and are not recorded: `device_view`,
`gpu_view`, `vsock_view`, `net_view` (read-only), `host_link` (data for
a disk the machine is waiting on, ADR 0014), `console_output`.
`device`, `gpu`, `keyboard`, `pointer`, `vsock`, `net` (mutable closures)
remain, but during a recording every use is an opaque event.

## Recording

| Method | |
|---|---|
| `start_recording(RecordOptions { keyframe_every })` | from here, between two `run`s; with `keyframe_every > 0` the first keyframe immediately |
| `is_recording()`, `recorded_events()` | |
| `stop_recording() -> Option<Log>` | with the fingerprint of the current state |

At the end of every recorded quantum: a keyframe if at least
`keyframe_every` instructions have passed since the previous one (not with the
machine stalled on a disk, not if there is already an event at the same instant),
then the deferred inputs.

## Replay and jumping

| Method | |
|---|---|
| `start_replay(&Log) -> Result<(), Divergence>` | from the current state, which must be the starting one (configuration and fingerprint) |
| `replay_from(&Log, n)` | from the last keyframe not beyond `n` (without keyframes: like `start_replay`) |
| `goto(&Log, n) -> Result<u64, Divergence>` | `replay_from` + replay up to the first boundary with at least `n` instructions; returns the instructions reached |
| `replay_status() -> Option<&ReplayStatus>` | `Running { next }`, `Finished`, `Diverged(Divergence)` |
| `digest() -> Digest` | fingerprint of the state (moves the UART output into the machine's buffer) |

During replay `run(budget)` stops at every event (quanta cut short,
JIT included) and at the end of the recording; there it compares the fingerprint
(without TLB if recording or replay used the JIT). After `Finished` or
`Diverged` the machine runs free.

`Divergence`: `Start(reason)`, `Event { index, step, what }` (registers or
console bytes differing before an input), `Missed { index, step, at
}`, `Opaque { index, step, slot }`, `End { what }` (instructions, console,
CPU, RAM, devices, MMU and TLB). `Display` in Italian.

State reading: `read_phys(pa, buf) -> bool` (RAM),
`translate(va) -> Option<u64>` and `read_virt(va, buf) -> Result<(), u64>`
(current tables at the current EL, without TLB, RAM only), `registers_text()`
(instructions, PC, SP, NZCV, EL, DAIF, X0–X30, SP_EL0/1, ELR, SPSR, ESR, FAR,
VBAR, SCTLR, TCR, TTBR0/1, TPIDR_EL0).

## Log format (version 1)

```
"VETROREC"  u32 LOG_VERSION  u64 configuration hash
u64 content length  u64 hash64 of the content
content: sections HEAD, EVTS, KEYF, END  (in this order)
```

(`vetro_snapshot::encode_container` / `decode_container`.)

| Section | Fields |
|---|---|
| `HEAD` | u32 snapshot version of the keyframes; u64 RAM, u64 time, u64 seed; bool JIT; u64 keyframe interval; starting fingerprint; with more than one core u32 cores (ADR 0042: absent = 1, so single-core logs keep their bytes) |
| `EVTS` | seq of events: u64 instruction, u64 CPU hash, u64 console bytes; u8 0 = input (encoding below), 1 = opaque (opt u32 slot) |
| `KEYF` | seq of keyframes: u64 instruction, u64 console bytes and u64 console hash, bytes snapshot (`Machine::save`) |
| `END ` | final fingerprint |

Fingerprint: 7 × u64 (instructions, CPU, MMU, platform, RAM, console
bytes, console hash). With several cores the CPU hash covers every core and
the round robin (the `SMP ` section of the snapshot), and the instruction
count is the clock (every core's instructions). Recording and replay need the
cores in turns: a machine whose cores run in parallel refuses to record
(`Machine::start_parallel` refuses during a recording or replay, and the app
stops the parallel cores first). Hash: `hash64` of `Cpu::save`, `Mmu::save`,
`Virt::save`, of the RAM bytes; console 64-bit FNV-1a.

Inputs: u8 type, then 0 `Console` bytes; 1 `Keyboard`, 2 `Pointer` seq of
(u16 type, u16 code, u32 value); 3 `Gpio` u32, bool; 4 `Display` 3 × u32;
5 `NetFrame` bytes; 6 `NetLink` bool; 7 `Vsock` u8 operation (0 listen, 1
unlisten, 2 accept, 3 connect: u32 port; 4 send: conn, bytes; 5 recv:
conn, u64; 6 shutdown_send, 7 close, 8 reset, 9 release: conn; 10
transport reset), conn = u32 host port, u32 guest port; 8
`HostNet` u8 operation (0 connect: u16; 1 send: u64 id, bytes; 2 recv: u64
id, u64; 3 shutdown, 4 abort, 5 release: u64 id).

`Log::decode` checks magic, version, checksum, sections, events in order
(non-decreasing, within the start–end interval) and increasing
keyframes.

## CLI

`vetro boot ... --record=FILE [--keyframes=N]` (default 100 million
instructions, 0 = none), `--replay=FILE` (config from the log; start from
`--kernel`, `--restore` or from the initial keyframe), `--goto=N` and
`--dump=VA:BYTES`. Codes: 0 identical replay, 1 different, 2 usage or
file error.

## In the browser (ADR 0023)

vetro-wasm (ABI 8, `docs/specs/wasm.md`): `vetro_record_start/stop`,
`vetro_rr_status`, `vetro_log_encode/load/info/events`, keyframes movable
out of the machine (`vetro_log_keyframe`, `_take`, `_put`, `_for`),
`vetro_replay_start(step)` from the nearest keyframe (the jump to
an instruction: then `vetro_run` with the quantum limited up to there),
`vetro_registers_text`, `vetro_read_virt`, `vetro_translate`,
`vetro_read_phys`. In JS `Recording` (`web/node/recording.mjs`) keeps the
keyframes in a `SnapshotStore` (OPFS `vetro-recordings/` in the Worker: `kf-<i>`
and `log` without the keyframe bytes) and reassembles the complete file for
download. The app: "Recording" panel (record, replay, download,
load, go to instruction, continue, registers, memory dump) and "go here"
on the timeline inputs.

## Tests

`vetro-machine` `record::tests` and `machine::record::tests`;
`tests/boot/tests/replay.rs` (guest kernel, `VETRO_REQUIRE_GUEST_KERNEL=1`,
release); `crates/vetro-cli/tests/boot_replay.rs`; in the browser
`cargo test -p vetro-wasm` (`replay::tests`), `tests/web/replay.mjs` and
`tests/web/browser-analysis.mjs`.
