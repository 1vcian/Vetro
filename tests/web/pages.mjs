#!/usr/bin/env node
// Il sito di GitHub Pages (tools/pages/build.sh) come lo serve Pages: sotto
// un sottopercorso (/Vetro/) e senza intestazioni COOP/COEP. In Chrome
// headless la pagina d'ingresso porta all'app, l'app avvia il kernel guest
// fino alla shell e la console risponde, l'ispettore di rete vede una
// richiesta del guest; i sorgenti GPL ci sono e il tarball
// del kernel ricomposto dai pezzi ha lo sha256 dichiarato.
//
//   node tests/web/pages.mjs [target/pages]
//
// Senza Chrome dice SKIP (non è un test passato), o fallisce con
// VETRO_REQUIRE_BROWSER=1.

import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serve } from '../../tools/web-serve.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, root, run } from './lib.mjs';

run(async () => {
  const site = process.argv[2] ?? join(root, 'target/pages');
  if (!existsSync(join(site, 'index.html'))) throw new Fail(`${site} mancante: esegui tools/pages/build.sh`);

  // Sorgenti GPL: il tarball ricomposto dai pezzi è quello dichiarato.
  const src = join(site, 'sources');
  const files = readdirSync(src);
  const sums = files.filter((f) => /^linux-.*\.tar\.xz\.sha256$/.test(f));
  check(sums.length === 1, `sources/: atteso un linux-*.tar.xz.sha256, trovati ${sums}`);
  const [want, name] = readFileSync(join(src, sums[0]), 'utf8').trim().split(/\s+/);
  const parts = files.filter((f) => f.startsWith(`${name}.a`)).sort();
  check(parts.length > 1, `sources/: pezzi di ${name} assenti`);
  const h = createHash('sha256');
  for (const p of parts) h.update(readFileSync(join(src, p)));
  check(h.digest('hex') === want, `sources/: ${name} ricomposto non ha lo sha256 ${want}`);
  for (const f of ['README', 'defconfig', 'vetro.config', 'VERSIONS', 'index.html']) check(files.includes(f), `sources/${f} assente`);
  check(files.some((f) => /^busybox-.*\.tar\.bz2$/.test(f)), 'sources/: sorgenti di BusyBox assenti');
  console.log(`sorgenti GPL: ${name} in ${parts.length} pezzi (sha256 giusto), BusyBox e patch di Alpine`);

  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome non trovato (VETRO_CHROME): sito non provato nel browser';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const srv = await serve({ mounts: [['/Vetro/', site]], isolation: false });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const { page } = await openPage(cdp, `${srv.url}/Vetro/`);
    const href = await page.waitFor('pagina d\'ingresso', () => page.eval("document.querySelector('a.button')?.href ?? ''"), 30_000);
    check(href.startsWith(`${srv.url}/Vetro/app/`), `il pulsante porta a ${href}`);
    const t0 = Date.now();
    await page.eval(`location.href = ${JSON.stringify(href)}`);
    let at = await page.until('# ');
    const ms = Date.now() - t0;
    const isolated = await page.eval('crossOriginIsolated');
    check(!isolated, 'la pagina non dovrebbe essere isolata (Pages non manda COOP/COEP)');
    await page.type('uname -sr');
    at = await page.until('Linux 6.18', at);
    const state = await page.state();
    check(state.boot?.mode === 'cold', `atteso un avvio da zero: ${JSON.stringify(state.boot)}`);
    // L'ispettore di rete del sito (ABI 8): una richiesta del guest compare.
    await page.type('udhcpc -i eth0 -n -q >/dev/null && wget -q -O /dev/null http://pages.vetro.test/prova; echo RETE-$((1+1))');
    at = await page.until('RETE-2', at);
    const req = await page.waitFor('richiesta nell\'ispettore', async () =>
      (await page.eval('window.vetroAnalysis.state().requests'))?.requests.find((r) => r.host === 'pages.vetro.test'), 30_000);
    check(req.method === 'GET' && req.path === '/prova' && req.status === 200, `ispettore: ${JSON.stringify(req)}`);
    console.log(`sito sotto /Vetro/ senza COOP/COEP: shell in ${(ms / 1000).toFixed(2)} s, la console risponde, l'ispettore vede la rete`);
  } finally {
    cdp.close();
    const exited = proc.exitCode !== null ? Promise.resolve() : new Promise((ok) => proc.once('exit', ok));
    proc.kill();
    await exited;
    rmSync(profile, { recursive: true, force: true });
    await srv.close();
  }
});
