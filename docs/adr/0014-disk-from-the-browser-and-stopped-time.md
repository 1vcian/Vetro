# ADR 0014 — Disks with data from the browser: `Stop::Blocked` and stopped guest time

- Status: accepted (M5, 2026-09-25). Extends ADR 0011 (`Machine::run`).

## Context
In the browser the disk images (AOSP: GiB) do not fit in memory and cannot
be read synchronously: they arrive via `fetch` and HTTP Range, in
pieces, or from a `File` chosen by the user (`Blob.slice`, asynchronous), and
are kept in an OPFS cache for restarts. The machine, instead, is synchronous:
`Machine::run` executes instructions and services the virtio devices in between.

`VirtioBlk` already knows how to leave a request pending when the backend
answers `BlockError::NotReady` and retry it at the next service. But if
the guest keeps running in the meantime, the moment the request
completes (and therefore the interrupt, and everything that follows) depends on the
network: the same boot would give different instructions and logs on every run,
against the principle of determinism (and the replay of M10).

## Decision
- **The machine stops.** After every virtio service `Board` checks whether a
  `VirtioBlk` has a pending request (`has_pending`). If so,
  `Machine::run` returns the new `Stop::Blocked` before executing
  another instruction, and keeps returning it (without executing anything) on
  every call as long as the request stays pending. The instruction
  counter, i.e. guest time, does not advance.
- **The host delivers and calls `run` again.** The data reach the backend via
  `Machine::device`, which marks the device to be serviced: at the next
  `run` the service repeats the request from scratch (it is idempotent) at the
  same instruction count and completes it. From then on execution is
  identical to one with an always-ready disk.
- **WFI.** If the pending request originates in the service done inside a
  WFI, the WFI resumes at the next `run` (`wfi_pending`), so the
  jump to the next timer deadline also stays the same.
- **Whoever observes the machine does not touch it.** Host reads that are
  not inputs (scanout image, counters, requested blocks) do not
  go through `Machine::device` and do not trigger device servicing; likewise, a
  delivery of blocks made while the machine is not stopped (read-ahead)
  does not mark the device. Only real inputs (keys,
  pointer, console, GPIO, data for an awaited disk) change when the
  guest sees something.
- **Quanta with fixed boundaries.** Whoever compares runs (the tests) treats
  `Blocked` as transparent: it serves the blocks and continues the same quantum
  up to its boundary, then looks at the console and gives the inputs. This way the
  inputs arrive at the same instruction counts with a local disk and
  with one over HTTP.
- **In vetro-wasm** (`docs/specs/wasm.md`): `HostDisk` (aligned blocks
  of `block_size` bytes, list of requested blocks, cache with optional limit)
  under a `CowBackend` (guest writes in memory; M6 will make them
  persistent); stop code 5 `BLOCKED`, `vetro_disk_wanted`,
  `vetro_disk_fill`, `vetro_disk_fail`. The JS (`web/node/disk.mjs`) looks for
  the blocks in the cache (OPFS with `FileSystemSyncAccessHandle` in the Worker),
  then in the source, merging contiguous blocks into one request.
- Blocks are requested by polling (export
  `vetro_disk_wanted` after `BLOCKED`) and not by an import called from Rust:
  `run` stops anyway, the JS already knows when to ask, and the code is
  tested the same way on the native target.

## Consequences
- Same instructions and same log with a local disk, over HTTP with an empty
  cache, from the cache, with 4 KiB or 64 KiB blocks, with and without
  read-ahead, with the JIT and with the interpreter, and equal to the native reference
  with a local disk (`tests/web/boot-disk.mjs` and `tests/boot/tests/web.rs`,
  182.9 million instructions in all cases).
- The real time spent waiting for the network does not exist for the guest: no
  I/O timeouts in the guest because of a slow network (a block that never arrives
  is declared failed with `vetro_disk_fail`, and the guest receives IOERR).
- The check after every virtio service walks the 32 slots (downcast): negligible
  cost compared to the service itself.
- `vetro-cli` (disks from files, always ready) never sees `Blocked`.
