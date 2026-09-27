// The app's Worker: the vetro-wasm machine runs here, in quanta, off the
// page's thread. It receives the configuration and the inputs (keyboard,
// pointer, touch, console, power button) as messages, and sends the page
// the console output, the changed rectangles of the scanout (transferred
// ArrayBuffers, no copies), the cursor and the statistics.
//
// Inputs reach the machine between one quantum and the next, that is at a
// precise instruction count: the `inputLog` register (instruction, event)
// is what the M10 replay will play back.
//
// Time: the guest counts instructions (10 ns each). With `realtime` the
// Worker does not let guest time run ahead of the real clock by more than
// AHEAD_MS: quanta end there (a WFI stops at the end of its quantum instead
// of jumping to its timer) and the Worker waits for the clock, woken at once
// by an input; without it, it goes at full speed. While a disk waits for
// data (`Blocked`) guest time is stopped (ADR 0014).
//
// File manager (M8, ADR 0020): with `files` the machine has virtio-vsock and
// the Worker holds the `GuestFiles` client of the guest's `vetro-files`
// daemon; the page's requests (`files` messages) are inputs like the others
// (recorded in `inputLog`), the client advances between one slice and the
// next, and replies, inotify events and status go back to the page
// (`files-reply`, `files-event`, `files-status`). After a snapshot restore
// the client is new: the connections left in the snapshot are closed.
//
// Network inspector and timeline (M7, ADR 0023): with the network the
// capture is on from boot; every ~0.7 s, if something changed, the Worker
// sends the page the list of requests and the timeline (`analysis`);
// detail, HAR and pcapng on request (`inspect`). User inputs are annotated
// by vetro-wasm; here the file manager commands are annotated (not the
// panel's reads) and, as effects, the changed files seen by the watches.
//
// Record & replay (M10, ADR 0019 and 0023): the page's commands (`rr`) run
// between one slice and the next. At the end of a recording (or after
// loading a log) the keyframes go to OPFS (`vetro-recordings/`,
// `Recording` of web/node/recording.mjs) and the log stays there for the
// next session. A replay (also a jump to an instruction) restarts from the
// nearest keyframe: during the replay the page's inputs are discarded, the
// file manager is closed, no real time and no cached snapshots; at the
// requested point the machine stops (`paused`: registers and memory are
// read with `inspect`), at the end of the replay the verdict
// (`replay-ended`: `Finished` = identical replay, `Diverged`) and the
// machine runs free again.
//
// Persistence (M6, ADR 0017):
// - the guest's writes to the disks go to the copy-on-write overlay, which
//   is saved in OPFS (`vetro-overlays/`) between one slice and the next (at
//   most once per second, and always when the guest stops waiting) and is
//   reapplied in the next session; an overlay of another base image is
//   discarded;
// - the machine snapshot is saved in OPFS (`vetro-snapshots/`) the first
//   time the guest is at rest (boot finished), and again when it is at rest
//   and the disks have changed since then, or on request from the page.
//   At rest: `Idle`, or REST_NS of guest time without console output,
//   without scanout changes, without inputs and without disk activity (the
//   kernel always has a timer, so `Idle` alone is not enough). The key
//   includes the format version, the hashes of kernel and initramfs, the
//   command line, the machine configuration and the identity of the disks;
//   the metadata hold the overlay generation of each disk at the time of
//   the save. In the next session the snapshot is restored instead of
//   booting the kernel, if the overlays are still at that generation.
//
// Vetro's AOSP image (M5/M6, ADR 0028), with `config.android`:
// - the version's manifest.json (R2 or a local server) gives the image hashes
//   (snapshot key) and their URLs; boot, vendor_boot and init_boot are
//   downloaded (checked with the sha256, kept in OPFS, `vetro-images/`) only
//   for a cold boot; the disk is the `web/disk.json` map (LayoutSource: super
//   and userdata from the sparse files with HTTP Range), with the block cache
//   in OPFS;
// - the boot phases (BootProgress) go to the page (`progress`);
// - after sys.boot_completed the ADB client (web/node/adb.mjs) connects to
//   adbd (TCP 5555 in the guest, GuestSocket), keeps the screen on, watches
//   for the home screen (launcher focused and drawn on the scanout) and
//   serves the page's requests (`adb`: shell, devices, install of a dropped
//   APK or of an app from the catalog, with push progress, and `open` of an
//   installed package, ADR 0033); the requests are inputs (recorded in
//   `inputLog`);
// - the snapshot is saved ANDROID_HOME_NS of guest time after the home screen
//   is drawn, after an APK install and on request; no console idle heuristic
//   (Android always writes). The snapshot is the unit of persistence: it
//   also holds the guest's disk writes (the copy-on-write layer), so there is
//   no separate overlay (hundreds of MiB more to write and read back). The
//   next session resumes from the last snapshot: writes made after it are
//   lost, like going back to the last saved state; without a snapshot the
//   first boot runs again. Snapshots go to and come from OPFS in chunks.

import { DEV, INOTIFY, INPUT, instantiate, Machine, TIMELINE_EFFECT, TIMELINE_INPUT } from '../node/vetro.mjs';
import { Recording } from '../node/recording.mjs';
import { BlobSource, DiskFeeder, LayoutSource, MemoryCache, OpfsCache, RangeSource } from '../node/disk.mjs';
import { AdbClient } from '../node/adb.mjs';
import { apkInfo } from '../node/apk.mjs';
import { ANDROID_DISK, ANDROID_HOME_NS, ANDROID_PARAMS, ANDROID_WAKE, ANDROID_GRAPHICS, BootProgress, gridColors, HOME_DRAW_NS, HOME_MIN_COLORS, HOME_POLL_NS, HOME_QUERY, isHome, machineDevices } from '../node/android.mjs';
import { DiskOverlay, fromBase64, opfsFile, sha256Hex, SnapshotStore, snapshotKey, staleReason, toBase64 } from '../node/persist.mjs';
import { androidSnapshotKey, downloadPrebuilt, findPrebuilt, PREBUILT_CHUNK, prebuiltSnapUrl } from '../node/prebuilt.mjs';

/** Largest quantum (instructions). */
const QUANTUM = 1_000_000;
/**
 * Wall time a quantum should take: the page's messages (inputs) get in only
 * between slices, and a slice ends after the quantum that crosses SLICE_MS.
 * With Android in the browser the guest runs at a few tens of MIPS, so a
 * fixed 1M-instruction quantum took 20-200 ms: quanta are sized from the
 * measured speed instead.
 */
const QUANTUM_MS = 3;
const SLICE_MS = 12;
/**
 * Real time: how far (ms) guest time may run ahead of the real clock. Past
 * it the Worker waits for the clock, woken at once by an input, instead of
 * running more quanta: a guest early by seconds would then crawl for as
 * long after an input (one quantum per wait), which the user feels as taps
 * doing nothing.
 */
const AHEAD_MS = 4;
/** Instructions per millisecond of guest time (10 ns each). */
const STEPS_PER_MS = 100_000;
/** Smallest quantum, so a quantum always makes progress. */
const MIN_QUANTUM = 20_000;
/** Measured guest speed (instructions per ms of wall time), for QUANTUM_MS. */
let stepsPerMs = 50_000;
/**
 * Automatic snapshots (after the home screen, after an install) wait until the
 * user has not touched the machine for this long: saving stops the guest for
 * seconds (tens on slow machines), during which a tap would do nothing.
 */
const SAVE_QUIET_MS = 4000;
/** ... but no longer than this after they were requested. */
const SAVE_DEFER_MAX_MS = 60_000;
/** Overlays are saved at most every this many ms while the guest works. */
const PERSIST_MS = 1000;
/** Guest time without activity after which the guest is at rest (1.5 s). */
const REST_NS = 1_500_000_000n;
/** Console tail kept for the snapshot (the page shows it again). */
const CONSOLE_TAIL = 64 * 1024;
/** If the home screen does not come within this long after sys.boot_completed, the snapshot is saved anyway. */
const HOME_GIVE_UP_NS = 3000_000_000_000n;
/** Wait (guest time) before trying to connect to adbd again. */
const ADB_RETRY_NS = 5_000_000_000n;
const EV_SYN = 0;
const EV_REL = 2;
const REL_WHEEL = 8;

let m = null;
let exports = null;
let feeder = null;
let cfg = null;
let running = false;
const inbox = [];
let wake = null;
const inputLog = [];
/** Persistent overlay of each disk (or null). */
let overlays = [];
let store = null;
let snapKey = null;
/** Metadata of the last snapshot saved or restored in this session. */
let lastSnapshot = null;
let saveRequested = false;
/** When (performance.now()) the pending snapshot was requested. */
let saveRequestedAt = 0;
/** Last user input reaching the machine (performance.now()). */
let lastUserInputAt = -Infinity;
/** Why the requested snapshot is saved (for the page). */
let saveWhy = 'requested';
/** Android state (config.android), or null with the test kernel. */
let android = null;
let startT0 = 0;
let consoleTail = [];
let consoleTailLen = 0;
/** File manager client (GuestFiles) and last status sent to the page. */
let files = null;
let filesKey = '';
/** Folder watched for each wd (for the timeline's file effects). */
const watchPaths = new Map();
/** 'live', 'replay' (replays the log), 'paused' (stopped at the requested point). */
let mode = 'live';
/** Instruction to stop at during the replay (BigInt) or null. */
let target = null;
/** Recording and replay commands, run between one slice and the next. */
const control = [];
let recording = null;
/** Attribution window of the timeline (µs, 0 = vetro-analysis's default). */
let windowUs = 0;
let lastAnalysis = -1n;
let lastAnalysisAt = 0;
let ignoredNotice = false;
/** Real-time reference (reset when guest time jumps). */
const clock = { t0: 0, g0: 0n, paused: 0 };

/** Page inputs: how long they waited in the Worker before reaching the machine (ms). */
const inputWait = { count: 0, lastMs: 0, maxMs: 0, sumMs: 0 };
/** Absolute time (ms since the epoch), comparable with the page's. */
const absNow = () => performance.timeOrigin + performance.now();

const post = (msg, transfer = []) => postMessage(msg, transfer);

/** Waits up to `ms` (Infinity: no limit), or until a message wakes the loop (`wake`). */
function rest(ms) {
  return new Promise((ok) => {
    const timer = Number.isFinite(ms) ? setTimeout(ok, ms) : null;
    wake = () => {
      if (timer !== null) clearTimeout(timer);
      ok();
    };
  }).finally(() => (wake = null));
}

// A task boundary without setTimeout's clamp (at least 4 ms once timeouts
// nest, a quarter of every 12 ms slice): the page's messages get in, and the
// machine goes on at once. A message to ourselves queues behind the page's
// messages; scheduler.yield() is not used: its continuation runs ahead of
// other tasks, so it could keep the page's inputs waiting.
const yieldChannel = new MessageChannel();
const yieldQueue = [];
yieldChannel.port1.onmessage = () => yieldQueue.shift()?.();
const yieldToEvents = () => new Promise((ok) => {
  yieldQueue.push(ok);
  yieldChannel.port2.postMessage(0);
});
const status = (text) => post({ type: 'status', text });

/** The bytes of a chosen file or of a URL. */
async function bytesOf(src, what) {
  if (!src) return null;
  if (src.file) return new Uint8Array(await src.file.arrayBuffer());
  status(`downloading ${what}: ${src.url}`);
  const res = await fetch(src.url);
  if (!res.ok) throw new Error(`${src.url}: status ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

async function openDisk(d, i, sources) {
  const source = sources[i] ?? (d.file ? new BlobSource(d.file) : d.layout ? await new LayoutSource(d.layout).open() : await new RangeSource(d.url).open());
  sources[i] = source;
  let cache = null;
  if (d.url || d.layout) {
    if (cfg.opfs) {
      try {
        cache = await OpfsCache.open(source.key, d.blockSize, Math.ceil(source.size / d.blockSize));
      } catch (e) {
        status(`OPFS not available (${e.message ?? e}): in-memory cache`);
      }
    }
    cache ??= new MemoryCache();
  }
  const index = feeder.add(source, { cache, blockSize: d.blockSize, maxBlocks: d.maxBlocks ?? 0, readOnly: d.readOnly, readahead: d.readahead ?? 1 });
  status(`disk ${i}: ${d.url ?? d.layout ?? d.file.name}, ${(source.size / 2 ** 20).toFixed(1)} MiB, ${d.blockSize >> 10} KiB blocks`);
  overlays[index] = null;
  // With Android the unit of persistence is the snapshot (which also holds
  // the guest's writes): no separate overlay (see the top of the file).
  if (cfg.persist && cfg.opfs && !d.readOnly && !android) {
    try {
      const file = await opfsFile('vetro-overlays', `${(await sha256Hex(source.key)).slice(0, 32)}.cow`);
      const o = DiskOverlay.open(m, index, file, source.key);
      overlays[index] = o;
      const info = o.info;
      if (o.opened.code === 'Mismatch' || o.opened.code === 'Corrupt') status(`disk ${i}: ${o.opened.message}`);
      else if (o.opened.code === 'Loaded') status(`disk ${i}: persistent overlay, ${info.clusters} clusters written in previous sessions`);
    } catch (e) {
      status(`disk ${i}: persistent overlay not available (${e.message ?? e}): writes in memory only`);
    }
  }
  return index;
}

const devicesOf = (c) => machineDevices(DEV, c);

/** New machine with the disks (and their overlays). */
async function build(c, sources) {
  for (const o of overlays) o?.close();
  overlays = [];
  m?.free();
  m = new Machine(exports, { ramSize: BigInt(c.ramMiB) << 20n, devices: devicesOf(c), width: c.width, height: c.height });
  feeder = new DiskFeeder(m);
  for (const [i, d] of (c.disks ?? []).entries()) await openDisk(d, i, sources);
}

/** Saves the changed overlays; returns whether it wrote anything. */
function persistOverlays() {
  let wrote = false;
  for (const o of overlays) if (o?.persist()) wrote = true;
  return wrote;
}

const generations = () => overlays.map((o) => (o ? o.generation : null));

/** Machine snapshot in OPFS, together with the overlays (saved first). */
async function saveSnapshot(why) {
  persistOverlays();
  const meta = {
    steps: String(m.steps),
    generations: generations(),
    console: toBase64(joinTail()),
    savedAt: new Date().toISOString(),
    why,
  };
  if (android) meta.progress = android.progress.events;
  // In chunks, straight to OPFS: with Android it is hundreds of MiB, which
  // whole would not fit in the module's memory (ADR 0028).
  const t0 = performance.now();
  let writeMs = 0;
  const memory = m.memoryBytes;
  post({ type: 'busy', text: 'saving the machine state: the screen answers again when it is done' });
  let size;
  try {
    size = await store.saveStream(snapKey, meta, (write) => m.snapshotSaveTo((b, at) => {
      const tw = performance.now();
      write(b, at);
      writeMs += performance.now() - tw;
    }));
  } finally {
    post({ type: 'busy', text: null });
  }
  const saveMs = performance.now() - t0 - writeMs;
  lastSnapshot = meta;
  post({ type: 'snapshot', why, steps: Number(m.steps), size, saveMs, writeMs, generations: meta.generations, memory: Math.max(memory, m.memoryBytes) });
}

function joinTail() {
  const out = new Uint8Array(consoleTailLen);
  let at = 0;
  for (const c of consoleTail) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

function keepTail(bytes) {
  consoleTail.push(bytes.slice());
  consoleTailLen += bytes.length;
  while (consoleTailLen - consoleTail[0].length >= CONSOLE_TAIL) consoleTailLen -= consoleTail.shift().length;
}

async function start(c) {
  cfg = c;
  const t0 = performance.now();
  startT0 = t0;
  const times = {};
  status('loading vetro-wasm');
  const wasm = await (await fetch(c.wasmUrl)).arrayBuffer();
  ({ exports } = await instantiate(wasm));
  times.wasm = performance.now() - t0;
  if (c.android) android = await prepareAndroid(c);
  const kernel = android ? null : await bytesOf(c.kernel, 'the kernel');
  const initrd = android ? null : await bytesOf(c.initrd, 'the initramfs');
  times.files = performance.now() - t0 - times.wasm;
  const sources = [];
  await build(c, sources);
  let restored = null;
  let prebuilt = false;
  if (c.snapshot && c.opfs) {
    try {
      store = await SnapshotStore.opfs();
      const t1 = performance.now();
      const common = {
        format: m.snapshotVersion,
        ramMiB: c.ramMiB,
        width: c.width,
        height: c.height,
        devices: devicesOf(c),
        disks: sources.map((s, i) => ({ identity: s.key, size: Math.floor(s.size / 512) * 512, readOnly: !!c.disks[i].readOnly })),
      };
      // Android: the key does not depend on where the image is served from,
      // and is the prebuilt snapshot's key too (ADR 0031).
      snapKey = android
        ? (await androidSnapshotKey(m, { machine: c, devices: devicesOf(c), manifest: android.manifest, images: android.images, params: android.params, layout: sources[0] })).key
        : await snapshotKey({ ...common, kernel: await sha256Hex(kernel), initrd: initrd ? await sha256Hex(initrd) : null, cmdline: c.cmdline });
      times.key = performance.now() - t1;
      const t2 = performance.now();
      let meta = await store.loadMeta(snapKey);
      times.read = performance.now() - t2;
      if (!meta && android && c.android.prebuilt !== false) {
        meta = await fetchPrebuilt(times);
        if (meta) prebuilt = true;
      }
      const stale = meta && staleReason(meta, overlays);
      if (meta && stale) status(`snapshot not used: ${stale}`);
      if (meta && !stale) {
        status(`restoring the snapshot (${(meta.size / 2 ** 20).toFixed(0)} MiB)`);
        const t3 = performance.now();
        try {
          // In chunks from OPFS: only the part before the RAM goes into the
          // module's memory (ADR 0028).
          const reader = await store.openReader(snapKey);
          let readMs = 0;
          try {
            m.snapshotRestoreStream(reader.size, (view, at) => {
              const tr = performance.now();
              reader.readAt(view, at);
              readMs += performance.now() - tr;
            });
          } finally {
            reader.close();
          }
          times.readSnapshot = readMs;
          times.restore = performance.now() - t3 - readMs;
          restored = { meta, size: meta.size, prebuilt };
        } catch (e) {
          status(`snapshot not used: ${e.message}`);
          // With 'Corrupt' the machine must be discarded: start over.
          if (e.code === 'Corrupt' || e.code === 'Memory') await build(c, sources);
        }
      }
    } catch (e) {
      if (e.prebuilt) throw e;
      status(`snapshot cache not available (${e.message ?? e})`);
      store = null;
    }
  }
  if (restored) {
    lastSnapshot = restored.meta;
    const tail = fromBase64(restored.meta.console ?? '');
    if (tail.length) keepTail(tail);
    if (android) {
      for (const ev of restored.meta.progress ?? []) android.progress.events.push(ev);
      android.progress.index = android.progress.events.length - 1;
      android.bootedNs = m.guestNs;
      if (android.progress.phase === 'home') android.homeNs = android.focusNs = m.guestNs;
      android.savedBoot = true;
    }
    times.total = performance.now() - t0;
    post({ type: 'restored', steps: Number(m.steps), size: restored.size, times, savedAt: restored.meta.savedAt, console: tail, prebuilt: restored.prebuilt,
      progress: android?.progress.events ?? null, memory: m.memoryBytes }, [tail.buffer]);
  } else if (android) {
    if (prebuilt) status('the prebuilt snapshot could not be restored: cold boot');
    const [boot, vendorBoot, initBoot] = await androidImages(android);
    times.images = performance.now() - t0 - times.wasm - times.files;
    const desc = m.loadAndroid({ boot, vendorBoot, initBoot, params: android.params });
    status(`Android images loaded: ${desc.split(';')[0]}`);
    times.total = performance.now() - t0;
    post({ type: 'cold', times, android: desc });
  } else {
    m.loadLinux(kernel, initrd, c.cmdline);
    times.total = performance.now() - t0;
    post({ type: 'cold', times });
  }
  if (c.jit) m.setJit();
  if (c.files) openFiles();
  if (c.net) m.capture(true);
  try {
    recording = new Recording(m, c.opfs ? await SnapshotStore.opfs('vetro-recordings') : SnapshotStore.memory());
    const info = await recording.restore();
    if (info && !info.sameMachine) status('saved recording of a differently configured machine: it cannot be replayed here');
  } catch (e) {
    status(`recording archive not available (${e.message ?? e}): in memory`);
    recording = new Recording(m, SnapshotStore.memory());
  }
  postRr();
  post({ type: 'started', pointer: c.pointer, restored: !!restored });
  running = true;
  loop().catch((e) => {
    running = false;
    post({ type: 'error', text: e.stack ?? String(e) });
  });
}

// ---- Vetro's AOSP image (ADR 0028) ----------------------------------------------

/**
 * Reads the version's manifest.json and prepares the configuration: the
 * disks (the map next to the manifest) and the image URLs.
 */
async function prepareAndroid(c) {
  const url = new URL(c.android.manifest, location.href).href;
  status(`Android image: ${url}`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: status ${res.status}`);
  const manifest = await res.json();
  const file = (path) => {
    const f = manifest.files.find((x) => x.path === path);
    if (!f) throw new Error(`manifest without ${path}`);
    return { ...f, url: new URL(path, url).href };
  };
  const images = ['boot.img', 'vendor_boot.img', 'init_boot.img'].map(file);
  const layout = new URL(c.android.layout ?? 'web/disk.json', url).href;
  c.disks = [{ layout, ...ANDROID_DISK }];
  return {
    manifest,
    manifestUrl: url,
    images,
    params: c.android.params ?? ANDROID_PARAMS,
    progress: new BootProgress(),
    bootedNs: null,
    homeNs: null,
    focusNs: null,
    focusDetail: '',
    homePollNs: 0n,
    homeQuery: false,
    savedBoot: false,
    adb: null,
    adbReady: false,
    adbRetryNs: 0n,
    ops: [],
    busy: false,
  };
}

/**
 * The prebuilt snapshot at the home screen (ADR 0031), looked up by this
 * machine's key next to the image and downloaded into the snapshot cache
 * (verified chunk by chunk, resumed after an interruption). Returns the
 * snapshot's metadata as the cache keeps it, or null if there is none for this
 * vetro-wasm and image (cold boot). A download that fails stops the start:
 * reloading the page resumes it.
 */
async function fetchPrebuilt(times) {
  const t0 = performance.now();
  const found = await findPrebuilt(android.manifestUrl, snapKey);
  times.prebuiltLookup = performance.now() - t0;
  if (!found.info) {
    status(`cold boot: ${found.missing}`);
    post({ type: 'prebuilt', state: 'missing', reason: found.missing });
    return null;
  }
  const { info } = found;
  const target = await store.downloadTarget(snapKey);
  const resumed = target.resume?.sha256 === info.sha256 ? target.resume.verified * PREBUILT_CHUNK : 0;
  post({ type: 'prebuilt', state: 'downloading', size: info.size, resumedFrom: resumed, key: snapKey });
  status(`downloading the home-screen snapshot (${(info.size / 2 ** 20).toFixed(0)} MiB${resumed ? `, resuming at ${(resumed / 2 ** 20).toFixed(0)} MiB` : ''})`);
  let last = 0;
  try {
    const r = await downloadPrebuilt(info, prebuiltSnapUrl(android.manifestUrl, snapKey), target.file, {
      resume: target.resume,
      saveResume: target.saveResume,
      onProgress: (p) => {
        const now = performance.now();
        if (now - last < 250) return;
        last = now;
        post({ type: 'prebuilt', state: 'downloading', ...p, ms: now - t0 });
      },
    });
    const meta = { ...info.meta };
    meta.size = await target.finish(meta);
    times.prebuilt = performance.now() - t0;
    post({ type: 'prebuilt', state: 'done', size: info.size, bytes: r.bytes, ms: times.prebuilt, resumedFrom: r.resumedFrom, retries: r.retries });
    return meta;
  } catch (e) {
    target.close();
    post({ type: 'prebuilt', state: 'failed', error: String(e.message ?? e) });
    throw Object.assign(new Error(`downloading the prebuilt snapshot failed (${e.message ?? e}): reload the page to resume the download, or choose a cold boot`), { prebuilt: true });
  }
}

/** The three boot images: from OPFS if there, otherwise downloaded and checked. */
async function androidImages(a) {
  const dir = cfg.opfs ? await navigator.storage.getDirectory().then((r) => r.getDirectoryHandle('vetro-images', { create: true })).catch(() => null) : null;
  const out = [];
  for (const f of a.images) {
    let bytes = null;
    if (dir) {
      try {
        const h = await (await dir.getFileHandle(`${f.sha256}.img`)).createSyncAccessHandle();
        try {
          if (h.getSize() === f.size) {
            bytes = new Uint8Array(f.size);
            h.read(bytes, { at: 0 });
          }
        } finally {
          h.close();
        }
      } catch {}
    }
    if (!bytes) {
      status(`downloading ${f.path} (${(f.size / 2 ** 20).toFixed(0)} MiB)`);
      const res = await fetch(f.url);
      if (!res.ok) throw new Error(`${f.url}: status ${res.status}`);
      bytes = new Uint8Array(await res.arrayBuffer());
      const got = await sha256Hex(bytes);
      if (got !== f.sha256) throw new Error(`${f.path}: sha256 ${got}, the manifest says ${f.sha256}`);
      if (dir) {
        try {
          const h = await (await dir.getFileHandle(`${f.sha256}.img`, { create: true })).createSyncAccessHandle();
          h.truncate(0);
          h.write(bytes, { at: 0 });
          h.flush();
          h.close();
        } catch {}
      }
    }
    out.push(bytes);
  }
  return out;
}

const latin1 = new TextDecoder('latin1');

/** The boot phases read from the console output. */
function androidConsole(bytes) {
  for (const ev of android.progress.feed(latin1.decode(bytes), Number(m.guestNs) / 1e9)) {
    post({ type: 'progress', ...ev, wallMs: performance.now() - startT0 });
  }
}

/** Connection to adbd, the page's requests, the snapshot after the boot. */
function androidTick() {
  const a = android;
  if (a.progress.phase === 'booted' && a.bootedNs === null) {
    a.bootedNs = m.guestNs;
    post({ type: 'booted', guestSecs: Number(m.guestNs) / 1e9, wallMs: performance.now() - startT0 });
  }
  if (a.bootedNs === null || mode !== 'live') return;
  if (!a.adb && m.guestNs >= a.adbRetryNs) {
    const sock = m.connectGuest(5555);
    const adb = new AdbClient(sock);
    a.adb = adb;
    post({ type: 'adb-status', state: 'connecting' });
    adb.connect().then(async (banner) => {
      // A virtual machine in the page: the screen stays on.
      await adb.shell(ANDROID_WAKE);
      // Lighter graphics unless the page asked for Android's own (ADR 0036).
      await adb.shell(ANDROID_GRAPHICS[cfg.android.graphics] ?? ANDROID_GRAPHICS.light);
      // The device profile's settings kept in /data (ADR 0035): idempotent,
      // so they run again after every connection.
      for (const c of cfg.android.setup ?? []) await adb.shell(c);
      a.adbReady = true;
      const devices = await adb.devices();
      post({ type: 'adb-status', state: 'ready', banner, devices });
    }).catch((e) => {
      sock.release();
      if (a.adb === adb) a.adb = null;
      a.adbReady = false;
      a.adbRetryNs = m.guestNs + ADB_RETRY_NS;
      post({ type: 'adb-status', state: 'waiting', error: String(e.message ?? e) });
    });
  }
  a.adb?.pump();
  if (a.adb?.lost && a.adbReady) {
    const why = a.adb.lost;
    a.adbReady = false;
    a.adb = null;
    a.adbRetryNs = m.guestNs + ADB_RETRY_NS;
    post({ type: 'adb-status', state: 'waiting', error: `connection closed (${why})` });
  }
  // The home screen: the focused window becomes the launcher, and the scanout
  // shows it (FallbackHome can stay for tens of seconds of guest time first).
  if (a.focusNs !== null && a.homeNs === null) {
    const size = m.displaySize();
    const px = size && m.displayPixels();
    const colors = px ? gridColors(px, size.width, size.height) : 0;
    if (colors >= HOME_MIN_COLORS || m.guestNs - a.focusNs >= HOME_DRAW_NS) {
      a.homeNs = m.guestNs;
      for (const ev of a.progress.mark('home', Number(m.guestNs) / 1e9)) {
        post({ type: 'progress', ...ev, wallMs: performance.now() - startT0, detail: a.focusDetail, colors, focusGuestSecs: Number(a.focusNs) / 1e9 });
      }
    }
  }
  if (a.adbReady && a.focusNs === null && !a.homeQuery && !a.busy && m.guestNs >= a.homePollNs) {
    a.homeQuery = true;
    a.adb.shell(HOME_QUERY).then((r) => {
      if (isHome(r.stdout) && a.focusNs === null) {
        a.focusNs = m.guestNs;
        a.focusDetail = r.stdout.trim();
      }
    }).catch(() => {}).finally(() => {
      a.homeQuery = false;
      a.homePollNs = m.guestNs + HOME_POLL_NS;
    });
  }
  if (a.adbReady && !a.busy && !a.homeQuery && a.ops.length) runAdbOp(a.ops.shift());
  const homeReady = a.homeNs !== null && m.guestNs - a.homeNs >= ANDROID_HOME_NS;
  if (store && !a.savedBoot && (homeReady || m.guestNs - a.bootedNs >= HOME_GIVE_UP_NS)) {
    a.savedBoot = true;
    if (!lastSnapshot) {
      saveRequested = true;
      saveRequestedAt = performance.now();
      saveWhy = homeReady ? 'home screen' : 'boot finished (home screen not seen)';
    }
  }
}

/** An ADB request from the page (one at a time). */
function runAdbOp(msg) {
  const a = android;
  a.busy = true;
  inputLog.push([Number(m.steps), { type: 'adb', op: msg.op, name: msg.name, cmd: msg.cmd, package: msg.package }]);
  const reply = (r) => post({ type: 'adb-reply', id: msg.id, ...r });
  const t0 = performance.now();
  let p;
  switch (msg.op) {
    case 'shell':
      m.timelineInput(TIMELINE_INPUT.OTHER, `adb shell ${msg.cmd}`);
      p = a.adb.shell(msg.cmd);
      break;
    case 'devices':
      p = a.adb.devices();
      break;
    case 'install':
      p = adbInstall(new Uint8Array(msg.bytes), msg);
      break;
    case 'open':
      p = adbOpen(msg.package, msg.launcher ?? null, msg);
      break;
    default:
      p = Promise.reject(new Error(`unknown adb operation ${msg.op}`));
  }
  p.then((result) => reply({ ok: true, result, ms: performance.now() - t0 }), (e) => reply({ ok: false, error: String(e.message ?? e), ms: performance.now() - t0 }))
    .finally(() => (a.busy = false));
}

/**
 * Installs an APK with adb (push + pm install) and opens its main activity
 * (unless `msg.open === false`, as the app catalog does: it offers Open
 * after). Progress: `adb-progress` messages with the text and, while the APK
 * is pushed, `fraction`.
 */
async function adbInstall(bytes, msg) {
  const adb = android.adb;
  const info = await apkInfo(bytes);
  m.timelineInput(TIMELINE_INPUT.OTHER, `install ${info.package}`);
  const kib = (bytes.length / 1024).toFixed(0);
  post({ type: 'adb-progress', id: msg.id, text: `installing ${info.package} (${kib} KiB)`, fraction: 0 });
  const t0 = performance.now();
  const onProgress = (sent, total) => post({ type: 'adb-progress', id: msg.id, text: sent < total ? `sending ${info.package} to the device (${(sent / 1024).toFixed(0)} of ${kib} KiB)` : `pm install ${info.package}`, fraction: sent / total });
  const output = await adb.install(bytes, { name: `${info.package}.apk`, onProgress });
  const installMs = performance.now() - t0;
  let component = null;
  let start = null;
  if (msg.open !== false) ({ component, start } = await adbOpen(info.package, info.launcher, msg));
  else if (info.launcher) component = `${info.package}/${info.launcher}`;
  if (store && msg.save !== false) {
    saveRequested = true;
    saveRequestedAt = performance.now();
    saveWhy = 'app installed';
  }
  return { info, output, component, start, installMs, openMs: performance.now() - t0 - installMs };
}

/**
 * Opens an installed package: its launcher activity (`launcher`, a full
 * class name, if known, else asked to the package manager) with `am start -W`.
 */
async function adbOpen(pkg, launcher, msg) {
  if (!/^[A-Za-z0-9_.]+$/.test(pkg ?? '')) throw new Error(`not a package name: ${pkg}`);
  const adb = android.adb;
  let component = launcher && /^[A-Za-z0-9_.$]+$/.test(launcher) ? `${pkg}/${launcher}` : null;
  if (!component) {
    const r = await adb.shell(`cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER ${pkg} | tail -n 1`);
    component = r.stdout.trim().includes('/') ? r.stdout.trim() : null;
  }
  if (!component) throw new Error(`${pkg} has no launcher activity`);
  m.timelineInput(TIMELINE_INPUT.OTHER, `open ${pkg}`);
  post({ type: 'adb-progress', id: msg.id, text: `opening ${component}` });
  const r = await adb.shell(`am start -W -n '${component}'`);
  return { component, start: `${r.stdout}${r.stderr}`.trim() };
}

function apply(msg) {
  if (mode !== 'live') {
    // During the replay the inputs come from the log.
    if (!ignoredNotice) status('replay in progress: the page\'s inputs do not reach the guest');
    ignoredNotice = true;
    return;
  }
  if (msg.t !== undefined) {
    // The page's timestamp is a measurement, not part of the guest input.
    const waited = absNow() - msg.t;
    inputWait.count++;
    inputWait.lastMs = waited;
    inputWait.maxMs = Math.max(inputWait.maxMs, waited);
    inputWait.sumMs += waited;
    msg = { ...msg };
    delete msg.t;
  }
  inputLog.push([Number(m.steps), msg]);
  if (msg.type !== 'files' && msg.type !== 'resize') lastUserInputAt = performance.now();
  switch (msg.type) {
    case 'serial':
      m.consoleWrite(msg.text);
      break;
    case 'key':
      m.key(msg.code, msg.down);
      break;
    case 'abs':
      m.pointerMove(msg.x, msg.y);
      break;
    case 'button':
      m.pointerButton(msg.code, msg.down);
      break;
    case 'wheel':
      m.inputEvents(INPUT.POINTER, [[EV_REL, REL_WHEEL, msg.delta], [EV_SYN, 0, 0]]);
      break;
    case 'touch':
      m.touch(msg.slot, msg.down ? [msg.x, msg.y] : null);
      break;
    case 'power':
      m.gpio(msg.down);
      break;
    case 'resize':
      m.displayResize(msg.width, msg.height);
      break;
    case 'files':
      filesRequest(msg);
      break;
    default:
      inputLog.pop();
      break;
  }
}

/** File manager operations requested by the page. */
const FILE_OPS = {
  stat: (a) => files.stat(a.path),
  list: (a) => files.list(a.path),
  read: (a) => files.read(a.path, a.offset ?? 0, a.length ?? null),
  write: (a) => files.writeFile(a.path, a.bytes, a.mode ?? 0o644),
  mkdir: (a) => files.mkdir(a.path, a.mode ?? 0o755),
  create: (a) => files.create(a.path, a.mode ?? 0o644),
  delete: (a) => files.delete(a.path, { recursive: !!a.recursive }),
  rename: (a) => files.rename(a.path, a.to),
  watch: (a) => files.watch(a.path).then((wd) => {
    watchPaths.set(wd, a.path);
    return wd;
  }),
  unwatch: (a) => files.unwatch(a.wd),
  // SQL in the guest with the real engine, as the owner of the database (ADR 0021).
  sql: (a) => files.sql(a.path, a.sql, a.params ?? [], { expect: a.expect ?? null, readonly: !!a.readonly }),
};

/** The file manager commands that are user actions (timeline). */
const FILE_COMMANDS = {
  write: (a) => `save ${a.path}`,
  mkdir: (a) => `new folder ${a.path}`,
  create: (a) => `new file ${a.path}`,
  delete: (a) => `delete ${a.path}`,
  rename: (a) => `rename ${a.path} → ${a.to}`,
  sql: (a) => (a.readonly ? null : `SQL on ${a.path}: ${a.sql.length > 80 ? `${a.sql.slice(0, 80)}…` : a.sql}`),
};

/** The inotify events that change files, with their name in the timeline. */
const FILE_CHANGES = [
  [INOTIFY.CREATE, 'created'], [INOTIFY.CLOSE_WRITE, 'written'], [INOTIFY.MOVED_TO, 'moved here'],
  [INOTIFY.MOVED_FROM, 'moved away'], [INOTIFY.DELETE, 'deleted'],
];

function fileEffect(e) {
  const change = FILE_CHANGES.find(([bit]) => e.mask & bit);
  if (!change || e.name.startsWith('.vetro-tmp.')) return;
  const dir = watchPaths.get(e.wd) ?? `wd ${e.wd}`;
  const path = e.name ? `${dir.replace(/\/$/, '')}/${e.name}` : dir;
  m.timelineEffect(TIMELINE_EFFECT.FILE, `${change[1]}${e.mask & INOTIFY.ISDIR ? ' (folder)' : ''} ${path}`);
}

function openFiles() {
  files = m.files();
  watchPaths.clear();
  files.onEvent = (event) => {
    fileEffect(event);
    post({ type: 'files-event', event });
  };
}

function filesRequest(msg) {
  const reply = (r) => post({ type: 'files-reply', id: msg.id, ...r }, r.result?.data ? [r.result.data.buffer] : []);
  const op = FILE_OPS[msg.op];
  if (!files || !op) return reply({ ok: false, error: files ? `unknown operation ${msg.op}` : 'file manager off' });
  const label = FILE_COMMANDS[msg.op]?.(msg.args);
  if (label) m.timelineInput(TIMELINE_INPUT.FILES, label);
  op(msg.args).then((result) => reply({ ok: true, result }), (e) => reply({ ok: false, error: e.message, code: e.code }));
}

/** Advances the file manager; sends the status to the page if it changed. */
function pumpFiles() {
  if (!files) return;
  files.pump();
  const st = files.status();
  const key = `${st.state}/${st.generation}`;
  if (key !== filesKey) {
    filesKey = key;
    post({ type: 'files-status', status: st });
  }
}

let lastUpdates = -1;
let lastCursor = -1;
let lastCursorResource = -1;

/** Console, frame, cursor; returns whether the guest showed anything. */
function flush() {
  let active = false;
  const out = m.consoleRead();
  if (out.length) {
    active = true;
    if (android) androidConsole(out);
    keepTail(out);
    post({ type: 'console', bytes: out }, [out.buffer]);
  }
  const updates = m.displayUpdates();
  if (updates !== lastUpdates) {
    active = true;
    lastUpdates = updates;
    const size = m.displaySize();
    if (!size) {
      post({ type: 'frame', off: true });
    } else {
      const rect = m.displayTakeDirty();
      if (rect) {
        const pixels = m.displayCopy(rect);
        post({ type: 'frame', width: size.width, height: size.height, rect, pixels, at: absNow() }, [pixels.buffer]);
      }
    }
  }
  const c = m.cursor();
  if (c && c.updates !== lastCursor) {
    lastCursor = c.updates;
    const msg = { type: 'cursor', ...c };
    if (c.resource !== lastCursorResource || c.updates === 1) {
      msg.image = m.cursorImage();
      lastCursorResource = c.resource;
    }
    post(msg, msg.image ? [msg.image.buffer] : []);
  }
  return active;
}

/** Resets the real-time reference (at the start and when guest time jumps). */
function resetClock() {
  clock.t0 = performance.now();
  clock.g0 = m.guestNs;
  clock.paused = 0;
}

/** Recording and replay status for the page. */
function postRr(extra = {}) {
  if (!m) return;
  post({ type: 'rr', status: m.rrStatus(), info: m.logInfo(), meta: recording?.meta ?? null, mode, steps: Number(m.steps),
    target: target === null ? null : Number(target), ...extra });
}

/** Inspector list and timeline, if they changed (at most every 700 ms, or at once with `force`). */
function postAnalysis(force = false) {
  const now = performance.now();
  if (!force && now - lastAnalysisAt < 700) return;
  lastAnalysisAt = now;
  const v = m.timelineVersion();
  if (!force && v === lastAnalysis) return;
  lastAnalysis = v;
  post({ type: 'analysis', requests: m.inspectRequests(), timeline: m.timeline(windowUs), capture: m.captureStats() });
}

/** At the requested point of the replay: stops and sends registers and state. */
function pause() {
  mode = 'paused';
  target = null;
  status(`replay stopped at instruction ${m.steps}: registers and memory in the Recording panel`);
  post({ type: 'paused', steps: Number(m.steps), registers: m.registersText() });
  postAnalysis(true);
  postRr();
}

/** End of the replay (identical or not): the machine runs free again. */
function replayEnded(st) {
  mode = 'live';
  target = null;
  ignoredNotice = false;
  resetClock();
  if (cfg.files) openFiles();
  post({ type: 'replay-ended', status: st, steps: Number(m.steps) });
  postAnalysis(true);
  postRr();
}

async function startReplay(step, stopAt) {
  if (m.rrStatus().state === 'Recording') {
    m.recordStop();
    await recording.store();
  }
  await recording.ensureKeyframe(step);
  try {
    m.replayStart(step);
  } finally {
    recording.dropKeyframes();
  }
  // vetro-wasm has already removed the file manager client (its
  // operations are in the log): here the pending requests are rejected.
  files?.close();
  files = null;
  filesKey = '';
  post({ type: 'files-status', status: { state: 'None', pending: 0, generation: 0, maxChunk: 0, selinux: false } });
  mode = 'replay';
  target = stopAt ? BigInt(step) : null;
  lastUpdates = -1;
  resetClock();
  post({ type: 'replay-started', from: Number(m.steps), target: stopAt ? Number(step) : null });
  if (target !== null && m.steps >= target) pause();
}

/** A recording or replay command. */
async function rr(cmd) {
  try {
    switch (cmd.op) {
      case 'record-start':
        if (mode !== 'live') throw new Error('finish the replay first');
        m.recordStart(cmd.keyframeEvery);
        status(`recording (keyframe every ${cmd.keyframeEvery / 1e6} M instructions)`);
        break;
      case 'record-stop': {
        if (!m.recordStop()) throw new Error('no recording in progress');
        const meta = await recording.store();
        status(`recording finished: ${meta.events} inputs, ${meta.keyframes} keyframes saved`);
        break;
      }
      case 'load-log': {
        const meta = await recording.load(new Uint8Array(cmd.bytes));
        status(`log loaded: ${meta.events} inputs, ${meta.keyframes} keyframes`);
        break;
      }
      case 'replay':
        await startReplay(cmd.step ?? 0, !!cmd.pause);
        break;
      case 'continue':
        if (mode === 'paused') {
          mode = 'replay';
          target = cmd.step !== undefined ? BigInt(cmd.step) : null;
        }
        break;
    }
  } catch (e) {
    status(`${cmd.op}: ${e.message ?? e}`);
    post({ type: 'rr-error', op: cmd.op, message: String(e.message ?? e) });
  }
  postRr();
}

async function loop() {
  resetClock();
  const guestMs = () => Number(m.guestNs - clock.g0) / 1e6;
  /** Guest time ahead of the real clock (ms). */
  const aheadMs = () => guestMs() - (performance.now() - clock.t0 - clock.paused);
  let lastStats = 0;
  let lastPersist = performance.now();
  // Last guest activity (guest time) and rest already used.
  let activeNs = m.guestNs;
  let rested = false;
  const activity = () => {
    activeNs = m.guestNs;
    rested = false;
  };
  let steps0 = m.steps;
  let wall0 = performance.now();
  for (;;) {
    while (control.length) await rr(control.shift());
    if (mode === 'paused') {
      while (inbox.length) apply(inbox.shift());
      if (!control.length) await new Promise((ok) => (wake = ok));
      wake = null;
      continue;
    }
    const input = inbox.length > 0;
    if (input) activity();
    while (inbox.length) apply(inbox.shift());
    pumpFiles();
    const realtime = cfg.realtime && mode === 'live';
    // Real time: a guest ahead of the clock waits for it (at most AHEAD_MS
    // and a quantum: a WFI stops at the end of the quantum), and an input
    // ends the wait at once.
    if (realtime && !input) {
      const ahead = aheadMs();
      if (ahead > AHEAD_MS) {
        await rest(Math.min(ahead - AHEAD_MS, 50));
        continue;
      }
    }
    const slice = performance.now();
    let stop;
    for (;;) {
      const size = Math.max(MIN_QUANTUM, Math.min(QUANTUM, Math.round(stepsPerMs * QUANTUM_MS)));
      let budget = target === null ? size : Math.min(size, Number(target - m.steps));
      // Real time: no further than AHEAD_MS past the clock.
      if (realtime) budget = Math.min(budget, Math.max(MIN_QUANTUM, Math.floor((AHEAD_MS - aheadMs()) * STEPS_PER_MS)));
      const q0 = performance.now();
      const s0 = m.steps;
      stop = budget > 0 ? m.run(budget) : 'Budget';
      const qMs = performance.now() - q0;
      // Speed from whole quanta that took measurable time (a WFI jump counts
      // as fast: the next quantum corrects it).
      if (stop === 'Budget' && qMs > 0.5) stepsPerMs = 0.7 * stepsPerMs + 0.3 * (Number(m.steps - s0) / qMs);
      if (stop === 'Blocked') {
        activity();
        const w = performance.now();
        await feeder.serve();
        clock.paused += performance.now() - w;
        continue;
      }
      if (mode === 'replay') {
        const st = m.rrStatus();
        if (st.state !== 'Replaying') {
          flush();
          replayEnded(st);
          break;
        }
        if (target !== null && m.steps >= target) {
          flush();
          pause();
          break;
        }
      }
      if (stop !== 'Budget' || performance.now() - slice > SLICE_MS) break;
      if (realtime && aheadMs() > AHEAD_MS) break;
    }
    if (mode === 'paused') continue;
    if (flush()) activity();
    pumpFiles();
    if (android) androidTick();
    const now = performance.now();
    // With Android there is no separate overlay (see the top of the file).
    if (!android && (stop !== 'Budget' || now - lastPersist > PERSIST_MS)) {
      if (persistOverlays()) activity();
      lastPersist = now;
    }
    // Snapshot: the first time the guest is at rest (boot finished), then at
    // rest if the disks have changed, or on request. Android: see androidTick.
    const atRest = !android && (stop === 'Idle' || (!rested && m.guestNs - activeNs >= REST_NS));
    if (atRest && stop !== 'Idle') {
      rested = true;
      if (persistOverlays()) activity();
    }
    // An automatic snapshot waits for a pause in the user's inputs (see SAVE_QUIET_MS).
    const saveNow = saveRequested && (saveWhy === 'requested' || now - lastUserInputAt >= SAVE_QUIET_MS || now - saveRequestedAt >= SAVE_DEFER_MAX_MS);
    if (store && mode === 'live' && (saveNow || (!saveRequested && atRest && (!lastSnapshot || String(generations()) !== String(lastSnapshot.generations))))) {
      const why = saveNow ? saveWhy : lastSnapshot ? 'disks changed' : 'boot finished';
      saveRequested = false;
      saveWhy = 'requested';
      status(`saving the snapshot (${why})`);
      await saveSnapshot(why).catch((e) => status(`snapshot not saved: ${e.message ?? e}`));
    }
    // After a replay the counter can go back.
    if (m.steps < steps0) {
      steps0 = m.steps;
      wall0 = now;
    }
    if (now - lastStats > 500) {
      const mips = Number(m.steps - steps0) / ((now - wall0) * 1000);
      post({
        type: 'stats',
        steps: Number(m.steps),
        guestSecs: Number(m.guestNs) / 1e9,
        mips,
        disks: feeder.disks.map((d, i) => ({ ...m.diskStats(i), http: d.source.stats, overlay: overlays[i]?.info ?? null })),
        feeder: feeder.stats,
        jit: m.jitStats(),
        inputs: inputLog.length,
        memory: m.memoryBytes,
        input: { ...inputWait },
        quantum: Math.round(stepsPerMs * QUANTUM_MS),
        // Guest time ahead of the real clock (ms; > 0: the guest is early).
        aheadMs: aheadMs(),
      });
      if (mode !== 'live' || m.rrStatus().state === 'Recording') postRr();
      lastStats = now;
      steps0 = m.steps;
      wall0 = now;
    }
    postAnalysis();
    if (stop === 'Idle' && mode === 'live') {
      status('the guest is waiting for input');
      // A deferred snapshot is saved once the inputs pause (SAVE_QUIET_MS).
      if (!inbox.length && !control.length) await rest(saveRequested ? SAVE_QUIET_MS : Infinity);
      continue;
    }
    if (stop !== 'Budget') {
      running = false;
      post({ type: 'stopped', reason: stop, steps: Number(m.steps) });
      return;
    }
    // Lets the page's messages in (inputs, reads): real time waits at the
    // top of the loop.
    await yieldToEvents();
  }
}

const enc = new TextEncoder();

/** The page's reads: detail, exports, registers, memory, timeline window. */
async function inspect(msg) {
  switch (msg.op) {
    case 'request':
      return { result: m.inspectRequest(msg.index) };
    case 'har': {
      const bytes = enc.encode(m.inspectHar(msg.epochUs ?? 0));
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'pcapng': {
      const bytes = m.inspectPcapng(msg.epochUs ?? 0);
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'log': {
      const bytes = await recording.encodeFull();
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'events':
      return { result: m.logEvents() };
    case 'registers':
      return { result: { steps: Number(m.steps), text: m.registersText() } };
    case 'memory': {
      const va = BigInt(msg.va);
      const r = m.readVirt(va, msg.length);
      if (!r.bytes) return { result: { va: msg.va, fault: `0x${r.fault.toString(16)}` } };
      return { result: { va: msg.va, bytes: r.bytes, pa: m.translate(va)?.toString(16) ?? null } };
    }
    case 'window':
      windowUs = msg.us;
      postAnalysis(true);
      return { result: true };
    case 'capture-clear':
      m.captureClear();
      postAnalysis(true);
      return { result: true };
    case 'timeline-clear':
      m.timelineClear();
      postAnalysis(true);
      return { result: true };
    default:
      throw new Error(`unknown read ${msg.op}`);
  }
}

onmessage = (e) => {
  const msg = e.data;
  if (msg.type === 'start') {
    if (running) return;
    start(msg.config).catch((err) => post({ type: 'error', text: err.stack ?? String(err) }));
    return;
  }
  if (!m) return;
  if (msg.type === 'rr') {
    control.push(msg);
    wake?.();
    return;
  }
  if (msg.type === 'inspect') {
    // Reads that do not touch the guest: at once, between one slice and the next.
    inspect(msg).then(
      ({ result, transfer = [] }) => post({ type: 'inspect-reply', id: msg.id, ok: true, result }, transfer),
      (err) => post({ type: 'inspect-reply', id: msg.id, ok: false, error: String(err.message ?? err) }),
    );
    return;
  }
  if (msg.type === 'adb') {
    if (!android) return post({ type: 'adb-reply', id: msg.id, ok: false, error: 'adb needs the Android image' });
    android.ops.push(msg);
    if (!android.adbReady) post({ type: 'adb-progress', id: msg.id, text: 'waiting for adbd (end of the boot)' });
    wake?.();
    return;
  }
  if (msg.type === 'save') {
    // Not a guest input: it is saved between two slices.
    if (store) {
      saveRequested = true;
      saveRequestedAt = performance.now();
      saveWhy = 'requested';
    } else status('snapshot cache not active');
    wake?.();
    return;
  }
  inbox.push(msg);
  wake?.();
};
