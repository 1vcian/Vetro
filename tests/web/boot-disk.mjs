#!/usr/bin/env node
// M5: virtio-blk disk over HTTP Range, in Node without a browser.
//
// The M3 guest kernel reads and writes a raw test disk (/dev/vda):
// `md5sum /dev/vda`, then 13 bytes written halfway through the disk, cache dropped,
// reread, and `md5sum` again. Three identical boots:
//   A. local in-memory disk (vetro_disk_add_mem, always ready);
//   B. the same file served by a local HTTP server with Range (64 KiB
//      blocks, empty in-memory cache): the machine stops at every
//      missing block (`Blocked`) and the DiskFeeder downloads it;
//   C. like B but with B's cache already full (the restart with OPFS): no
//      reads from the network;
//   D. HTTP with 4 KiB blocks and read-ahead of 8 blocks (contiguous
//      blocks merged into one request), without a cache.
// Instructions and log must match byte for byte; the MD5 sums read by the
// guest must be those of the file (before and after the write), and the
// file on the server stays intact (the writes are in the copy-on-write).
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
// 3 MiB + 5 sectors + an unaligned tail: the last block is short and the
// size seen by the guest is rounded to 512 (like QEMU for raw disks).
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
    // A: local disk.
    const A = await session(x, kernel, (s) => s.m.addDiskMem(img));
    // B: HTTP Range, empty cache.
    const cache = new MemoryCache();
    const srcB = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederB;
    const B = await session(x, kernel, (s) => {
      s.feeder = feederB = new DiskFeeder(s.m);
      feederB.add(srcB, { cache, blockSize: BLOCK });
    });
    const statsB = B.disk;
    // C: HTTP Range, full cache (restart).
    const srcC = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederC;
    const C = await session(x, kernel, (s) => {
      s.feeder = feederC = new DiskFeeder(s.m);
      feederC.add(srcC, { cache, blockSize: BLOCK });
    });
    // D: small blocks, read-ahead, no cache.
    const srcD = await new RangeSource(`${srv.url}/disk.img`).open();
    let feederD;
    const D = await session(x, kernel, (s) => {
      s.feeder = feederD = new DiskFeeder(s.m);
      feederD.add(srcD, { blockSize: 4096, readahead: 8 });
    });

    for (const [name, r] of [['A', A], ['B', B], ['C', C], ['D', D]]) {
      console.log(`${name}: ${r.steps} instructions, ${(r.ms / 1000).toFixed(2)} s, ${r.s.blocked} Blocked stops`);
    }
    console.log(`B: ${JSON.stringify(feederB.stats)}; HTTP ${JSON.stringify(srcB.stats)}; disco ${JSON.stringify(statsB)}`);
    console.log(`C: ${JSON.stringify(feederC.stats)}; HTTP ${JSON.stringify(srcC.stats)}`);
    console.log(`D: ${JSON.stringify(feederD.stats)}; HTTP ${JSON.stringify(srcD.stats)}`);

    check(A.first.includes(`${before}  /dev/vda`), `guest md5sum differs from the file (${before}):\n${A.first}`);
    // The kernel's message about drop_caches may arrive right after the text.
    check(A.second.includes(`\n${WRITTEN}`), `written bytes not read back:\n${A.second}`);
    check(A.second.includes(`${after}  /dev/vda`), `md5sum after the write differs (${after}):\n${A.second}`);
    check(A.s.blocked === 0, 'the local disk must never stop the machine');
    check(B.s.blocked > 0 && feederB.stats.fromSource > 0, 'the HTTP disk should have downloaded blocks');
    check(srcB.stats.requests > 1, 'no Range request after the first');
    check(statsB.dirtyClusters > 0, "the guest's writes should have ended up in the copy-on-write");
    check(srcC.stats.requests === 1 && feederC.stats.fromSource === 0 && feederC.stats.fromCache > 0,
      `at restart the blocks should have come from the cache: ${JSON.stringify(feederC.stats)}, HTTP ${JSON.stringify(srcC.stats)}`);
    check(requests.every((r) => r.status === 206), `non-206 responses: ${JSON.stringify(requests.filter((r) => r.status !== 206))}`);
    check(feederD.stats.readahead > 0 && srcD.stats.requests - 1 < feederD.stats.fromSource,
      `D: read-ahead should have merged blocks: ${JSON.stringify(feederD.stats)}, HTTP ${JSON.stringify(srcD.stats)}`);
    for (const [name, r] of [['B', B], ['C', C], ['D', D]]) {
      check(r.steps === A.steps, `${name}: ${r.steps} instructions, local disk ${A.steps}: guest time must not depend on the network`);
      check(r.log === A.log, `${name}: log differs from the one with the local disk`);
    }
    check(md5(readFileSync(file)) === md5(img), 'the file on the server changed');
    writeFileSync(join(dir, 'boot-disk.log'), A.log, 'latin1');
    compareNative('disk', B.steps, B.raw);
    console.log(`disk over HTTP Range: ok (same ${A.steps} instructions and same log as the local disk; md5 ${before} -> ${after})`);
  } finally {
    await srv.close();
  }
});
