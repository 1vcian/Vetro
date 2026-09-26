// Booting from Android images and 3 GiB of RAM in vetro-wasm (M5, ADR 0018
// and 0028), fast: the M3 guest kernel packaged the way a GKI build packages
// it (a v4 `boot.img` with the gzip kernel, a v4 `init_boot.img` with the
// initramfs, from AOSP's `mkbootimg.py` in tools/mkbootimg/) and loaded with
// `vetro_load_android` (ABI 12) on a machine with 3 GiB of RAM: on wasm32 the
// RAM beyond 2 GiB is a region outside the allocator (board.rs).
//
// - Instructions and log equal to the native reference `ram3g`
//   (tests/boot/tests/web.rs: same RAM, direct `load_linux` with Image and
//   initramfs): the bootloader hands the kernel the same bytes, and the
//   wasm32 region behaves like the host's `Vec`.
// - Snapshot at the prompt (whole and chunked: the same file), machine
//   destroyed, a new 3 GiB machine (reusing the region, which must be zeroed)
//   and a chunked restore: the rest of the log and the final instruction
//   count are those of the uncut run.
// - A tiny JIT code limit (jit-engine.mjs): many resets, same execution.
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
    throw new Fail(`gzip not available: ${e.message}`);
  }
  writeFileSync(join(dir, 'Image.gz'), gz);
  writeFileSync(join(dir, 'initramfs.cpio.gz'), kernel.initrd);
  const mk = (args) => {
    try {
      execFileSync('python3', [join(root, 'tools/mkbootimg/mkbootimg.py'), ...args], { stdio: 'pipe' });
    } catch (e) {
      throw new Fail(`mkbootimg.py (needs python3): ${e.stderr?.toString() ?? e.message}`);
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
  check(/kernel gzip/.test(desc) && desc.includes('init_boot') && desc.includes(`command line: ${CMDLINE}`), `bootloader description: ${desc}`);
  const memBefore = s.m.memoryBytes;
  check(memBefore >= 3 * 2 ** 30, `module memory ${memBefore} bytes: the 3 GiB of RAM are missing`);
  let at = await s.until(SHELL_PROMPT);
  const [, end] = await s.command('head -1 /proc/meminfo', at);
  const kb = Number(/MemTotal:\s+(\d+) kB/.exec(s.text(at, end))?.[1]);
  check(kb > 2.9 * 2 ** 20, `MemTotal ${kb} kB with 3 GiB of RAM`);
  const snap = s.m.snapshotSave();
  // Chunked (vetro_snapshot_save_stream + import vetro_host.snapshot_write): the same file.
  const parts = [];
  const total = s.m.snapshotSaveTo((bytes, at) => parts.push([at, bytes.slice()]));
  const streamed = new Uint8Array(total);
  for (const [at, bytes] of parts) streamed.set(bytes, at);
  check(total === snap.length && Buffer.compare(Buffer.from(streamed), Buffer.from(snap)) === 0, `chunked snapshot differs (${total} against ${snap.length} bytes)`);
  check(parts.length > 2, `snapshot in ${parts.length} chunks`);
  const cut = s.log.length;
  const cutSteps = s.m.steps;
  await s.poweroff(end);
  console.log(`boot from boot.img with 3 GiB: ${s.m.steps} instructions in ${((performance.now() - t0) / 1000).toFixed(1)} s, MemTotal ${kb} kB, snapshot ${(snap.length / 2 ** 20).toFixed(1)} MiB, module memory ${(memBefore / 2 ** 20).toFixed(0)} MiB`);
  compareNative('ram3g', s.m.steps, s.log);
  const steps = s.m.steps;
  const tail = s.log.slice(cut);
  s.m.free();

  // A new machine: reuses the first one's region (zeroed) and restores.
  const t1 = performance.now();
  const r = new Session(exports, kernel, { jit, machine: { ramSize: RAM }, setup: (sess) => {
    // Chunked restore (vetro_snapshot_restore_stream, import vetro_host.snapshot_read).
    const t = performance.now();
    sess.m.snapshotRestoreStream(snap.length, (view, at) => view.set(snap.subarray(at, at + view.length)));
    sess.restoreMs = performance.now() - t;
  }, load: () => {} });
  check(r.m.steps === cutSteps, `restored at ${r.m.steps} instructions instead of ${cutSteps}`);
  check(r.m.memoryBytes < memBefore + 2 ** 30, `second machine: memory ${r.m.memoryBytes} bytes, the region was not reused`);
  await r.poweroff(0);
  check(r.m.steps === steps, `after the restore ${r.m.steps} instructions instead of ${steps}`);
  check(r.log === tail, `different log continuation after the restore:\n${r.tail()}`);
  console.log(`restore on a new 3 GiB machine: ${r.restoreMs.toFixed(0)} ms, identical continuation (${((performance.now() - t1) / 1000).toFixed(1)} s)`);
  r.m.free();

  // JIT code limit (jit-engine.mjs, ADR 0028): with a small limit the engine
  // resets often, and the execution stays the same.
  if (jit) {
    const small = await loadVetro({ jitBudget: 256 << 10 });
    const b = new Session(small.exports, kernel, { jit, machine: { ramSize: RAM }, load });
    const at2 = await b.until(SHELL_PROMPT);
    const [, end2] = await b.command('head -1 /proc/meminfo', at2);
    await b.poweroff(end2);
    const st = b.m.jitStats();
    check(b.m.steps === steps && b.log === s.log, `with the JIT code limit: ${b.m.steps} instructions instead of ${steps}, or a different log`);
    check(st.resets > 2 && small.jit.stats.refused > 2, `JIT code limit: ${st.resets} resets, ${small.jit.stats.refused} refusals`);
    console.log(`256 KiB JIT code limit: ${st.resets} resets, same execution (${b.m.steps} instructions)`);
    b.m.free();
  }
});
