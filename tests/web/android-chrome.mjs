#!/usr/bin/env node
// L'immagine AOSP di Vetro nell'app, in Chrome headless (M5/M6, ADR 0028).
// Lungo (decine di minuti o più): gira solo con VETRO_ANDROID=1.
//
//   VETRO_ANDROID=1 node tests/web/android-chrome.mjs
//
// 1. Primo avvio: `/app/?os=android&manifest=...&autostart=1`, immagini e
//    disco da un server locale (target/aosp/out, `/aosp/`) o da
//    VETRO_ANDROID_MANIFEST (per R2 serve la porta 8080: VETRO_WEB_PORT=8080,
//    il CORS del bucket la ammette). Le fasi dell'avvio con i tempi reali,
//    fino a sys.boot_completed e alla home (launcher in primo piano, visto
//    con adb); adb collegato; lo snapshot salvato in OPFS
//    (dimensione, tempi, memoria del modulo). Schermata della home in
//    target/aosp/chrome-home-1.png.
// 2. Secondo avvio, stesso profilo: dallo snapshot. Tempo dall'apertura
//    della pagina alla macchina pronta e al primo fotogramma (criterio di M6:
//    home < 15 s dalla cache), schermata in target/aosp/chrome-home-2.png.
// 3. APK di prova (tests/apps/tocco, target/apps/tocco.apk) installato con
//    `window.vetroAndroid.install` (lo stesso percorso del trascinamento) e
//    aperto: il centro dello schermo diventa blu; un clic vero sul canvas
//    (tocco sul touchscreen) lo fa diventare arancione. adb devices e shell
//    dalla pagina. Schermate chrome-app-*.png.
//
// I tempi finiscono su stdout e in target/aosp/chrome-misure.json.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: test lungo di Android in Chrome (VETRO_ANDROID=1 per eseguirlo)');
  process.exit(0);
}

const BOOT_LIMIT_MS = Number(process.env.VETRO_ANDROID_BOOT_MINUTES ?? 240) * 60_000;
const out = join(root, 'target/aosp');
const misure = { inizio: new Date().toISOString() };
const secs = (ms) => (ms / 1000).toFixed(1);

/** PNG del canvas (dataURL) su file. */
async function screenshot(page, name) {
  const url = await page.eval("document.getElementById('screen').toDataURL('image/png')");
  writeFileSync(join(out, name), Buffer.from(url.split(',')[1], 'base64'));
  console.log(`schermata: target/aosp/${name}`);
}

/** Pixel al centro del canvas e varietà dell'immagine (colori distinti su una griglia). */
const SCREEN = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
  const at = (x, y) => { const o = (y * c.width + x) * 4; return [d[o], d[o + 1], d[o + 2]]; };
  const colors = new Set();
  for (let y = 0; y < c.height; y += 16) for (let x = 0; x < c.width; x += 16) colors.add(at(x, y).join());
  return { w: c.width, h: c.height, center: at(c.width >> 1, c.height >> 1), colors: colors.size, off: !document.getElementById('screen-off').hidden };
})()`;
const near = (a, b) => a && a.every((v, i) => Math.abs(v - b[i]) <= 8);
const BLU = [0x15, 0x65, 0xc0];
const ARANCIONE = [0xef, 0x6c, 0x00];

async function session(chrome, profile, url, first) {
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    if (first) {
      // Fasi dell'avvio, fino a sys.boot_completed.
      let seen = 0;
      await page.waitFor('sys.boot_completed', async () => {
        const st = await page.eval('window.vetroAndroid.state()');
        for (const p of st.phases.slice(seen)) console.log(`fase: ${p.label} a ${p.guestSecs.toFixed(0)} s di guest, ${secs(p.wallMs)} s reali`);
        seen = st.phases.length;
        const s = await page.eval('window.vetroState');
        if (s.stopped) throw new Fail(`macchina ferma: ${JSON.stringify(s.stopped)}`);
        return st.booted;
      }, BOOT_LIMIT_MS);
      const st = await page.eval('window.vetroAndroid.state()');
      misure.fasi = st.phases;
      misure.boot_completed = st.booted;
      console.log(`avvio finito: ${st.booted.guestSecs.toFixed(0)} s di guest, ${secs(st.booted.wallMs)} s reali`);
      const home = await page.waitFor('home (launcher)', async () => (await page.eval('window.vetroAndroid.state()')).home, BOOT_LIMIT_MS);
      misure.home = home;
      console.log(`home: ${home.guestSecs.toFixed(0)} s di guest, ${secs(home.wallMs)} s reali (${home.activity})`);
      const snap = await page.waitFor('snapshot dopo l\'avvio', async () => (await page.state()).snapshots[0], 30 * 60_000);
      misure.snapshot_avvio = snap;
      console.log(`snapshot: ${(snap.size / 2 ** 20).toFixed(0)} MiB, salvataggio ${snap.saveMs.toFixed(0)} ms, scrittura OPFS ${snap.writeMs.toFixed(0)} ms, memoria del modulo ${(snap.memory / 2 ** 20).toFixed(0)} MiB`);
      const adb = await page.waitFor('adb collegato', async () => {
        const a = (await page.eval('window.vetroAndroid.state()')).adb;
        return a.state === 'ready' ? a : null;
      }, 10 * 60_000);
      console.log(`adb: ${JSON.stringify(adb.devices)}`);
      const s = await page.state();
      misure.memoria_max = s.memory;
      misure.stats = s.stats;
      console.log(`memoria del modulo (massimo): ${(s.memory / 2 ** 20).toFixed(0)} MiB`);
      await screenshot(page, 'chrome-home-1.png');
      misure.home_1 = await page.eval(SCREEN);
      check(misure.home_1.colors > 20, `home: schermo quasi uniforme (${misure.home_1.colors} colori)`);
      return;
    }
    // Secondo avvio: dallo snapshot.
    const boot = await page.waitFor('ripristino', async () => (await page.state()).boot, 5 * 60_000);
    check(boot.mode === 'snapshot', `secondo avvio da ${boot.mode}, non dallo snapshot`);
    const frame = await page.waitFor('primo fotogramma', async () => (await page.state()).firstFrame, 60_000);
    misure.ripristino = { pronto_ms: boot.ms, primo_fotogramma_ms: frame, apertura_ms: Date.now() - t0, times: boot.times, size: boot.size, memoria: boot.memory };
    console.log(`secondo avvio dallo snapshot: pronto in ${secs(boot.ms)} s dall'apertura della pagina (lettura ${boot.times.readSnapshot?.toFixed(0)} ms, ripristino ${boot.times.restore?.toFixed(0)} ms), primo fotogramma a ${secs(frame)} s`);
    await new Promise((ok) => setTimeout(ok, 3000));
    await screenshot(page, 'chrome-home-2.png');
    misure.home_2 = await page.eval(SCREEN);
    check(misure.home_2.colors > 20, `home dopo il ripristino: schermo quasi uniforme (${misure.home_2.colors} colori)`);
    // adb dopo il ripristino, poi l'APK.
    await page.waitFor('adb collegato dopo il ripristino', async () => (await page.eval('window.vetroAndroid.state()')).adb.state === 'ready', 10 * 60_000);
    const devices = await page.eval('window.vetroAndroid.devices()');
    check(devices[0]?.serial === 'VETRO00001', `adb devices: ${JSON.stringify(devices)}`);
    const sh = await page.eval("window.vetroAndroid.shell('getprop sys.boot_completed; getprop ro.product.model')");
    check(sh.stdout.startsWith('1\n'), `adb shell: ${JSON.stringify(sh)}`);
    console.log(`adb dopo il ripristino: ${JSON.stringify(devices)}, shell ${JSON.stringify(sh.stdout)}`);
    const apk = join(root, 'target/apps/tocco.apk');
    check(existsSync(apk), 'target/apps/tocco.apk mancante: tests/apps/tocco/build.sh');
    const b64 = readFileSync(apk).toString('base64');
    const ti = Date.now();
    const inst = await page.eval(`window.vetroAndroid.install(Uint8Array.from(atob('${b64}'), (c) => c.charCodeAt(0)).buffer, 'tocco.apk')`);
    misure.install = { ms: Date.now() - ti, installMs: inst.installMs, openMs: inst.openMs, component: inst.component };
    console.log(`APK installato e aperto in ${secs(Date.now() - ti)} s (install ${secs(inst.installMs)} s, apertura ${secs(inst.openMs)} s): ${inst.component}`);
    check(inst.info.package === 'it.vetro.tocco' && inst.component === 'it.vetro.tocco/it.vetro.tocco.Main', `install: ${JSON.stringify(inst)}`);
    await page.waitFor('app a schermo (centro blu)', async () => near((await page.eval(SCREEN)).center, BLU), 5 * 60_000);
    await screenshot(page, 'chrome-app-1.png');
    // Un clic vero sul canvas: tocco sul touchscreen del guest.
    const r = await page.eval("(() => { const b = document.getElementById('screen').getBoundingClientRect(); return { x: b.left + b.width / 2, y: b.top + b.height / 2 }; })()");
    const tt = Date.now();
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    await new Promise((ok) => setTimeout(ok, 300));
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    await page.waitFor('tocco ricevuto (centro arancione)', async () => near((await page.eval(SCREEN)).center, ARANCIONE), 5 * 60_000);
    misure.tocco_ms = Date.now() - tt;
    console.log(`tocco ricevuto dall'app: il centro è arancione dopo ${secs(misure.tocco_ms)} s`);
    await screenshot(page, 'chrome-app-2.png');
    const top = await page.eval("window.vetroAndroid.shell('dumpsys activity activities | grep -m1 mResumedActivity')");
    check(top.stdout.includes('it.vetro.tocco'), `attività in primo piano: ${top.stdout}`);
    misure.snapshot_app = await page.waitFor('snapshot dopo l\'installazione', async () => (await page.state()).snapshots.find((x) => x.why === 'app installata'), 10 * 60_000);
    console.log(`snapshot dopo l'installazione: ${(misure.snapshot_app.size / 2 ** 20).toFixed(0)} MiB in ${(misure.snapshot_app.saveMs + misure.snapshot_app.writeMs).toFixed(0)} ms`);
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
  if (!chrome) throw new Fail('Chrome non trovato (VETRO_CHROME)');
  mkdirSync(out, { recursive: true });
  const port = Number(process.env.VETRO_WEB_PORT ?? 0);
  const srv = await serve({ mounts: appMounts(), port });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? `${srv.url}/aosp/manifest.json`;
  if (!process.env.VETRO_ANDROID_MANIFEST) check(existsSync(join(out, 'out/web/disk.json')), 'target/aosp/out/web/disk.json mancante: node tools/aosp/web-disk.mjs');
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}`;
  misure.manifest = manifest;
  const profile = mkdtempSync(join(tmpdir(), 'vetro-android-chrome-'));
  try {
    await session(chrome, profile, url, true);
    await session(chrome, profile, url, false);
  } finally {
    misure.fine = new Date().toISOString();
    writeFileSync(join(out, 'chrome-misure.json'), JSON.stringify(misure, null, 2));
    console.log('misure: target/aosp/chrome-misure.json');
    await srv.close();
    try {
      rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    } catch (e) {
      console.log(`avviso: profilo di Chrome non cancellato (${profile}): ${e.message}`);
    }
  }
});
