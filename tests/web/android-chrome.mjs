#!/usr/bin/env node
// Vetro's AOSP image in the app, in headless Chrome (M5/M6, ADR 0028). Long
// (tens of minutes or more): runs only with VETRO_ANDROID=1.
//
//   VETRO_ANDROID=1 node tests/web/android-chrome.mjs
//
// 1. First boot: `/app/?os=android&manifest=...&autostart=1`, images and disk
//    from a local server (target/aosp/out, `/aosp/`) or from
//    VETRO_ANDROID_MANIFEST (R2 needs port 8080: VETRO_WEB_PORT=8080, the
//    bucket's CORS allows it). The boot phases with wall times, up to
//    sys.boot_completed and the home screen (launcher focused, seen with adb,
//    and drawn on the scanout); adb connected; the snapshot saved in OPFS
//    (size, times, module memory). Home screenshot in
//    target/aosp/chrome-home-1.png.
// 2. Second start, same profile: from the snapshot. Time from opening the
//    page to the machine ready and to the first frame (M6's criterion: home
//    screen in under 15 s from the cache), screenshot in
//    target/aosp/chrome-home-2.png.
// 3. The test APK (tests/apps/tocco, target/apps/tocco.apk) installed with
//    `window.vetroAndroid.install` (the same path as a drop) and opened: the
//    centre of the screen turns blue; a real click on the canvas (a touch on
//    the touchscreen) turns it orange. adb devices and shell from the page.
//    Screenshots chrome-app-*.png.
//
// The times go to stdout and to target/aosp/chrome-measurements.json.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { colorSeen } from '../../web/node/android.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: long Android test in Chrome (VETRO_ANDROID=1 to run it)');
  process.exit(0);
}

const BOOT_LIMIT_MS = Number(process.env.VETRO_ANDROID_BOOT_MINUTES ?? 240) * 60_000;
const out = join(root, 'target/aosp');
const misure = { inizio: new Date().toISOString() };
const secs = (ms) => (ms / 1000).toFixed(1);
/** The AOSP state in the page (also before main.mjs is loaded). */
const ANDROID_STATE = "window.vetroAndroid ? window.vetroAndroid.state() : { phases: [], adb: { state: 'none' } }";

/** The canvas PNG (dataURL) to a file. */
async function screenshot(page, name) {
  const url = await page.eval("document.getElementById('screen').toDataURL('image/png')");
  writeFileSync(join(out, name), Buffer.from(url.split(',')[1], 'base64'));
  console.log(`screenshot: target/aosp/${name}`);
}

/** Pixel at the centre of the canvas and image variety (distinct colours on a grid). */
const SCREEN = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
  const at = (x, y) => { const o = (y * c.width + x) * 4; return [d[o], d[o + 1], d[o + 2]]; };
  const colors = new Set();
  for (let y = 0; y < c.height; y += 16) for (let x = 0; x < c.width; x += 16) colors.add(at(x, y).join());
  return { w: c.width, h: c.height, center: at(c.width >> 1, c.height >> 1), colors: colors.size, off: !document.getElementById('screen-off').hidden };
})()`;
const near = (a, b) => colorSeen(a, b) !== null;
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];

async function session(chrome, profile, url, first) {
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    if (first) {
      // Boot phases, up to sys.boot_completed.
      let seen = 0;
      await page.waitFor('sys.boot_completed', async () => {
        const st = await page.eval(ANDROID_STATE);
        for (const p of st.phases.slice(seen)) console.log(`phase: ${p.label} at ${p.guestSecs.toFixed(0)} s of guest time, ${secs(p.wallMs)} s wall`);
        seen = st.phases.length;
        const s = await page.eval('window.vetroState ?? {}');
        if (s.stopped) throw new Fail(`machine stopped: ${JSON.stringify(s.stopped)}`);
        return st.booted;
      }, BOOT_LIMIT_MS);
      const st = await page.eval(ANDROID_STATE);
      misure.fasi = st.phases;
      misure.boot_completed = st.booted;
      console.log(`boot finished: ${st.booted.guestSecs.toFixed(0)} s of guest time, ${secs(st.booted.wallMs)} s wall`);
      const home = await page.waitFor('home screen (launcher)', async () => (await page.eval(ANDROID_STATE)).home, BOOT_LIMIT_MS);
      misure.home = home;
      console.log(`home screen drawn: ${home.guestSecs.toFixed(0)} s of guest time, ${secs(home.wallMs)} s wall (launcher focused at ${home.focusGuestSecs?.toFixed(0)} s, ${home.colors} colours; ${home.activity})`);
      const snap = await page.waitFor('snapshot after the boot', async () => (await page.state())?.snapshots[0], 30 * 60_000);
      misure.snapshot_avvio = snap;
      console.log(`snapshot: ${(snap.size / 2 ** 20).toFixed(0)} MiB, save ${snap.saveMs.toFixed(0)} ms, OPFS write ${snap.writeMs.toFixed(0)} ms, module memory ${(snap.memory / 2 ** 20).toFixed(0)} MiB`);
      const adb = await page.waitFor('adb connected', async () => {
        const a = (await page.eval(ANDROID_STATE)).adb;
        return a.state === 'ready' ? a : null;
      }, 10 * 60_000);
      console.log(`adb: ${JSON.stringify(adb.devices)}`);
      const s = await page.state();
      misure.memoria_max = s.memory;
      misure.stats = s.stats;
      console.log(`module memory (peak from the stats): ${(s.memory / 2 ** 20).toFixed(0)} MiB`);
      await screenshot(page, 'chrome-home-1.png');
      misure.home_1 = await page.eval(SCREEN);
      check(misure.home_1.colors > 20, `home screen: almost uniform screen (${misure.home_1.colors} colours)`);
      return;
    }
    // Second start: from the snapshot.
    const boot = await page.waitFor('restore', async () => (await page.state())?.boot, 5 * 60_000);
    check(boot.mode === 'snapshot', `second start from ${boot.mode}, not from the snapshot`);
    const frame = await page.waitFor('first frame', async () => (await page.state())?.firstFrame, 60_000);
    misure.ripristino = { pronto_ms: boot.ms, primo_fotogramma_ms: frame, apertura_ms: Date.now() - t0, times: boot.times, size: boot.size, memoria: boot.memory };
    console.log(`second start from the snapshot: ready ${secs(boot.ms)} s after the page opened (read ${boot.times.readSnapshot?.toFixed(0)} ms, restore ${boot.times.restore?.toFixed(0)} ms), first frame at ${secs(frame)} s`);
    await new Promise((ok) => setTimeout(ok, 3000));
    await screenshot(page, 'chrome-home-2.png');
    misure.home_2 = await page.eval(SCREEN);
    check(misure.home_2.colors > 20, `home screen after the restore: almost uniform screen (${misure.home_2.colors} colours)`);
    // adb after the restore, then the APK.
    await page.waitFor('adb connected after the restore', async () => (await page.eval(ANDROID_STATE)).adb.state === 'ready', 10 * 60_000);
    const devices = await page.eval('window.vetroAndroid.devices()');
    check(devices[0]?.serial === 'VETRO00001', `adb devices: ${JSON.stringify(devices)}`);
    const sh = await page.eval("window.vetroAndroid.shell('getprop sys.boot_completed; getprop ro.product.model')");
    check(sh.stdout.startsWith('1\n'), `adb shell: ${JSON.stringify(sh)}`);
    console.log(`adb after the restore: ${JSON.stringify(devices)}, shell ${JSON.stringify(sh.stdout)}`);
    const apk = join(root, 'target/apps/tocco.apk');
    check(existsSync(apk), 'target/apps/tocco.apk missing: tests/apps/tocco/build.sh');
    const b64 = readFileSync(apk).toString('base64');
    const ti = Date.now();
    const inst = await page.eval(`window.vetroAndroid.install(Uint8Array.from(atob('${b64}'), (c) => c.charCodeAt(0)).buffer, 'tocco.apk')`);
    misure.install = { ms: Date.now() - ti, installMs: inst.installMs, openMs: inst.openMs, component: inst.component };
    console.log(`APK installed and opened in ${secs(Date.now() - ti)} s (install ${secs(inst.installMs)} s, open ${secs(inst.openMs)} s): ${inst.component}`);
    check(inst.info.package === 'it.vetro.tocco' && inst.component === 'it.vetro.tocco/it.vetro.tocco.Main', `install: ${JSON.stringify(inst)}`);
    await page.waitFor('app on screen (blue centre)', async () => near((await page.eval(SCREEN)).center, BLU), 5 * 60_000);
    await screenshot(page, 'chrome-app-1.png');
    // A real click on the canvas: a touch on the guest's touchscreen.
    const r = await page.eval("(() => { const b = document.getElementById('screen').getBoundingClientRect(); return { x: b.left + b.width / 2, y: b.top + b.height / 2 }; })()");
    const tt = Date.now();
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    await new Promise((ok) => setTimeout(ok, 300));
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    await page.waitFor('touch received (orange centre)', async () => near((await page.eval(SCREEN)).center, ARANCIONE), 5 * 60_000);
    misure.tocco_ms = Date.now() - tt;
    console.log(`touch received by the app: the centre is orange after ${secs(misure.tocco_ms)} s`);
    await screenshot(page, 'chrome-app-2.png');
    const top = await page.eval("window.vetroAndroid.shell('dumpsys window | grep -m1 mCurrentFocus')");
    check(top.stdout.includes('it.vetro.tocco'), `focused window: ${top.stdout}`);
    misure.snapshot_app = await page.waitFor('snapshot after the install', async () => (await page.state())?.snapshots.find((x) => x.why === 'app installed'), 10 * 60_000);
    console.log(`snapshot after the install: ${(misure.snapshot_app.size / 2 ** 20).toFixed(0)} MiB in ${(misure.snapshot_app.saveMs + misure.snapshot_app.writeMs).toFixed(0)} ms`);
  } finally {
    await Promise.race([cdp.send('Browser.close').catch(() => {}), new Promise((ok) => setTimeout(ok, 3000))]);
    cdp.close();
    try {
      process.kill(-proc.pid, 'SIGKILL');
    } catch {}
    await new Promise((ok) => setTimeout(ok, 1000));
  }
}

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  mkdirSync(out, { recursive: true });
  const port = Number(process.env.VETRO_WEB_PORT ?? 0);
  const srv = await serve({ mounts: appMounts(), port });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? `${srv.url}/aosp/manifest.json`;
  if (!process.env.VETRO_ANDROID_MANIFEST) check(existsSync(join(out, 'out/web/disk.json')), 'target/aosp/out/web/disk.json missing: node tools/aosp/web-disk.mjs');
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}`;
  misure.manifest = manifest;
  // VETRO_ANDROID_PROFILE: a Chrome profile to keep (with the snapshot in
  // OPFS); VETRO_ANDROID_SKIP_BOOT=1 skips the first boot and resumes from the
  // snapshot already there.
  const keep = process.env.VETRO_ANDROID_PROFILE;
  const profile = keep ?? mkdtempSync(join(tmpdir(), 'vetro-android-chrome-'));
  if (keep) mkdirSync(keep, { recursive: true });
  let ok = false;
  try {
    if (process.env.VETRO_ANDROID_SKIP_BOOT !== '1') await session(chrome, profile, url, true);
    await session(chrome, profile, url, false);
    ok = true;
  } finally {
    misure.fine = new Date().toISOString();
    writeFileSync(join(out, 'chrome-measurements.json'), JSON.stringify(misure, null, 2));
    console.log('measurements: target/aosp/chrome-measurements.json');
    await srv.close();
    if (!keep && ok) {
      try {
        rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      } catch (e) {
        console.log(`warning: Chrome profile not removed (${profile}): ${e.message}`);
      }
    } else console.log(`Chrome profile kept: ${profile}`);
  }
});
