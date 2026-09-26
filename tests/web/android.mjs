// Android in Node/V8 (M5, ADR 0027): l'immagine AOSP di Vetro (target/aosp,
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
//   --save=FILE        a sys.boot_completed salva lo snapshot (tempi e dimensione)
//
// Stampa le fasi dell'avvio (tempo del guest e reale), la memoria (lineare di
// WASM e RSS del processo) e i contatori dei dischi.

import { open, stat, writeFile } from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DEV } from '../../web/node/vetro.mjs';
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
  const m = new Machine(exports, { ramSize: BigInt(ramMiB) << 20n, devices: DEV.DEFAULT });
  const feeder = new DiskFeeder(m);
  const disk = await new FileSource(join(aosp, 'disk.img')).open();
  feeder.add(disk, { cache: new MemoryCache(), blockSize: 1 << 20, maxBlocks: 64, readahead: 1 });
  const read = (f) => new Uint8Array(readFileSync(join(aosp, 'out', f)));
  const desc = m.loadAndroid({ boot: read('boot.img'), vendorBoot: read('vendor_boot.img'), initBoot: read('init_boot.img'), params: 'nokaslr' });
  console.log(`vetro: ${desc.split(';')[0]}`);
  if (jit) m.setJit();
  const progress = new BootProgress();
  const t0 = performance.now();
  let log = '';
  let lastReport = 0;
  let saved = false;
  const report = (why) => {
    const mem = process.memoryUsage();
    const wasmMem = exports.memory.buffer.byteLength;
    const d = m.diskStats(0);
    console.log(`[${why}] guest ${(Number(m.guestNs) / 1e9).toFixed(1)} s, reale ${((performance.now() - t0) / 1000).toFixed(0)} s, ` +
      `${(Number(m.steps) / 1e6).toFixed(0)} M istr., WASM ${mib(wasmMem)} MiB, RSS ${mib(mem.rss)} MiB, ` +
      `disco: ${d.fills} blocchi consegnati, ${d.cachedBlocks} in memoria, cow ${d.dirtyClusters} cluster (${mib(d.dirtyClusters * 4096)} MiB), ` +
      `sorgente ${mib(disk.stats.bytes)} MiB`);
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
    if (progress.phase === 'booted' && savePath && !saved) {
      saved = true;
      report('prima dello snapshot');
      const ts = performance.now();
      const snap = m.snapshotSave();
      const saveMs = performance.now() - ts;
      await writeFile(savePath, snap);
      console.log(`snapshot: ${mib(snap.length)} MiB (${snap.length} byte) in ${saveMs.toFixed(0)} ms, a ${m.steps} istruzioni`);
      report('dopo lo snapshot');
    }
    if (until === 'boot' && progress.phase === 'booted') break;
    if (Number(m.guestNs) / 1e9 >= guestSecs) break;
  }
  report('fine');
  await writeFile(logPath, log);
}

main().catch((e) => {
  console.error(e instanceof Fail ? `FALLITO: ${e.message}` : e);
  process.exit(1);
});
