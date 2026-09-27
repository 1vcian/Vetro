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
// run when the median tap-to-frame time is above it.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { colorSeen, DEFAULT_MANIFEST } from '../../web/node/android.mjs';

if (process.env.VETRO_ANDROID !== '1' || !process.env.VETRO_ANDROID_PROFILE) {
  console.log('SKIP: Android responsiveness in Chrome (VETRO_ANDROID=1 and VETRO_ANDROID_PROFILE=DIR to run it)');
  process.exit(0);
}

const TAPS = Number(process.env.VETRO_TAPS ?? 8);
const out = join(root, 'target/aosp');
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
const median = (xs) => {
  const s = xs.filter((x) => x !== null).sort((a, b) => a - b);
  return s.length ? s[s.length >> 1] : null;
};
const CENTRE = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(c.width >> 1, c.height >> 1, 1, 1).data;
  return [d[0], d[1], d[2]];
})()`;
const PERF = `(() => { const p = window.vetroState.perf; return { frames: p.frames, pixels: p.pixels, drawMs: p.drawMs, maxDrawMs: p.maxDrawMs, input: p.input ?? null }; })()`;

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  mkdirSync(out, { recursive: true });
  const srv = await serve({ mounts: appMounts(), port: Number(process.env.VETRO_WEB_PORT ?? 8080) });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST;
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}`;
  const res = { manifest, start: new Date().toISOString() };
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
    const r = await page.eval("(() => { const b = document.getElementById('screen').getBoundingClientRect(); return { x: b.left + b.width / 2, y: b.top + b.height / 2 }; })()");
    res.taps = [];
    let colour = (await page.eval(CENTRE));
    for (let i = 0; i < TAPS; i++) {
      const want = colorSeen(colour, BLU) ? ARANCIONE : BLU;
      const n = (await page.eval('window.vetroState.perf.taps.length'));
      const tt = Date.now();
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
      await sleep(80);
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
      let polled = null;
      for (;;) {
        colour = await page.eval(CENTRE);
        if (colorSeen(colour, want)) {
          polled = Date.now() - tt;
          break;
        }
        if (Date.now() - tt > 120_000) break;
        await sleep(20);
      }
      const tap = await page.eval(`(() => {
        const p = window.vetroState.perf, tap = p.taps[${n}], c = document.getElementById('screen');
        const cx = c.width >> 1, cy = c.height >> 1;
        const f = p.frameLog.find((f) => f.t > tap.t && f.x <= cx && cx < f.x + f.w && f.y <= cy && cy < f.y + f.h);
        return { frameMs: tap.frameMs, centreMs: f ? f.t - tap.t : null, workerMs: f?.workerMs ?? null, applied: p.input };
      })()`);
      const one = { frame_ms: tap.frameMs, centre_ms: tap.centreMs, polled_ms: polled, input_wait_ms: tap.applied?.lastMs ?? null };
      res.taps.push(one);
      console.log(`tap ${i + 1}: first frame ${one.frame_ms?.toFixed(0)} ms, centre changed ${one.centre_ms?.toFixed(0)} ms ` +
        `(pixel polled ${polled} ms), waited in the Worker ${one.input_wait_ms?.toFixed(1)} ms`);
      check(polled !== null, `tap ${i + 1}: the centre did not change colour in 120 s`);
      await sleep(1500);
    }
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
