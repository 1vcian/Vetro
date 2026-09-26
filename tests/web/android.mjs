// Android in Node/V8 (M5, ADR 0028): Vetro's AOSP image (target/aosp,
// tools/aosp/fetch.sh) booted with vetro-wasm and the JIT in V8, as in the
// app but without a browser. Long (tens of minutes): runs only with
// VETRO_ANDROID=1.
//
//   VETRO_ANDROID=1 node --max-old-space-size=8192 tests/web/android.mjs [options]
//
// Options:
//   --ram=MiB          guest RAM (default 3072)
//   --guest-secs=N     stops after N seconds of guest time (default 1500)
//   --until=boot       stops at sys.boot_completed (default: home + adb)
//   --no-jit           interpreter
//   --log=FILE         guest console (default target/aosp/node-android.log)
//   --save=FILE        saves the snapshot when the home screen is up (times and size)
//   --save-after-app=FILE  saves another snapshot after the app reacted to the touch
//   --restore=FILE     resumes from a --save snapshot (same RAM) instead of booting
//   --params=LINE      bootloader parameters (default ANDROID_PARAMS in web/node/android.mjs)
//   --aosp=DIR         images and disk (default target/aosp)
//   --compact          before the snapshot: drop_caches and free memory
//                      filled with zeros in the guest (an ADR 0028 experiment)
//   --apk=FILE         APK to install (default target/apps/tocco.apk, from
//                      tests/apps/tocco/build.sh)
//
// Prints the boot phases (guest and wall time), memory (WASM linear memory
// and process RSS) and disk counters. With --until=home (the default), after
// the boot: adb over GuestSocket (devices, shell, install of the test APK,
// am start), the pixel at the centre of the screen becomes the app's (blue,
// or with red and blue swapped), a touch on the touchscreen turns it orange.

import { open, stat, writeFile } from 'node:fs/promises';
import { deflateSync } from 'node:zlib';
import { closeSync, existsSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { DEV } from '../../web/node/vetro.mjs';
import { AdbClient } from '../../web/node/adb.mjs';
import { apkInfo } from '../../web/node/apk.mjs';
import { DiskFeeder, MemoryCache } from '../../web/node/disk.mjs';
import { ANDROID_PARAMS, BootProgress, colorSeen, HOME_QUERY } from '../../web/node/android.mjs';
import { Fail, loadVetro, root } from './lib.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: long Android test in Node (VETRO_ANDROID=1 to run it)');
  process.exit(0);
}

const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const flag = (name) => process.argv.includes(`--${name}`);
const ramMiB = Number(arg('ram', 3072));
const guestSecs = Number(arg('guest-secs', 1500));
const until = arg('until', 'home');
const jit = !flag('no-jit');
const logPath = arg('log', join(root, 'target/aosp/node-android.log'));
const aosp = arg('aosp', join(root, 'target/aosp'));
const compact = flag('compact');
const saveAfterApp = arg('save-after-app', null);
const savePath = arg('save', null);
const restorePath = arg('restore', null);
const params = arg('params', ANDROID_PARAMS);
const apkPath = arg('apk', join(root, 'target/apps/tocco.apk'));

/** A disk source from a local file (Node only). */
class FileSource {
  stats = { requests: 0, bytes: 0 };
  constructor(path) {
    this.path = path;
  }
  async open() {
    this.fh = await open(this.path, 'r');
    const st = await stat(this.path);
    this.size = st.size;
    this.key = `file:${this.path}|${st.size}|${st.mtimeMs}`;
    return this;
  }
  async read(offset, length) {
    const buf = new Uint8Array(length);
    const { bytesRead } = await this.fh.read(buf, 0, length, offset);
    if (bytesRead !== length) throw new Error(`${this.path}: read ${bytesRead} bytes instead of ${length}`);
    this.stats.requests++;
    this.stats.bytes += length;
    return buf;
  }
}

const mib = (n) => (n / 2 ** 20).toFixed(0);

async function main() {
  for (const f of ['out/boot.img', 'out/vendor_boot.img', 'out/init_boot.img', 'disk.img']) {
    if (!existsSync(join(aosp, f))) throw new Fail(`target/aosp/${f} missing: tools/aosp/fetch.sh (or by hand from R2) and tools/aosp/mkdisk.sh`);
  }
  const { exports } = await loadVetro();
  const { Machine } = await import('../../web/node/vetro.mjs');
  // Like the app: a touchscreen instead of the tablet.
  const m = new Machine(exports, { ramSize: BigInt(ramMiB) << 20n, devices: DEV.GPU | DEV.KEYBOARD | DEV.MULTITOUCH | DEV.NET });
  const feeder = new DiskFeeder(m);
  const disk = await new FileSource(join(aosp, 'disk.img')).open();
  feeder.add(disk, { cache: new MemoryCache(), blockSize: 1 << 20, maxBlocks: 64, readahead: 1 });
  const progress = new BootProgress();
  if (restorePath) {
    const t = performance.now();
    const bytes = readFileSync(restorePath);
    m.snapshotRestoreStream(bytes.length, (view, at) => view.set(bytes.subarray(at, at + view.length)));
    console.log(`restored from ${restorePath}: ${mib(bytes.length)} MiB in ${(performance.now() - t).toFixed(0)} ms, at ${m.steps} instructions`);
    progress.feed('sys-boot-completed-set\n', Number(m.guestNs) / 1e9);
  } else {
    const read = (f) => new Uint8Array(readFileSync(join(aosp, 'out', f)));
    const desc = m.loadAndroid({ boot: read('boot.img'), vendorBoot: read('vendor_boot.img'), initBoot: read('init_boot.img'), params });
    console.log(`vetro: ${desc.split(';')[0]}`);
  }
  if (jit) m.setJit();
  const t0 = performance.now();
  let log = '';
  let lastReport = 0;
  let saved = false;
  // After the boot: the adb and touch script, in steps between quanta.
  let flow = null;
  let bootedNs = null;
  const jitInfo = () => {
    const j = m.jitStats();
    return j ? `, JIT ${j.modules} modules, ${j.blocks} blocks, ${j.resets} resets` : '';
  };
  const report = (why) => {
    const mem = process.memoryUsage();
    const wasmMem = exports.memory.buffer.byteLength;
    const d = m.diskStats(0);
    console.log(`[${why}] guest ${(Number(m.guestNs) / 1e9).toFixed(1)} s, wall ${((performance.now() - t0) / 1000).toFixed(0)} s, ` +
      `${(Number(m.steps) / 1e6).toFixed(0)} M instr., WASM ${mib(wasmMem)} MiB, RSS ${mib(mem.rss)} MiB, ` +
      `disk: ${d.fills} blocks delivered, ${d.cachedBlocks} in memory, cow ${d.dirtyClusters} clusters (${mib(d.dirtyClusters * 4096)} MiB), ` +
      `source ${mib(disk.stats.bytes)} MiB${jitInfo()}`);
  };
  for (;;) {
    const stop = m.run(1_000_000);
    if (stop === 'Blocked') {
      await feeder.serve();
      continue;
    }
    const out = m.consoleRead();
    if (out.length) {
      const text = Buffer.from(out).toString('latin1');
      log += text;
      for (const ev of progress.feed(text, Number(m.guestNs) / 1e9)) {
        console.log(`phase: ${ev.phase} (${ev.label}) at ${ev.guestSecs.toFixed(1)} s of guest time, ${((performance.now() - t0) / 1000).toFixed(0)} s wall`);
      }
    }
    if (stop !== 'Budget') {
      report(stop);
      throw new Fail(`the machine stopped: ${stop}`);
    }
    const now = performance.now();
    if (now - lastReport > 30_000) {
      lastReport = now;
      report('status');
      await writeFile(logPath, log);
      screenshot(m, 'node-live.png', true);
    }
    if (flow?.wantSave && (!saved || typeof flow.wantSave === 'string')) {
      const path = typeof flow.wantSave === 'string' ? flow.wantSave : savePath;
      flow.wantSave = false;
      saved = true;
      report('before the snapshot');
      const ts = performance.now();
      const fh = openSync(path, 'w');
      const size = m.snapshotSaveTo((b, at) => writeSync(fh, b, 0, b.length, at));
      closeSync(fh);
      const saveMs = performance.now() - ts;
      console.log(`snapshot: ${mib(size)} MiB (${size} bytes) in ${saveMs.toFixed(0)} ms (chunked, including the write), at ${m.steps} instructions, module memory ${mib(m.memoryBytes)} MiB`);
      report('after the snapshot');
      flow.saved();
    }
    if (until === 'boot' && progress.phase === 'booted') break;
    if (progress.phase === 'booted') {
      bootedNs ??= m.guestNs;
      // The home screen gets a few seconds of guest time to draw.
      if (!flow && m.guestNs - bootedNs >= 10_000_000_000n) flow = homeFlow(m, t0, !!savePath && !saved);
      flow?.pump?.();
      // The script waits on Promises and conditions: give it a turn.
      if (flow) await new Promise((ok) => setImmediate(ok));
      if (flow?.result) {
        if (flow.result instanceof Error) throw flow.result;
        break;
      }
    }
    if (Number(m.guestNs) / 1e9 >= guestSecs) break;
  }
  report('end');
  await writeFile(logPath, log);
}

/** RGBA pixel at the centre of the screen (or null with the scanout off). */
function center(m) {
  const size = m.displaySize();
  const px = m.displayPixels();
  if (!size || !px) return null;
  const o = ((size.height >> 1) * size.width + (size.width >> 1)) * 4;
  return [px[o], px[o + 1], px[o + 2]];
}

/** Keeps the virtual machine's screen on and wakes it (the same command as the Worker). */
const WAKE = 'svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard';


/** The scanout as a PNG in target/aosp (to look at it). */
function screenshot(m, name, quiet = false) {
  const size = m.displaySize();
  const px = m.displayPixels();
  if (!size || !px) return;
  const { width: w, height: h } = size;
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0;
    Buffer.from(px.buffer, px.byteOffset + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  }
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const c = Buffer.alloc(4);
    c.writeUInt32BE(crc(td));
    return Buffer.concat([len, td, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const png = Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
  writeFileSync(join(aosp, name), png);
  if (!quiet) console.log(`screenshot: target/aosp/${name}`);
}
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];

/**
 * adb and touch after the boot. The machine runs in the main loop; here we
 * wait on the client's Promises (which advance with `pump`) and on screen
 * conditions. Returns { pump, result } (result: 'done' or the error).
 */
function homeFlow(m, t0, save) {
  const state = { adb: null, sock: null };
  let savedOk;
  const savedP = new Promise((ok) => (savedOk = ok));
  const secs = () => ((performance.now() - t0) / 1000).toFixed(0);
  const waitGuest = async (what, pred, guestSecs) => {
    const limit = m.guestNs + BigInt(guestSecs * 1e9);
    while (!pred()) {
      if (m.guestNs > limit) throw new Fail(`${what}: not reached in ${guestSecs} s of guest time (centre ${center(m)})`);
      await new Promise((ok) => setImmediate(ok));
    }
  };
  const run = async () => {
    const home = center(m);
    console.log(`start: pixel at the centre ${home}, screen ${JSON.stringify(m.displaySize())}`);
    screenshot(m, 'node-home.png');
    for (let attempt = 0; ; attempt++) {
      state.sock = m.connectGuest(5555);
      state.adb = new AdbClient(state.sock);
      try {
        const banner = await state.adb.connect();
        console.log(`adb: connected (${secs()} s wall): ${banner.props['ro.product.model']}, ${banner.features.length} features`);
        break;
      } catch (e) {
        state.sock.release();
        state.adb = null;
        if (attempt > 20) throw e;
        await waitGuest('waiting for adbd', () => false, 3).catch(() => {});
      }
    }
    const adb = state.adb;
    console.log(`adb devices: ${JSON.stringify(await adb.devices())}`);
    // A virtual machine: the screen stays on (as the app's Worker does).
    console.log(`screen on: ${JSON.stringify(await adb.shell(WAKE))}`);
    await waitGuest('screen on', () => false, 3).catch(() => {});
    // The home screen: the focused window is the launcher (before it there is
    // FallbackHome, "Phone is starting").
    const b0 = m.guestNs;
    let top = '';
    for (;;) {
      top = (await adb.shell(HOME_QUERY)).stdout.trim();
      if (/launcher/i.test(top)) break;
      if (process.env.VETRO_ANDROID_DEBUG) console.log(`focused: ${JSON.stringify(top)} (${(Number(m.guestNs) / 1e9).toFixed(0)} s of guest time)`);
      await waitGuest('waiting for the home screen', () => false, 5).catch(() => {});
      if (m.guestNs - b0 > 3000_000_000_000n) throw new Fail(`home screen never came: ${top}`);
    }
    console.log(`home: ${top} at ${(Number(m.guestNs) / 1e9).toFixed(1)} s of guest time, ${secs()} s wall`);
    await waitGuest('home drawn', () => false, 5).catch(() => {});
    screenshot(m, 'node-home-accesa.png');
    if (compact) {
      // The page cache is dropped and free memory is filled with zeros (a
      // file in /dev, which is tmpfs, then deleted): zero pages do not go into
      // the snapshot.
      const tc = performance.now();
      const r = await adb.shell("su 0 sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches; free=$(awk \"/MemFree/ {print int(\\$2/1024) - 96}\" /proc/meminfo); dd if=/dev/zero of=/dev/vetro-zeri bs=1M count=$free 2>&1 | tail -1; rm -f /dev/vetro-zeri; grep -E \"MemFree|^Cached\" /proc/meminfo'");
      console.log(`compaction: ${JSON.stringify(r)} (${secs()} s wall, ${((performance.now() - tc) / 1000).toFixed(1)} s)`);
    }
    if (save) {
      holder.wantSave = true;
      await savedP;
    }
    const r = await adb.shell('getprop sys.boot_completed; getprop ro.build.version.release; cat /proc/meminfo | head -3');
    console.log(`adb shell: ${JSON.stringify(r)}`);
    if (!existsSync(apkPath)) throw new Fail(`${apkPath} missing: tests/apps/tocco/build.sh`);
    const apk = new Uint8Array(readFileSync(apkPath));
    const info = await apkInfo(apk);
    let t = performance.now();
    const out = await adb.install(apk, { name: `${info.package}.apk` });
    console.log(`adb install ${info.package}: ${out} (${((performance.now() - t) / 1000).toFixed(1)} s wall)`);
    t = performance.now();
    const st = await adb.shell(`am start -W -n ${info.package}/${info.launcher}`);
    console.log(`am start: ${st.stdout.trim().split('\n').join(' | ')} (${((performance.now() - t) / 1000).toFixed(1)} s wall)`);
    await waitGuest('the app on screen (blue centre)', () => colorSeen(center(m), BLU), 300);
    console.log(`app on screen: centre ${center(m)} (${colorSeen(center(m), BLU) === 'bgr' ? 'red and blue swapped by the scanout' : 'correct colours'})`);
    screenshot(m, 'node-app-1.png');
    const size = m.displaySize();
    m.touch(0, [16384, 16384]);
    await waitGuest('touch (down)', () => false, 0.2).catch(() => {});
    m.touch(0, null);
    await waitGuest('the app received the touch (orange centre)', () => colorSeen(center(m), ARANCIONE), 120);
    screenshot(m, 'node-app-2.png');
    const log = await adb.shell('logcat -d -s vetro-tocco:I | tail -3');
    console.log(`touch received: centre ${center(m)} on ${size.width}x${size.height}; logcat: ${log.stdout.trim().split('\n').join(' | ')}`);
    if (saveAfterApp) {
      holder.wantSave = saveAfterApp;
      await new Promise((ok) => (holder.saved = ok));
    }
  };
  const holder = { pump: () => state.adb?.pump(), result: null, wantSave: false, saved: () => savedOk() };
  run().then(() => (holder.result = 'done'), (e) => (holder.result = e));
  return holder;
}

main().catch((e) => {
  console.error(e instanceof Fail ? `FAILED: ${e.message}` : e);
  process.exit(1);
});
