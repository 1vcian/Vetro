#!/usr/bin/env node
// Android performance in Node/V8 (M4): the real workload, measured phase by
// phase. Restores the prebuilt home-screen snapshot (ADR 0031) on the app's
// machine, then, like the app's Worker: adb over GuestSocket, the launcher
// idle, `pm install` of a small catalog app (ADR 0033, Flowit by default),
// `am start` until the app is focused, the app idle. Or, with --cold, a cold
// boot for a number of guest seconds.
//
//   node --max-old-space-size=8192 tools/aosp/android-perf.mjs [options]
//
// Options:
//   --wasm=FILE      vetro-wasm (default target/wasm32-unknown-unknown/release)
//   --manifest=URL   image manifest (default: the app's, on R2)
//   --snap=FILE      the prebuilt snapshot (default: target/aosp/cache/<key>.snap,
//                    downloaded from R2 the first time)
//   --cache=DIR      disk block cache on the local disk (default target/aosp/cache/disk):
//                    the image is read from R2 only once
//   --app=ID         catalog app to install (default flowit)
//   --idle=S         guest seconds of launcher (and app) idle (default 20)
//   --profile        interpreter instruction classes (VETRO_JIT_PROFILE=1 does the same;
//                    costly: not for timings)
//   --names          region functions named r<el>_<pc> for V8 CPU profiles
//                    (tools/aosp/perf-report.mjs; no other cost)
//   --no-jit         interpreter only
//   --bg-compile     JIT modules compiled in a Worker (ADR 0038)
//   --threshold=N    JIT hot threshold (default 64, the app's)
//   --jit-budget=MIB live JIT code budget in V8 (default CODE_BUDGET of jit-engine.mjs)
//   --cold=S         cold boot for S guest seconds instead of the restore
//   --restore-only   stops after the restore (to profile it)
//   --stop-after=P   stops after phase P (e.g. "adb ready")
//   --samples        guest PC samples (one per quantum of 1 M instructions:
//                    weighted by instructions) and, at the end, the guest's
//                    /proc/kallsyms and executable mappings (for
//                    tools/aosp/perf-report.mjs; changes the guest's work
//                    only after the last phase)
//   --diag           after the launcher idle: `top` in the guest (changes the guest's work)
//   --out=FILE       measurements as JSON (default target/aosp/perf.json)
//
// The host's actions depend only on the guest (instruction counts), never on
// wall time: the same wasm gives the same instructions in every phase, and two
// vetro-wasm builds with the same guest behaviour give the same instructions
// too. What changes is the wall time. For a V8 profile add
// `--cpu-prof --cpu-prof-dir=DIR` to node.

import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readSync, writeFileSync, writeSync, fstatSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DEV, instantiate, Machine } from '../../web/node/vetro.mjs';
import { AdbClient } from '../../web/node/adb.mjs';
import { DiskFeeder, LayoutSource } from '../../web/node/disk.mjs';
import { ANDROID_DISK, ANDROID_MACHINE, ANDROID_PARAMS, ANDROID_WAKE, DEFAULT_MANIFEST, HOME_QUERY, machineDevices } from '../../web/node/android.mjs';
import { androidSnapshotKey, prebuiltSnapUrl } from '../../web/node/prebuilt.mjs';
import { CATALOG_URL, loadCatalog } from '../../web/node/catalog.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '../..');
const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const flag = (name) => process.argv.includes(`--${name}`);
const wasmPath = arg('wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm'));
const manifestUrl = arg('manifest', DEFAULT_MANIFEST);
const cacheDir = arg('cache', join(root, 'target/aosp/cache/disk'));
const appId = arg('app', 'flowit');
const idleSecs = Number(arg('idle', 20));
const profile = flag('profile') || process.env.VETRO_JIT_PROFILE === '1';
const jit = !flag('no-jit');
const cold = arg('cold', null);
const outPath = arg('out', join(root, 'target/aosp/perf.json'));

const t0 = performance.now();
const cpu = () => {
  const u = process.cpuUsage();
  return (u.user + u.system) / 1000;
};
const wall = () => (performance.now() - t0) / 1000;
const log = (s) => console.log(`[${wall().toFixed(1)} s] ${s}`);
const mib = (n) => (n / 2 ** 20).toFixed(0);

/** Disk blocks cached in a sparse file plus a presence map (Node). */
class FileCache {
  stats = { hits: 0, puts: 0 };
  constructor(dir, key, blockSize, blocks) {
    mkdirSync(dir, { recursive: true });
    const name = createHash('sha256').update(`${key}|${blockSize}`).digest('hex').slice(0, 24);
    this.blockSize = blockSize;
    this.data = openSync(join(dir, `${name}.img`), existsSync(join(dir, `${name}.img`)) ? 'r+' : 'w+');
    const mapPath = join(dir, `${name}.map`);
    this.bits = new Uint8Array(Math.ceil(blocks / 8));
    if (existsSync(mapPath)) this.bits.set(readFileSync(mapPath).subarray(0, this.bits.length));
    this.map = openSync(mapPath, existsSync(mapPath) ? 'r+' : 'w+');
    if (fstatSync(this.map).size < this.bits.length) writeSync(this.map, this.bits, 0, this.bits.length, 0);
  }
  has(b) {
    return (this.bits[b >> 3] & (1 << (b & 7))) !== 0;
  }
  get(b, length) {
    this.stats.hits++;
    const buf = new Uint8Array(length);
    readSync(this.data, buf, 0, length, b * this.blockSize);
    return buf;
  }
  put(b, bytes) {
    this.stats.puts++;
    writeSync(this.data, bytes, 0, bytes.length, b * this.blockSize);
    this.bits[b >> 3] |= 1 << (b & 7);
    writeSync(this.map, this.bits, b >> 3, 1, b >> 3);
  }
}

async function fetchTo(url, path) {
  log(`downloading ${url}`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: status ${res.status}`);
  const fh = openSync(`${path}.part`, 'w');
  let at = 0;
  for await (const chunk of res.body) {
    writeSync(fh, chunk, 0, chunk.length, at);
    at += chunk.length;
  }
  closeSync(fh);
  const { renameSync } = await import('node:fs');
  renameSync(`${path}.part`, path);
  log(`downloaded ${mib(at)} MiB`);
}

async function main() {
  const manifest = await (await fetch(manifestUrl)).json();
  const images = ['boot.img', 'vendor_boot.img', 'init_boot.img'].map((path) => {
    const f = manifest.files.find((x) => x.path === path);
    return { ...f, url: new URL(path, manifestUrl).href };
  });
  const budget = arg('jit-budget', null);
  const { exports, jit: engine } = await instantiate(readFileSync(wasmPath), budget ? { jitBudget: Number(budget) << 20 } : {});
  if (flag('bg-compile')) await engine.startBackground();
  const M = ANDROID_MACHINE;
  const devices = machineDevices(DEV, M);
  const m = new Machine(exports, { ramSize: BigInt(M.ramMiB) << 20n, devices, width: M.width, height: M.height });
  const feeder = new DiskFeeder(m);
  const layout = await new LayoutSource(new URL('web/disk.json', manifestUrl).href).open();
  const cache = new FileCache(cacheDir, layout.key, ANDROID_DISK.blockSize, Math.ceil(layout.size / ANDROID_DISK.blockSize));
  feeder.add(layout, { cache, ...ANDROID_DISK });
  const { key } = await androidSnapshotKey(m, { machine: M, devices, manifest, images, params: ANDROID_PARAMS, layout });
  const res = { wasm: wasmPath, key, jit, profile, bgCompile: flag('bg-compile'), phases: [] };
  // The app is fetched before the machine runs: the host's actions must not
  // wait on the network while the guest advances.
  const cat = await loadCatalog(CATALOG_URL);
  const app = cat.apps.find((a) => a.id === appId);
  if (!app) throw new Error(`no ${appId} in the catalog`);
  const apkPath = join(root, `target/aosp/cache/${appId}-${app.versionCode}.apk`);
  if (!existsSync(apkPath)) {
    mkdirSync(dirname(apkPath), { recursive: true });
    await fetchTo(app.apk, apkPath);
  }
  const apk = new Uint8Array(readFileSync(apkPath));

  if (cold) {
    const bytes = [];
    for (const f of images) bytes.push(new Uint8Array(await (await fetch(f.url)).arrayBuffer()));
    m.loadAndroid({ boot: bytes[0], vendorBoot: bytes[1], initBoot: bytes[2], params: ANDROID_PARAMS });
  } else {
    const snapPath = arg('snap', join(root, `target/aosp/cache/${key}.snap`));
    if (!existsSync(snapPath)) {
      mkdirSync(dirname(snapPath), { recursive: true });
      await fetchTo(prebuiltSnapUrl(manifestUrl, key), snapPath);
    }
    const bytes = readFileSync(snapPath);
    const tr = performance.now();
    const cr = cpu();
    m.snapshotRestoreStream(bytes.length, (view, at) => view.set(bytes.subarray(at, at + view.length)));
    res.restoreMs = performance.now() - tr;
    res.restoreCpuMs = cpu() - cr;
    log(`restored ${mib(bytes.length)} MiB in ${res.restoreMs.toFixed(0)} ms (${res.restoreCpuMs.toFixed(0)} ms CPU), at ${m.steps} instructions`);
  }
  if (flag('restore-only')) {
    writeFileSync(outPath, `${JSON.stringify(res, null, 1)}\n`);
    return;
  }
  if (jit) {
    const withFlags = !!exports.vetro_machine_set_jit_with;
    m.setJit(Number(arg('threshold', 64)), 16, { profile: profile && withFlags, names: flag('names') && withFlags });
  }

  // Phase accounting.
  let mark = null;
  // Older vetro-wasm builds (for "before" measurements) lack the counters.
  const perf = () => (exports.vetro_perf ? m.perf() : null);
  const snap = () => ({ wall: performance.now(), cpu: cpu(), steps: m.steps, wait: feeder.stats.waitMs, jit: m.jitStats(), perf: perf() });
  const begin = () => (mark = snap());
  const end = (name, extra = {}) => {
    const now = snap();
    const d = (a, b) => (a && b ? Object.fromEntries(Object.keys(b).map((k) => [k, (b[k] ?? 0) - (a[k] ?? 0)])) : null);
    const steps = Number(now.steps - mark.steps);
    const p = {
      name,
      wallMs: Math.round(now.wall - mark.wall),
      // CPU of all the process's threads (V8 compiles on background threads):
      // steadier than wall time on a shared machine.
      cpuMs: Math.round(now.cpu - mark.cpu),
      diskWaitMs: Math.round(now.wait - mark.wait),
      steps,
      guestSecs: steps / 1e8,
      jit: d(mark.jit, now.jit),
      perf: d(mark.perf, now.perf),
      rssMiB: Math.round(process.memoryUsage().rss / 2 ** 20),
      ...extra,
    };
    const ran = steps - (p.perf?.wfiSteps ?? 0);
    const cpuMs = p.wallMs - p.diskWaitMs;
    p.mips = cpuMs > 0 ? +(ran / cpuMs / 1000).toFixed(1) : null;
    res.phases.push(p);
    if (arg('stop-after', null) === name) stopNow = true;
    samplePhase = `after ${name}`.replace(/ /g, '_');
    const j = p.jit;
    log(`== ${name}: ${(p.wallMs / 1000).toFixed(1)} s wall, ${(p.cpuMs / 1000).toFixed(1)} s CPU (disk wait ${(p.diskWaitMs / 1000).toFixed(1)} s), ${p.guestSecs.toFixed(1)} s guest, RSS ${p.rssMiB} MiB, ` +
      `${(ran / 1e6).toFixed(0)} M executed (${p.mips} MIPS), interp ${((p.perf?.interpSteps ?? 0) / 1e6).toFixed(1)} M, WFI skip ${((p.perf?.wfiSteps ?? 0) / 1e6).toFixed(0)} M` +
      (j ? `, jit ${(j.jitSteps / 1e6).toFixed(0)} M, ${j.blocks} regions, ${mib(j.wasmBytes)} MiB wasm, epochs ${j.epochsRegs}/${j.epochsTlbi}/${j.epochsCode}, ` +
        `resolves ${j.resolves}, host ld/st ${j.hostLds}/${j.hostSts}, faults ${j.faults}, svcs ${j.svcs}, runs ${j.runs}, calls ${j.calls}, ` +
        `yields ${j.yields}, resets ${j.resets}` + (j.evictions !== undefined ? `, evictions ${j.evictions} (${j.evictedModules} modules), regime switches ${j.regimeSwitches}, memo ${j.memoHits}` : '') : ''));
    mark = now;
    return p;
  };

  // The loop: quanta of 1 M instructions, disks served, host flow between quanta.
  let stopNow = false;
  let flow = null;
  let flowDone = false;
  let flowError = null;
  const guest = () => m.steps;
  const latin1 = new TextDecoder('latin1');
  let consoleTail = '';
  const samples = flag('samples') ? new Map() : null;
  let samplePhase = 'start';
  const step = async () => {
    const stop = m.run(1_000_000);
    if (samples && stop === 'Budget') {
      const t = m.registersText();
      const pc = /pc +([0-9a-f]+)/.exec(t)[1];
      const el = /el (\d)/.exec(t)[1];
      const asid = /ttbr0_el1 +([0-9a-f]{4})/.exec(t)[1];
      const k = `${samplePhase} ${el} ${asid} ${pc}`;
      samples.set(k, (samples.get(k) ?? 0) + 1);
    }
    if (stop === 'Blocked') {
      await feeder.serve();
      return;
    }
    const out = m.consoleRead();
    if (out.length) consoleTail = (consoleTail + latin1.decode(out)).slice(-8192);
    if (stop !== 'Budget') throw new Error(`the machine stopped: ${stop}`);
  };
  /** Runs the machine until `pred()` or `secs` of guest time (then throws unless `soft`). */
  const waitGuest = (what, pred, secs, soft = false) => new Promise((ok, ko) => {
    const limit = guest() + BigInt(Math.round(secs * 1e8));
    waiters.push({ pred, limit, ok, ko: soft ? ok : () => ko(new Error(`${what}: not within ${secs} s of guest time`)) });
  });
  const waiters = [];
  let adb = null;

  const script = async () => {
    begin();
    // adb: connect (retry every 3 guest seconds), keep the screen on.
    for (let attempt = 0; ; attempt++) {
      const sock = m.connectGuest(5555);
      adb = new AdbClient(sock);
      try {
        await adb.connect();
        break;
      } catch (e) {
        sock.release();
        adb = null;
        if (attempt > 40) throw e;
        await waitGuest('adb retry', () => false, 3, true);
      }
    }
    await adb.shell(ANDROID_WAKE);
    end('adb ready');
    await waitGuest('launcher idle', () => false, idleSecs, true);
    end(`launcher idle ${idleSecs} s`);
    if (flag('diag')) {
      // What the guest does while "idle" (changes the guest's work: not for timings).
      const t = await adb.shell('top -b -n 1 -m 15; cat /proc/loadavg; cat /sys/devices/system/cpu/cpuidle/current_driver 2>&1');
      console.log(t.stdout);
      end('diag');
    }
    begin();
    await adb.push(`/data/local/tmp/${app.package}.apk`, apk);
    end('adb push');
    const r = await adb.shell(`pm install -r /data/local/tmp/${app.package}.apk; e=$?; rm -f /data/local/tmp/${app.package}.apk; exit $e`);
    if (!/\bSuccess\b/.test(r.stdout + r.stderr)) throw new Error(`pm install: ${r.stdout}${r.stderr}`);
    end('pm install');
    const act = (await adb.shell(`cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER ${app.package} | tail -n 1`)).stdout.trim();
    end('resolve-activity');
    const st = await adb.shell(`am start -W -n '${act}'`);
    end('am start -W', { output: st.stdout.trim().split('\n').slice(-4).join(' | ') });
    for (;;) {
      const f = await adb.shell(HOME_QUERY);
      if (f.stdout.includes(app.package)) break;
      await waitGuest('focus poll', () => false, 2, true);
    }
    end('focused');
    await waitGuest('app idle', () => false, idleSecs, true);
    end(`app idle ${idleSecs} s`);
    if (profile) {
      // Before the dump below (it changes the guest's work).
      res.profileReport = m.jitProfile(80);
      writeFileSync(outPath.replace(/\.json$/, '.profile.txt'), res.profileReport);
    }
    if (samples) {
      // Where the guest's code lives: kernel symbols and every process's
      // executable mappings (zygote's children share their libraries' addresses).
      const sh = async (c) => {
        const r = await adb.shell(c);
        return r.stdout;
      };
      const base = outPath.replace(/\.json$/, '');
      writeFileSync(`${base}.samples`, [...samples].map(([k, v]) => `${v} ${k}`).join('\n') + '\n');
      // One grep over all the mappings and one ps (the guest is slow: no
      // process per pid, no shell loop reading byte by byte).
      const maps = await sh(`su 0 sh -c "ps -A -o PID,NAME; grep -H ' r-xp ' /proc/[0-9]*/maps"`);
      writeFileSync(`${base}.maps`, maps);
      // Text symbols only, compressed in the guest (the full list is ~10 MiB
      // through adb at guest speed).
      const ks = await sh(`su 0 sh -c 'grep " [tT] " /proc/kallsyms | gzip -1 | base64'`);
      writeFileSync(`${base}.kallsyms`, gunzipSync(Buffer.from(ks.replace(/\s+/g, ''), 'base64')));
      log(`samples: ${samples.size} keys, kallsyms ${ks.length} bytes (base64 gzip), maps ${maps.length} bytes`);
      end('dump');
    }
  };

  if (cold) {
    begin();
    const limit = BigInt(Math.round(Number(cold) * 1e8));
    let last = performance.now();
    while (m.steps < limit) {
      await step();
      if (performance.now() - last > 60_000) {
        last = performance.now();
        log(`guest ${(Number(m.steps) / 1e8).toFixed(0)} s, ${mib(layout.stats.bytes)} MiB from the source`);
      }
    }
    end(`cold boot ${cold} s`);
  } else {
    flow = script().then(() => (flowDone = true), (e) => (flowError = e));
    while (!flowDone && !stopNow) {
      if (flowError) throw flowError;
      await step();
      adb?.pump();
      for (let i = waiters.length - 1; i >= 0; i--) {
        const w = waiters[i];
        if (w.pred()) {
          waiters.splice(i, 1);
          w.ok();
        } else if (guest() >= w.limit) {
          waiters.splice(i, 1);
          w.ko();
        }
      }
      // Give the script's promises a turn.
      await new Promise((ok) => setImmediate(ok));
    }
    if (flowError) throw flowError;
  }
  res.totalWallMs = Math.round(performance.now() - t0);
  res.engine = engine.stats;
  log(`JS engine: ${JSON.stringify(engine.stats)}`);
  res.steps = String(m.steps);
  if (profile) console.log(res.profileReport);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, `${JSON.stringify(res, null, 1)}\n`);
  log(`measurements: ${outPath}`);
  void flow;
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
