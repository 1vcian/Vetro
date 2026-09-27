# ADR 0031 — Prebuilt Android snapshot, smaller snapshots, Android in nightly CI

- Status: accepted (M5/M6, 2026-09-27). Builds on ADR 0015 (snapshots),
  0017 (snapshot cache), 0025 (CI), 0028 (AOSP in the browser). Details:
  `docs/specs/snapshot.md` (format 4), `docs/specs/wasm.md` (ABI 13, "The
  prebuilt Android snapshot"), `docs/progress/M6.md` (all measurements).

## Context
In the app a first-time visitor waited about 45 minutes for Vetro's AOSP
image to reach the home screen (ADR 0028); only from the second start does it
resume from the snapshot saved in OPFS (5 s). ADR 0028 left two things for
later: a snapshot at the home screen made once and downloaded by everyone, and
the long Android tests in CI (they needed `VETRO_ANDROID=1` and a local image).
A prebuilt snapshot is only usable with the exact snapshot format, machine
configuration and image it was made with, and it is downloaded by every new
visitor: every MiB counts.

## Measurements (Node 22 / V8, Apple silicon Mac shared with other agents)
Snapshot at the home screen, image bd09e2f, 2 GiB, the app's machine
(1280x800, touchscreen, network, vsock):
- **Format 3 (per-page LZ)**: 1297 MiB (1 360 098 296 bytes), saved in 24 s;
  the cold boot to the home screen took 44.5 min of wall time (home at
  1450 s of guest time). zstd -3 of the file: 1047 MiB, gzip -6: 1043 MiB
  (-20%).
- **What is in it**: RAM with 1925 MiB of non-zero pages (the page cache
  fills the guest), 214 MiB of them identical to another page; the disk
  copy-on-write layer 368 MiB, **incompressible** (userdata is encrypted by
  the guest: gzip gains 0.6% on it).
- **RAM encodings** (the same 1925 MiB): per-page LZ (format 3) 925 MiB;
  with repeated pages as references 848 MiB; **LZ77 + Huffman over frames of
  64 pages 573 MiB**; for reference gzip -6 601 MiB, zstd -3 579 MiB, zstd -9
  537 MiB, zstd -3 --long=30 487 MiB. Natively, on compacted RAM (1476 MiB):
  fast level 7.1 s to write and 2.9 s to read, small level 24 s and 3.6 s.
- **In-guest compaction** (`sync; drop_caches; dd` of zeros over free memory
  in a tmpfs file, then removed; `ANDROID_COMPACT`): non-zero RAM from 1925 to
  1476 MiB (Cached 648 MiB after: the rest of the page cache is in use);
  RAM at the small level 377 MiB. Whole snapshot with format 3: 1020 MiB
  (-21%).
- **Result**: the prebuilt snapshot (small level, compacted) is about
  750 MiB instead of 1297 (final numbers in `docs/progress/M6.md`).

## Decision

### Snapshot format 4: repeated blocks and a small level
`vetro_snapshot::blocks` replaces the per-page loop of `compress` and of the
RAM section (same framing: length, count of non-zero blocks, entries):
- encoding 2: a block with the same bytes as an earlier one is a reference
  (found by `hash64`, confirmed byte by byte, so deterministic);
- encoding 3: a **frame** of up to 64 blocks compressed together with
  `vetro_snapshot::lzh`, our LZ77 (hash chains over the frame, one step of lazy
  matching) with canonical Huffman codes limited to 12 bits (literal/length
  and distance alphabets with bucketed values), no dependencies;
- `Level::Fast` (encodings 0, 1, 2) for the snapshots the app saves while the
  guest waits (18-24 s already, and saving compresses twice, ADR 0028);
  `Level::Small` (0, 2, 3) for snapshots that are downloaded: several times
  slower to write, a third smaller, about 20% slower to read. The reader
  accepts both. `Machine::save_with`/`save_stream_with(level, ...)`,
  `Writer::set_level`; vetro-wasm ABI 13 `vetro_snapshot_set_level`.
- A restored machine keeps saving at the fast level: the app's own snapshots
  do not get slower.

### In-guest compaction for the prebuilt snapshot only
The generator runs `ANDROID_COMPACT` over adb after the home screen is drawn,
then saves. It costs minutes once, when the snapshot is made, and -40% for
every visitor. After restoring, the guest reads again from the disk (HTTP
Range) what it dropped: inactive page cache from the boot; what is in use
stays. Not for the app's own snapshots (root via adb, lmkd pressure, minutes
of guest time for every save).

### One key for local and prebuilt snapshots
`androidKeyParts` (web/node/android.mjs): snapshot format and machine
configuration hash of the running vetro-wasm (ABI 13
`vetro_snapshot_config_hash`, after the disk is added), RAM, screen, devices,
the image version and the sha256 of its boot images, the bootloader
parameters, and the disk map's sha256 and size. **Not the URL** the image is
served from (format 3 keys had it): the same image from R2 or a local server
gives the same key, so a snapshot made with a local copy is valid for R2.
The Worker computes the key; the app's own snapshots use it too.

### Published by key next to the image
`aosp/<version>/snapshots/<key>.snap` and `<key>.json` (info: key and parts,
size, sha256, SHA-256 of every 16 MiB chunk, the metadata the app keeps with
a snapshot: console tail, boot phases). The app looks for exactly its key:
**a snapshot for another vetro-wasm cannot even be found** (404 = cold boot),
and several can live side by side. The key does not include a hash of the
vetro-wasm binary: that would invalidate the snapshot at every build even
when nothing that a snapshot contains changed; the snapshot format version and
the configuration hash are the contract (every new saved field bumps the
format, ADR 0015).

### Download in the app
With no snapshot of its own for the key and "cold boot" not chosen, the Worker
fetches the info file and downloads the snapshot straight into the snapshot
cache (`SnapshotStore.downloadTarget`): each 16 MiB chunk is checked against
its SHA-256 before it is written, progress is kept in `<key>.part.json`, the
metadata file is written last (only then does the cache see the snapshot).
Network errors are retried with an HTTP Range from the first unverified chunk;
a download that still fails stops the start with a message (reloading the page
resumes it) rather than silently falling back to a 45-minute boot. Then the
normal chunked restore. The page shows a progress bar (MiB, rate, time left),
the expected size before starting (the site's hint, below), and a "cold boot"
box (`&cold=1`) for advanced users.

### Tooling and the site guard
- `tools/aosp/prebuilt-snapshot.mjs`: the app's machine and Worker logic in
  Node (boot phases, adb, screen on, launcher focused and drawn, +5 s),
  compaction, small level; writes `<key>.snap` and `<key>.json`.
  `--restore` resumes from one of its snapshots (experiments).
- `tools/aosp/upload-snapshot.sh`: checks size and sha256, uploads the
  snapshot then the info file; the same key with other bytes is an error
  unless `VETRO_REPLACE=1`.
- `tools/aosp/prebuilt-key.mjs`: the key of a vetro-wasm build without booting,
  and whether R2 has that snapshot. `tools/pages/build.sh` runs it on the
  site's vetro-wasm and writes `app/android-prebuilt.json` (size, key) only if
  the snapshot is there (a warning otherwise, an error with
  `VETRO_REQUIRE_PREBUILT=1`); `tests/web/pages.mjs` recomputes the key from
  the site's vetro-wasm and checks the hint and the object on R2. So the site
  never announces a snapshot its vetro-wasm cannot restore; and even a stale
  hint would only change the text shown before starting.

### CI
- **Nightly** (`.github/workflows/nightly.yml`, job `android`, x86_64 ubuntu
  runner with Chrome): vetro-wasm built from the commit, the test APK from the
  runner's Android SDK, `prebuilt-key.mjs --require`, then
  `VETRO_ANDROID_PREBUILT=1 tests/web/android-chrome.mjs` against R2: the
  prebuilt snapshot found, downloaded, verified and restored in headless
  Chrome, adb devices and shell, the APK dropped through the page installed
  and opened, a real click that changes its colour, the "app installed"
  snapshot saved; then a second start from OPFS. This is the nightly form of
  the M5/M6 exit criteria. A vetro-wasm change that bumps the format or the
  configuration turns it red until the snapshot is regenerated: that is the
  intended signal.
- **Weekly** (Sundays) and by hand (`workflow_dispatch`, `cold`): the cold
  boot in Chrome (`android-cold`, 350 min limit), then the second start. On
  the reference Mac the boot takes 44.5 min in Chrome; the hosted runner's
  time was not measurable from this branch (no push): the first runs record
  it in the job summary.
- Regenerating and uploading the snapshot stays a manual step on a machine
  with the R2 credentials (not in CI: no write credentials in the repository).

## Rejected
- Transport compression only (gzip through `DecompressionStream`, or
  `Content-Encoding`): -20%, and the stored snapshot stays 1.3 GiB; the small
  level gives -27% by itself and applies to the file itself.
- Porting zstd: much more code for about 10% more on RAM (zstd -3 579 MiB vs
  573 MiB for ours; only its long-range mode is clearly better).
- Transcoding the prebuilt snapshot to the fast level after restoring it: an
  18-24 s pause right after the first start, for 20% faster later restores.
- Discard (TRIM) of freed disk clusters to shrink the copy-on-write layer:
  needs virtio-blk discard support (another area); worth measuring later,
  since that layer is now half of the snapshot.

## Consequences
- A first-time visitor downloads about 0.75 GiB (about 30 s at 25 MB/s)
  instead of waiting 45 minutes; the app's own snapshots get about 8% smaller
  (repeated pages) at the same speed.
- Format 4 invalidates every format 3 snapshot, local ones included: one
  prebuilt download (or cold boot) per user.
- Every change of snapshot format or machine configuration needs a new
  prebuilt snapshot (about an hour on the Mac, then the upload), or the site
  cold boots Android and the nightly job is red.
