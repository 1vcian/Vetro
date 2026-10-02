# ADR 0044 — Disk prefetch list and parallel block fetches

- Status: accepted (M5/M6, 2026-10-02). Builds on ADR 0014 (disks over
  HTTP Range), 0017 (OPFS block cache), 0028 (the AOSP disk map), 0031
  (prebuilt snapshot). Measurements: `docs/progress/M5.md` (2026-10-02,
  "The live site's path").

## Context
A visitor of the live site restores the prebuilt home-screen snapshot; from
then on every disk block the guest touches comes from R2 with HTTP Range
(1 MiB blocks, one block of read-ahead) while guest time stops (`Blocked`).
The prebuilt's in-guest compaction drops the inactive page cache (ADR 0031),
so a restored guest reads again much of what the boot had read: on the
owner's machine 132 requests and 31 s of disk wait in a short session; on
the build VM (next to R2) 118 requests and 8.5 s in the first two minutes,
153 requests and 11 s over a 9-minute path, and a second visit still read
115 blocks (8.4 s) it had never needed before. The `DiskFeeder` fetched the
missing runs of one guest request one after the other.

## Decision
1. **Prefetch list** next to the prebuilt snapshot:
   `aosp/<version>/snapshots/<key>.blocks.json`, `{ format: 'vetro-prefetch',
   version: 1, key, disk, blockSize, blocks }` (`disk` = SHA-256 of the disk
   map's text, as in the snapshot key; `blocks` in the order a restored guest
   first needs them). Made from the disk traces of real sessions on the user's
   path (`tools/aosp/live-path.mjs --trace`: restore, home screen, app drawer,
   Settings, a catalog app installed and opened) by
   `tools/aosp/prefetch-list.mjs` (union, each block at its earliest
   position), published with `tools/aosp/upload-prefetch.sh` (replaceable:
   it only orders downloads the app makes anyway).
2. **The Worker fetches it after every restore** of an Android snapshot
   (prebuilt or the user's own: same key) and gives it to
   `DiskFeeder.prefetch`: batches of 8 list entries, contiguous blocks
   merged up to 4 MiB, 2 requests in flight, none started while the guest
   waits for a block; the blocks go **into the OPFS block cache only**, not
   into the machine (its memory does not grow, and what the guest sees, and
   when, is unchanged: replay stays exact). Blocks already cached are
   skipped, so later visits cost one small JSON request. A block the guest
   asks for while it is on its way is awaited, not fetched twice. No list
   (404), another disk or block size, or no OPFS: nothing happens.
3. **Parallel demand fetches**: the runs of one guest request go out
   together, up to 6 in flight.
4. **A disk trace in the stats** (`stats.diskTrace`: per disk, the blocks
   fetched from the network in order, at most 8192) and the JIT host's
   compile times (`stats.jitHost`), read by the tools from
   `window.vetroState.stats`; no change to the page.

## Rejected
- Downloading the whole disk in the background: 1.5 GB for a few hundred
  MiB actually read.
- Prefetching into the machine (`diskFill`): the module's memory would grow
  by the list's size; the OPFS cache serves a request in milliseconds.
- Larger blocks or more read-ahead for every request: Android's reads are
  scattered over `super.img`; more bytes per miss on a slow link.
- Keeping the page cache in the prebuilt snapshot (no compaction): -40% of
  snapshot size for every visitor (ADR 0031); the list gets most of the gain
  for a fraction of the bytes, after the home screen is already usable.

## Consequences
- A new prebuilt snapshot needs its own list (the key changes): the snapshot
  tools say so; without one the app works as before.
- The list is made on the build VM with the app in headless Chrome against
  R2, then published; see `docs/specs/wasm.md`, "Prebuilt snapshot".
