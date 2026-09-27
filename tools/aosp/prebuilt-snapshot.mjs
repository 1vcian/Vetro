#!/usr/bin/env node
// Makes the prebuilt Android snapshot (ADR 0031): Vetro's AOSP image booted
// with vetro-wasm in Node (JIT in V8) on exactly the machine the app builds
// (ANDROID_MACHINE in web/node/android.mjs), up to the home screen drawn, like
// the app's Worker: boot phases from the console, adb over GuestSocket, the
// screen kept on, the launcher focused and drawn, 5 s of guest time; then
// optionally the in-guest compaction, and the snapshot. Long (about 45 min on
// an Apple silicon Mac).
//
//   node --max-old-space-size=8192 tools/aosp/prebuilt-snapshot.mjs [options]
//
// Options:
//   --manifest=URL   the image version's manifest.json (default: target/aosp/out
//                    served locally if it has manifest.json and web/disk.json,
//                    otherwise the app's DEFAULT_MANIFEST on R2)
//   --out=DIR        output directory (default target/aosp/prebuilt)
//   --no-compact     no in-guest compaction before the snapshot
//   --level=L        snapshot compression: small (default) or fast
//   --restore=FILE   resumes from a snapshot made by this tool (same machine)
//                    instead of booting: to try compaction or compression
//   --wasm=FILE      vetro-wasm (default target/wasm32-unknown-unknown/release)
//   --guest-limit=S  gives up after S seconds of guest time (default 4000)
//   --profile=P      a device profile (ADR 0035): a starter id (web/app/profiles)
//                    or a JSON file. The machine and boot parameters become
//                    the profile's (profileMachine, profileBootParams), its adb
//                    commands run after the connection, and at the home screen
//                    the guest is asked what it reports (screen size, density,
//                    serial, SKU, time zone, device name): a mismatch fails.
//                    Without it: ANDROID_MACHINE and ANDROID_PARAMS (the same
//                    as the default profile).
//
// Also writes <out>/<key>.png, the scanout at the home screen.
//
// Writes <out>/<key>.snap (the snapshot, as the app stores it in OPFS) and
// <out>/<key>.json: the key and its parts (androidKeyParts), size, sha256,
// SHA-256 of each chunk (the app verifies while downloading), the metadata the
// app keeps with a snapshot (console tail, boot phases), and measurements.
// tools/aosp/upload-snapshot.sh publishes them next to the image on R2.

import { closeSync, existsSync, mkdirSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DEV, instantiate, Machine } from '../../web/node/vetro.mjs';
import { AdbClient } from '../../web/node/adb.mjs';
import { DiskFeeder, LayoutSource, MemoryCache } from '../../web/node/disk.mjs';
import {
  ANDROID_COMPACT, ANDROID_DISK, ANDROID_HOME_NS, ANDROID_MACHINE, ANDROID_PARAMS, ANDROID_WAKE, BootProgress, DEFAULT_MANIFEST,
  gridColors, HOME_DRAW_NS, HOME_MIN_COLORS, HOME_POLL_NS, HOME_QUERY, isHome, machineDevices,
} from '../../web/node/android.mjs';
import { toBase64 } from '../../web/node/persist.mjs';
import { androidSnapshotKey, PREBUILT_CHUNK, PREBUILT_FORMAT } from '../../web/node/prebuilt.mjs';
import {
  parseProfile, parseProfileReport, PROFILE_REPORT, profileAdbCommands, profileBootParams, profileMachine, profileMismatches, STARTER_PROFILES,
} from '../../web/node/profiles.mjs';
import { crc32, deflateSync } from 'node:zlib';

const root = join(dirname(fileURLToPath(import.meta.url)), '../..');
const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const flag = (name) => process.argv.includes(`--${name}`);
const outDir = arg('out', join(root, 'target/aosp/prebuilt'));
const compact = !flag('no-compact');
const restorePath = arg('restore', null);
const wasmPath = arg('wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm'));
const guestLimit = Number(arg('guest-limit', 4000));
const level = arg('level', 'small');
const CONSOLE_TAIL = 64 * 1024;
const profileArg = arg('profile', null);
const profile = profileArg === null ? null
  : parseProfile(readFileSync(STARTER_PROFILES.includes(profileArg) ? join(root, 'web/app/profiles', `${profileArg}.json`) : profileArg, 'utf8'));
const machine = profile ? profileMachine(profile) : ANDROID_MACHINE;
const params = profile ? profileBootParams(profile) : ANDROID_PARAMS;
const setup = profile ? profileAdbCommands(profile) : [];

/** RGBA pixels as a PNG file. */
function writePng(path, px, w, h) {
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) Buffer.from(px.buffer, px.byteOffset + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const c = Buffer.alloc(4);
    c.writeUInt32BE(crc32(td));
    return Buffer.concat([len, td, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  writeFileSync(path, Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
}

const mib = (n) => (n / 2 ** 20).toFixed(0);
const t0 = performance.now();
const wall = () => ((performance.now() - t0) / 1000).toFixed(0);
const log = (s) => console.log(`[${wall()} s] ${s}`);

async function localManifest() {
  const out = join(root, 'target/aosp/out');
  if (!existsSync(join(out, 'manifest.json')) || !existsSync(join(out, 'web/disk.json'))) return null;
  const { serve } = await import('../web-serve.mjs');
  const srv = await serve({ mounts: [['/aosp/', out]] });
  return { url: `${srv.url}/aosp/manifest.json`, close: () => srv.close() };
}

async function main() {
  const local = arg('manifest', null) ? null : await localManifest();
  const manifestUrl = arg('manifest', null) ?? local?.url ?? DEFAULT_MANIFEST;
  log(`image: ${manifestUrl}`);
  const manifest = await (await fetch(manifestUrl)).json();
  const file = (path) => {
    const f = manifest.files.find((x) => x.path === path);
    if (!f) throw new Error(`manifest without ${path}`);
    return { ...f, url: new URL(path, manifestUrl).href };
  };
  const images = ['boot.img', 'vendor_boot.img', 'init_boot.img'].map(file);
  const { exports } = await instantiate(readFileSync(wasmPath));
  const M = machine;
  if (profile) log(`profile ${profile.id}: ${M.width}x${M.height} at ${profile.screen.density} dpi, ${M.ramMiB} MiB, parameters "${params}"`);
  const devices = machineDevices(DEV, M);
  const m = new Machine(exports, { ramSize: BigInt(M.ramMiB) << 20n, devices, width: M.width, height: M.height });
  const feeder = new DiskFeeder(m);
  const layout = await new LayoutSource(new URL('web/disk.json', manifestUrl).href).open();
  feeder.add(layout, { cache: new MemoryCache(), ...ANDROID_DISK });
  const { key, parts } = await androidSnapshotKey(m, { machine: M, devices, manifest, images, params, layout });
  log(`key ${key}: ${JSON.stringify(parts)}`);

  const progress = new BootProgress();
  const tail = [];
  let tailLen = 0;
  const keepTail = (b) => {
    tail.push(b.slice());
    tailLen += b.length;
    while (tailLen - tail[0].length >= CONSOLE_TAIL) tailLen -= tail.shift().length;
  };
  const measures = { manifest: manifestUrl, compact, level };
  if (restorePath) {
    const bytes = readFileSync(restorePath);
    const meta = JSON.parse(readFileSync(restorePath.replace(/\.snap$/, '.json'), 'utf8')).meta;
    const tr = performance.now();
    m.snapshotRestoreStream(bytes.length, (view, at) => view.set(bytes.subarray(at, at + view.length)));
    log(`restored ${restorePath} (${mib(bytes.length)} MiB) in ${(performance.now() - tr).toFixed(0)} ms`);
    for (const ev of meta.progress) progress.events.push(ev);
    progress.index = progress.events.length - 1;
    keepTail(Buffer.from(meta.console, 'base64'));
  } else {
    const bytes = [];
    for (const f of images) {
      const b = new Uint8Array(await (await fetch(f.url)).arrayBuffer());
      const got = createHash('sha256').update(b).digest('hex');
      if (got !== f.sha256) throw new Error(`${f.path}: sha256 ${got}, the manifest says ${f.sha256}`);
      bytes.push(b);
    }
    const desc = m.loadAndroid({ boot: bytes[0], vendorBoot: bytes[1], initBoot: bytes[2], params });
    log(`vetro: ${desc.split(';')[0]}`);
  }
  m.setJit();

  // The Worker's Android logic (web/app/worker.mjs, androidTick), sequential.
  const st = { bootedNs: restorePath ? m.guestNs : null, adb: null, ready: false, retryNs: 0n, focusNs: restorePath ? m.guestNs : null, homeNs: restorePath ? m.guestNs : null, pollNs: 0n, query: false, done: false, busy: false };
  let lastReport = 0;
  const latin1 = new TextDecoder('latin1');
  for (;;) {
    const stop = m.run(1_000_000);
    if (stop === 'Blocked') {
      await feeder.serve();
      continue;
    }
    const out = m.consoleRead();
    if (out.length) {
      keepTail(out);
      for (const ev of progress.feed(latin1.decode(out), Number(m.guestNs) / 1e9)) log(`phase: ${ev.label} at ${ev.guestSecs.toFixed(0)} s of guest time`);
    }
    if (stop !== 'Budget') throw new Error(`the machine stopped: ${stop}`);
    const guestSecs = Number(m.guestNs) / 1e9;
    if (guestSecs > guestLimit) throw new Error(`no home screen within ${guestLimit} s of guest time`);
    if (performance.now() - lastReport > 60_000) {
      lastReport = performance.now();
      log(`guest ${guestSecs.toFixed(0)} s, ${(Number(m.steps) / 1e6).toFixed(0)} M instr., memory ${mib(m.memoryBytes)} MiB, source ${mib(layout.stats.bytes)} MiB`);
    }
    if (progress.phase === 'booted' && st.bootedNs === null) {
      st.bootedNs = m.guestNs;
      measures.booted = { guestSecs, wallSecs: Number(wall()) };
    }
    if (st.bootedNs === null) continue;
    if (!st.adb && m.guestNs >= st.retryNs) {
      const sock = m.connectGuest(5555);
      const adb = new AdbClient(sock);
      st.adb = adb;
      adb.connect().then(async () => {
        await adb.shell(ANDROID_WAKE);
        for (const c of setup) {
          const r = await adb.shell(c);
          log(`profile: adb shell ${c}: ${JSON.stringify(`${r.stdout}${r.stderr}`.trim())}`);
        }
        st.ready = true;
        log('adb connected, screen kept on');
      }).catch(() => {
        sock.release();
        st.adb = null;
        st.retryNs = m.guestNs + 5_000_000_000n;
      });
    }
    st.adb?.pump();
    if (st.ready && st.focusNs === null && !st.query && m.guestNs >= st.pollNs) {
      st.query = true;
      st.adb.shell(HOME_QUERY).then((r) => {
        if (isHome(r.stdout) && st.focusNs === null) {
          st.focusNs = m.guestNs;
          log(`launcher focused at ${(Number(m.guestNs) / 1e9).toFixed(0)} s of guest time: ${r.stdout.trim()}`);
        }
      }).catch(() => {}).finally(() => {
        st.query = false;
        st.pollNs = m.guestNs + HOME_POLL_NS;
      });
    }
    if (st.focusNs !== null && st.homeNs === null) {
      const size = m.displaySize();
      const px = size && m.displayPixels();
      const colors = px ? gridColors(px, size.width, size.height) : 0;
      if (colors >= HOME_MIN_COLORS || m.guestNs - st.focusNs >= HOME_DRAW_NS) {
        st.homeNs = m.guestNs;
        progress.mark('home', guestSecs);
        measures.home = { guestSecs, wallSecs: Number(wall()), colors };
        log(`home screen drawn at ${guestSecs.toFixed(0)} s of guest time (${colors} colours)`);
      }
    }
    const settled = st.homeNs !== null && m.guestNs - st.homeNs >= ANDROID_HOME_NS && st.ready && !st.busy && !st.query;
    if (settled && !st.shot) {
      st.shot = true;
      const size = m.displaySize();
      const px = size && m.displayPixels();
      if (px) {
        mkdirSync(outDir, { recursive: true });
        writePng(join(outDir, `${key}.png`), px, size.width, size.height);
        measures.screen = { width: size.width, height: size.height };
        log(`scanout ${size.width}x${size.height}: ${join(outDir, `${key}.png`)}`);
      }
    }
    if (settled && profile && !st.checked) {
      // What the guest reports for the profile, before the compaction.
      st.busy = true;
      st.adb.shell(PROFILE_REPORT).then((r) => {
        const report = parseProfileReport(r.stdout);
        const bad = profileMismatches(profile, report);
        if (measures.screen && (measures.screen.width !== M.width || measures.screen.height !== M.height)) {
          bad.push(`scanout ${measures.screen.width}x${measures.screen.height} instead of ${M.width}x${M.height}`);
        }
        measures.profile = { id: profile.id, params, report, mismatches: bad };
        log(`profile ${profile.id}: the guest reports ${JSON.stringify(report)}${bad.length ? `; MISMATCH: ${bad.join('; ')}` : ': as the profile says'}`);
        if (bad.length) st.error = new Error(`profile ${profile.id}: ${bad.join('; ')}`);
      }, (e) => {
        st.error = e;
      }).finally(() => {
        st.checked = true;
        st.busy = false;
      });
    } else if (settled) {
      if (!compact || st.done) break;
      st.busy = true;
      const tc = performance.now();
      st.adb.shell(ANDROID_COMPACT).then((r) => {
        measures.compaction = { ms: performance.now() - tc, output: `${r.stdout}${r.stderr}`.trim() };
        log(`compaction in ${(measures.compaction.ms / 1000).toFixed(0)} s: ${measures.compaction.output.split('\n').join(' | ')}`);
      }, (e) => {
        st.error = e;
      }).finally(() => {
        st.done = true;
        st.busy = false;
      });
    }
    if (st.error) throw st.error;
    if (st.adb || st.query || st.busy) await new Promise((ok) => setImmediate(ok));
  }

  // Like the app: the adb connection stays in the snapshot (the next session
  // gets a new client and the old connection closes).
  mkdirSync(outDir, { recursive: true });
  const snapPath = join(outDir, `${key}.snap`);
  const hash = createHash('sha256');
  const chunks = [];
  const ts = performance.now();
  m.snapshotLevel = level;
  const fh = openSync(snapPath, 'w');
  // The chunks arrive in order except the header (written at offset 0 last):
  // the hashes are computed from the file afterwards.
  const size = m.snapshotSaveTo((b, at) => writeSync(fh, b, 0, b.length, at));
  closeSync(fh);
  measures.saveMs = performance.now() - ts;
  const bytes = readFileSync(snapPath);
  hash.update(bytes);
  for (let at = 0; at < size; at += PREBUILT_CHUNK) chunks.push(createHash('sha256').update(bytes.subarray(at, Math.min(size, at + PREBUILT_CHUNK))).digest('hex'));
  const all = new Uint8Array(tailLen);
  let o = 0;
  for (const c of tail) {
    all.set(c, o);
    o += c.length;
  }
  const meta = {
    steps: String(m.steps),
    generations: [null],
    console: toBase64(all),
    savedAt: new Date().toISOString(),
    why: 'prebuilt (home screen)',
    progress: progress.events,
    prebuilt: true,
  };
  const info = { format: PREBUILT_FORMAT, version: 1, key, parts, size, sha256: hash.digest('hex'), chunk: PREBUILT_CHUNK, chunks, meta, measures };
  writeFileSync(join(outDir, `${key}.json`), `${JSON.stringify(info, null, 1)}\n`);
  log(`snapshot ${mib(size)} MiB (${size} bytes) in ${(measures.saveMs / 1000).toFixed(1)} s: ${snapPath}`);
  st.adb?.close?.();
  await local?.close();
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
