// Avvio da immagini Android e RAM di 3 GiB in vetro-wasm (M5, ADR 0018 e
// 0028), veloce: il kernel guest di M3 impacchettato come lo impacchetta una
// build GKI (`boot.img` v4 col kernel gzip, `init_boot.img` v4 con
// l'initramfs, da `mkbootimg.py` di AOSP in tools/mkbootimg/) e caricato con
// `vetro_load_android` (ABI 12) su una macchina con 3 GiB di RAM: su wasm32
// la RAM oltre 2 GiB è una regione fuori dall'allocatore (board.rs).
//
// - Istruzioni e log uguali al riferimento nativo `ram3g` (tests/boot/tests/web.rs:
//   stessa RAM, `load_linux` diretto con Image e initramfs): il bootloader dà
//   al kernel gli stessi byte, e la regione di wasm32 si comporta come il
//   `Vec` dell'host.
// - Snapshot al prompt, macchina distrutta, macchina nuova da 3 GiB (riusa la
//   regione, che va azzerata) e ripristino: il seguito del log e le
//   istruzioni finali sono quelli dell'esecuzione senza taglio.
//
//   node tests/web/android-boot.mjs [--no-jit]

import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { check, compareNative, Fail, guestKernel, loadVetro, root, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const RAM = 3n << 30n;
const CMDLINE = 'console=ttyAMA0 vetro.noautotest';

function images(kernel) {
  const dir = join(root, 'target/web-test/android-boot');
  mkdirSync(dir, { recursive: true });
  let gz;
  try {
    gz = execFileSync('gzip', ['-9', '-n', '-c'], { input: kernel.image, maxBuffer: 1 << 28 });
  } catch (e) {
    throw new Fail(`gzip non disponibile: ${e.message}`);
  }
  writeFileSync(join(dir, 'Image.gz'), gz);
  writeFileSync(join(dir, 'initramfs.cpio.gz'), kernel.initrd);
  const mk = (args) => {
    try {
      execFileSync('python3', [join(root, 'tools/mkbootimg/mkbootimg.py'), ...args], { stdio: 'pipe' });
    } catch (e) {
      throw new Fail(`mkbootimg.py (serve python3): ${e.stderr?.toString() ?? e.message}`);
    }
  };
  mk(['--header_version', '4', '--kernel', join(dir, 'Image.gz'), '--cmdline', CMDLINE, '-o', join(dir, 'boot.img')]);
  mk(['--header_version', '4', '--ramdisk', join(dir, 'initramfs.cpio.gz'), '-o', join(dir, 'init_boot.img')]);
  return { boot: readFileSync(join(dir, 'boot.img')), initBoot: readFileSync(join(dir, 'init_boot.img')) };
}

run(async () => {
  const kernel = guestKernel();
  const { exports } = await loadVetro();
  const img = images(kernel);
  let desc = '';
  const load = (m) => {
    desc = m.loadAndroid({ boot: new Uint8Array(img.boot), initBoot: new Uint8Array(img.initBoot), params: '' });
  };
  const t0 = performance.now();
  const s = new Session(exports, kernel, { jit, machine: { ramSize: RAM }, load });
  check(/kernel gzip/.test(desc) && desc.includes('init_boot') && desc.includes(`riga di comando: ${CMDLINE}`), `descrizione del bootloader: ${desc}`);
  const memBefore = s.m.memoryBytes;
  check(memBefore >= 3 * 2 ** 30, `memoria del modulo ${memBefore} byte: manca la RAM di 3 GiB`);
  let at = await s.until(SHELL_PROMPT);
  const [, end] = await s.command('head -1 /proc/meminfo', at);
  const kb = Number(/MemTotal:\s+(\d+) kB/.exec(s.text(at, end))?.[1]);
  check(kb > 2.9 * 2 ** 20, `MemTotal ${kb} kB con 3 GiB di RAM`);
  const snap = s.m.snapshotSave();
  const cut = s.log.length;
  const cutSteps = s.m.steps;
  await s.poweroff(end);
  console.log(`avvio da boot.img con 3 GiB: ${s.m.steps} istruzioni in ${((performance.now() - t0) / 1000).toFixed(1)} s, MemTotal ${kb} kB, snapshot ${(snap.length / 2 ** 20).toFixed(1)} MiB, memoria del modulo ${(memBefore / 2 ** 20).toFixed(0)} MiB`);
  compareNative('ram3g', s.m.steps, s.log);
  const steps = s.m.steps;
  const tail = s.log.slice(cut);
  s.m.free();

  // Macchina nuova: riusa la regione della prima (azzerata) e ripristina.
  const t1 = performance.now();
  const r = new Session(exports, kernel, { jit, machine: { ramSize: RAM }, restore: snap });
  check(r.m.steps === cutSteps, `ripristinata a ${r.m.steps} istruzioni invece di ${cutSteps}`);
  check(r.m.memoryBytes < memBefore + 2 ** 30, `seconda macchina: memoria ${r.m.memoryBytes} byte, la regione non è stata riusata`);
  await r.poweroff(0);
  check(r.m.steps === steps, `dopo il ripristino ${r.m.steps} istruzioni invece di ${steps}`);
  check(r.log === tail, `seguito del log diverso dopo il ripristino:\n${r.tail()}`);
  console.log(`ripristino su una macchina nuova da 3 GiB: ${r.restoreMs.toFixed(0)} ms, seguito identico (${((performance.now() - t1) / 1000).toFixed(1)} s)`);
  r.m.free();
});
