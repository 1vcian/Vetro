#!/usr/bin/env node
// The prebuilt Android snapshot (ADR 0031) made by the app itself in headless
// Chrome: for the accelerated graphics path (ADR 0037, `gpu=webgl`), whose
// snapshot holds GPU contents only a WebGL2 context can read back, so the
// Node tool (prebuilt-snapshot.mjs, no WebGL) cannot make it. Works for the
// SwiftShader path too (`--gpu=swiftshader`).
//
//   node tools/aosp/prebuilt-chrome.mjs [options]
//
// The page is the app with a cold boot (`cold=1&cpus=1`), the device profile
// and the image's manifest; it boots, finds the home screen and saves its
// snapshot in OPFS like for any visitor. Then, with the page's adb: settle
// (`--settle=S`: the guest's 1-minute load below `--settle-load`, default 3,
// polled every 30 s of wall time, at most S seconds of wall time), the warm-up
// (`--warm`: ANDROID_WARMUP, as prebuilt-snapshot.mjs), the in-guest
// compaction (ANDROID_COMPACT, unless `--no-compact`), and a snapshot saved
// on request (the page's Save button). That snapshot is read out of OPFS and
// written as <out>/<key>.snap + <out>/<key>.json, the same files as
// prebuilt-snapshot.mjs (key, parts, size, sha256, chunk hashes, the app's
// metadata, measurements); tools/aosp/upload-snapshot.sh publishes them.
//
// Options:
//   --manifest=URL  the image's manifest.json (default: the app's DEFAULT_MANIFEST
//                   on R2; R2's CORS needs --port=8080)
//   --profile=P     starter profile id (default light)
//   --gpu=G         webgl (default) or swiftshader
//   --port=N        port of the local app server (default 8080)
//   --out=DIR       output directory (default target/aosp/prebuilt-chrome)
//   --minutes=N     gives up after N minutes (default 300)
//   --settle=S, --settle-load=L, --warm, --no-compact: see above
//
// Chrome runs with WebGL2 on its own SwiftShader (no GPU on the build VM):
// the guest only ever sees the decoder's fixed answers (ADR 0037), so the
// snapshot restores on any WebGL2 that meets the profile's limits.

import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appMounts, serve } from '../web-serve.mjs';
import { closeChrome, findChrome, launch, openPage } from '../../tests/web/chrome.mjs';
import { ANDROID_COMPACT, ANDROID_WARMUP, DEFAULT_MANIFEST } from '../../web/node/android.mjs';
import { PREBUILT_CHUNK, PREBUILT_FORMAT } from '../../web/node/prebuilt.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '../..');
const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const flag = (name) => process.argv.includes(`--${name}`);
const manifest = arg('manifest', DEFAULT_MANIFEST);
const profileId = arg('profile', 'light');
const gpu = arg('gpu', 'webgl');
const port = Number(arg('port', 8080));
const outDir = arg('out', join(root, 'target/aosp/prebuilt-chrome'));
const limitMs = Number(arg('minutes', 300)) * 60_000;
const settleMs = Number(arg('settle', 0)) * 1000;
const settleLoad = Number(arg('settle-load', 3));
const warm = flag('warm');
const compact = !flag('no-compact');

const t0 = Date.now();
const log = (s) => console.log(`[${((Date.now() - t0) / 1000).toFixed(0)} s] ${s}`);
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

const srv = await serve({ mounts: appMounts(), port });
const query = new URLSearchParams({ os: 'android', cold: '1', cpus: '1', profile: profileId, manifest });
if (gpu === 'webgl') query.set('gpu', 'webgl');
const url = `${srv.url}/app/?${query}`;
log(`page ${url}`);
const chrome = findChrome();
if (!chrome) throw new Error('Chrome not found (VETRO_CHROME)');
const profileDir = mkdtempSync(join(tmpdir(), 'vetro-prebuilt-chrome-'));
const { proc, cdp } = await launch(chrome, profileDir, { webgl: true });
const { page } = await openPage(cdp, url);
const ev = (e) => page.eval(e);
const measures = { url, gpu, profile: profileId, warm, compact };

/** Waits until `expr` is truthy (polled every `every` ms), with the overall limit. */
async function until(what, expr, every = 5000) {
  let lastStatus = '';
  for (;;) {
    const v = await ev(expr).catch(() => null);
    if (v) return v;
    if (Date.now() - t0 > limitMs) throw new Error(`gave up waiting for ${what}`);
    const s = await ev("document.getElementById('status')?.textContent ?? ''").catch(() => '');
    if (s && s !== lastStatus) {
      lastStatus = s;
      log(`status: ${s}`);
    }
    await sleep(every);
  }
}

const sh = async (cmd) => (await ev(`window.vetroAndroid.shell(${JSON.stringify(cmd)})`)).stdout ?? '';

try {
  const mode = await until('the graphics mode', 'window.vetroState?.gpu?.mode ?? (window.vetroState?.boot ? "swiftshader" : null)');
  log(`graphics: ${mode}${(await ev('window.vetroState.gpu?.why ?? ""')) ? ` (${await ev('window.vetroState.gpu.why')})` : ''}`);
  if (gpu === 'webgl' && mode !== 'webgl') throw new Error('the page fell back to SwiftShader');
  await until('the home screen snapshot', '(window.vetroState?.snapshots ?? []).length > 0', 10_000);
  const first = await ev('window.vetroState.snapshots[0]');
  measures.home = { wallSecs: (Date.now() - t0) / 1000, snapshot: first };
  log(`home screen snapshot (${first.why}, ${(first.size / 2 ** 20).toFixed(0)} MiB) after ${measures.home.wallSecs.toFixed(0)} s`);
  await until('adb', "window.vetroAndroid.state().adb.state === 'ready'", 2000);
  if (settleMs) {
    const ts = Date.now();
    for (;;) {
      const out = await sh('cat /proc/loadavg; top -b -n 1 -m 6 | tail -n 6');
      const load = Number(out.split(' ')[0]);
      (measures.settle ??= []).push({ wallSecs: (Date.now() - t0) / 1000, load });
      log(`settling: load ${load}: ${out.split('\n').slice(1).map((l) => l.trim().split(/\s+/).slice(-1)[0]).filter(Boolean).join(' ')}`);
      if (load < settleLoad || Date.now() - ts > settleMs) break;
      await sleep(30_000);
    }
  }
  if (warm) {
    const tw = Date.now();
    const { width, height } = await ev('({ width: document.getElementById("screen").width, height: document.getElementById("screen").height })');
    for (const c of ANDROID_WARMUP(width, height)) log(`warm-up: ${c}: ${(await sh(c)).trim().split('\n').slice(-1)[0] ?? ''}`);
    measures.warmup = { ms: Date.now() - tw };
  }
  if (compact) {
    const tc = Date.now();
    const out = (await sh(ANDROID_COMPACT)).trim();
    measures.compaction = { ms: Date.now() - tc, output: out };
    log(`compaction in ${((Date.now() - tc) / 1000).toFixed(0)} s: ${out.split('\n').join(' | ')}`);
  }
  const n = await ev('window.vetroState.snapshots.length');
  await ev("document.getElementById('save').click()");
  await until('the requested snapshot', `window.vetroState.snapshots.length > ${n}`, 2000);
  const saved = await ev(`window.vetroState.snapshots[${n}]`);
  measures.saveMs = saved.saveMs;
  log(`snapshot saved (${(saved.size / 2 ** 20).toFixed(0)} MiB, ${(saved.saveMs / 1000).toFixed(1)} s)`);
  await sleep(2000);

  // The snapshot out of OPFS: <key>.json (the app's metadata) and <key>.snap.
  const files = await ev(`(async () => {
    const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('vetro-snapshots');
    const out = [];
    for await (const [name, h] of d.entries()) if (name.endsWith('.json')) out.push({ name, text: await (await h.getFile()).text() });
    return out;
  })()`);
  if (files.length !== 1) throw new Error(`OPFS has ${files.length} snapshots, expected 1: ${files.map((f) => f.name).join(', ')}`);
  const key = files[0].name.replace(/\.json$/, '');
  const appMeta = JSON.parse(files[0].text);
  const size = appMeta.size;
  log(`key ${key}, ${size} bytes`);
  mkdirSync(outDir, { recursive: true });
  const parts = [];
  const STEP = 8 << 20;
  for (let at = 0; at < size; at += STEP) {
    const b64 = await ev(`(async () => {
      const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('vetro-snapshots');
      const f = await (await d.getFileHandle('${key}.snap')).getFile();
      const b = new Uint8Array(await f.slice(${at}, ${Math.min(size, at + STEP)}).arrayBuffer());
      let s = '';
      for (let i = 0; i < b.length; i += 0x8000) s += String.fromCharCode.apply(null, b.subarray(i, i + 0x8000));
      return btoa(s);
    })()`);
    parts.push(Buffer.from(b64, 'base64'));
  }
  const bytes = Buffer.concat(parts);
  if (bytes.length !== size) throw new Error(`read ${bytes.length} bytes of ${size}`);
  writeFileSync(join(outDir, `${key}.snap`), bytes);
  const chunks = [];
  for (let at = 0; at < size; at += PREBUILT_CHUNK) chunks.push(createHash('sha256').update(bytes.subarray(at, Math.min(size, at + PREBUILT_CHUNK))).digest('hex'));
  const { size: _s, ...rest } = appMeta;
  const meta = { ...rest, why: 'prebuilt (home screen)', prebuilt: true };
  // The key's parts as the page computed them (androidKeyParts, in the Worker).
  const keyParts = await ev('window.vetroState.snapshotKey ?? null');
  const info = { format: PREBUILT_FORMAT, version: 1, key, parts: keyParts?.key === key ? keyParts.parts : null, size, sha256: createHash('sha256').update(bytes).digest('hex'), chunk: PREBUILT_CHUNK, chunks, meta, measures };
  writeFileSync(join(outDir, `${key}.json`), `${JSON.stringify(info, null, 1)}\n`);
  const shot = await ev("document.getElementById('screen').toDataURL('image/png')");
  writeFileSync(join(outDir, `${key}.png`), Buffer.from(shot.split(',')[1], 'base64'));
  log(`written ${join(outDir, `${key}.snap`)} and .json (sha256 ${info.sha256.slice(0, 12)}…)`);
} finally {
  await closeChrome(proc, cdp, profileDir);
  srv.close();
}
