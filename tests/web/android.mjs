// Android in Node/V8 (M5, ADR 0028): l'immagine AOSP di Vetro (target/aosp,
// tools/aosp/fetch.sh) avviata con vetro-wasm e il JIT in V8, come nell'app
// ma senza browser. Lungo (decine di minuti): gira solo con VETRO_ANDROID=1.
//
//   VETRO_ANDROID=1 node --max-old-space-size=4096 tests/web/android.mjs [opzioni]
//
// Opzioni:
//   --ram=MiB          RAM del guest (default 3072)
//   --guest-secs=N     si ferma dopo N secondi di tempo del guest (default 1500)
//   --until=boot       si ferma a sys.boot_completed (default: home + adb)
//   --no-jit           interprete
//   --log=FILE         console del guest (default target/aosp/node-android.log)
//   --save=FILE        salva lo snapshot quando la home è a schermo (tempi e dimensione)
//   --restore=FILE     riparte da uno snapshot di --save (stessa RAM) invece di avviare
//   --apk=FILE         APK da installare (default target/apps/tocco.apk, da
//                      tests/apps/tocco/build.sh)
//
// Stampa le fasi dell'avvio (tempo del guest e reale), la memoria (lineare di
// WASM e RSS del processo) e i contatori dei dischi. Con --until=home (il
// default), dopo l'avvio: adb via GuestSocket (devices, shell, install
// dell'APK di prova, am start), il pixel al centro dello schermo diventa
// quello dell'app (blu), un tocco sul touchscreen lo fa diventare arancione.

import { open, stat, writeFile } from 'node:fs/promises';
import { deflateSync } from 'node:zlib';
import { closeSync, existsSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { DEV } from '../../web/node/vetro.mjs';
import { AdbClient } from '../../web/node/adb.mjs';
import { apkInfo } from '../../web/node/apk.mjs';
import { DiskFeeder, MemoryCache } from '../../web/node/disk.mjs';
import { BootProgress } from '../../web/node/android.mjs';
import { Fail, loadVetro, root } from './lib.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: test lungo di Android in Node (VETRO_ANDROID=1 per eseguirlo)');
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
const aosp = join(root, 'target/aosp');
const savePath = arg('save', null);
const restorePath = arg('restore', null);
const apkPath = arg('apk', join(root, 'target/apps/tocco.apk'));

/** Una sorgente di disco da un file locale (solo Node). */
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
    if (bytesRead !== length) throw new Error(`${this.path}: ${bytesRead} byte letti invece di ${length}`);
    this.stats.requests++;
    this.stats.bytes += length;
    return buf;
  }
}

const mib = (n) => (n / 2 ** 20).toFixed(0);

async function main() {
  for (const f of ['out/boot.img', 'out/vendor_boot.img', 'out/init_boot.img', 'disk.img']) {
    if (!existsSync(join(aosp, f))) throw new Fail(`target/aosp/${f} mancante: tools/aosp/fetch.sh (o a mano da R2) e tools/aosp/mkdisk.sh`);
  }
  const { exports } = await loadVetro();
  const { Machine } = await import('../../web/node/vetro.mjs');
  // Come l'app: touchscreen al posto del tablet.
  const m = new Machine(exports, { ramSize: BigInt(ramMiB) << 20n, devices: DEV.GPU | DEV.KEYBOARD | DEV.MULTITOUCH | DEV.NET });
  const feeder = new DiskFeeder(m);
  const disk = await new FileSource(join(aosp, 'disk.img')).open();
  feeder.add(disk, { cache: new MemoryCache(), blockSize: 1 << 20, maxBlocks: 64, readahead: 1 });
  const progress = new BootProgress();
  if (restorePath) {
    const t = performance.now();
    const bytes = readFileSync(restorePath);
    await m.snapshotRestoreWith(bytes.length, (view) => view.set(bytes));
    console.log(`ripristinato da ${restorePath}: ${mib(bytes.length)} MiB in ${(performance.now() - t).toFixed(0)} ms, a ${m.steps} istruzioni`);
    progress.feed('sys-boot-completed-set\n', Number(m.guestNs) / 1e9);
  } else {
    const read = (f) => new Uint8Array(readFileSync(join(aosp, 'out', f)));
    const desc = m.loadAndroid({ boot: read('boot.img'), vendorBoot: read('vendor_boot.img'), initBoot: read('init_boot.img'), params: 'nokaslr' });
    console.log(`vetro: ${desc.split(';')[0]}`);
  }
  if (jit) m.setJit();
  const t0 = performance.now();
  let log = '';
  let lastReport = 0;
  let saved = !!restorePath;
  // Dopo l'avvio: il copione di adb e del tocco, a passi fra un quanto e l'altro.
  let flow = null;
  let bootedNs = null;
  const jitInfo = () => {
    const j = m.jitStats();
    return j ? `, JIT ${j.modules} moduli, ${j.blocks} blocchi, ${j.resets} reset` : '';
  };
  const report = (why) => {
    const mem = process.memoryUsage();
    const wasmMem = exports.memory.buffer.byteLength;
    const d = m.diskStats(0);
    console.log(`[${why}] guest ${(Number(m.guestNs) / 1e9).toFixed(1)} s, reale ${((performance.now() - t0) / 1000).toFixed(0)} s, ` +
      `${(Number(m.steps) / 1e6).toFixed(0)} M istr., WASM ${mib(wasmMem)} MiB, RSS ${mib(mem.rss)} MiB, ` +
      `disco: ${d.fills} blocchi consegnati, ${d.cachedBlocks} in memoria, cow ${d.dirtyClusters} cluster (${mib(d.dirtyClusters * 4096)} MiB), ` +
      `sorgente ${mib(disk.stats.bytes)} MiB${jitInfo()}`);
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
        console.log(`fase: ${ev.phase} (${ev.label}) a ${ev.guestSecs.toFixed(1)} s di guest, ${((performance.now() - t0) / 1000).toFixed(0)} s reali`);
      }
    }
    if (stop !== 'Budget') {
      report(stop);
      throw new Fail(`la macchina si è fermata: ${stop}`);
    }
    const now = performance.now();
    if (now - lastReport > 30_000) {
      lastReport = now;
      report('stato');
      await writeFile(logPath, log);
    }
    if (flow?.wantSave && !saved) {
      saved = true;
      report('prima dello snapshot');
      const ts = performance.now();
      const fh = openSync(savePath, 'w');
      const size = m.snapshotSaveTo((b, at) => writeSync(fh, b, 0, b.length, at));
      closeSync(fh);
      const saveMs = performance.now() - ts;
      console.log(`snapshot: ${mib(size)} MiB (${size} byte) in ${saveMs.toFixed(0)} ms (a pezzi, scrittura compresa), a ${m.steps} istruzioni, memoria del modulo ${mib(m.memoryBytes)} MiB`);
      report('dopo lo snapshot');
      flow.saved();
    }
    if (until === 'boot' && progress.phase === 'booted') break;
    if (progress.phase === 'booted') {
      bootedNs ??= m.guestNs;
      // La home ha qualche secondo di guest per disegnarsi.
      if (!flow && m.guestNs - bootedNs >= 10_000_000_000n) flow = homeFlow(m, t0, !!savePath && !saved);
      flow?.pump?.();
      // Il copione aspetta Promise e condizioni: gli si lascia il turno.
      if (flow) await new Promise((ok) => setImmediate(ok));
      if (flow?.result) {
        if (flow.result instanceof Error) throw flow.result;
        break;
      }
    }
    if (Number(m.guestNs) / 1e9 >= guestSecs) break;
  }
  report('fine');
  await writeFile(logPath, log);
}

/** Pixel RGBA al centro dello schermo (o null a scanout spento). */
function center(m) {
  const size = m.displaySize();
  const px = m.displayPixels();
  if (!size || !px) return null;
  const o = ((size.height >> 1) * size.width + (size.width >> 1)) * 4;
  return [px[o], px[o + 1], px[o + 2]];
}

const near = (a, b) => a && a.every((v, i) => Math.abs(v - b[i]) <= 8);
/** Tiene acceso lo schermo della macchina virtuale e lo risveglia (lo stesso comando del Worker). */
const WAKE = 'svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard';
/** L'attività in primo piano (lo stesso comando del Worker). */
const HOME_QUERY = 'dumpsys activity activities | grep -m1 mResumedActivity';

/** Lo scanout in un PNG in target/aosp (per guardarlo). */
function screenshot(m, name) {
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
  console.log(`schermata: target/aosp/${name}`);
}
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];

/**
 * adb e tocco dopo l'avvio. La macchina gira nel ciclo principale; qui si
 * aspettano le Promise del client (che avanzano con `pump`) e le condizioni
 * sullo schermo. Restituisce { pump, result } (result: 'fatto' o l'errore).
 */
function homeFlow(m, t0, save) {
  const state = { adb: null, sock: null };
  let savedOk;
  const savedP = new Promise((ok) => (savedOk = ok));
  const secs = () => ((performance.now() - t0) / 1000).toFixed(0);
  const waitGuest = async (what, pred, guestSecs) => {
    const limit = m.guestNs + BigInt(guestSecs * 1e9);
    while (!pred()) {
      if (m.guestNs > limit) throw new Fail(`${what}: non arrivato in ${guestSecs} s di guest (centro ${center(m)})`);
      await new Promise((ok) => setImmediate(ok));
    }
  };
  const run = async () => {
    const home = center(m);
    console.log(`home: pixel al centro ${home}, schermo ${JSON.stringify(m.displaySize())}`);
    screenshot(m, 'node-home.png');
    for (let attempt = 0; ; attempt++) {
      state.sock = m.connectGuest(5555);
      state.adb = new AdbClient(state.sock);
      try {
        const banner = await state.adb.connect();
        console.log(`adb: collegato (${secs()} s reali): ${banner.props['ro.product.model']}, ${banner.features.length} funzioni`);
        break;
      } catch (e) {
        state.sock.release();
        state.adb = null;
        if (attempt > 20) throw e;
        await waitGuest('attesa di adbd', () => false, 3).catch(() => {});
      }
    }
    const adb = state.adb;
    console.log(`adb devices: ${JSON.stringify(await adb.devices())}`);
    // Una macchina virtuale: lo schermo resta acceso (come fa il Worker dell'app).
    console.log(`schermo acceso: ${JSON.stringify(await adb.shell(WAKE))}`);
    await waitGuest('schermo acceso', () => false, 3).catch(() => {});
    // La home: l'attività in primo piano è il launcher (prima c'è
    // FallbackHome, "Phone is starting").
    const b0 = m.guestNs;
    let top = '';
    for (;;) {
      top = (await adb.shell(HOME_QUERY)).stdout.trim();
      if (/launcher/i.test(top)) break;
      await waitGuest('attesa della home', () => false, 5).catch(() => {});
      if (m.guestNs - b0 > 3000_000_000_000n) throw new Fail(`home non arrivata: ${top}`);
    }
    console.log(`home: ${top} a ${(Number(m.guestNs) / 1e9).toFixed(1)} s di guest, ${secs()} s reali`);
    await waitGuest('home disegnata', () => false, 5).catch(() => {});
    screenshot(m, 'node-home-accesa.png');
    if (save) {
      holder.wantSave = true;
      await savedP;
    }
    const r = await adb.shell('getprop sys.boot_completed; getprop ro.build.version.release; cat /proc/meminfo | head -3');
    console.log(`adb shell: ${JSON.stringify(r)}`);
    if (!existsSync(apkPath)) throw new Fail(`${apkPath} mancante: tests/apps/tocco/build.sh`);
    const apk = new Uint8Array(readFileSync(apkPath));
    const info = await apkInfo(apk);
    let t = performance.now();
    const out = await adb.install(apk, { name: `${info.package}.apk` });
    console.log(`adb install ${info.package}: ${out} (${((performance.now() - t) / 1000).toFixed(1)} s reali)`);
    t = performance.now();
    const st = await adb.shell(`am start -W -n ${info.package}/${info.launcher}`);
    console.log(`am start: ${st.stdout.trim().split('\n').join(' | ')} (${((performance.now() - t) / 1000).toFixed(1)} s reali)`);
    await waitGuest("l'app a schermo (centro blu)", () => near(center(m), BLU), 120);
    console.log(`app a schermo: centro ${center(m)}`);
    screenshot(m, 'node-app-1.png');
    const size = m.displaySize();
    m.touch(0, [16384, 16384]);
    await waitGuest('tocco (giù)', () => false, 0.2).catch(() => {});
    m.touch(0, null);
    await waitGuest("l'app ha ricevuto il tocco (centro arancione)", () => near(center(m), ARANCIONE), 60);
    screenshot(m, 'node-app-2.png');
    const log = await adb.shell('logcat -d -s vetro-tocco:I | tail -3');
    console.log(`tocco ricevuto: centro ${center(m)} su ${size.width}x${size.height}; logcat: ${log.stdout.trim().split('\n').join(' | ')}`);
  };
  const holder = { pump: () => state.adb?.pump(), result: null, wantSave: false, saved: () => savedOk() };
  run().then(() => (holder.result = 'fatto'), (e) => (holder.result = e));
  return holder;
}

main().catch((e) => {
  console.error(e instanceof Fail ? `FALLITO: ${e.message}` : e);
  process.exit(1);
});
