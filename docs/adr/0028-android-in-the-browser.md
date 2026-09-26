# ADR 0028 — Vetro's AOSP image in the browser: memory, disk, adb, snapshots

- Status: accepted (M5 and M6, 2026-09-26). Builds on ADR 0014 and 0017
  (disks from the browser), 0015 (snapshots), 0018 (Android bootloader),
  0022 (AOSP image), 0024 and 0026 (JIT). Details: `docs/specs/wasm.md`
  ("The AOSP image in the app"), `docs/specs/net.md` (adb).

## Context
Vetro's AOSP 15 image reaches the home screen under `vetro boot --jit`
(3 GiB, one CPU). M5's exit criterion needs it in the browser, with disks
fetched from R2, screen and input, and adb from the page; M6 needs the second
start to resume from a snapshot. Four questions: how much RAM the guest can
have inside wasm32 (4 GiB of linear memory in total); how the disks get there
(the published images are Android sparse files, the GPT disk is 15 GiB of
mostly zeros); how the page talks to adbd without sockets; where and when the
state is saved.

## Measurements (Node 22 / V8 and Chrome headless, Apple silicon, shared machine)
- **QEMU, 3 GiB, at sys.boot_completed + 2 min** (adb, `/proc/meminfo`):
  MemFree 242 MiB, Cached 1972 MiB, AnonPages 423 MiB, Slab 157 MiB. The
  guest "uses" almost all of its RAM, but as page cache: anonymous memory is
  under half a GiB.
- **vetro-wasm with 3 GiB** (`tests/web/android.mjs --ram=3072`): at first it
  did not even start: on wasm32 a `Vec<u8>` cannot exceed `isize::MAX`
  (2 GiB - 1), "capacity overflow". With the RAM as a region taken with
  `memory.grow` (below) it starts: sys.boot_completed at 788.6 s of guest
  time in 1516 s of wall time (25 min, JIT in V8), linear memory 3298 MiB at
  30 s, 3632 MiB at zygote, 3887 MiB at the end of the boot (RAM 3072 +
  copy-on-write of the disks 330 MiB + disk blocks 64 MiB + JIT and the
  rest); process RSS 0.4–1.1 GiB (pages never touched take no memory). The
  **snapshot fails**: saving the copy-on-write layer needs a buffer beyond
  4 GiB (`handle_alloc_error` in `vetro_snapshot::compress`).
- **vetro-wasm with 2 GiB**: sys.boot_completed at 759.7 s of guest time in
  1492 s; linear memory 2274 MiB at 40 s, 2862 MiB at the end of the boot
  (RAM 2048 + copy-on-write 330 + blocks 64 + JIT and the rest). A snapshot
  with `Machine::save` **fails here too**: the file `Vec` grows by doubling
  (and a reallocation needs old + new). With the chunked save (below) it
  works: 1376 MiB in 36 s (including the disk write), linear memory peaking
  at 3209 MiB. The home screen (launcher in the foreground) comes at 1372 s of
  guest time.
- **Chrome** (headless, same Mac, image bd09e2f, 2 GiB): boot finished at
  769–781 s of guest time in 1466–1485 s (about 25 min), home drawn at
  1401 s of guest time (2667 s, 44.5 min); snapshot of 1313–1320 MiB,
  17–19 s to save plus 1–2 s to write to OPFS, linear memory peaking at
  2998 MiB. **Second start from the snapshot: machine ready 5.0 s after the
  page opened** (OPFS read 381 ms, restore 4.5 s), first frame at 5.1 s.
- **Snapshot size at the home screen** (Node): 1326 MiB (RAM compressed with
  our LZ plus 357 MiB of copy-on-write); zstd -3 brings it to 1070 MiB,
  gzip -6 to 1096 MiB (the snapshot is already compressed). Dropping the page
  cache and filling free memory with zeros in the guest before saving
  (`--compact` in `tests/web/android.mjs`, via adb): 1014 MiB (-24%), at the
  cost of about 2 minutes.
- **JIT code**: after about 30 min V8 killed the process ("Exceeding maximum
  wasm committed code space", 4 GiB of compiled code): the JS engine never
  freed modules (below).
- memory64 was not tried: the JIT emits modules with 32-bit memory
  (`vetro-jit`, ADR 0012/0024) and the software TLB points into linear
  memory; switching means another JIT backend and explicit bounds checks
  (no guard pages), a cost the measurements above do not justify.

## Decision

### RAM: 2 GiB in the browser, a region beyond 2 GiB is possible
- In the app the guest RAM with AOSP is **2048 MiB** (`ANDROID_RAM_MIB`,
  `?ram=` overrides it). The guest fits comfortably (under half a GiB of
  anonymous memory at the end of the boot with 3 GiB) and almost 2 GiB of
  linear memory stay free for the copy-on-write layer (330 MiB after the
  first boot), the block cache, the JIT and above all the snapshot.
  `androidboot.ddr_size=3072MB` stays in the image's bootconfig: it only
  sizes a few properties.
- `vetro_machine::board::Ram` becomes a contiguous block that, on wasm32
  beyond `isize::MAX`, is a **region taken with `memory.grow` outside the
  allocator**, read and written only in small pieces (hash, comparisons,
  page-by-page snapshots in the same format as `vetro_snapshot::compress`).
  Contiguous as before: the JIT's software TLB (`SysPhys::ram_region`) does
  not change. The region of a destroyed machine is reused (zeroed). It serves
  whoever wants 3 GiB without snapshots, and made the measurement possible.
- **Chunked snapshots**: `Machine::save_stream` (vetro-wasm
  `vetro_snapshot_save_stream`, import `vetro_host.snapshot_write`) hands the
  file to JS in 1 MiB chunks, which the Worker writes to OPFS as they come;
  only the part before the RAM stays in memory (devices and copy-on-write,
  with the buffer sized in advance). The RAM is compressed twice: the length
  of the content enters the header hash before everything else, and the file
  format does not change (ADR 0015). Restoring is chunked too
  (`Machine::load_state_stream`, `vetro_snapshot_restore_stream`, import
  `vetro_host.snapshot_read`): only the part before the RAM goes into the
  module's memory, and the checksum is verified at the end (a damaged file is
  detected with the machine already changed, to be discarded). With a buffer
  as large as the snapshot, memory stayed at 3780 MiB and fragmented, and the
  save after installing an APK failed; chunked, it stays at 2847 MiB and the
  save works. In OPFS the new file replaces the old one only when the save
  succeeded. `Machine::save` writes the header in place.
- **JIT code limit in V8** (`web/node/jit-engine.mjs`): at most 96 MiB of
  modules between two resets, then `compile` refuses and vetro-jit resets
  the engine (the same path as a full table); on reset the engine clears the
  `__indirect_function_table` entries handed to Rust, which kept the
  dispatcher, its table and therefore every block alive. No change in
  vetro-jit.
- Rejected: "RAM with pages allocated on demand" (pages never touched already
  take no physical memory in V8: the limit is the address space, and the
  guest's page cache fills it anyway; and the JIT wants contiguous RAM),
  memory64 (above).

### Disks: a disk map over the sparse files already published
- `tools/aosp/web-disk.mjs` writes `web/disk.json` (size and extents
  `[disk offset, length, file, file offset]`, with fills and holes) and
  `web/disk-head.bin` (GPT and metadata, 364 KiB): super and userdata point
  into the sparse `super.img` and `userdata.img`, chunk by chunk (222 and
  22 chunks, 160 extents in total). Verified byte for byte against
  `target/aosp/disk.img` (15.07 GiB). Published with
  `tools/aosp/upload-web.sh` next to the version (`aosp/<version>/web/`):
  the version's files do not change, no 15 GiB disk on R2.
- `LayoutSource` (web/node/disk.mjs) reads the bytes with HTTP Range from the
  map's files; the key (OPFS block cache, snapshots) is the map URL + the
  SHA-256 of its text. In the Worker: 1 MiB blocks, 64 in memory, the rest in
  the OPFS cache.
- The boot images (boot, vendor_boot, init_boot: 136 MiB, mostly zeros) are
  downloaded only for a cold boot, verified with the manifest's sha256 and
  kept in OPFS; the snapshot key uses the manifest's sha256 values, so a
  restore downloads nothing.

### Booting from boot.img in vetro-wasm (ABI 12)
`vetro_load_android` passes the images to the bootloader in
`vetro_machine::android` (ADR 0018), with the same parameters as
`tools/aosp/vetro.sh` (`nokaslr`). Tested on an image built by `mkbootimg.py`
around the M3 kernel with 3 GiB: same instructions and log as the native
reference.

### adb: a JS client over GuestSocket
`web/node/adb.mjs` speaks the adb protocol over the TCP connection to port
5555 of the guest (port forwarding, ABI 5): CNXN, AUTH (WebCrypto RSA key,
ready for a user build), `shell,v2,raw:`, `sync:` for push, install = push
+ `pm install -r`, devices. In the Worker it connects after
sys.boot_completed; the page's requests are inputs (in `inputLog` and in the
timeline). Dropped APK: `web/node/apk.mjs` reads the binary manifest
(package, main activity), then install and `am start -W -n`. A WebSocket
bridge to a host adb was rejected: it needs a process outside the browser.

### Bootloader: vendor_boot lines can be replaced (amends ADR 0018)
An `androidboot.*` parameter with the same key as a line of the vendor_boot
bootconfig section replaces that line in place instead of repeating it (the
kernel would discard the whole block, and the boot with it). This lets a boot
property be tried without rebuilding the image; the app's default
parameters stay `nokaslr` (`ANDROID_PARAMS`), like `tools/aosp/vetro.sh`.

### Boot phases
From the console (`web/node/android.mjs`): kernel, init first and second
stage, zygote, surfaceflinger, system_server (the first init message with
"(system_server)"), boot finished (`sys-boot-completed-set`, init's event on
`sys.boot_completed=1`: the "processing action (sys.boot_completed=1…)"
line does not show up in kmsg). Then the **home screen**, which is not in the
console: after sys.boot_completed FallbackHome ("Phone is starting") stays
for a long time (600 s of guest time with 2 GiB). The Worker asks adb for
`dumpsys window | grep -m1 mCurrentFocus` every 5 s of guest time until it is
the launcher (`mResumedActivity` from `dumpsys activity` is gone in
Android 15), then waits until the scanout actually shows it (at least 40
distinct colours on a 16-pixel grid, or 300 s of guest time): with the
launcher focused, FallbackHome can stay on screen for tens of seconds of
guest time. Once connected, the Worker keeps the screen on
(`svc power stayon true`).

### Android snapshots
- Saved 5 s of guest time after the home screen is drawn (or 3000 s of guest
  time after sys.boot_completed if it never is), after installing an APK and
  with the button; the console idle heuristic (ADR 0017) does not apply to
  Android.
- **The snapshot is the unit of persistence**: it already contains the disk
  copy-on-write layer (ADR 0015), so with Android there is no separate
  overlay in OPFS (330 MiB more to write at every save and to read at every
  start). The next session resumes from the last snapshot; writes made after
  it are lost; without a snapshot the first boot runs again.
- Pre-built snapshot on R2: see "Consequences".

## Consequences
- AOSP's home screen reaches the app in Chrome in 45 minutes on the first
  boot, and in 5 seconds from the snapshot on the second (M6's target:
  under 15 s). Times and sizes also in `docs/progress/M5.md` and `M6.md`.
- A 3 GiB guest in the browser boots but cannot be saved: the real limit is
  linear memory, not the Mac's RAM.
- A pre-built snapshot downloaded from R2 at the first start would avoid the
  45-minute first boot, but it must come from the same snapshot format
  version, the same machine configuration and the same vetro-wasm as the one
  downloading it (a new Vetro version invalidates it), and it weighs as much
  as a local snapshot: 1.3 GiB, 1.05 GiB with zstd (measurements above;
  about 0.8 GiB with in-guest compaction), i.e. 40–60 s at 25 MB/s instead of
  45 minutes. Worth it, but only as one artifact per image/vetro-wasm pair
  published by CI: to do when the site publishes pinned vetro-wasm versions.
- In-guest compaction before the snapshot (-24%) is not on by default: it
  needs root via adb, puts lmkd under pressure for a few seconds and costs
  two minutes; to revisit together with the pre-built snapshot, where every
  MiB counts for everyone.
- Colours: the blue test app reaches the scanout with red and blue swapped
  (the launcher does not): an app buffer presented as XRGB8888 while it is
  written as RGBA. `display_framebuffer_format=bgra` (with the bootloader
  replacing the line) changes nothing; to be fixed in the image (composer or
  gralloc, ADR 0022). Tests accept both orders (`colorSeen`) and report which
  one they saw. The home screen also keeps a stale "Phone is starting" area
  in the guest's framebuffer (seen in the scanout read directly in Node too).
- `androidboot.ddr_size` and the dalvik properties stay the 3 GiB ones: if
  lmkd turned out too aggressive with 2 GiB, a browser bootconfig in the image
  (ADR 0022), or a replaced line through the bootloader, is the way.
