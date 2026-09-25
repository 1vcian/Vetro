#!/usr/bin/env node
// M6: snapshot della macchina e overlay persistente dei dischi in Node/V8,
// dall'API di vetro-wasm (ABI 4 e 6), come li usa l'app (web/node/persist.mjs).
//
// Il kernel guest di M3 con un disco via HTTP Range (DiskFeeder) e il suo
// overlay persistente su un file in memoria (MemFile, al posto di OPFS):
//   A. avvio completo, col JIT: snapshot a metà avvio (40 M istruzioni) e
//      al prompt (con la copia del file dell'overlay in quel momento), poi
//      scrittura sul disco (dd + sync), rilettura a cache svuotata, md5sum,
//      spegnimento;
//   B. macchina nuova, stessi dischi, overlay dal file del momento dello
//      snapshot, ripristino dello snapshot del prompt, stesso copione col JIT;
//   C. come B con l'interprete;
//   D. ripristino dello snapshot di metà avvio, fino al prompt e poi lo
//      stesso copione.
//   B, C e D devono stampare esattamente il seguito del log di A dal punto
//   del salvataggio, arrivare allo stesso numero di istruzioni, e lasciare il
//   file dell'overlay identico byte per byte a quello di A.
//   E. avvio da zero con l'overlay finale di A: la scrittura c'è (rilettura
//      e md5sum del disco modificato);
//   F. l'immagine base cambia (altro file sul server): l'overlay si scarta,
//      il guest legge la base nuova, il file si riscrive da capo.
// Stampa tempi e dimensioni di salvataggio e ripristino in V8.
//
//   node tests/web/snapshot.mjs [--no-jit]

import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { DiskFeeder, MemoryCache, RangeSource } from '../../web/node/disk.mjs';
import { DiskOverlay, MemFile, SnapshotStore, snapshotKey, staleReason } from '../../web/node/persist.mjs';
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

// Più 300 000 byte a 1,2 MiB: una settantina di cluster nell'overlay.
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

  /** Sessione col disco via HTTP e l'overlay su `file`. */
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
    // ---- A: avvio completo con i due salvataggi -------------------------
    const fileA = new MemFile();
    const saves = {};
    const store = SnapshotStore.memory();
    const save = (label, s, overlay) => {
      const t0 = performance.now();
      const bytes = s.m.snapshotSave();
      saves[label] = { bytes, ms: performance.now() - t0, steps: s.m.steps, pos: s.log.length, file: fileA.bytes(), gen: overlay.generation };
    };
    const A = await session('A', fileA, {
      onQuantum: (s, overlay) => {
        if (!saves.mid && s.m.steps >= MID) save('mid', s, overlay);
      },
    });
    check(A.overlay.opened.code === 'New', `overlay nuovo atteso: ${JSON.stringify(A.overlay.opened)}`);
    const t0 = performance.now();
    const at = await A.s.until(SHELL_PROMPT);
    const bootMs = performance.now() - t0;
    save('prompt', A.s, A.overlay);
    // La cache degli snapshot come nell'app: chiave, metadati con la
    // generazione dell'overlay, riletta uguale.
    const key = await snapshotKey({ v: A.s.m.snapshotVersion, disk: A.src.key, cmdline: 'x' });
    await store.save(key, { generations: [saves.prompt.gen], steps: String(saves.prompt.steps) }, saves.prompt.bytes);
    const cached = await store.load(key);
    check(cached && Buffer.compare(cached.bytes, saves.prompt.bytes) === 0, 'snapshot riletto dalla cache diverso');
    check(staleReason(cached.meta, [A.overlay]) === null, 'snapshot appena salvato già vecchio');
    await script(A, at);
    check(A.read.includes(`LETTO-${TEXT}-FINE`), `A: scrittura non riletta:\n${A.read}`);
    check(A.read.includes(`${md5(modified)}  /dev/vda`), `A: md5sum del disco modificato diverso:\n${A.read}`);
    check(staleReason(cached.meta, [A.overlay]) !== null, 'dopo la scrittura lo snapshot del prompt non vale più');
    const fileAEnd = fileA.bytes();
    console.log(`A: ${A.steps} istruzioni, prompt in ${ms(bootMs)} (a ${saves.prompt.steps}), overlay ${A.overlay.info.clusters} cluster, generazione ${A.overlay.generation}, ${fileAEnd.length} byte`);

    // ---- B, C, D: ripristino e seguito ---------------------------------
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
      check(r.s.m.steps === sv.steps, `${name}: ripristinato a ${r.s.m.steps}, salvato a ${sv.steps}`);
      const from = label === 'mid' ? await r.s.until(SHELL_PROMPT) : 0;
      await script(r, from);
      const expected = A.log.slice(sv.pos);
      check(r.steps === A.steps, `${name}: ${r.steps} istruzioni, A ${A.steps}`);
      check(r.log === expected, `${name}: log diverso dal seguito di A\n--- ${name} ---\n${r.s.tail()}`);
      check(Buffer.compare(file.bytes(), fileAEnd) === 0, `${name}: file dell'overlay diverso da quello di A`);
      console.log(`${name} (${label}, ${useJit ? 'JIT' : 'interprete'}): snapshot ${mib(sv.bytes.length)}, salvato in ${ms(sv.ms)}, ripristinato in ${ms(r.s.restoreMs)} ` +
        `(macchina e dischi compresi ${ms(r.sessionMs)}); ` +
        `${r.steps} istruzioni e log uguali ad A (${r.log.length} byte dopo il ripristino), overlay identico`);
      r.s.m.free();
    }

    // ---- E: avvio da zero con l'overlay di A ----------------------------
    const fileE = new MemFile(fileAEnd);
    const E = await session('E', fileE);
    check(E.overlay.opened.code === 'Loaded', `E: ${JSON.stringify(E.overlay.opened)}`);
    let e = await E.s.until(SHELL_PROMPT);
    [, e] = await E.s.command(READ, e);
    const readE = E.s.text();
    check(readE.includes(`LETTO-${TEXT}-FINE`), `E: la scrittura della sessione A non c'è:\n${E.s.tail()}`);
    check(readE.includes(`${md5(modified)}  /dev/vda`), `E: md5sum diverso da quello del disco modificato:\n${E.s.tail()}`);
    E.s.m.free();
    console.log('E: avvio da zero con l\'overlay di A: la scrittura c\'è (rilettura e md5sum)');

    // ---- F: altra immagine base -----------------------------------------
    const fileF = new MemFile(fileAEnd);
    const F = await session('F', fileF, { url: '/disk2.img' });
    check(F.overlay.opened.code === 'Mismatch', `F: ${JSON.stringify(F.overlay.opened)}`);
    let f = await F.s.until(SHELL_PROMPT);
    [, f] = await F.s.command(READ, f);
    const readF = F.s.text();
    check(!readF.includes(TEXT) && readF.includes(`${md5(img2)}  /dev/vda`), `F: overlay di un'altra base applicato:\n${F.s.tail()}`);
    check(fileF.getSize() === 4096, `F: file dell'overlay di ${fileF.getSize()} byte, atteso solo l'intestazione`);
    F.s.m.free();
    console.log(`F: base cambiata: overlay scartato (${F.overlay.opened.message})`);
    A.s.m.free();
    console.log('snapshot e overlay in V8: ok');
  } finally {
    await srv.close();
  }
});
