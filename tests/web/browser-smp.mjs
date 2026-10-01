#!/usr/bin/env node
// ADR 0042: the web app with two guest cores in parallel, in headless Chrome.
//
//   1. `/app/?cpus=2&autostart=1` on the isolated local server (COOP/COEP):
//      the threads build of vetro-wasm, core 1 in a Worker of its own; the M3
//      kernel boots to the shell and sees two processors; the status says
//      the cores run in parallel. The snapshot at the prompt is saved with
//      the cores back in turns (the page's state says so) and the cores go
//      parallel again; the shell still answers.
//   2. Second session, same profile: the machine resumes from that snapshot
//      with its two cores in parallel again; the shell answers.
//   3. `?cpus=auto` picks from navigator.hardwareConcurrency (the page's
//      `autoCpus`, the rule 6+ logical cores → 4, 3+ → 2, else 1).
//
// Chrome: VETRO_CHROME, otherwise the usual paths (SKIP without it; with
// VETRO_REQUIRE_BROWSER=1 an error).
//
//   node tests/web/browser-smp.mjs

import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail('Chrome not found (VETRO_CHROME)');
    console.log('SKIP: Chrome not found (VETRO_CHROME)');
    return;
  }
  const threads = join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm_threads.wasm');
  check(existsSync(threads), 'the threads build is missing: tools/wasm-threads.sh');
  const srv = await serve({ mounts: appMounts() });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-smp-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const q = new URLSearchParams({ cpus: '2', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1' });
    const url = `${srv.url}/app/?${q}`;
    const t0 = Date.now();
    let { page, targetId } = await openPage(cdp, url);
    check(await page.eval('crossOriginIsolated'), 'the page is not cross-origin isolated');
    let at = await page.until('# ');
    console.log(`two cores: prompt in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
    const snap = await page.waitFor('snapshot at the prompt', async () => (await page.state()).snapshots[0], 120_000);
    console.log(`snapshot with the cores in turns: ${(snap.size / 2 ** 20).toFixed(1)} MiB in ${snap.saveMs.toFixed(0)} ms`);
    await page.waitFor('cores in parallel', async () => (await page.eval("document.getElementById('status').textContent")).includes('2 cores in parallel') || null, 30_000)
      .catch(async (e) => {
        throw new Fail(`${e.message}: status "${await page.eval("document.getElementById('status').textContent")}"`);
      });
    await page.type('grep -c ^processor /proc/cpuinfo; echo VETRO-NPROC-$(nproc)');
    at = await page.until('VETRO-NPROC-2', at);
    console.log('the guest sees two processors, the cores run in parallel');
    // Work on both cores at once, then the shell answers.
    await page.type('(i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done) & (i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done) & wait; echo VETRO-PAR-OK');
    at = await page.until('VETRO-PAR-OK', at);
    console.log('two busy shells on two cores: done');
    await cdp.send('Target.closeTarget', { targetId });

    // Second session: from the snapshot, cores parallel again.
    const t1 = Date.now();
    ({ page, targetId } = await openPage(cdp, url));
    const boot = await page.waitFor('restore', async () => (await page.state())?.boot, 120_000);
    check(boot.mode === 'snapshot', `second session from ${boot.mode}`);
    await page.waitFor('cores in parallel after the restore', async () => (await page.eval("document.getElementById('status').textContent")).includes('2 cores in parallel') || null, 30_000);
    await page.type('echo VETRO-AGAIN-$(nproc)');
    await page.until('VETRO-AGAIN-2');
    console.log(`second session from the snapshot: two cores in parallel, the shell answers (${((Date.now() - t1) / 1000).toFixed(1)} s)`);

    // The automatic choice.
    const auto = await page.eval("import('./main.mjs').then((m) => [1, 2, 3, 4, 6, 8, 16].map((n) => m.autoCpus(n)))");
    check(JSON.stringify(auto) === '[1,1,2,2,4,4,4]', `autoCpus: ${JSON.stringify(auto)}`);
    console.log(`auto: ${JSON.stringify(auto)} for 1, 2, 3, 4, 6, 8, 16 logical cores`);
  } finally {
    await closeChrome(proc, cdp, profile);
    await srv.close();
  }
});
