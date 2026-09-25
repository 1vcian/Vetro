#!/usr/bin/env node
// M5: l'app web (web/app) in un Chrome headless vero, pilotato col
// protocollo DevTools (WebSocket di Node 22, nessuna dipendenza npm).
//
//   1. la pagina avvia il kernel M3 nel Worker con un disco via HTTP Range
//      (cache OPFS vuota); dalla console della pagina (tasti veri) si lancia
//      `md5sum /dev/vda` e `vetro-dev drm-hold`: il canvas deve mostrare il
//      motivo del guest pixel per pixel e il cursore dev'essere visibile;
//   2. col canvas a fuoco un tasto vero (KeyA) arriva al guest da
//      virtio-input (`vetro-dev input-read`);
//   3. ricaricata la pagina (stesso profilo), il disco si legge da OPFS:
//      nessuna lettura Range oltre la prima (dimensione).
//
// Chrome: VETRO_CHROME, altrimenti i percorsi soliti. Senza Chrome il test
// dice SKIP (non è un test passato) ed esce con 0, o con 1 se
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser.mjs

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';

const CANDIDATES = [
  process.env.VETRO_CHROME,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/google-chrome',
  '/usr/bin/google-chrome-stable',
  '/usr/bin/chromium',
  '/usr/bin/chromium-browser',
].filter(Boolean);

const SIZE = 2 * 1024 * 1024 + 4096;

class Cdp {
  #ws;
  #id = 0;
  #pending = new Map();

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((ok, ko) => {
      ws.onopen = ok;
      ws.onerror = () => ko(new Fail(`DevTools: connessione a ${url} fallita`));
    });
    return new Cdp(ws);
  }

  constructor(ws) {
    this.#ws = ws;
    ws.onmessage = (e) => {
      const msg = JSON.parse(e.data);
      const p = msg.id !== undefined && this.#pending.get(msg.id);
      if (!p) return;
      this.#pending.delete(msg.id);
      if (msg.error) p.ko(new Fail(`DevTools ${p.method}: ${msg.error.message}`));
      else p.ok(msg.result);
    };
  }

  send(method, params = {}, sessionId = undefined) {
    const id = ++this.#id;
    this.#ws.send(JSON.stringify({ id, method, params, sessionId }));
    return new Promise((ok, ko) => this.#pending.set(id, { ok, ko, method }));
  }

  close() {
    this.#ws.close();
  }
}

class Page {
  constructor(cdp, sessionId) {
    this.cdp = cdp;
    this.s = sessionId;
  }

  async eval(expr) {
    const r = await this.cdp.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true }, this.s);
    if (r.exceptionDetails) throw new Fail(`nella pagina: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value;
  }

  consoleText() {
    return this.eval("document.getElementById('console').textContent");
  }

  async waitFor(what, pred, ms = 120_000) {
    const t0 = Date.now();
    for (;;) {
      const v = await pred();
      if (v) return v;
      if (Date.now() - t0 > ms) {
        const status = await this.eval("document.getElementById('status').textContent");
        const tail = (await this.consoleText()).split('\n').slice(-15).join('\n');
        throw new Fail(`${what}: non arrivato in ${ms / 1000} s (stato: ${status})\n${tail}`);
      }
      await new Promise((ok) => setTimeout(ok, 200));
    }
  }

  async until(needle, after = 0) {
    return this.waitFor(JSON.stringify(needle), async () => {
      const t = await this.consoleText();
      const i = t.indexOf(needle, after);
      return i >= 0 ? i + needle.length : 0;
    });
  }

  async key(key, code, text) {
    const vk = key === 'Enter' ? 13 : key.length === 1 ? key.toUpperCase().charCodeAt(0) : 0;
    const base = { key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
    await this.cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', ...base, ...(text ? { text } : {}) }, this.s);
    await this.cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base }, this.s);
  }

  /** Scrive una riga nella console della pagina, tasto per tasto. */
  async type(line) {
    await this.eval("document.getElementById('console').focus()");
    for (const ch of line) await this.key(ch, '', ch);
    await this.key('Enter', 'Enter');
  }
}

function findChrome() {
  return CANDIDATES.find((p) => existsSync(p));
}

async function launch(chrome, profile) {
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run',
    '--no-default-browser-check', '--disable-gpu', '--window-size=1400,1000',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  const url = await new Promise((ok, ko) => {
    let err = '';
    const t = setTimeout(() => ko(new Fail(`Chrome non risponde:\n${err}`)), 30_000);
    proc.stderr.on('data', (d) => {
      err += d;
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(err);
      if (m) {
        clearTimeout(t);
        ok(m[1]);
      }
    });
    proc.on('exit', (c) => ko(new Fail(`Chrome uscito (${c}):\n${err}`)));
  });
  return { proc, cdp: await Cdp.connect(url) };
}

async function openPage(cdp, url) {
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = new Page(cdp, sessionId);
  await cdp.send('Page.enable', {}, sessionId);
  await cdp.send('Page.navigate', { url }, sessionId);
  return { page, targetId };
}

/** Il motivo di `vetro-dev drm` (tests/boot/tests/devices.rs), controllato nella pagina. */
const CHECK_CANVAS = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
  let bad = 0, first = null;
  for (let y = 0; y < c.height; y++) for (let x = 0; x < c.width; x++) {
    const w = x >= 32 && x < 96 && y >= 16 && y < 48;
    const e = w ? [255, 255, 255] : [x & 255, y & 255, (x ^ y) & 255];
    const o = (y * c.width + x) * 4;
    if (d[o] !== e[0] || d[o + 1] !== e[1] || d[o + 2] !== e[2]) { bad++; first ??= [x, y, d[o], d[o + 1], d[o + 2]]; }
  }
  const cur = document.getElementById('cursor');
  return { w: c.width, h: c.height, bad, first, cursor: !cur.hidden, off: !document.getElementById('screen-off').hidden };
})()`;

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome non trovato (VETRO_CHROME): test nel browser non eseguito';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const dir = join(root, 'target/web-test');
  mkdirSync(dir, { recursive: true });
  const disk = new Uint8Array(SIZE).map((_, i) => (i * 7 + (i >> 9)) & 0xff);
  const file = join(dir, 'browser-disk.img');
  writeFileSync(file, disk);
  const md5 = createHash('md5').update(disk).digest('hex');
  const ranges = [];
  const srv = await serve({
    mounts: [['/disks/test.img', file], ...appMounts()],
    onRequest: (r) => r.path === '/disks/test.img' && ranges.push(r),
  });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const q = new URLSearchParams({ disk: '/disks/test.img', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1' });
    const url = `${srv.url}/app/?${q}`;
    let { page, targetId } = await openPage(cdp, url);
    let at = await page.until('# ');
    await page.type('md5sum /dev/vda');
    at = await page.until(`${md5}  /dev/vda`, at);
    await page.type('vetro-dev drm-hold');
    at = await page.until('VETRO-DRM-PRONTO', at);
    const cv = await page.waitFor('motivo sul canvas', async () => {
      const r = await page.eval(CHECK_CANVAS);
      return r.w === 1280 && r.bad === 0 && r.cursor && !r.off ? r : null;
    }, 20_000).catch(async (e) => {
      throw new Fail(`${e.message}\ncanvas: ${JSON.stringify(await page.eval(CHECK_CANVAS))}`);
    });
    console.log(`canvas: ${cv.w}x${cv.h}, motivo del guest pixel per pixel, cursore visibile`);
    await page.type('');
    at = await page.until('# ', at);
    await page.type('vetro-dev input-read /dev/input/event1 4');
    at = await page.until('VETRO-INPUT-PRONTO', at);
    await page.eval("document.getElementById('screen').focus()");
    await page.key('a', 'KeyA', 'a');
    at = await page.until('vetro-dev: evento 1 30 0', at);
    console.log('tastiera: KeyA dal canvas arrivato al guest come KEY_A (virtio-input)');
    const first = ranges.length;
    await cdp.send('Target.closeTarget', { targetId });

    // Riavvio: i blocchi del disco vengono da OPFS.
    ({ page, targetId } = await openPage(cdp, url));
    at = await page.until('# ');
    await page.type('md5sum /dev/vda');
    await page.until(`${md5}  /dev/vda`, at);
    const second = ranges.length - first;
    const status = await page.eval("document.getElementById('status').textContent");
    check(first > 1, `primo avvio: ${first} richieste al disco, attese letture Range`);
    check(second === 1, `riavvio: ${second} richieste al disco (attesa 1, la dimensione): OPFS non usato? stato: ${status}`);
    console.log(`OPFS: primo avvio ${first} richieste Range, riavvio ${second} (solo la dimensione)`);
    console.log('app nel browser: ok');
  } finally {
    cdp.close();
    proc.kill();
    await srv.close();
    await new Promise((ok) => setTimeout(ok, 300));
    rmSync(profile, { recursive: true, force: true });
  }
});
