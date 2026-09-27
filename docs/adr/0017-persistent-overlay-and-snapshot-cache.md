# ADR 0017 — Persistent copy-on-write overlay and snapshot cache in the browser

- Status: accepted (M6, second part, 2026-09-26). Extends ADR 0014
  (disks from the browser) and ADR 0015 (snapshot).

## Context
Disk images arrive via HTTP Range and are read-only; the guest's writes
ended up in an in-memory `CowBackend` and were lost at every page reload.
M6 asks for them to persist from one session to the next, and for the
machine to resume from a snapshot from the second boot on instead of
booting the kernel again. Two problems:

1. **Where and how to keep the writes.** In the browser there is OPFS
   (`FileSystemSyncAccessHandle` in the Worker, synchronous writes at an
   offset); the CLI has files. The same format must work for both
   (`vetro boot --disk=base.img --overlay=FILE`).
2. **Consistency between snapshot and disk.** The snapshot contains the
   RAM, and with it the page cache and the state of the guest's
   filesystem: it is valid only with the disk as it was at the moment of
   the save. If the session continues and the guest writes more, restoring
   the previous snapshot on top of the new disk corrupts the filesystem;
   restoring it with the old disk loses the writes.

## Decision

### The overlay file (`vetro_snapshot::overlay`)
- 4 KiB header (magic `VETROCOW`, version, 4 KiB clusters, disk size,
  **generation**, number of slots, **base identity**, checksum), then
  slots of 16 + 4096 bytes: cluster index (or free), check (`hash64` of
  the data bound to the index), data.
- **One cluster, one slot, rewritten in place.** No log to compact: the
  file is as large as the live clusters. A removed cluster (after a
  restore) frees its slot, which the next new cluster reuses.
- **Write order**: first the slots, then (after a flush) the header with
  generation and number of slots. An interruption before the header
  leaves the new slots out of the count; a half-written slot has the
  wrong check and is ignored (that cluster reverts to the base's); a
  corrupted header discards the overlay.
- **Base identity**: a host string, compared exactly, plus the disk size.
  Browser: URL, size and `ETag` (or `Last-Modified`), the same key as the
  block cache; file chosen by the user: name, size, modification date.
  CLI: file name, size, modification date in ns. An overlay of another
  base **is discarded** (with a message) and the file is rewritten from
  scratch: applied to another image it would be a corrupted filesystem.
  No content hash: reading GiBs of image at every boot is not feasible.
- The module does no I/O. `Overlay::load` reads the whole file (the
  clusters are in memory anyway in the `CowBackend`) and
  `Overlay::update`/`sync` return the writes to perform (`Patches`:
  optional truncation, then offset/bytes pairs), which the JS applies with
  `FileSystemSyncAccessHandle` and the CLI with `write_at`. So the format
  and slot allocation are in one place only, and two identical sessions
  produce byte-for-byte identical files.

### Who knows what changed
- `CowBackend` (vetro-platform) keeps the set of clusters written by the
  guest since the last `take_dirty`, and provides `cluster`, `clusters`,
  `load_cluster` (the clusters of a loaded overlay do not count as
  writes). It is host bookkeeping: it is not included in snapshots, and
  `restore_state` clears it.
- After a restore the clusters in memory are those of the snapshot:
  whoever persists does a **full comparison** (`Overlay::sync`: rewrites
  the differing clusters, removes those no longer there, leaves the equal
  ones). If snapshot and file match (the normal case, see below) it
  writes nothing.
- Persistence happens **between one quantum and the next**, outside
  `Machine::device`: the guest sees nothing, and the timing does not
  change the execution.

### vetro-wasm, ABI 6
`vetro_overlay_open` (file content and identity; codes `LOADED`, `NEW`,
`MISMATCH`, `CORRUPT`, `NO_DISK`), `vetro_overlay_take` / `_ptr` / `_clear`
(the encoded writes), `vetro_overlay_info` (generation, clusters, slots,
corrupted slots, file length). `vetro_snapshot_restore` marks the open
overlays for the full comparison. Details in `docs/specs/wasm.md`.

### Snapshot cache in the browser
- **Key**: SHA-256 of the snapshot format version
  (`vetro_snapshot_version`), SHA-256 of kernel and initramfs, command
  line, RAM, resolution, devices, and for each disk the base identity,
  size, read-only flag. JIT, real time and cache block size are not
  included (they do not change the guest state). Two files in OPFS
  (`vetro-snapshots/<key>.snap` and `.json`); the metadata is written
  after the bytes and serves as the mark of a complete snapshot.
- **Snapshot and overlay are saved together**: first the overlays (all
  the guest's writes into the file), then the snapshot, with the
  **generation** of each disk's overlay at that moment in the metadata.
- **On restore** the snapshot is valid only if every overlay is still at
  the saved generation. If the disk moved on after the snapshot (session
  writes after the last save), the snapshot is left alone and the machine
  **boots from scratch with the overlay**: writes are never lost, at most
  the fast resume is lost, and at the next idle a new snapshot is saved.
  The snapshot also contains the clusters (ADR 0015), so even without a
  persistent overlay it is self-consistent.
- **When to save**: the first time the guest is **idle** (boot
  finished), then when idle if the overlay generation has changed since
  the snapshot, and on request ("Save state" button). Idle =
  `Stop::Idle`, or 1.5 s of guest time with no console output, no scanout
  changes, no input and no disk activity: the kernel always has a timer
  running, so `Idle` alone never arrives at the prompt. There is no save
  when the page closes (the Worker dies without notice).
- On restore the page shows again the console tail saved in the metadata
  (64 KiB), without answering the terminal's requests again (`ESC[6n`):
  the saved session had already answered.

## Consequences
- Guest writes persist across sessions, in the browser and in the CLI,
  with the same format; changing the base image discards the overlay.
- From the second boot the M3 guest kernel is ready in 0.69 s from page
  open (restore 263 ms, 11.7 MiB snapshot) versus 2.2 s from scratch; in
  Node/V8 save 117 ms and restore 148–353 ms of a 10.1 MiB snapshot at
  the prompt (`docs/progress/M6.md`).
- Overlays live entirely in memory (in the `CowBackend`) and the file is
  read whole on open: for Android (hundreds of MiB written) this will have
  to be measured; the format allows reading clusters on demand without
  changing it.
- Idle is heuristic: a guest that keeps writing to the console (or
  Android with the home animation) does not save by itself until it calms
  down; there is the button. For Android the "boot finished" signal (for
  example `sys.boot_completed` via adb) will be decided when it is there.
- Persistence is not synchronous with the guest's FLUSH: a write already
  acknowledged to the guest is lost if the page closes before the next
  save (at most 1 s in the browser, one quantum in the CLI). An
  interruption halfway through an in-place rewrite makes that cluster
  revert to the base's (wrong check). Tying the FLUSH acknowledgement to
  the save would mean stopping the guest (`Blocked`) at every FLUSH: to
  be evaluated with Android, by measuring.
