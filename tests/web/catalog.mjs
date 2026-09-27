#!/usr/bin/env node
// The app catalog end to end without a guest (M6, ADR 0033): a catalog and
// its APKs on a local HTTP server (tools/web-serve.mjs), loaded and parsed
// (web/node/catalog.mjs), an APK downloaded with progress and verified, read
// with apk.mjs, installed with the ADB client (web/node/adb.mjs, push
// progress) into the fake adbd of tests/web/fake-adbd.mjs, and the installed
// packages read back with `pm list packages`; the card states follow the same
// sequence as in the page. A tampered APK and a truncated one fail the check
// and never reach the device; a missing catalog, or one of another format,
// is an error (the page hides the panel).
//
//   node tests/web/catalog.mjs

import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { serve } from '../../tools/web-serve.mjs';
import { AdbClient } from '../../web/node/adb.mjs';
import { apkInfo } from '../../web/node/apk.mjs';
import { downloadApk, initialState, loadCatalog, nextState, PACKAGES_QUERY, parsePackages } from '../../web/node/catalog.mjs';
import { drive, FakeAdbd, Pipe } from './fake-adbd.mjs';
import { check, makeZip, root, run } from './lib.mjs';

const ok = (cond, what) => {
  check(cond, what);
  console.log(`ok: ${what}`);
};
const sha = (b) => createHash('sha256').update(b).digest('hex');

async function fails(p) {
  try {
    await p;
  } catch (e) {
    return e.message;
  }
  return null;
}

run(async () => {
  const dir = join(root, 'target/web-test/catalog');
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(join(dir, 'www/apks'), { recursive: true });
  // The test app's manifest in a ZIP, with a 2.5 MB stored asset so the push
  // goes in several progress steps.
  const axml = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/tocco-manifest.axml')));
  const filler = new Uint8Array(2_500_000).map((_, i) => (i * 31 + 7) & 0xff);
  const apk = makeZip([['AndroidManifest.xml', axml, 0], ['assets/filler.bin', filler, 0]]);
  const tampered = apk.slice();
  tampered[apk.length - 100] ^= 0x40;
  writeFileSync(join(dir, 'www/apks/tocco.apk'), apk);
  writeFileSync(join(dir, 'www/apks/tampered.apk'), tampered);
  writeFileSync(join(dir, 'www/apks/short.apk'), apk.subarray(0, apk.length - 10));
  const entry = (id, file, over = {}) => ({
    id, name: `Tocco ${id}`, package: `it.vetro.${id}`, version: '1.0', versionCode: 1, apk: `apks/${file}`, size: apk.length, sha256: sha(apk),
    license: 'GPL-3.0-only', source: 'https://example.org/vetro/tocco/tree/v1.0', icon: null, description: 'The test app', minImage: null, advanced: false, ...over,
  });
  const catalog = {
    format: 1,
    updated: new Date().toISOString(),
    apps: [
      entry('tocco', 'tocco.apk', { package: 'it.vetro.tocco' }),
      entry('tampered', 'tampered.apk'),
      entry('short', 'short.apk'),
      entry('broken', 'tocco.apk', { sha256: 'not-a-hash' }),
    ],
  };
  writeFileSync(join(dir, 'www/v1.json'), JSON.stringify(catalog));
  writeFileSync(join(dir, 'www/v2.json'), JSON.stringify({ ...catalog, format: 2 }));
  const srv = await serve({ mounts: [['/catalog/', join(dir, 'www')]] });
  try {
    const url = `${srv.url}/catalog/v1.json`;
    const c = await loadCatalog(url);
    ok(c.apps.map((a) => a.id).join() === 'tocco,tampered,short' && c.problems.length === 1 && c.problems[0].includes('broken'), 'catalog loaded over HTTP: 3 entries, the broken one named');
    const tocco = c.apps[0];
    ok(tocco.apk === `${srv.url}/catalog/apks/tocco.apk`, 'APK URL resolved against the catalog');
    ok((await fails(loadCatalog(`${srv.url}/catalog/missing.json`)))?.includes('status 404'), 'missing catalog: error (panel hidden)');
    ok((await fails(loadCatalog(`${srv.url}/catalog/v2.json`)))?.includes('format 2'), 'catalog of another format: error (panel hidden)');

    // The device.
    const adbd = new FakeAdbd({ identify: (b) => (b.length === apk.length && sha(b) === sha(apk) ? { package: 'it.vetro.tocco', versionCode: 1 } : null) });
    const client = new AdbClient(new Pipe(adbd));
    await drive(client, client.connect());
    const packages = async () => parsePackages((await drive(client, client.shell(PACKAGES_QUERY))).stdout);

    // Before: not installed.
    let st = nextState(initialState(), { type: 'packages', packages: await packages() }, tocco);
    const phases = [st.phase];
    const step = (ev) => {
      const next = nextState(st, ev, tocco);
      if (next.phase !== st.phase) phases.push(next.phase);
      st = next;
    };
    step({ type: 'download', total: tocco.size });
    const loaded = [];
    const bytes = await downloadApk(tocco, { onProgress: (p) => (loaded.push(p.loaded), step({ type: 'progress', ...p })) });
    ok(loaded[0] === 0 && loaded.at(-1) === apk.length && st.fraction === 1, `download progress up to ${apk.length} bytes (${loaded.length} steps)`);
    const info = await apkInfo(bytes);
    ok(info.package === tocco.package, `downloaded APK read: ${info.package} ${info.versionName}`);
    step({ type: 'downloaded' });
    const pushed = [];
    const out = await drive(client, client.install(bytes, { name: `${info.package}.apk`, onProgress: (sent, total) => (pushed.push(sent), step({ type: 'pushing', fraction: sent / total })) }));
    step({ type: 'installed', versionCode: info.versionCode });
    ok(out === 'Success' && adbd.installed.length === apk.length && sha(adbd.installed) === sha(apk), 'installed through adb: the device got the exact bytes');
    ok(pushed.length === 3 && pushed.at(-1) === apk.length, `push progress: ${pushed.join(', ')}`);
    ok(adbd.commands.some((x) => x.startsWith('pm install -r /data/local/tmp/it.vetro.tocco.apk')), 'pm install -r of the pushed file');
    // After: the package list says installed (as after a reload or a restore).
    const after = await packages();
    ok(after.get('it.vetro.tocco') === 1, 'pm list packages shows it.vetro.tocco versionCode 1');
    const fresh = nextState(initialState(), { type: 'packages', packages: after }, tocco);
    ok(phases.join(' > ') === 'absent > downloading > installing > installed' && st.phase === 'installed' && fresh.phase === 'installed',
      `states: ${phases.join(' > ')}; a fresh card from the package list: ${fresh.phase}`);

    // Tampered and truncated APKs: failed, nothing pushed.
    const syncs = adbd.opened.filter((s) => s === 'sync:').length;
    for (const bad of [c.apps[1], c.apps[2]]) {
      let s = nextState(initialState(), { type: 'download' }, bad);
      const msg = await fails(downloadApk(bad));
      s = nextState(s, { type: 'failed', error: msg }, bad);
      ok(s.phase === 'failed' && /SHA-256|bytes instead of/.test(s.error), `${bad.id}: ${s.error}`);
    }
    ok(adbd.opened.filter((s) => s === 'sync:').length === syncs, 'bad APKs never reached the device');
    console.log('app catalog: all good');
  } finally {
    await srv.close();
  }
});
