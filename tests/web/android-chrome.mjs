#!/usr/bin/env node
// Vetro's AOSP image in the app, in headless Chrome (M5/M6, ADR 0028). Long
// (tens of minutes or more): runs only with VETRO_ANDROID=1.
//
//   VETRO_ANDROID=1 node tests/web/android-chrome.mjs
//
// The page is the zero-choice default (`/app/`, M10: it starts by itself),
// with only the image's location given (`?manifest=...`).
//
// 1. First boot: `/app/?cold=1&manifest=...`, images and disk
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
// With VETRO_ANDROID_PREBUILT=1 (ADR 0031, the nightly criterion): no cold
// boot. 1. First start with an empty profile: the prebuilt snapshot at the
// home screen is found by the app's key next to the image (a local server
// serves target/aosp/prebuilt as /aosp/snapshots/, or R2 with
// VETRO_ANDROID_MANIFEST), downloaded with verification into OPFS and
// restored (download and restore times); then the checks of 3 (adb, the APK
// installed and opened, a real click). 2. Second start from OPFS: ready time
// and first frame (M6: under 15 s), adb, three taps on the test app with the
// time from the press to its redraw (tests/web/taps.mjs).
//
// The times go to stdout and to target/aosp/chrome-measurements.json.
//
// VETRO_SCREENSHOTS=DIR with the prebuilt run also saves page screenshots
// (JPEG) for docs/user/: the advanced setup form (`?advanced=1`), the home
// screen, the test app, and with the Tools toggle open the adb panel, the
// analysis tabs and the file manager.

import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { colorSeen } from '../../web/node/android.mjs';
import { measureTaps, median } from './taps.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: long Android test in Chrome (VETRO_ANDROID=1 to run it)');
  process.exit(0);
}

const PREBUILT = process.env.VETRO_ANDROID_PREBUILT === '1';
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

/**
 * VETRO_SCREENSHOTS=DIR (prebuilt run): screenshots of the whole page for the
 * user documentation (docs/user/images), as JPEG: the setup form, the phone at
 * the home screen, the test app, the analysis tabs and the file manager.
 */
const SHOTS = process.env.VETRO_SCREENSHOTS ?? null;

/** The page (or the element with id `element`) as a JPEG in SHOTS. */
async function pageShot(page, name, element = null) {
  if (!SHOTS) return;
  mkdirSync(SHOTS, { recursive: true });
  await page.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1400, height: 1000, deviceScaleFactor: 1, mobile: false }, page.s);
  const clip = element && await page.eval(`(() => { const b = document.getElementById(${JSON.stringify(element)}).getBoundingClientRect();
    return { x: b.left + scrollX, y: b.top + scrollY, width: b.width, height: b.height, scale: 1 }; })()`);
  const r = await page.cdp.send('Page.captureScreenshot', { format: 'jpeg', quality: 80, captureBeyondViewport: true, ...(clip ? { clip } : {}) }, page.s);
  writeFileSync(join(SHOTS, `${name}.jpg`), Buffer.from(r.data, 'base64'));
  console.log(`page screenshot: ${join(SHOTS, `${name}.jpg`)}`);
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
const near = colorSeen;
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];

async function session(chrome, profile, url, kind) {
  const { proc, cdp } = await launch(chrome, profile);
  try {
    if (SHOTS && kind === 'prebuilt') {
      // The advanced setup form, before anything starts (the phone profile selected).
      const { page: form, targetId } = await openPage(cdp, `${url}&advanced=1&profile=phone`);
      await form.waitFor('device profiles in the form', () => form.eval("document.querySelector('select[name=profile]')?.options.length >= 4"), 30_000);
      await pageShot(form, 'setup');
      await cdp.send('Target.closeTarget', { targetId });
    }
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    if (kind === 'cold') {
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
    // From a snapshot: the prebuilt one (downloaded) or the one in OPFS.
    const prebuilt = kind === 'prebuilt';
    const boot = await page.waitFor(prebuilt ? 'prebuilt snapshot downloaded and restored' : 'restore', async () => {
      const st = await page.state();
      const pb = (await page.eval(ANDROID_STATE)).prebuilt;
      if (pb?.state === 'failed') throw new Fail(`prebuilt download failed: ${pb.error}`);
      if (pb?.state === 'missing') throw new Fail(`no prebuilt snapshot: ${pb.reason}`);
      if (st?.stopped) throw new Fail(`machine stopped: ${JSON.stringify(st.stopped)}`);
      return st?.boot;
    }, (prebuilt ? 60 : 5) * 60_000);
    check(boot.mode === 'snapshot', `start from ${boot.mode}, not from a snapshot`);
    check(boot.prebuilt === prebuilt, `prebuilt ${boot.prebuilt}, expected ${prebuilt}`);
    const frame = await page.waitFor('first frame', async () => (await page.state())?.firstFrame, 60_000);
    // The zero-choice page: one line says the phone is ready, no browser notice.
    const line = await page.eval("({ text: document.getElementById('progress-text').textContent, support: window.vetroState.support, tools: document.getElementById('tools').open })");
    check(line.text === 'Ready' && JSON.stringify(line.support) === '[]' && !line.tools, `progress line after the start: ${JSON.stringify(line)}`);
    const pb = (await page.eval(ANDROID_STATE)).prebuilt;
    const m = { pronto_ms: boot.ms, primo_fotogramma_ms: frame, apertura_ms: Date.now() - t0, times: boot.times, size: boot.size, memoria: boot.memory };
    if (prebuilt) {
      misure.prebuilt = { ...m, download_ms: pb.ms, bytes: pb.bytes, retries: pb.retries };
      console.log(`first start from the prebuilt snapshot: ${(pb.size / 2 ** 20).toFixed(0)} MiB downloaded and verified in ${secs(pb.ms)} s ` +
        `(${(pb.bytes / pb.ms / 1000).toFixed(1)} MB/s, ${pb.retries} retries), ready ${secs(boot.ms)} s after the page opened ` +
        `(restore ${boot.times.restore?.toFixed(0)} ms), first frame at ${secs(frame)} s`);
    } else {
      misure.ripristino = m;
      console.log(`second start from the snapshot: ready ${secs(boot.ms)} s after the page opened (read ${boot.times.readSnapshot?.toFixed(0)} ms, restore ${boot.times.restore?.toFixed(0)} ms), first frame at ${secs(frame)} s`);
      // M6: the screen back (the first frame drawn) in under 15 s from opening the page.
      check(Math.max(boot.ms, frame) < 15_000 || process.env.VETRO_ANDROID_SLOW === '1',
        `second start: first frame ${secs(frame)} s after the page opened (ready ${secs(boot.ms)} s): M6 wants the home screen in under 15 s (VETRO_ANDROID_SLOW=1 on slow machines)`);
    }
    await new Promise((ok) => setTimeout(ok, 3000));
    const shot = prebuilt ? 'chrome-home-prebuilt.png' : 'chrome-home-2.png';
    await screenshot(page, shot);
    if (prebuilt) await pageShot(page, 'home');
    let screen = await page.eval(SCREEN);
    misure[prebuilt ? 'home_prebuilt' : 'home_2'] = screen;
    if (kind === 'second') {
      // The last snapshot was saved right after the test app was opened: the
      // app is on screen (blue, or orange if the click came first). The save
      // may come while its launch is still showing the splash screen: then
      // the restored guest finishes the launch.
      const onScreen = (c) => near(c, BLU) || near(c, ARANCIONE);
      if (!onScreen(screen.center)) {
        const t1 = Date.now();
        await page.waitFor('the test app on screen after the restore', async () => onScreen((screen = await page.eval(SCREEN)).center), 120_000)
          .catch(() => {});
        misure.app_after_restore_ms = Date.now() - t1;
        console.log(`the restored guest finished the app's launch ${secs(misure.app_after_restore_ms)} s later`);
      }
      check(onScreen(screen.center), `second start: the test app is not on screen (centre ${screen.center})`);
    } else {
      check(screen.colors > 20, `screen after the restore: almost uniform (${screen.colors} colours)`);
    }
    // adb after the restore, then the APK.
    await page.waitFor('adb connected after the restore', async () => (await page.eval(ANDROID_STATE)).adb.state === 'ready', 10 * 60_000);
    misure[prebuilt ? 'adb_prebuilt_ms' : 'adb_ms'] = Date.now() - t0;
    console.log(`adb ready ${secs(Date.now() - t0)} s after the page opened`);
    const devices = await page.eval('window.vetroAndroid.devices()');
    check(devices[0]?.serial === 'VETRO00001', `adb devices: ${JSON.stringify(devices)}`);
    const sh = await page.eval("window.vetroAndroid.shell('getprop sys.boot_completed; getprop ro.product.model')");
    check(sh.stdout.startsWith('1\n'), `adb shell: ${JSON.stringify(sh)}`);
    console.log(`adb after the restore: ${JSON.stringify(devices)}, shell ${JSON.stringify(sh.stdout)}`);
    // After the prebuilt run the second start checks the restore and adb,
    // then how fast the test app answers taps (reported, not enforced).
    if (kind === 'second') {
      misure.taps = await measureTaps(page, cdp, 3, { limitMs: 60_000 });
      const med = (k) => median(misure.taps.map((t) => t[k]));
      misure.tap_median = { frame_ms: med('frame_ms'), centre_ms: med('centre_ms'), polled_ms: med('polled_ms') };
      console.log(`taps after the second start: median ${misure.tap_median.centre_ms?.toFixed(0)} ms from the press to the app's redraw ` +
        `(first frame ${misure.tap_median.frame_ms?.toFixed(0)} ms, pixel polled ${misure.tap_median.polled_ms} ms)`);
      check(misure.taps.every((t) => t.ripple), 'the page did not draw its touch feedback');
      return;
    }
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
    if (SHOTS) {
      await pageShot(page, 'app');
      // The adb line, then the analysis tabs and the file manager.
      await page.eval("window.vetroAndroid.shell('getprop ro.product.model')");
      await page.eval("document.getElementById('adb-cmd').value = 'getprop ro.build.version.release'; document.getElementById('adb-shell').requestSubmit()");
      await new Promise((ok) => setTimeout(ok, 3000));
      await page.eval("document.getElementById('tools').open = true");
      await pageShot(page, 'adb', 'android-dev');
      for (const tab of ['net', 'timeline', 'replay']) {
        await page.eval(`document.querySelector('[data-tab=${tab}]').click()`);
        await new Promise((ok) => setTimeout(ok, 1500));
        await pageShot(page, `tab-${tab}`, 'analysis-box');
      }
      await page.eval("window.vetroFiles.setRoots(['/data/local/tmp', '/sdcard/Download'])");
      await new Promise((ok) => setTimeout(ok, 5000));
      await pageShot(page, 'files', 'files-box');
    }
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
  if (PREBUILT && !process.env.VETRO_ANDROID_MANIFEST) check(readdirSync(join(out, 'prebuilt')).some((f) => f.endsWith('.json')), 'target/aosp/prebuilt has no snapshot: tools/aosp/prebuilt-snapshot.mjs');
  // VETRO_APP_QUERY: more URL parameters (a device profile, `graphics=full`).
  const url = `${srv.url}/app/?${PREBUILT ? '' : 'cold=1&'}manifest=${encodeURIComponent(manifest)}${process.env.VETRO_APP_QUERY ?? ''}`;
  misure.manifest = manifest;
  // VETRO_ANDROID_PROFILE: a Chrome profile to keep (with the snapshot in
  // OPFS); VETRO_ANDROID_SKIP_BOOT=1 skips the first boot and resumes from the
  // snapshot already there.
  const keep = process.env.VETRO_ANDROID_PROFILE;
  const profile = keep ?? mkdtempSync(join(tmpdir(), 'vetro-android-chrome-'));
  if (keep) mkdirSync(keep, { recursive: true });
  let ok = false;
  try {
    if (PREBUILT) {
      if (process.env.VETRO_ANDROID_SKIP_BOOT !== '1') await session(chrome, profile, url, 'prebuilt');
      await session(chrome, profile, url, 'second');
    } else {
      if (process.env.VETRO_ANDROID_SKIP_BOOT !== '1') await session(chrome, profile, url, 'cold');
      await session(chrome, profile, url, 'restore');
    }
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
