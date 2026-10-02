#!/usr/bin/env node
// M6: machine snapshots and persistent disk overlay in Node/V8,
// from the vetro-wasm API (ABI 4 and 6), as the app uses them (web/node/persist.mjs).
//
// The M3 guest kernel with a disk over HTTP Range (DiskFeeder) and its
// persistent overlay on an in-memory file (MemFile, instead of OPFS):
//   A. full boot, with the JIT: snapshot halfway through boot (40 M instructions) and
//      at the prompt (with the copy of the overlay file at that moment), then
//      write to the disk (dd + sync), reread with the cache dropped, md5sum,
//      power-off;
//   B. new machine, same disks, overlay from the file at the moment of the
//      snapshot, restore of the prompt snapshot, same script with the JIT;
//   C. like B with the interpreter;
//   D. restore of the mid-boot snapshot, up to the prompt and then the
//      same script.
//   B, C and D must print exactly the continuation of A's log from the point
//   of the save, reach the same instruction count, and leave the
//   overlay file identical byte for byte to A's.
//   E. boot from scratch with A's final overlay: the write is there (reread
//      and md5sum of the modified disk);
//   F. the base image changes (another file on the server): the overlay is discarded,
//      the guest reads the new base, the file is rewritten from scratch.
//   G. (ADR 0046) at A's prompt a deferred save starts too and is taken piece by
//      piece while A goes on writing the disk, assembled by a SaveJob on another
//      vetro-wasm instance into a SnapshotStore: the file is the prompt snapshot
//      byte for byte (and B and C, restored from that one, continue as A).
// Prints save and restore timings and sizes in V8.
//
//   node tests/web/snapshot.mjs [--no-jit]

import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { DiskFeeder, MemoryCache, RangeSource } from '../../web/node/disk.mjs';
import { DiskOverlay, MemFile, SnapshotStore, snapshotKey, staleReason } from '../../web/node/persist.mjs';
import { BackgroundSave, SaveJob } from '../../web/node/background-save.mjs';
import { serve } from '../../tools/web-serve.mjs';
import { check, guestKernel, loadVetro, root, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const BLOCK = 64 * 1024;
const SIZE = 2 * 1024 * 1024 + 3 * 512;
const WRITE_AT = 700_001;
const TEXT = 'VETRO-OVERLAY-OPFS';
const MID = 40_000_000n;

function makeDisk(seed) {
  const img = new Uint8Array(SIZE);
  let s = seed;
  for (let i = 0; i < SIZE; i++) {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    img[i] = s & 0xff;
  }
  return img;
}

const md5 = (b) => createHash('md5').update(b).digest('hex');
const ms = (t) => `${t.toFixed(0)} ms`;
const mib = (n) => `${(n / 2 ** 20).toFixed(1)} MiB`;

// Plus 300 000 bytes at 1.2 MiB: about seventy clusters in the overlay.
const BULK_AT = 300 * 4096;
const BULK = 300_000;
const WRITE =
  `printf ${TEXT} | dd of=/dev/vda bs=1 seek=${WRITE_AT} conv=notrunc 2>/dev/null; ` +
  `yes VETRO | head -c ${BULK} | dd of=/dev/vda bs=4096 seek=300 conv=notrunc 2>/dev/null; sync`;
const READ =
  `echo 3 > /proc/sys/vm/drop_caches; echo LETTO-$(dd if=/dev/vda bs=1 skip=${WRITE_AT} count=${TEXT.length} 2>/dev/null)-FINE; ` +
  'md5sum /dev/vda';

run(async () => {
  const kernel = guestKernel();
  const { exports: x } = await loadVetro();
  const img = makeDisk(0x2545f491);
  const img2 = makeDisk(0x6b43a9b5);
  const modified = img.slice();
  modified.set(new TextEncoder().encode(TEXT), WRITE_AT);
  modified.set(new TextEncoder().encode('VETRO\n'.repeat(BULK / 6 + 1).slice(0, BULK)), BULK_AT);
  const dir = join(root, 'target/web-test');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'snap-disk.img'), img);
  writeFileSync(join(dir, 'snap-disk2.img'), img2);
  const srv = await serve({ mounts: [['/disk.img', join(dir, 'snap-disk.img')], ['/disk2.img', join(dir, 'snap-disk2.img')]] });
  const cache = new MemoryCache();
  const cache2 = new MemoryCache();

  /** Session with the disk over HTTP and the overlay on `file`. */
  async function session(name, file, { restore = null, useJit = jit, url = '/disk.img', onQuantum = null } = {}) {
    const src = await new RangeSource(`${srv.url}${url}`).open();
    let overlay;
    const t0 = performance.now();
    const s = new Session(x, kernel, {
      jit: useJit,
      restore,
      setup: (s) => {
        s.feeder = new DiskFeeder(s.m);
        const disk = s.feeder.add(src, { cache: url === '/disk.img' ? cache : cache2, blockSize: BLOCK });
        overlay = DiskOverlay.open(s.m, disk, file, src.key);
      },
      onQuantum: (s) => {
        overlay.persist();
        onQuantum?.(s, overlay);
      },
    });
    return { name, s, overlay, src, sessionMs: performance.now() - t0 };
  }

  async function script(r, at) {
    const s = r.s;
    const [a1, b1] = await s.command(WRITE, at);
    const [a2, b2] = await s.command(READ, b1);
    await s.poweroff(b2);
    r.read = s.text(a2, b2);
    r.steps = s.m.steps;
    r.log = s.log;
    return r;
  }

  try {
    // ---- A: full boot with the two saves ---------------------------------
    const fileA = new MemFile();
    const saves = {};
    const store = SnapshotStore.memory();
    const save = (label, s, overlay) => {
      const t0 = performance.now();
      const bytes = s.m.snapshotSave();
      saves[label] = { bytes, ms: performance.now() - t0, steps: s.m.steps, pos: s.log.length, file: fileA.bytes(), gen: overlay.generation };
    };
    // G: the deferred save, its saver on another instance (as in its Worker).
    const { exports: x2 } = await loadVetro();
    const bgStore = SnapshotStore.memory();
    let bg = null;
    let bgPieces = 0;
    const job = new SaveJob(x2, bgStore, (msg) => bg.reply(msg));
    const A = await session('A', fileA, {
      onQuantum: (s, overlay) => {
        if (!saves.mid && s.m.steps >= MID) save('mid', s, overlay);
        if (bg?.pump(1)) bgPieces++;
      },
    });
    check(A.overlay.opened.code === 'New', `new overlay expected: ${JSON.stringify(A.overlay.opened)}`);
    const t0 = performance.now();
    const at = await A.s.until(SHELL_PROMPT);
    const bootMs = performance.now() - t0;
    save('prompt', A.s, A.overlay);
    bg = new BackgroundSave(A.s.m, (msg) => job.handle(msg), 'bg', { steps: String(A.s.m.steps) });
    // The snapshot cache as in the app: key, metadata with the
    // overlay generation, read back the same.
    const key = await snapshotKey({ v: A.s.m.snapshotVersion, disk: A.src.key, cmdline: 'x' });
    await store.save(key, { generations: [saves.prompt.gen], steps: String(saves.prompt.steps) }, saves.prompt.bytes);
    const cached = await store.load(key);
    check(cached && Buffer.compare(cached.bytes, saves.prompt.bytes) === 0, 'snapshot read back from the cache differs');
    check(staleReason(cached.meta, [A.overlay]) === null, 'snapshot just saved already stale');
    await script(A, at);
    while (bg.active) {
      bg.pump();
      await new Promise((ok) => setTimeout(ok, 0));
    }
    const done = await bg.done;
    const deferred = await bgStore.load('bg');
    check(deferred && Buffer.compare(deferred.bytes, saves.prompt.bytes) === 0, 'G: the deferred save differs from the prompt snapshot');
    check(bgPieces > 2, `G: the deferred save went on while the guest ran (${bgPieces} slices)`);
    console.log(`G: deferred save at the prompt equal to the synchronous one (${mib(done.size)}): ${bg.beginMs.toFixed(1)} ms to start, ` +
      `${ms(bg.pumpMs)} of copies in ${bgPieces} slices on the machine's thread, ${ms(done.ms)} in the assembler, kept pages at most ${mib(bg.keptMax)}`);
    check(A.read.includes(`LETTO-${TEXT}-FINE`), `A: write not read back:\n${A.read}`);
    check(A.read.includes(`${md5(modified)}  /dev/vda`), `A: md5sum of the modified disk differs:\n${A.read}`);
    check(staleReason(cached.meta, [A.overlay]) !== null, 'after the write the prompt snapshot is no longer valid');
    const fileAEnd = fileA.bytes();
    console.log(`A: ${A.steps} instructions, prompt in ${ms(bootMs)} (at ${saves.prompt.steps}), overlay ${A.overlay.info.clusters} clusters, generation ${A.overlay.generation}, ${fileAEnd.length} bytes`);

    // ---- B, C, D: restore and continuation ---------------------------------
    const cases = [
      ['B', 'prompt', jit],
      ['C', 'prompt', false],
      ['D', 'mid', jit],
    ];
    for (const [name, label, useJit] of cases) {
      const sv = saves[label];
      const file = new MemFile(sv.file);
      const r = await session(name, file, { restore: sv.bytes, useJit });
      check(r.overlay.opened.code === 'Loaded', `${name}: ${JSON.stringify(r.overlay.opened)}`);
      check(r.s.m.steps === sv.steps, `${name}: restored at ${r.s.m.steps}, saved at ${sv.steps}`);
      const from = label === 'mid' ? await r.s.until(SHELL_PROMPT) : 0;
      await script(r, from);
      const expected = A.log.slice(sv.pos);
      check(r.steps === A.steps, `${name}: ${r.steps} instructions, A ${A.steps}`);
      check(r.log === expected, `${name}: log differs from the continuation of A\n--- ${name} ---\n${r.s.tail()}`);
      check(Buffer.compare(file.bytes(), fileAEnd) === 0, `${name}: overlay file differs from A's`);
      console.log(`${name} (${label}, ${useJit ? 'JIT' : 'interpreter'}): snapshot ${mib(sv.bytes.length)}, saved in ${ms(sv.ms)}, restored in ${ms(r.s.restoreMs)} ` +
        `(machine and disks included ${ms(r.sessionMs)}); ` +
        `${r.steps} instructions and log equal to A (${r.log.length} bytes after the restore), overlay identical`);
      r.s.m.free();
    }

    // ---- E: boot from scratch with A's overlay ----------------------------
    const fileE = new MemFile(fileAEnd);
    const E = await session('E', fileE);
    check(E.overlay.opened.code === 'Loaded', `E: ${JSON.stringify(E.overlay.opened)}`);
    let e = await E.s.until(SHELL_PROMPT);
    [, e] = await E.s.command(READ, e);
    const readE = E.s.text();
    check(readE.includes(`LETTO-${TEXT}-FINE`), `E: the write of session A is not there:\n${E.s.tail()}`);
    check(readE.includes(`${md5(modified)}  /dev/vda`), `E: md5sum differs from the modified disk's:\n${E.s.tail()}`);
    E.s.m.free();
    console.log("E: boot from scratch with A's overlay: the write is there (reread and md5sum)");

    // ---- F: another base image -------------------------------------------
    const fileF = new MemFile(fileAEnd);
    const F = await session('F', fileF, { url: '/disk2.img' });
    check(F.overlay.opened.code === 'Mismatch', `F: ${JSON.stringify(F.overlay.opened)}`);
    let f = await F.s.until(SHELL_PROMPT);
    [, f] = await F.s.command(READ, f);
    const readF = F.s.text();
    check(!readF.includes(TEXT) && readF.includes(`${md5(img2)}  /dev/vda`), `F: overlay of another base applied:\n${F.s.tail()}`);
    check(fileF.getSize() === 4096, `F: overlay file of ${fileF.getSize()} bytes, expected only the header`);
    F.s.m.free();
    console.log(`F: base changed: overlay discarded (${F.overlay.opened.message})`);
    A.s.m.free();
    console.log('snapshot e overlay in V8: ok');
  } finally {
    await srv.close();
  }
});
