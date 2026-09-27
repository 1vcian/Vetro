#!/usr/bin/env node
// Responsiveness of Vetro's AOSP image in the app as the user feels it, in
// headless Chrome (M5/M6): how long a tap takes to change the screen, frames
// per second at rest and during an animation, how far guest time runs from
// the real clock. Long and needs a Chrome profile whose last snapshot has the
// test app (tests/apps/tocco) on screen, as left by android-chrome.mjs:
//
//   VETRO_ANDROID=1 VETRO_ANDROID_PREBUILT=1 VETRO_ANDROID_PROFILE=DIR node tests/web/android-chrome.mjs
//   VETRO_ANDROID=1 VETRO_ANDROID_PROFILE=DIR node tests/web/android-responsiveness.mjs
//
// One session from that profile: the start time from the snapshot (M6's
// second start), 20 s at rest (frames, guest speed and its distance from the
// real clock), VETRO_TAPS taps (default 8) at the centre of the test app, each
// flipping its colour (blue/orange): the time from the press to the first
// frame drawn and to the first frame that covers the centre, polled pixel
// check as a cross-check; then the Home key (the launcher animation): frames
// per second over 4 s. Results on stdout and in
// target/aosp/responsiveness.json. VETRO_TAP_LIMIT_MS (unset: none) fails the
// run when the median tap-to-frame time is above it; VETRO_APP_QUERY adds URL
// parameters (a device profile, `graphics=full`).

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { colorSeen, DEFAULT_MANIFEST } from '../../web/node/android.mjs';
import { ARANCIONE, BLU, CENTRE, measureTaps, median } from './taps.mjs';

if (process.env.VETRO_ANDROID !== '1' || !process.env.VETRO_ANDROID_PROFILE) {
  console.log('SKIP: Android responsiveness in Chrome (VETRO_ANDROID=1 and VETRO_ANDROID_PROFILE=DIR to run it)');
  process.exit(0);
}

const TAPS = Number(process.env.VETRO_TAPS ?? 8);
const out = join(root, 'target/aosp');
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
const PERF = `(() => { const p = window.vetroState.perf; return { frames: p.frames, pixels: p.pixels, drawMs: p.drawMs, maxDrawMs: p.maxDrawMs, input: p.input ?? null }; })()`;

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  mkdirSync(out, { recursive: true });
  const srv = await serve({ mounts: appMounts(), port: Number(process.env.VETRO_WEB_PORT ?? 8080) });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST;
  // VETRO_APP_QUERY: more URL parameters, e.g. `&profile=default&graphics=full`.
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}${process.env.VETRO_APP_QUERY ?? ''}`;
  const res = { manifest, query: process.env.VETRO_APP_QUERY ?? '', start: new Date().toISOString() };
  const { proc, cdp } = await launch(chrome, process.env.VETRO_ANDROID_PROFILE);
  try {
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    const boot = await page.waitFor('restore', async () => {
      const st = await page.state();
      if (st?.stopped) throw new Fail(`machine stopped: ${JSON.stringify(st.stopped)}`);
      return st?.boot;
    }, 10 * 60_000);
    check(boot.mode === 'snapshot', `start from ${boot.mode}: the profile has no snapshot`);
    const firstFrame = await page.waitFor('first frame', async () => (await page.state())?.firstFrame, 60_000);
    res.second_start = { ready_ms: boot.ms, first_frame_ms: firstFrame, open_ms: Date.now() - t0, times: boot.times, size: boot.size };
    console.log(`start from the snapshot: ready ${(boot.ms / 1000).toFixed(1)} s after the page opened (read ${boot.times.readSnapshot?.toFixed(0)} ms, ` +
      `restore ${boot.times.restore?.toFixed(0)} ms), first frame at ${(firstFrame / 1000).toFixed(1)} s`);
    await sleep(5000);
    const c0 = await page.eval(CENTRE);
    check(colorSeen(c0, BLU) || colorSeen(c0, ARANCIONE), `the test app is not on screen (centre ${c0}): run android-chrome.mjs with this profile first`);

    // At rest.
    const samples = [];
    const p0 = await page.eval(PERF);
    const r0 = Date.now();
    for (let i = 0; i < 20; i++) {
      await sleep(1000);
      const s = (await page.state()).stats;
      samples.push({ wall: Date.now() - r0, guestSecs: s.guestSecs, mips: s.mips, aheadMs: s.aheadMs });
    }
    const p1 = await page.eval(PERF);
    const restWall = (Date.now() - r0) / 1000;
    const guestRate = (samples.at(-1).guestSecs - samples[0].guestSecs) / ((samples.at(-1).wall - samples[0].wall) / 1000);
    res.rest = {
      fps: (p1.frames - p0.frames) / restWall,
      mips: median(samples.map((s) => s.mips)),
      guest_per_real: guestRate,
      ahead_ms: samples.map((s) => Math.round(s.aheadMs ?? 0)),
    };
    console.log(`at rest (${restWall.toFixed(0)} s): ${res.rest.fps.toFixed(2)} frames/s, ${res.rest.mips.toFixed(0)} MIPS, ` +
      `guest time ${guestRate.toFixed(2)}x real, ahead of the real clock ${res.rest.ahead_ms[0]} -> ${res.rest.ahead_ms.at(-1)} ms`);

    // Taps at the centre of the test app.
    res.taps = await measureTaps(page, cdp, TAPS);
    check(res.taps.length === TAPS && res.taps.every((t) => t.polled_ms !== null), 'a tap did not change the test app\'s colour in 120 s');
    res.tap_median = { frame_ms: median(res.taps.map((t) => t.frame_ms)), centre_ms: median(res.taps.map((t) => t.centre_ms)), polled_ms: median(res.taps.map((t) => t.polled_ms)) };
    console.log(`taps: median first frame ${res.tap_median.frame_ms?.toFixed(0)} ms, centre ${res.tap_median.centre_ms?.toFixed(0)} ms, polled ${res.tap_median.polled_ms} ms`);

    // An animation: Home (KEY_HOMEPAGE) from the app to the launcher.
    await page.eval("document.getElementById('screen').focus()");
    const a0 = await page.eval(PERF);
    const ta = await page.eval('performance.now()');
    const base = { code: 'BrowserHome', key: 'BrowserHome', windowsVirtualKeyCode: 0xac, nativeVirtualKeyCode: 0xac };
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', ...base }, page.s);
    await sleep(80);
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base }, page.s);
    await sleep(4000);
    const a1 = await page.eval(PERF);
    const first = await page.eval(`(() => { const f = window.vetroState.perf.frameLog.find((f) => f.t > ${ta}); return f ? f.t - ${ta} : null; })()`);
    const busy = await page.eval(`(() => { const l = window.vetroState.perf.frameLog.filter((f) => f.t > ${ta}); return l.length > 1 ? (l.length - 1) / ((l.at(-1).t - l[0].t) / 1000) : null; })()`);
    res.home_animation = { frames: a1.frames - a0.frames, fps_4s: (a1.frames - a0.frames) / 4, fps_while_drawing: busy, first_frame_ms: first,
      draw_ms_mean: (a1.drawMs - a0.drawMs) / Math.max(1, a1.frames - a0.frames), max_draw_ms: a1.maxDrawMs };
    console.log(`Home key: first frame ${first?.toFixed(0)} ms, ${res.home_animation.frames} frames in 4 s (${busy?.toFixed(1)} frames/s while drawing), ` +
      `draw ${res.home_animation.draw_ms_mean.toFixed(2)} ms per frame (max ${a1.maxDrawMs.toFixed(1)} ms)`);
    const limit = process.env.VETRO_TAP_LIMIT_MS;
    if (limit) check(res.tap_median.frame_ms <= Number(limit), `median tap-to-frame ${res.tap_median.frame_ms} ms > ${limit} ms`);
  } finally {
    res.end = new Date().toISOString();
    writeFileSync(join(out, 'responsiveness.json'), JSON.stringify(res, null, 2));
    console.log('measurements: target/aosp/responsiveness.json');
    await Promise.race([cdp.send('Browser.close').catch(() => {}), sleep(3000)]);
    cdp.close();
    try {
      process.kill(-proc.pid, 'SIGKILL');
    } catch {}
    await srv.close();
  }
});
