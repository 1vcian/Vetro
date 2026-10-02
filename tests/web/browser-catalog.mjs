#!/usr/bin/env node
// The app catalog panel (web/app/catalog.mjs, M6, ADR 0033) in headless
// Chrome, without a guest: the app page is loaded (not started) and a
// CatalogPanel is built on its real elements with fake adb requests, against
// a catalog served by the same server (with the app's COOP/COEP headers, so
// the icon must load like the R2 ones do). Checked on the DOM: cards with
// icon, name, size, licence and source link, advanced apps in their own
// section; Install -> downloading -> installing (push progress) -> Open, and
// Open calls the adb `open` request with the launcher from the install; a
// tampered APK ends in "failed" with Retry and never reaches install; an
// older version on the device shows Update; a missing catalog leaves the
// panel hidden.
//
// Without Chrome it says SKIP (it is not a passed test), or fails with
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser-catalog.mjs

import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, makeZip, root, run } from './lib.mjs';

const ok = (cond, what) => {
  check(cond, what);
  console.log(`ok: ${what}`);
};
const sha = (b) => createHash('sha256').update(b).digest('hex');

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome not found (VETRO_CHROME): catalog panel test not run';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const dir = join(root, 'target/web-test/browser-catalog');
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const axml = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/tocco-manifest.axml')));
  const apk = makeZip([['AndroidManifest.xml', axml, 0], ['assets/filler.bin', new Uint8Array(300_000).fill(7), 0]]);
  const bad = apk.slice();
  bad[apk.length - 50] ^= 1;
  writeFileSync(join(dir, 'tocco.apk'), apk);
  writeFileSync(join(dir, 'bad.apk'), bad);
  writeFileSync(join(dir, 'icon.png'), readFileSync(join(root, 'web/app/icons/icon-32.png')));
  const entry = (id, over) => ({
    id, name: `App ${id}`, package: 'it.vetro.tocco', version: '1.0', versionCode: 1, apk: 'tocco.apk', size: apk.length, sha256: sha(apk),
    license: 'GPL-3.0-only', source: `https://example.org/${id}/tree/v1.0`, icon: 'icon.png', description: `Description of ${id}`, minImage: null, advanced: false, ...over,
  });
  writeFileSync(join(dir, 'v1.json'), JSON.stringify({
    format: 1,
    apps: [
      entry('tocco', {}),
      entry('bad', { package: 'it.vetro.bad', apk: 'bad.apk', license: 'MIT' }),
      entry('old', { package: 'it.vetro.old', versionCode: 5 }),
      entry('big', { package: 'it.vetro.big', advanced: true }),
      entry('future', { package: 'it.vetro.future', minImage: 'android-99.0.0_r1' }),
    ],
  }));
  const srv = await serve({ mounts: [['/catalog-test/', dir], ...appMounts()] });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const { page } = await openPage(cdp, `${srv.url}/app/?advanced=1`);
    await page.waitFor('page loaded', () => page.eval("typeof window.vetroCatalog === 'object'"), 30_000);
    // A panel on the page's own elements, with fake adb requests.
    await page.eval(`(async () => {
      const { CatalogPanel } = await import('./catalog.mjs');
      document.getElementById('machine').hidden = false;
      document.getElementById('android-box').hidden = false;
      const calls = window.catalogCalls = [];
      const el = (id) => document.getElementById(id);
      const panel = new CatalogPanel({ box: el('catalog-box'), list: el('catalog-list'), advancedBox: el('catalog-advanced'), advancedList: el('catalog-advanced-list'), note: el('catalog-note') }, {
        install: async (bytes, name, onProgress) => {
          calls.push(['install', name, bytes.length]);
          for (const f of [0.25, 0.5, 1]) { onProgress({ fraction: f }); await new Promise((ok) => setTimeout(ok, 50)); }
          return { info: { package: 'it.vetro.tocco' }, component: 'it.vetro.tocco/it.vetro.tocco.Main' };
        },
        open: async (pkg, launcher) => { calls.push(['open', pkg, launcher]); return { component: pkg + '/' + launcher }; },
        shell: async (cmd) => { calls.push(['shell', cmd]); return { stdout: 'package:it.vetro.old versionCode:3\\n' }; },
      });
      window.testPanel = panel;
      await panel.load('/catalog-test/v1.json', 'android-15.0.0_r36-BP1A.250505.005.D1-bd09e2f');
      await panel.refresh();
    })()`);
    const cards = await page.eval(`[...document.querySelectorAll('.app-card')].map((li) => ({
      id: li.dataset.app, state: li.dataset.state, advanced: !!li.closest('#catalog-advanced'), button: li.querySelector('button').textContent,
      text: li.textContent, href: li.querySelector('a').href, icon: li.querySelector('img').naturalWidth, iconMissing: li.querySelector('img').classList.contains('missing'),
    }))`);
    ok(await page.eval("!document.getElementById('catalog-box').hidden"), 'panel shown');
    ok(cards.map((c) => c.id).join() === 'tocco,bad,old,big', `cards: ${cards.map((c) => c.id).join()} (the one needing a newer image hidden)`);
    const t = cards[0];
    ok(t.text.includes('App tocco') && t.text.includes('1.0') && t.text.includes('KiB') && t.text.includes('GPL-3.0-only') && t.text.includes('source code (this version)') &&
      t.href === 'https://example.org/tocco/tree/v1.0', `card text and source link: ${t.text}`);
    ok(cards[1].text.includes('MIT') && !cards[1].text.includes('(this version)'), 'permissive licence: plain source link');
    await page.waitFor('icon loaded under COEP', async () => (await page.eval("document.querySelector('.app-card img').naturalWidth")) > 0, 10_000);
    ok(true, 'icon loaded (crossorigin under COEP)');
    ok(cards.find((c) => c.id === 'big').advanced && !(await page.eval("document.getElementById('catalog-advanced').hidden")), 'advanced app in its own section');
    ok(cards.find((c) => c.id === 'old').button === 'Update' && cards[0].button === 'Install', 'older version on the device: Update; absent: Install');
    ok((await page.eval('window.catalogCalls'))[0]?.[1] === 'pm list packages --show-versioncode', 'installed packages read with pm');

    // Install: watch the card's states.
    await page.eval(`window.seen = []; {
      const li = document.querySelector('[data-app="tocco"]');
      window.watch = new MutationObserver(() => {
        const s = li.dataset.state + '|' + li.querySelector('button').textContent;
        if (window.seen.at(-1) !== s) window.seen.push(s);
      });
      window.watch.observe(li, { attributes: true, childList: true, subtree: true, characterData: true });
    } window.testPanel.install('tocco').then((s) => (window.done = s));`);
    const done = await page.waitFor('install finished', () => page.eval('window.done'), 30_000);
    const seen = await page.eval('window.watch.disconnect(), window.seen');
    ok(done.phase === 'installed', `install state: ${JSON.stringify(done)}`);
    ok(seen.join(' > ') === 'downloading|Downloading… > installing|Installing… > installed|Open', `card states: ${seen.join(' > ')}`);
    const calls = await page.eval('window.catalogCalls');
    ok(calls.some((c) => c[0] === 'install' && c[1] === 'it.vetro.tocco.apk' && c[2] === apk.length), 'verified APK handed to adb install');
    await page.eval("document.querySelector('[data-app=\"tocco\"] button').click()");
    await page.waitFor('open request', async () => (await page.eval('window.catalogCalls')).some((c) => c[0] === 'open'), 10_000);
    const open = (await page.eval('window.catalogCalls')).find((c) => c[0] === 'open');
    ok(open[1] === 'it.vetro.tocco' && open[2] === 'it.vetro.tocco.Main', `Open: adb open ${open.slice(1).join(' ')}`);

    // A tampered APK: failed, Retry, never installed.
    const badState = await page.eval("window.testPanel.install('bad')");
    const badCard = await page.eval("(() => { const li = document.querySelector('[data-app=\"bad\"]'); return { state: li.dataset.state, button: li.querySelector('button').textContent, status: li.querySelector('.app-status').textContent }; })()");
    ok(badState.phase === 'failed' && badCard.button === 'Retry' && badCard.status.includes('SHA-256'), `tampered APK: ${JSON.stringify(badCard)}`);
    ok((await page.eval('window.catalogCalls')).filter((c) => c[0] === 'install').length === 1, 'the tampered APK never reached adb');

    // A missing catalog: the panel stays hidden.
    const hidden = await page.eval(`(async () => {
      const { CatalogPanel } = await import('./catalog.mjs');
      const box = document.createElement('div');
      const mk = () => document.createElement('ul');
      const p = new CatalogPanel({ box, list: mk(), advancedBox: document.createElement('details'), advancedList: mk(), note: document.createElement('p') }, {});
      const r = await p.load('/catalog-test/none.json', null);
      return { r, hidden: box.hidden, error: p.error };
    })()`);
    ok(hidden.r === false && hidden.hidden && hidden.error.includes('404'), `missing catalog: hidden (${hidden.error})`);
    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' }, page.s);
    writeFileSync(join(dir, 'panel.png'), Buffer.from(shot.data, 'base64'));
    console.log('screenshot: target/web-test/browser-catalog/panel.png');
    console.log('catalog panel: all good');
  } finally {
    await closeChrome(proc, cdp, profile);
    await srv.close();
  }
});
