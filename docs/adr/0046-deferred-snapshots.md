# ADR 0046 — Deferred snapshots: copy on the machine's thread, compress in a saver Worker

- Status: accepted (M6, 2026-10-02). Builds on ADR 0015 (snapshot format),
  0017 (snapshot cache in OPFS), 0028 (chunked saves), 0031 (format 4, fast
  and small levels), 0039 (saves wait for a pause in the inputs), 0042
  (parallel cores). Details: `docs/specs/snapshot.md` ("Deferred saves"),
  `docs/specs/wasm.md` (ABI 16). Measurements: `docs/progress/M6.md`
  (2026-10-02, "Deferred snapshots").

## Context
The live-path profile (ADR 0045's tooling) showed 46% of the machine Worker's
time in `vetro_snapshot::lz::compress` while the first app opens after an
install: the automatic "app installed" snapshot (`saveSnapshot`) runs
between two slices and holds the machine's thread for the whole save, 20-36
s with Android (ADR 0031), because `save_stream` compresses the whole RAM
twice (its length enters the checksum before the content) and the disk's
copy-on-write layer (hundreds of MiB, incompressible) once. ADR 0039 already
made automatic saves wait for SAVE_QUIET_MS without inputs, but the catalog's
Open is an adb request, not an input, so the save starts while the app is
being opened. The owner's rule: automatic saves must never slow the guest.

## Decision

### Copy now, compress elsewhere, same bytes
- **Deferred writer** (`vetro_snapshot::Writer::deferred`): the same `save`
  code of every device runs into it; `compress` copies the data as it is and
  notes a mark, `section` notes where its length goes. The marks (`Plan`) and
  the bytes (the *raw stream*) are what the machine's thread produces: a copy,
  no compression.
- **Assembler** (`vetro_snapshot::deferred::Assembler`), anywhere: turns plan
  and raw stream (pieces of any size) into the file, encoding blocks as
  `blocks::encode` at the fast level does, one at a time; section lengths and
  block counts are written when known (positioned writes); a repeated block
  is confirmed byte by byte against the first block with its hash **read back
  from the file** (that block may have changed in the RAM since); at the end
  the content is read back once for the checksum and the header goes at 0.
  The result is byte for byte the file `Machine::save` gives at the instant the
  save started: the format does not change (still version 4), restores and
  prebuilt snapshots are untouched, a restored machine is the same as today.
- **The RAM is trapped copy-on-write** (`Machine::save_deferred`,
  `Ram::trap_arm`): one bit per page set at the start; the first write to a
  page with its bit set (interpreter, devices' DMA, the JIT's slow path,
  compare-and-exchange) copies its old bytes aside before writing; the save
  takes each page in order, kept or read now, and clears its bit. The JIT's
  software TLB gets no write entry for a page whose bit is set
  (`SysPhys::write_trapped`) and its existing write entries are forgotten at
  the start (`flush_writes`): direct stores by translated code cannot bypass
  the trap. Under a lock, a bit is cleared only after the copy, so with cores
  in parallel a writer that saw it set waits for the page to be safe. Refused
  while cores run in parallel (their JITs may hold write entries): the app
  stops them for the start, as before every save, and starts them again at
  once (new JITs, empty TLBs). A restore, or another deferred save, voids the
  one in progress (its pump fails, the saver drops its file).
- **Only the fast level**: the small level (frames of `lzh`, downloaded
  snapshots made by tools) stays synchronous.

### In the app
- `saveSnapshot` starts a `BackgroundSave` (web/node/background-save.mjs):
  the start captures devices, disk copy-on-write and CPU, and arms the trap;
  the guest goes on in the same slice. Between slices one piece of 4 MiB of
  the raw stream (`Machine.snapshotPump`) is copied out and **transferred** to
  the **saver Worker** (web/app/save-worker.mjs: its own instance of the same
  compiled module, our LZ, `SnapshotAssembler` over an OPFS sync access
  handle, `SnapshotStore.saveTarget`); at most 16 MiB are unacknowledged;
  while the guest is idle (`Idle`) the window is filled at once. Pieces are
  handed over only after SAVE_QUIET_MS without user inputs (the existing
  deferral rule), unless the guest has written more than KEPT_MAX = 256 MiB of
  pages the save still has to take (each written page is kept until then).
  The snapshot replaces the previous one only when complete; another save
  waits for it; a new machine cancels it. Without a Worker the save is
  synchronous as before.
- **Why not CompressionStream**: it gives gzip/deflate, a new block encoding
  for format 5, and an asynchronous decoder for a restore that is synchronous
  (`snapshotRestoreStream` reads chunks inside a wasm call). Our LZ in the
  saver Worker keeps the format and is fast enough off the machine's thread.
  Workers have no OS thread priority in Chrome; "low priority" is the pacing
  above (nothing handed over while the user is acting) plus a separate thread.
- **Not incremental**: see "Incremental saves" below.

## Measurements
`tools/aosp/live-path.mjs --cpu-profile-actions 30` on the build VM (headless
Chrome, the app served locally, image f08b79e from R2, the light prebuilt;
first visit then second visit), main (d96814b) against this branch, one run
each, one after the other:

| | main | this branch |
|---|---|---|
| "app installed" save on the machine's thread | 18.7 s (16.4 s compression, 2.4 s writes) | **1.1 s** (start 346 ms, copies 758 ms spread over the slices) |
| the same save in the saver Worker | - | 12.8 s wall |
| Worker profile, first 30 s of opening Minesweeper | 52% in `lz::compress`, `literals`, `blocks::encode`; guest 3.0 s | **1.3%** (`snapshotPump` 1.1%); guest **7.8 s** |
| Minesweeper's screen after Open | 45.7 s | 34.6 s |
| pages kept by the trap at most | - | 29 MiB |
| snapshot size | 747 MiB | 747 MiB (same format) |
| second start: ready, Minesweeper on screen | 5.3 s, 15.0 s | 7.0 s, 13.8 s |

The save started 3 s after the install, while the app was opening (the case
of the finding). The app drawer and Settings profiles are unchanged (0.3-0.4%
in functions matching snapshot code, the same in both builds). The second visit
restores the background-saved snapshot and finds the app installed. Tap
timings on this VM vary by several seconds between runs (home taps from 0.1 to
13 s in both builds), so one run says nothing about them beyond "not worse".
The remaining cost on the machine's thread is the start (346 ms: copying
devices and the disk's copy-on-write layer, about 90 MiB) and 4 MiB copies,
one per slice (about 1 ms each).

## Incremental saves
Measured, not done. Between the prebuilt snapshot a visitor restores and the
"app installed" one, 77 972 of 390 217 non-zero RAM pages differ (304 of 1524
MiB, 20%); a RAM section of only those pages would be 101 MiB at the fast
level instead of 656 MiB, and the head (devices, disk copy-on-write) is 90
MiB either way. With the compression off the machine's thread this would save
the saver's time and OPFS writes, not guest time, and it needs a format with a
base reference (the restore reads two files, the base must stay in OPFS, a
dirty bitmap kept across restores). Not a clear win for the goal of this
ADR; the write trap above is the dirty tracking it would use, if OPFS writes
or the saver's CPU become the problem.

## Verification
- `vetro-snapshot` `deferred` tests: deferred writer + assembler give the
  normal file for nested, empty and external sections, with pieces from 1 byte
  to the whole stream, repeats whose source was flushed long before.
- `vetro-machine` `deferred_save_is_the_file_of_its_instant`: while the guest
  runs and "devices" write behind, ahead of and across the save's position
  (also a compare-and-exchange and a write over two pages), the assembled file
  equals the `save` of its instant, and the machine ends identical to a twin
  without the save; red without the trap. `deferred_save_voided_by_restore`.
- `tests/boot/tests/snapshot.rs` (guest kernel, 1 GiB): `Cut::Deferred` during
  boot (interpreter; and with the JIT, after a rewind) and while the guest
  writes the disk with the JIT: each file equals the `save` of its instant
  and the runs equal the uncut one (console, instructions, CPU, RAM, devices).
- `vetro-wasm` `deferred_snapshot_from_the_api` (ABI 16), `tests/web/snapshot.mjs`
  case G (V8, two instances, `BackgroundSave` and `SaveJob`).

## With accelerated graphics (2026-11-16)
The gfxstream default (ADR 0047) was merged while deferred saves declined
on that path: the part before the RAM also holds the GPU's contents
(colour buffers, surfaces, texture levels, buffer data, read back from the
executor at save time), and a deferred writer keeps large data
uncompressed. The first Android runs on hosted runners showed what the
foreground save costs there: 12.7–12.9 s with the machine stopped for the
713 MiB snapshot after an install, and 33–41% of the Worker's time in
`vetro_snapshot::lz` while the app just installed is opening.

Decision: deferred saves on that path too, with the memory accounted for.
- `Renderer3d::save_bytes_hint` (default 0; gfxstream: the bytes its
  readbacks fill, from the same list the save uses) says how much large
  data the renderer writes; `VirtioGpu::save_bytes_hint` passes it on.
- `vetro_snapshot_bg_begin` sizes the writer with it (no doubling while it
  grows) and, before starting, asks the allocator for the two copies there
  will be for a moment (the readback buffer and the writer), **fallibly**:
  if they do not fit (wasm32 has 4 GiB) the deferred save is refused with a
  message and the app saves in the foreground, compressing as it goes, as
  before. A refusal must never be an abort: the machine is the user's
  session.
- Nothing else changes: the marks of `compress` in a deferred writer are
  the same for the GPU's data as for any other, so the assembled file is
  the file a foreground save writes (`vetro-gfxstream`
  `deferred_save_is_the_normal_save_and_the_hint_is_its_readbacks`,
  `vetro-wasm` `deferred_snapshot_with_gpu_contents_or_refused`).
- On the machine's thread a save now costs the readbacks and one copy of
  the GPU's contents at the start, then the page copies between slices.

Measured on Android: see `docs/progress/M6.md` (2026-11-16).

## Consequences
- ABI 16; `SysPhys::write_trapped` (default false) in the JIT's interface.
- `Renderer3d::save_bytes_hint` in the renderer's interface (2026-11-16).
- Memory during a save: the head (devices and disk copy-on-write, as large as
  today's head buffer; with accelerated graphics also the GPU's contents,
  uncompressed, and a second copy of them while they are read back), the
  pages written before the save took them (at most
  KEPT_MAX before pieces are forced), 16 MiB in flight; the saver Worker's own
  instance (a few tens of MiB).
- Whoever adds a direct write path to guest RAM (outside `Ram`'s methods and
  the JIT's software TLB) must respect the trap.

## Update (2026-11-17): automatic saves start with the guest at rest
An automatic save (home screen, app installed) also waits until the Worker
ran the machine for less than half of the last second (`autoSaveDue`,
`SAVE_BUSY_MAX` in `web/node/persist.mjs`; still at most SAVE_DEFER_MAX_MS
after the request). The inputs' pause alone let the "app installed" save
start while the app it had just opened was still starting: the trap then
copied every page the app wrote (`Ram::trap_take` and `Ram::read` about 13%
of the Worker in that window on the Mac). Minesweeper on screen after its
install, Mac, one run each: 12.4 s before, 6.5 s after
(`docs/progress/M6.md`).
