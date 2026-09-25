#!/usr/bin/env node
// M5: disco virtio-blk via HTTP Range, in Node senza browser.
//
// Il kernel guest di M3 legge e scrive un disco raw di prova (/dev/vda):
// `md5sum /dev/vda`, poi 13 byte scritti a metà disco, cache svuotata,
// riletti, e di nuovo `md5sum`. Tre avvii identici:
//   A. disco locale in memoria (vetro_disk_add_mem, sempre pronto);
//   B. lo stesso file servito da un server HTTP locale con Range (blocchi
//      da 64 KiB, cache in memoria vuota): la macchina si ferma a ogni
//      blocco mancante (`Blocked`) e il DiskFeeder lo scarica;
//   C. come B ma con la cache di B già piena (il riavvio con OPFS): nessuna
//      lettura dalla rete;
//   D. HTTP con blocchi da 4 KiB e lettura anticipata di 8 blocchi (blocchi
//      contigui uniti in una richiesta), senza cache.
// Istruzioni e log devono coincidere byte per byte; le somme MD5 lette dal
// guest devono essere quelle del file (prima e dopo la scrittura), e il
// file sul server resta intatto (le scritture stanno nel copy-on-write).
//
//   node tests/web/boot-disk.mjs [--no-jit]

import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { DiskFeeder, MemoryCache, RangeSource } from '../../web/node/disk.mjs';
import { serve } from '../../tools/web-serve.mjs';
import { check, compareNative, guestKernel, loadVetro, root, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const BLOCK = 64 * 1024;
// 3 MiB + 5 settori + una coda non allineata: l'ultimo blocco è corto e la
// dimensione vista dal guest è arrotondata a 512 (come QEMU per i raw).
const SIZE = 3 * 1024 * 1024 + 5 * 512 + 100;
const WRITE_AT = 1_000_000;
const WRITTEN = 'VETRO-SCRITTO';

function makeDisk() {
  const img = new Uint8Array(SIZE);
  let s = 0x9e3779b9;
  for (let i = 0; i < SIZE; i++) {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    img[i] = s & 0xff;
  }
  return img;
}

const md5 = (b) => createHash('md5').update(b).digest('hex');

async function session(x, kernel, attach) {
  const t0 = performance.now();
  const s = new Session(x, kernel, { jit, setup: attach });
  let at = await s.until(SHELL_PROMPT);
  const [a1, b1] = await s.command('md5sum /dev/vda', at);
  const cmd =
    `printf ${WRITTEN} | dd of=/dev/vda bs=1 seek=${WRITE_AT} conv=notrunc 2>/dev/null; sync; ` +
    `echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1 skip=${WRITE_AT} count=${WRITTEN.length} 2>/dev/null; ` +
    'echo; md5sum /dev/vda';
  const [a2, b2] = await s.command(cmd, b1);
  await s.poweroff(b2);
  const r = { s, first: s.text(a1, b1), second: s.text(a2, b2), log: s.text(), raw: s.log, steps: s.m.steps, disk: s.m.diskStats(0) };
  r.ms = performance.now() - t0;
  s.m.free();
  return r;
}

run(async () => {
  const kernel = guestKernel();
  const { exports: x } = await loadVetro();
  const img = makeDisk();
  const seen = img.subarray(0, Math.floor(SIZE / 512) * 512);
  const before = md5(seen);
  const modified = seen.slice();
  modified.set(new TextEncoder().encode(WRITTEN), WRITE_AT);
  const after = md5(modified);

  const dir = join(root, 'target/web-test');
  mkdirSync(dir, { recursive: true });
  const file = join(dir, 'disk.img');
  writeFileSync(file, img);
  const requests = [];
  const srv = await serve({ mounts: [['/disk.img', file]], onRequest: (r) => requests.push(r) });
  try {
    // A: disco locale.
    const A = await session(x, kernel, (s) => s.m.addDiskMem(img));
    // B: HTTP Range, cache vuota.
    const cache = new MemoryCache();
    const srcB = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederB;
    const B = await session(x, kernel, (s) => {
      s.feeder = feederB = new DiskFeeder(s.m);
      feederB.add(srcB, { cache, blockSize: BLOCK });
    });
    const statsB = B.disk;
    // C: HTTP Range, cache piena (riavvio).
    const srcC = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederC;
    const C = await session(x, kernel, (s) => {
      s.feeder = feederC = new DiskFeeder(s.m);
      feederC.add(srcC, { cache, blockSize: BLOCK });
    });
    // D: blocchi piccoli, lettura anticipata, senza cache.
    const srcD = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederD;
    const D = await session(x, kernel, (s) => {
      s.feeder = feederD = new DiskFeeder(s.m);
      feederD.add(srcD, { blockSize: 4096, readahead: 8 });
    });

    for (const [name, r] of [['A', A], ['B', B], ['C', C], ['D', D]]) {
      console.log(`${name}: ${r.steps} istruzioni, ${(r.ms / 1000).toFixed(2)} s, ${r.s.blocked} arresti Blocked`);
    }
    console.log(`B: ${JSON.stringify(feederB.stats)}; HTTP ${JSON.stringify(srcB.stats)}; disco ${JSON.stringify(statsB)}`);
    console.log(`C: ${JSON.stringify(feederC.stats)}; HTTP ${JSON.stringify(srcC.stats)}`);
    console.log(`D: ${JSON.stringify(feederD.stats)}; HTTP ${JSON.stringify(srcD.stats)}`);

    check(A.first.includes(`${before}  /dev/vda`), `md5sum del guest diverso dal file (${before}):\n${A.first}`);
    check(A.second.includes(`\n${WRITTEN}\n`), `byte scritti non riletti:\n${A.second}`);
    check(A.second.includes(`${after}  /dev/vda`), `md5sum dopo la scrittura diverso (${after}):\n${A.second}`);
    check(A.s.blocked === 0, 'il disco locale non deve mai fermare la macchina');
    check(B.s.blocked > 0 && feederB.stats.fromSource > 0, 'il disco HTTP doveva scaricare blocchi');
    check(srcB.stats.requests > 1, 'nessuna richiesta Range dopo la prima');
    check(statsB.dirtyClusters > 0, 'le scritture del guest dovevano finire nel copy-on-write');
    check(srcC.stats.requests === 1 && feederC.stats.fromSource === 0 && feederC.stats.fromCache > 0,
      `al riavvio i blocchi dovevano venire dalla cache: ${JSON.stringify(feederC.stats)}, HTTP ${JSON.stringify(srcC.stats)}`);
    check(requests.every((r) => r.status === 206), `risposte non 206: ${JSON.stringify(requests.filter((r) => r.status !== 206))}`);
    check(feederD.stats.readahead > 0 && srcD.stats.requests - 1 < feederD.stats.fromSource,
      `D: la lettura anticipata doveva unire blocchi: ${JSON.stringify(feederD.stats)}, HTTP ${JSON.stringify(srcD.stats)}`);
    for (const [name, r] of [['B', B], ['C', C], ['D', D]]) {
      check(r.steps === A.steps, `${name}: ${r.steps} istruzioni, disco locale ${A.steps}: il tempo del guest non deve dipendere dalla rete`);
      check(r.log === A.log, `${name}: log diverso da quello col disco locale`);
    }
    check(md5(readFileSync(file)) === md5(img), 'il file sul server è cambiato');
    writeFileSync(join(dir, 'boot-disk.log'), A.log, 'latin1');
    compareNative('disk', B.steps, B.raw);
    console.log(`disco via HTTP Range: ok (stesse ${A.steps} istruzioni e stesso log del disco locale; md5 ${before} -> ${after})`);
  } finally {
    await srv.close();
  }
});
