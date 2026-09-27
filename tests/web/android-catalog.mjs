#!/usr/bin/env node
// The app catalog on Vetro's AOSP image in headless Chrome (M6, ADR 0033).
// Long and optional: runs only with VETRO_ANDROID=1 (on the build VM:
// tools/remote/test.sh -- env VETRO_ANDROID=1 node tests/web/android-catalog.mjs).
//
// 1. Empty profile: the page restores the prebuilt home-screen snapshot
//    (ADR 0031) of the image on R2 (VETRO_ANDROID_MANIFEST, default: the
//    app's image), loads the catalog (VETRO_CATALOG, default: the app's, on
//    R2), waits for adb and the installed-packages query, then installs a
//    small catalog app (VETRO_CATALOG_APP, default `flowit`) with the card's
//    own path (`window.vetroCatalog.install`): download from R2 with the
//    SHA-256 check, push with progress, pm install. Then Open: the app's
//    window gets the focus. The Worker saves the "app installed" snapshot.
// 2. Same profile again: resumed from that snapshot in OPFS, the card says
//    installed (read from the device with pm) and Open works.
//
// R2's CORS allows the app on port 8080 (VETRO_WEB_PORT, default 8080 here).
// Times go to stdout and to target/aosp/catalog-measurements.json.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { DEFAULT_MANIFEST } from '../../web/node/android.mjs';
import { CATALOG_URL } from '../../web/node/catalog.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, root, run } from './lib.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: long app catalog test in Chrome (VETRO_ANDROID=1 to run it)');
  process.exit(0);
}

const APP = process.env.VETRO_CATALOG_APP ?? 'flowit';
const out = join(root, 'target/aosp');
const m = { start: new Date().toISOString(), app: APP };
const secs = (ms) => (ms / 1000).toFixed(1);
const CATALOG = 'window.vetroCatalog ? window.vetroCatalog.state() : { loaded: false }';
const ANDROID = "window.vetroAndroid ? window.vetroAndroid.state() : { adb: { state: 'none' } }";

async function session(chrome, profile, url, kind) {
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    const boot = await page.waitFor(kind === 'first' ? 'prebuilt snapshot downloaded and restored' : 'restore from OPFS', async () => {
      const st = await page.state();
      const pb = (await page.eval(ANDROID)).prebuilt;
      if (pb?.state === 'failed') throw new Fail(`prebuilt download failed: ${pb.error}`);
      if (pb?.state === 'missing') throw new Fail(`no prebuilt snapshot for this vetro-wasm: ${pb.reason}`);
      if (st?.stopped) throw new Fail(`machine stopped: ${JSON.stringify(st.stopped)}`);
      return st?.boot;
    }, (kind === 'first' ? 60 : 5) * 60_000);
    check(boot.mode === 'snapshot', `start from ${boot.mode}, not from a snapshot`);
    const cat = await page.waitFor('catalog loaded', async () => {
      const c = await page.eval(CATALOG);
      if (c.error) throw new Fail(`catalog not shown: ${c.error}`);
      return c.loaded ? c : null;
    }, 60_000);
    const shown = await page.eval("!document.getElementById('catalog-box').hidden");
    check(shown, 'catalog panel hidden although loaded');
    console.log(`${kind}: ready ${secs(boot.ms)} s after opening (prebuilt ${boot.prebuilt}); catalog: ${cat.apps.map((a) => `${a.id}${a.advanced ? ' (advanced)' : ''}`).join(', ')}`);
    check(cat.apps.some((a) => a.id === APP), `no ${APP} in the catalog`);
    const adbMs = await page.waitFor('adb connected', async () => ((await page.eval(ANDROID)).adb.state === 'ready' ? Date.now() - t0 : null), 10 * 60_000);
    // The installed-packages query runs when adb connects.
    await page.eval('window.vetroCatalog.refresh()');
    const before = (await page.eval(CATALOG)).apps.find((a) => a.id === APP);
    const r = { ready_ms: boot.ms, prebuilt: boot.prebuilt, adb_ms: adbMs, before: before.phase };
    if (kind === 'first') {
      check(before.phase === 'absent', `${APP} already installed on a fresh prebuilt snapshot (${before.phase})`);
      const ti = Date.now();
      // Poll the card while installing, to time the phases.
      const phases = {};
      const poll = setInterval(async () => {
        try {
          const a = (await page.eval(CATALOG)).apps.find((x) => x.id === APP);
          if (a && !phases[a.phase]) phases[a.phase] = Date.now() - ti;
        } catch {}
      }, 100);
      const st = await page.eval(`window.vetroCatalog.install(${JSON.stringify(APP)})`);
      clearInterval(poll);
      r.install_ms = Date.now() - ti;
      r.phases_ms = phases;
      check(st.phase === 'installed', `install: ${JSON.stringify(st)}`);
      const inst = (await page.eval(ANDROID)).installs.find((x) => x.source === 'catalog');
      r.worker = { installMs: inst?.installMs, ms: inst?.ms };
      console.log(`${APP}: installed in ${secs(r.install_ms)} s (download+check until ${secs(phases.installing ?? 0)} s, adb push+pm ${secs(inst?.installMs ?? 0)} s)`);
    } else {
      check(before.phase === 'installed' && before.button === 'Open', `after the restore the card says ${before.phase}/${before.button}`);
    }
    const pkg = (await page.eval(CATALOG)).apps.find((a) => a.id === APP).package;
    const to = Date.now();
    const opened = await page.eval(`window.vetroCatalog.open(${JSON.stringify(APP)})`);
    r.open_ms = Date.now() - to;
    const focus = await page.waitFor(`${pkg} focused`, async () => {
      const f = await page.eval("window.vetroAndroid.shell('dumpsys window | grep -m1 mCurrentFocus')");
      return f.stdout.includes(pkg) ? f.stdout.trim() : null;
    }, 5 * 60_000);
    r.focus_ms = Date.now() - to;
    console.log(`${APP}: opened (${opened.component}) in ${secs(r.open_ms)} s, focused after ${secs(r.focus_ms)} s: ${focus}`);
    const shot = await page.eval("document.getElementById('screen').toDataURL('image/png')");
    writeFileSync(join(out, `catalog-${kind}.png`), Buffer.from(shot.split(',')[1], 'base64'));
    if (kind === 'first') {
      const snap = await page.waitFor('snapshot after the install', async () => (await page.state())?.snapshots.find((x) => x.why === 'app installed'), 15 * 60_000);
      r.snapshot = { size: snap.size, ms: snap.saveMs + snap.writeMs };
      console.log(`snapshot after the install: ${(snap.size / 2 ** 20).toFixed(0)} MiB in ${secs(snap.saveMs + snap.writeMs)} s`);
    }
    m[kind] = r;
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
  const srv = await serve({ mounts: appMounts(), port: Number(process.env.VETRO_WEB_PORT ?? 8080) });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST;
  const catalog = process.env.VETRO_CATALOG ?? CATALOG_URL;
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}&catalog=${encodeURIComponent(catalog)}`;
  Object.assign(m, { manifest, catalog });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-catalog-chrome-'));
  let ok = false;
  try {
    await session(chrome, profile, url, 'first');
    await session(chrome, profile, url, 'second');
    ok = true;
  } finally {
    m.end = new Date().toISOString();
    writeFileSync(join(out, 'catalog-measurements.json'), JSON.stringify(m, null, 2));
    console.log('measurements: target/aosp/catalog-measurements.json');
    await srv.close();
    if (ok) rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    else console.log(`Chrome profile kept: ${profile}`);
  }
});
