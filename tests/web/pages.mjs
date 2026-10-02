#!/usr/bin/env node
// The GitHub Pages site (tools/pages/build.sh) as Pages serves it: under a
// subpath (/Vetro/) and without COOP/COEP headers. In headless Chrome the
// landing page's one button leads to the zero-choice app, which starts the
// phone by itself (the snapshot download, or the cold boot if none is
// published, on the progress line; no setup form, no browser notice); the
// landing page's small "Linux demo" link boots the guest kernel to the shell
// and the console answers, the network inspector sees a guest request (empty
// bodies as "0 B"); the GPL sources are there and the kernel tarball put back
// together from its pieces has the declared sha256; the prebuilt Android
// snapshot the site announces, if any, is the one for its vetro-wasm; the
// user guide (docs/user as HTML in docs/) is there and linked.
//
//   node tests/web/pages.mjs [target/pages]
//
// Without Chrome it says SKIP (not a passed test), or fails with
// VETRO_REQUIRE_BROWSER=1. The phone's download comes from R2, whose CORS
// allows the origin http://127.0.0.1:8080 only: with VETRO_WEB_PORT=8080 (CI)
// the default page must reach the download; on another port it must start
// and reach R2's refusal ("Failed to fetch"), and the test says so.

import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serve } from '../../tools/web-serve.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { prebuiltKey } from '../../tools/aosp/prebuilt-key.mjs';
import { findPrebuilt } from '../../web/node/prebuilt.mjs';

run(async () => {
  const site = process.argv[2] ?? join(root, 'target/pages');
  if (!existsSync(join(site, 'index.html'))) throw new Fail(`${site} missing: run tools/pages/build.sh`);

  // GPL sources: the tarball put back together from its pieces is the declared one.
  const src = join(site, 'sources');
  const files = readdirSync(src);
  const sums = files.filter((f) => /^linux-.*\.tar\.xz\.sha256$/.test(f));
  check(sums.length === 1, `sources/: expected one linux-*.tar.xz.sha256, found ${sums}`);
  const [want, name] = readFileSync(join(src, sums[0]), 'utf8').trim().split(/\s+/);
  const parts = files.filter((f) => f.startsWith(`${name}.a`)).sort();
  check(parts.length > 1, `sources/: pieces of ${name} missing`);
  const h = createHash('sha256');
  for (const p of parts) h.update(readFileSync(join(src, p)));
  check(h.digest('hex') === want, `sources/: ${name} put back together does not have sha256 ${want}`);
  for (const f of ['README', 'defconfig', 'vetro.config', 'VERSIONS', 'index.html']) check(files.includes(f), `sources/${f} missing`);
  check(files.some((f) => /^busybox-.*\.tar\.bz2$/.test(f)), 'sources/: BusyBox sources missing');
  console.log(`GPL sources: ${name} in ${parts.length} pieces (right sha256), BusyBox and Alpine's patches`);

  // The user guide: every page of docs/user as HTML, linked from the landing page.
  const guide = join(site, 'docs');
  const pagesMd = readdirSync(join(root, 'docs/user')).filter((f) => f.endsWith('.md'));
  for (const f of pagesMd) {
    const html = f === 'README.md' ? 'index.html' : f.replace(/\.md$/, '.html');
    check(existsSync(join(guide, html)), `docs/${html} missing (from docs/user/${f})`);
  }
  check(/href="docs\/"/.test(readFileSync(join(site, 'index.html'), 'utf8')), 'the landing page does not link the user guide');
  const index = readFileSync(join(guide, 'index.html'), 'utf8');
  check(index.includes('<title>Vetro user guide</title>') && index.includes('href="getting-started.html"'), 'docs/index.html: title or links');
  console.log(`user guide: ${pagesMd.length} pages in docs/, linked from the landing page`);

  // The prebuilt Android snapshot (ADR 0031): if the site announces one, it
  // is the one for the site's own vetro-wasm (same key) and R2 has it with
  // that size and sha256.
  const hintPath = join(site, 'app/android-prebuilt.json');
  if (existsSync(hintPath)) {
    const hint = JSON.parse(readFileSync(hintPath, 'utf8'));
    const { key } = await prebuiltKey(readFileSync(join(site, 'wasm/vetro_wasm.wasm')), hint.manifest);
    check(hint.key === key, `android-prebuilt.json: key ${hint.key}, the site's vetro-wasm has ${key}`);
    const found = await findPrebuilt(hint.manifest, key);
    check(found.info && found.info.size === hint.size && found.info.sha256 === hint.sha256, `prebuilt snapshot on R2: ${found.missing ?? 'size or sha256 differ'}`);
    console.log(`prebuilt Android snapshot: key ${key} matches the site's vetro-wasm, ${(hint.size / 2 ** 20).toFixed(0)} MiB on R2`);
  } else {
    console.log('prebuilt Android snapshot: none announced (the app cold boots Android)');
  }

  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome not found (VETRO_CHROME): site not tried in the browser';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const port = Number(process.env.VETRO_WEB_PORT ?? 0);
  const srv = await serve({ mounts: [['/Vetro/', site]], isolation: false, port });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const { page } = await openPage(cdp, `${srv.url}/Vetro/`);
    const links = await page.waitFor('landing page', () => page.eval(`(() => { const b = [...document.querySelectorAll('a.button')];
      return b.length && { buttons: b.map((a) => [a.textContent, a.href]), demo: document.querySelector('a.demo')?.href ?? '' }; })()`), 30_000);
    check(JSON.stringify(links.buttons) === JSON.stringify([['Launch Vetro', `${srv.url}/Vetro/app/`]]), `one button, Launch Vetro to app/: ${JSON.stringify(links.buttons)}`);
    check(links.demo === `${srv.url}/Vetro/app/?os=linux`, `the Linux demo link: ${links.demo}`);
    // The button: the phone starts by itself, nothing to choose.
    await page.eval(`location.href = ${JSON.stringify(links.buttons[0][1])}`);
    const START = `(() => window.vetroState && { progress: window.vetroState.progress ?? '', kind: document.getElementById('progress-line').dataset.kind,
      support: window.vetroState.support, form: document.getElementById('setup').checkVisibility(), tools: document.getElementById('tools').open })()`;
    const phone = await page.waitFor('the phone starting by itself', async () => {
      const r = await page.eval(START);
      if (r?.kind === 'error') {
        // R2 refuses origins other than :8080 (CORS): the page did start the phone.
        if (port !== 8080 && r.progress.includes('Failed to fetch')) return { ...r, refused: true };
        throw new Fail(`the default page failed: ${JSON.stringify(r)}`);
      }
      return r && /^(Downloading the phone|Starting the phone from scratch|Ready)/.test(r.progress) ? r : null;
    }, 120_000);
    check(!phone.form && !phone.tools && JSON.stringify(phone.support) === '[]', `the default page: ${JSON.stringify(phone)}`);
    console.log(`Launch Vetro: the phone starts by itself ("${phone.progress}"), no form, Tools closed, no browser notice` +
      (phone.refused ? ` (R2 refused origin ${srv.url}: VETRO_WEB_PORT=8080 to check the download too)` : ''));
    // The Linux demo link.
    const t0 = Date.now();
    await page.eval(`location.href = ${JSON.stringify(links.demo)}`);
    await page.waitFor('the Linux demo page', () => page.eval("location.search === '?os=linux' && !!window.vetroState"), 30_000);
    let at = await page.until('# ');
    const ms = Date.now() - t0;
    const isolated = await page.eval('crossOriginIsolated');
    check(!isolated, 'the page should not be isolated (Pages sends no COOP/COEP)');
    await page.type('uname -sr');
    at = await page.until('Linux 6.18', at);
    const state = await page.state();
    check(state.boot?.mode === 'cold', `expected a cold boot: ${JSON.stringify(state.boot)}`);
    // The site's network inspector (ABI 8): a guest request shows up.
    await page.type('udhcpc -i eth0 -n -q >/dev/null && wget -q -O /dev/null http://pages.vetro.test/prova; echo RETE-$((1+1))');
    at = await page.until('RETE-2', at);
    const req = await page.waitFor('request in the inspector', async () =>
      (await page.eval('window.vetroAnalysis.state().requests'))?.requests.find((r) => r.host === 'pages.vetro.test'), 30_000);
    check(req.method === 'GET' && req.path === '/prova' && req.status === 200, `inspector: ${JSON.stringify(req)}`);
    // The sinkhole's response is empty (Content-Length: 0, no Content-Type):
    // "0 B" in the list for both bodies, type from the content (the
    // inspector's own label, "empty").
    const cells = await page.waitFor('row in the table', () => page.eval(`(() => {
      const r = document.querySelector('#net-table tr[data-i="${req.i}"]');
      return r ? [...r.cells].slice(6, 9).map((c) => c.textContent) : null;
    })()`), 10_000);
    check(JSON.stringify(cells) === JSON.stringify(['0 B', '0 B', 'empty']), `request, response, type cells: ${JSON.stringify(cells)}`);
    console.log(`site under /Vetro/ without COOP/COEP: shell in ${(ms / 1000).toFixed(2)} s, the console answers, the inspector sees the network (empty bodies: 0 B)`);
  } finally {
    await srv.close();
    await closeChrome(proc, cdp, profile);
  }
});
