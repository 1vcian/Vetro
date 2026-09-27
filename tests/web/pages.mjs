#!/usr/bin/env node
// The GitHub Pages site (tools/pages/build.sh) as Pages serves it: under a
// subpath (/Vetro/) and without COOP/COEP headers. In headless Chrome the
// landing page leads to the app, the app boots the guest kernel to the shell
// and the console answers, the network inspector sees a guest request (empty
// bodies as "0 B"); the GPL sources are there and the kernel tarball put back
// together from its pieces has the declared sha256; the prebuilt Android
// snapshot the site announces, if any, is the one for its vetro-wasm.
//
//   node tests/web/pages.mjs [target/pages]
//
// Without Chrome it says SKIP (not a passed test), or fails with
// VETRO_REQUIRE_BROWSER=1.

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
  const srv = await serve({ mounts: [['/Vetro/', site]], isolation: false });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const { page } = await openPage(cdp, `${srv.url}/Vetro/`);
    const href = await page.waitFor('landing page', () => page.eval("document.querySelector('a.button')?.href ?? ''"), 30_000);
    check(href.startsWith(`${srv.url}/Vetro/app/`), `the button leads to ${href}`);
    const t0 = Date.now();
    await page.eval(`location.href = ${JSON.stringify(href)}`);
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
