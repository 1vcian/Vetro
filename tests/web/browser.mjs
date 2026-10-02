#!/usr/bin/env node
// M5: the web app (web/app) in a real headless Chrome, driven with the
// DevTools protocol (Node 22 WebSocket, no npm dependencies).
//
//   1. the page boots the M3 kernel in the Worker with a disk over HTTP Range
//      (empty OPFS cache); from the page console (real keys) we launch
//      `md5sum /dev/vda`; with the scanout off the screen shows the message
//      that explains why, and its button types `timeout 30 vetro-dev
//      drm-hold`: the canvas must show the guest's pattern pixel for
//      pixel, the cursor must be visible; Enter closes it and the message
//      comes back;
//   2. with the canvas focused a real key (KeyA) reaches the guest through
//      virtio-input (`vetro-dev input-read`);
//   3. file manager (M8): with the roots set by the test the panel
//      shows the tree, updates by itself when a guest process
//      creates a file, opens a file, edits it and saves it in the guest, which
//      rereads it with cat (mode preserved), an empty folder opened shows
//      "(empty)"; a cell of a SQLite database
//      in WAL kept open by the guest changed from the panel (preview of the
//      query, SQL run in the guest, table reread from the -wal, value
//      reread with sqlite3) and a SharedPreferences value changed
//      in the table (ADR 0021);
//   3b. the first boot saves the snapshot in OPFS at the prompt; the guest writes
//      to the disk (dd + sync) and the snapshot is saved again together with the overlay
//      (M6, ADR 0016);
//   4. second session without a snapshot (`snapshot=0`, same profile):
//      boot from scratch, the write of the first session is there (overlay in
//      OPFS), and the disk blocks come from the OPFS cache: no Range
//      read beyond the first one (size);
//   5. third session: resumes from the snapshot instead of booting the kernel
//      (time measured), the console answers, the write is there, the file
//      manager reconnects to the snapshot's daemon.
//   6. the AOSP image selector (`?advanced=1`, the setup form, not started):
//      the default system is AOSP, the default version (ANDROID_VERSIONS[0])
//      in the manifest field, the previous one selectable, "other" for a
//      typed URL;
//   7. the zero-choice default page (`app/`, M10): no setup form, the phone
//      starts by itself (here with a manifest that does not exist), the
//      screen, the Power button and the APK drop visible, the developer
//      panels (console included) under a closed "Tools" toggle, no
//      browser notice in Chrome, and the failure said on the progress line;
//   8. the same page in a browser without WebAssembly SIMD (stood in for by
//      a `WebAssembly.validate` that says no): the notice says Vetro cannot
//      run here, and nothing starts.
//
// Sessions 1-5 run the Linux demo (`?os=linux`).
//
// Chrome: VETRO_CHROME, otherwise the usual paths. Without Chrome the test
// says SKIP (it is not a passed test) and exits with 0, or with 1 if
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser.mjs

import { createHash } from 'node:crypto';
import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { ANDROID_VERSIONS, DEFAULT_MANIFEST } from '../../web/node/android.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';

const SIZE = 2 * 1024 * 1024 + 4096;
const WRITE_AT = 1048576;
const TEXT = 'VETRO-OPFS-42';

/** The pattern of `vetro-dev drm` (tests/boot/tests/devices.rs), checked in the page. */
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
  return { w: c.width, h: c.height, bad, first, cursor: !cur.hidden, hostCursor: getComputedStyle(c).cursor, off: !document.getElementById('screen-off').hidden };
})()`;

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome not found (VETRO_CHROME): browser test not run';
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
  const modified = disk.slice();
  modified.set(new TextEncoder().encode(TEXT), WRITE_AT);
  const md5After = createHash('md5').update(modified).digest('hex');
  // Reread with the guest cache dropped: the bytes come from the disk.
  const READ = `echo 3 > /proc/sys/vm/drop_caches; echo L-$(dd if=/dev/vda bs=1 skip=${WRITE_AT} count=${TEXT.length} 2>/dev/null)-F; md5sum /dev/vda`;
  const ranges = [];
  const srv = await serve({
    mounts: [['/disks/test.img', file], ...appMounts()],
    onRequest: (r) => r.path === '/disks/test.img' && ranges.push(r),
  });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    const q = new URLSearchParams({ os: 'linux', disk: '/disks/test.img', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1' });
    const url = `${srv.url}/app/?${q}`;
    let t0 = Date.now();
    let { page, targetId } = await openPage(cdp, url);
    let at = await page.until('# ');
    // No guest cursor yet: the host pointer must stay visible over the screen
    // (touchscreen guests such as Android never draw one).
    const hostCursor = await page.eval("getComputedStyle(document.getElementById('screen')).cursor");
    check(hostCursor === 'crosshair', `host pointer over the screen without a guest cursor: ${hostCursor}`);
    const coldMs = Date.now() - t0;
    const boot1 = (await page.state()).boot;
    check(boot1?.mode === 'cold', `first boot: expected from scratch, ${JSON.stringify(boot1)}`);
    const snap1 = await page.waitFor('first snapshot', async () => (await page.state()).snapshots[0], 60_000);
    check(snap1.why === 'boot finished', `first snapshot: ${JSON.stringify(snap1)}`);
    console.log(`first boot (from scratch): prompt in ${(coldMs / 1000).toFixed(2)} s; snapshot at the prompt ${(snap1.size / 2 ** 20).toFixed(1)} MiB, ` +
      `saved in ${snap1.saveMs.toFixed(0)} ms + OPFS write ${snap1.writeMs.toFixed(0)} ms`);
    await page.type('md5sum /dev/vda');
    at = await page.until(`${md5}  /dev/vda`, at);
    // Scanout off: the message explains that the guest is not drawing; the
    // button types the test pattern command in the console.
    const offMsg = "(() => { const o = document.getElementById('screen-off'); return o.hidden ? null : o.textContent.replace(/\\s+/g, ' ').trim(); })()";
    const shownOff = await page.eval(offMsg);
    check(shownOff?.includes('The guest is not drawing') && shownOff.includes('Draw a test pattern'), `message with the scanout off: ${shownOff}`);
    await page.eval("document.getElementById('screen-demo').click()");
    at = await page.until('timeout 30 vetro-dev drm-hold', at);
    at = await page.until('VETRO-DRM-PRONTO', at);
    const cv = await page.waitFor('pattern on the canvas', async () => {
      const r = await page.eval(CHECK_CANVAS);
      return r.w === 1280 && r.bad === 0 && r.cursor && r.hostCursor === 'none' && !r.off ? r : null;
    }, 20_000).catch(async (e) => {
      throw new Fail(`${e.message}\ncanvas: ${JSON.stringify(await page.eval(CHECK_CANVAS))}`);
    });
    console.log(`canvas: ${cv.w}x${cv.h}, the guest's pattern pixel for pixel, cursor visible`);
    // Enter in the console closes the pattern before the 30 s: the scanout
    // turns off and the message comes back.
    await page.type('');
    at = await page.until('vetro-dev: drm chiuso', at);
    at = await page.until('# ', at);
    await page.waitFor('message again with the scanout off', () => page.eval(offMsg), 20_000);
    console.log('scanout off: message visible, the button draws the test pattern, Enter closes it and the message comes back');
    await page.type('vetro-dev input-read /dev/input/event1 4');
    at = await page.until('VETRO-INPUT-PRONTO', at);
    await page.eval("document.getElementById('screen').focus()");
    await page.key('a', 'KeyA', 'a');
    at = await page.until('vetro-dev: evento 1 30 0', at);
    console.log('keyboard: KeyA from the canvas reached the guest as KEY_A (virtio-input)');
    at = await page.until('# ', at);

    // File manager (M8, ADR 0020): the panel shows the roots
    // set, updates by itself when a guest process creates a
    // file (inotify), opens a file, edits it and saves it in the guest, which
    // rereads it with cat (mode preserved).
    await page.type('mkdir -p /tmp/web/vuota && echo prima > /tmp/web/nota.txt && chmod 640 /tmp/web/nota.txt');
    at = await page.until('# ', at);
    const files = (expr) => page.eval(`window.vetroFiles.state()${expr}`);
    await page.waitFor('file manager connected', async () => (await files('.state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/web'])");
    await page.waitFor('nota.txt in the tree', async () => (await files('.shown')).includes('/tmp/web/nota.txt'), 30_000);
    // An empty folder opened says "(empty)" (a line that is not an entry).
    await page.waitFor('empty folder in the tree', async () => (await files('.shown')).includes('/tmp/web/vuota'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/vuota"]').click()`);
    const emptyNote = `(() => { const r = document.querySelector('[data-path="/tmp/web/vuota"]'); const n = r?.nextElementSibling; return r?.querySelector('.twisty').textContent === '▾' && n?.classList.contains('fempty') ? n.textContent : null; })()`;
    const empty = await page.waitFor('"(empty)" under the empty folder', async () => (await page.eval(emptyNote)) === '(empty)' || null, 30_000);
    check(empty, 'empty folder');
    console.log('file manager: an empty folder opened shows "(empty)"');
    await page.type('echo dal-guest > /tmp/web/nuovo.txt');
    at = await page.until('# ', at);
    await page.waitFor('nuovo.txt appeared by itself', async () => (await files('.shown')).includes('/tmp/web/nuovo.txt'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/nota.txt"]').click()`);
    await page.waitFor('nota.txt open', async () => (await page.eval("document.querySelector('#files-content textarea')?.value")) === 'prima\n', 30_000);
    await page.eval(`(() => {
      const a = document.querySelector('#files-content textarea');
      a.value = 'modificato dal pannello\\n';
      a.dispatchEvent(new Event('input'));
      document.getElementById('files-save').click();
    })()`);
    await page.waitFor('save in the guest', async () => (await files('.message')).startsWith('saved in the guest'), 30_000);
    await page.type("cat /tmp/web/nota.txt; stat -c 'modo-%a' /tmp/web/nota.txt");
    at = await page.until('modificato dal pannello', at);
    at = await page.until('modo-640', at);
    at = await page.until('# ', at);
    console.log('file manager: tree updated live, file edited in the panel and reread by the guest with cat (mode 640 preserved)');

    // Editing a SQLite cell from the panel (ADR 0021): WAL database
    // kept open by a guest process; preview of the query, SQL
    // run in the guest, table reread with the -wal, value reread by the
    // guest with sqlite3.
    await page.type(`sqlite3 -batch -list /tmp/web/app.db "PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'prima'), (2, 'altra');"`);
    at = await page.until('wal', at);
    at = await page.until('# ', at);
    await page.type("(echo 'SELECT 1 FROM t;'; sleep 100000) | sqlite3 /tmp/web/app.db >/dev/null &");
    at = await page.until('# ', at);
    await page.waitFor('app.db in the tree', async () => (await files('.shown')).includes('/tmp/web/app.db-shm'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/app.db"]').click()`);
    const cell = (r, c) => `document.querySelector('#files-content td[data-r="${r}"][data-c="${c}"]')`;
    await page.waitFor('SQLite table', async () => (await page.eval(`${cell(0, 1)}?.textContent`)) === 'prima', 30_000);
    await page.eval(`${cell(0, 1)}.click()`);
    await page.eval(`(() => {
      const v = document.querySelector('#files-content .fsql-value');
      v.value = 'dal pannello';
      document.querySelector('#files-content .fsql-preview').click();
    })()`);
    const query = await page.eval("document.querySelector('#files-content .fsql-query')?.value");
    check(query === 'UPDATE "t" SET "v" = ?1 WHERE rowid = ?2', `query preview: ${query}`);
    const params = await page.eval("document.querySelector('#files-content .fsql-params')?.textContent");
    check(params === "?1 = 'dal pannello'   ?2 = 1", `preview parameters: ${params}`);
    await page.eval("document.querySelector('#files-content .fsql-run').click()");
    await page.waitFor('SQL run in the guest', async () => (await files('.message')).startsWith('run in the guest: 1 rows changed'), 30_000)
      .catch(async (e) => {
        throw new Fail(`${e.message}: ${await files('.message')}`);
      });
    await page.waitFor('cell reread with the WAL', async () => (await page.eval(`${cell(0, 1)}?.textContent`)) === 'dal pannello', 30_000);
    const walNote = await page.eval("document.querySelector('#files-content .fsql .fnote')?.textContent");
    check(/WAL: \d+ frames applied/.test(walNote), `the table doesn't come from the WAL: ${walNote}`);
    await page.type("sqlite3 -batch -list /tmp/web/app.db 'SELECT v FROM t WHERE id = 1'");
    at = await page.until('dal pannello', at);
    at = await page.until('# ', at);
    console.log('file manager: SQLite cell edited from the panel (preview, SQL in the guest on an open WAL database), table reread from the -wal, value reread by the guest with sqlite3');

    // SharedPreferences: a value changed in the table, XML rewritten like Android and saved.
    await page.type(`printf '%s\\n' "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>" '<map>' '    <int name="avvii" value="3" />' '</map>' > /tmp/web/prefs.xml`);
    at = await page.until('# ', at);
    await page.waitFor('prefs.xml in the tree', async () => (await files('.shown')).includes('/tmp/web/prefs.xml'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/prefs.xml"]').click()`);
    await page.waitFor('SharedPreferences table', async () => (await page.eval("document.querySelector('#files-content .fpref-value')?.value")) === '3', 30_000);
    await page.eval(`(() => {
      const v = document.querySelector('#files-content .fpref-value');
      v.value = '42';
      v.dispatchEvent(new Event('change'));
      document.getElementById('files-save').click();
    })()`);
    await page.waitFor('SharedPreferences saved', async () => (await files('.message')).startsWith('saved in the guest'), 30_000);
    await page.type("grep -c 'value=\"42\"' /tmp/web/prefs.xml | sed 's/^/conteggio-/'");
    at = await page.until('conteggio-1', at);
    at = await page.until('# ', at);
    console.log('file manager: SharedPreferences changed in the table, XML rewritten and reread by the guest');
    await page.type(`printf ${TEXT} | dd of=/dev/vda bs=1 seek=${WRITE_AT} conv=notrunc 2>/dev/null; sync`);
    at = await page.until('# ', at);
    const snap2 = await page.waitFor('snapshot after the write', async () => (await page.state()).snapshots.find((x) => x.why === 'disks changed'), 60_000);
    check(snap2.generations[0] > snap1.generations[0], `overlay generation did not grow: ${JSON.stringify([snap1, snap2])}`);
    console.log(`write to the disk: snapshot saved again with the overlay at generation ${snap2.generations[0]}`);
    const first = ranges.length;
    await cdp.send('Target.closeTarget', { targetId });

    // Second session without a snapshot: boot from scratch, disk from OPFS.
    ({ page, targetId } = await openPage(cdp, `${url}&snapshot=0`));
    at = await page.until('# ');
    check((await page.state()).boot?.mode === 'cold', 'with snapshot=0 it boots from scratch');
    await page.type(READ);
    at = await page.until(`${md5After}  /dev/vda`, at);
    const text2 = await page.consoleText();
    check(text2.includes(`L-${TEXT}-F`), `write of the first session lost:\n${text2.split('\n').slice(-8).join('\n')}`);
    const second = ranges.length - first;
    const status = await page.eval("document.getElementById('status').textContent");
    check(first > 1, `first boot: ${first} requests to the disk, Range reads expected`);
    check(second === 1, `restart: ${second} requests to the disk (expected 1, the size): OPFS not used? state: ${status}`);
    console.log(`second session (from scratch): the write of the first one is there (overlay in OPFS, md5 ${md5After}); ` +
      `Range requests: first boot ${first}, restart ${second} (only the size)`);
    await cdp.send('Target.closeTarget', { targetId });

    // Third session: from the snapshot.
    const before3 = ranges.length;
    t0 = Date.now();
    ({ page, targetId } = await openPage(cdp, url));
    const boot3 = await page.waitFor('restore from the snapshot', async () => (await page.state())?.boot, 60_000);
    const restoredMs = Date.now() - t0;
    check(boot3.mode === 'snapshot', `third session: expected from the snapshot, ${JSON.stringify(boot3)}`);
    at = (await page.consoleText()).length;
    check((await page.consoleText()).includes('# '), 'the snapshot console was not shown again');
    await page.type(READ);
    at = await page.until(`${md5After}  /dev/vda`, at);
    const answeredMs = Date.now() - t0;
    check((await page.consoleText()).slice(-400).includes(`L-${TEXT}-F`), 'after the restore the write is not there');
    // The file manager reconnects to the snapshot's daemon (the
    // connection of the first session, left in the snapshot, is closed).
    await page.waitFor('file manager after the restore', async () => (await page.eval('window.vetroFiles.state().state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/web'])");
    await page.waitFor('tree after the restore', async () => (await page.eval('window.vetroFiles.state().shown')).includes('/tmp/web/nota.txt'), 30_000);
    const t = boot3.times;
    console.log(`third session (snapshot): ready in ${(restoredMs / 1000).toFixed(2)} s from opening the page ` +
      `(in the Worker: wasm ${t.wasm.toFixed(0)} ms, kernel and initramfs ${t.files.toFixed(0)} ms, key ${t.key.toFixed(0)} ms, ` +
      `OPFS read ${t.read.toFixed(0)} ms, restore ${t.restore.toFixed(0)} ms; snapshot ${(boot3.size / 2 ** 20).toFixed(1)} MiB), ` +
      `first command run at ${(answeredMs / 1000).toFixed(2)} s; first boot from scratch ${(coldMs / 1000).toFixed(2)} s; ` +
      `Range requests ${ranges.length - before3}`);
    // 6. The AOSP image selector (the machine is not started).
    await cdp.send('Target.closeTarget', { targetId });
    ({ page, targetId } = await openPage(cdp, `${srv.url}/app/?advanced=1`));
    const SELECTOR = `(() => { const f = document.getElementById('setup')?.elements;
      return f?.androidVersion?.options.length > 1 && { version: f.androidVersion.value, manifest: f.manifestUrl.value, options: [...f.androidVersion.options].map((o) => o.value),
        os: f.os.value, form: !document.getElementById('setup').hidden, machine: !document.getElementById('machine').hidden }; })()`;
    const sel = await page.waitFor('AOSP version selector', () => page.eval(SELECTOR), 30_000);
    check(sel.form && !sel.machine && sel.os === 'android', `?advanced=1: the form, AOSP by default, nothing started: ${JSON.stringify(sel)}`);
    check(sel.manifest === DEFAULT_MANIFEST && sel.version === DEFAULT_MANIFEST, `default image: ${JSON.stringify(sel)}`);
    check(JSON.stringify(sel.options) === JSON.stringify([...ANDROID_VERSIONS.map((v) => v.manifest), '']), `versions offered: ${JSON.stringify(sel.options)}`);
    const pick = (value) => page.eval(`(() => { const f = document.getElementById('setup').elements; f.androidVersion.value = ${JSON.stringify(value)};
      f.androidVersion.dispatchEvent(new Event('change')); return f.manifestUrl.value; })()`);
    const previous = ANDROID_VERSIONS.find((v) => v.version.endsWith('-bd09e2f')).manifest;
    check((await pick(previous)) === previous, 'bd09e2f selected: the manifest field follows');
    const typed = await page.eval(`(() => { const f = document.getElementById('setup').elements; f.manifestUrl.value = '/aosp/manifest.json';
      f.manifestUrl.dispatchEvent(new Event('change')); return f.androidVersion.value; })()`);
    check(typed === '', `a typed URL selects "other": ${JSON.stringify(typed)}`);
    console.log(`AOSP image selector: default ${ANDROID_VERSIONS[0].version}, ${ANDROID_VERSIONS.length} versions offered`);

    // 7. The zero-choice default page: nothing to choose, the phone starts.
    await cdp.send('Target.closeTarget', { targetId });
    ({ page, targetId } = await openPage(cdp, `${srv.url}/app/?manifest=/aosp-missing/manifest.json&catalog=/catalog-missing.json`));
    const PAGE = `(() => {
      const $ = (id) => document.getElementById(id);
      // Rendered: not hidden, not under a hidden parent or a closed <details>.
      const shown = (id) => $(id)?.checkVisibility() ?? false;
      return { form: shown('setup'), machine: shown('machine'), screen: shown('screen'), power: shown('power'), drop: shown('apk-drop'),
        tools: $('tools').open, toolsShown: shown('tools'), console: shown('console'), consoleInTools: !!$('console').closest('#tools'),
        analysis: shown('analysis-box'), forget: shown('forget'), status: shown('status'), progress: $('progress-text').textContent,
        kind: $('progress-line').dataset.kind, unsupported: shown('unsupported'), support: window.vetroState?.support ?? null,
        android: window.vetroAndroid ? true : false };
    })()`;
    const zc = await page.waitFor('the failure on the progress line', async () => {
      const r = await page.eval(PAGE);
      return r.kind === 'error' ? r : null;
    }, 60_000).catch(async (e) => {
      throw new Fail(`${e.message}: ${JSON.stringify(await page.eval(PAGE))}`);
    });
    check(!zc.form && zc.machine && zc.screen && zc.power && zc.drop, `default page: the phone and its controls, no form: ${JSON.stringify(zc)}`);
    check(!zc.tools && zc.toolsShown && !zc.analysis && !zc.forget && !zc.console && zc.consoleInTools && !zc.status,
      `default page: the developer panels under a closed Tools toggle: ${JSON.stringify(zc)}`);
    check(JSON.stringify(zc.support) === '[]' && !zc.unsupported, `headless Chrome: no browser notice expected: ${JSON.stringify(zc.support)}`);
    check(zc.progress.includes('aosp-missing') && zc.progress.includes('Reload'), `the failure on the progress line: ${zc.progress}`);
    // The Tools toggle opens the panels.
    await page.eval("document.querySelector('#tools > summary').click()");
    const opened = await page.eval(PAGE);
    check(opened.tools && opened.analysis && opened.forget && opened.console, `Tools opened: ${JSON.stringify(opened)}`);
    console.log(`zero-choice page: started by itself, Tools closed, the error on the progress line: "${zc.progress}"`);

    // 8. A browser without WebAssembly SIMD: a notice, nothing started.
    await cdp.send('Target.closeTarget', { targetId });
    ({ page, targetId } = await openPage(cdp, `${srv.url}/app/?catalog=/catalog-missing.json`, { before: 'WebAssembly.validate = () => false;' }));
    const noSimd = await page.waitFor('the notice without SIMD', () => page.eval(`(() => {
      const $ = (id) => document.getElementById(id);
      return window.vetroState?.support && { support: window.vetroState.support.map((i) => i.id + (i.fatal ? '!' : '')), notice: $('unsupported').checkVisibility(),
        text: $('unsupported').textContent.replace(/\\s+/g, ' ').trim(), machine: $('machine').checkVisibility(), boot: window.vetroState.boot,
        progress: $('progress-text').textContent, kind: $('progress-line').dataset.kind };
    })()`), 30_000);
    check(JSON.stringify(noSimd.support) === '["simd!"]' && noSimd.notice && noSimd.text.includes('cannot run Vetro') && noSimd.text.includes('SIMD'),
      `without SIMD: the notice: ${JSON.stringify(noSimd)}`);
    await new Promise((ok) => setTimeout(ok, 2000));
    const after = await page.eval("({ machine: document.getElementById('machine').checkVisibility(), boot: window.vetroState.boot, stats: window.vetroState.stats ?? null })");
    check(!after.machine && after.boot === null && after.stats === null && noSimd.kind === 'error', `without SIMD nothing starts: ${JSON.stringify({ ...noSimd, ...after })}`);
    console.log(`browser without SIMD: "${noSimd.text}", nothing started`);
    console.log('app in the browser: ok');
  } finally {
    await srv.close();
    await closeChrome(proc, cdp, profile);
  }
});
