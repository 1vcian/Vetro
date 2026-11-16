#!/usr/bin/env node
// ADR 0051: the guest CPU speed in the web app, in headless Chrome.
//
// At speed 1 the guest's clock allows 100 M instructions per second of guest
// time, and the page keeps guest time from running ahead of real time: a
// host that can do more waits while the guest still has work. The Worker
// raises the speed when that happens (and lowers it when the guest falls
// behind); `&speed=N` fixes it.
//
//   1. `/app/?os=linux&autostart=1&speed=1&snapshot=0`: a shell loop of
//      LOOP iterations (about 15 000 instructions each) cannot finish before
//      its instructions' worth of guest time has passed in real time.
//   2. `/app/?os=linux&autostart=1` (automatic): the same loop is over
//      sooner, the page's stats show a speed above 1, and the guest's own
//      seconds are still real seconds (`sleep 2` takes about 2 s).
//   3. A second session from the snapshot saved with the speed raised: it
//      restores (the snapshot carries speed and clock bias) and answers.
//
// Chrome: VETRO_CHROME, otherwise the usual paths (SKIP without it; with
// VETRO_REQUIRE_BROWSER=1 an error).
//
//   node tests/web/browser-speed.mjs

import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, run } from './lib.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';

const LOOP = 60_000;
const BUSY = `i=0; while [ $i -lt ${LOOP} ]; do i=$((i+1)); done; echo VETRO-"L"OOP-$i`;

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail('Chrome not found (VETRO_CHROME)');
    console.log('SKIP: Chrome not found (VETRO_CHROME)');
    return;
  }
  const srv = await serve({ mounts: appMounts() });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-speed-'));
  const { proc, cdp } = await launch(chrome, profile);
  const stats = (page) => page.eval('JSON.stringify(window.vetroState?.stats ? { speed: window.vetroState.stats.speed, steps: window.vetroState.stats.steps, guest: window.vetroState.stats.guestSecs } : null)').then(JSON.parse);
  /** Types `cmd`, waits for `marker`: [wall seconds, guest instructions, guest seconds, console position]. */
  const timed = async (page, cmd, marker, at) => {
    const s0 = await stats(page);
    const t0 = Date.now();
    await page.type(cmd);
    const end = await page.until(marker, at);
    const wall = (Date.now() - t0) / 1000;
    // The stats are posted about once a second: wait for one after the end.
    await new Promise((ok) => setTimeout(ok, 1300));
    const s1 = await stats(page);
    return { wall, steps: s1.steps - s0.steps, guest: s1.guest - s0.guest, speed: s1.speed, end };
  };
  try {
    const base = { os: 'linux', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1' };
    // ---- 1. Speed 1: the clock's limit ------------------------------------
    let { page, targetId } = await openPage(cdp, `${srv.url}/app/?${new URLSearchParams({ ...base, speed: '1', snapshot: '0' })}`);
    let at = await page.until('# ');
    await page.waitFor('stats', async () => (await stats(page)) || null, 20_000);
    const one = await timed(page, BUSY, `VETRO-LOOP-${LOOP}`, at);
    check(one.speed === 1, `speed=1 is not kept: ${JSON.stringify(one)}`);
    // What the loop itself costs, from the guest's own time at 100 M
    // instructions per second (the stats' window adds idle time around it).
    const instr = LOOP * 15_000;
    check(one.wall >= 0.85 * instr / 1e8, `at speed 1 the loop (about ${(instr / 1e6).toFixed(0)} M instructions) took ${one.wall.toFixed(2)} s: faster than its clock allows`);
    console.log(`speed 1: the loop took ${one.wall.toFixed(2)} s of real time (the clock allows 100 M instructions per second)`);
    await cdp.send('Target.closeTarget', { targetId });

    // ---- 2. Automatic -------------------------------------------------------
    const url = `${srv.url}/app/?${new URLSearchParams(base)}`;
    ({ page, targetId } = await openPage(cdp, url));
    at = await page.until('# ');
    await page.waitFor('stats', async () => (await stats(page)) || null, 20_000);
    const auto = await timed(page, BUSY, `VETRO-LOOP-${LOOP}`, at);
    check(auto.speed > 1, `the speed was not raised: ${JSON.stringify(auto)} (speed 1: ${JSON.stringify(one)})`);
    check(auto.wall < 0.85 * one.wall, `automatic speed: the loop took ${auto.wall.toFixed(2)} s against ${one.wall.toFixed(2)} s at speed 1`);
    // Guest seconds are still real seconds: the clock goes on from its value
    // at every change, and an idle guest waits for real time.
    const t0 = Date.now();
    await page.type('sleep 2; echo VETRO-"S"LEPT');
    at = await page.until('VETRO-SLEPT', auto.end);
    const slept = (Date.now() - t0) / 1000;
    check(slept > 1.8 && slept < 4, `sleep 2 took ${slept.toFixed(2)} s of real time at speed ${auto.speed}`);
    console.log(`automatic: speed ×${auto.speed}, the loop took ${auto.wall.toFixed(2)} s; sleep 2 took ${slept.toFixed(2)} s`);

    // ---- 3. From the snapshot, with the speed in it ---------------------------
    await page.waitFor('snapshot at the prompt', async () => (await page.state()).snapshots[0], 120_000);
    await cdp.send('Target.closeTarget', { targetId });
    ({ page, targetId } = await openPage(cdp, url));
    const boot = await page.waitFor('restore', async () => (await page.state())?.boot, 120_000);
    check(boot.mode === 'snapshot', `second session from ${boot.mode}`);
    await page.type('echo VETRO-"A"GAIN-$((6*7))');
    await page.until('VETRO-AGAIN-42');
    const t1 = Date.now();
    await page.type('sleep 1; echo VETRO-"S"LEPT2');
    await page.until('VETRO-SLEPT2');
    const slept2 = (Date.now() - t1) / 1000;
    check(slept2 > 0.9 && slept2 < 3, `after the restore sleep 1 took ${slept2.toFixed(2)} s`);
    console.log(`second session from the snapshot: the shell answers, sleep 1 took ${slept2.toFixed(2)} s`);
  } finally {
    await closeChrome(proc, cdp, profile);
    await srv.close();
  }
});
