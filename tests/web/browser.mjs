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
//   3. gestore dei file (M8): con le radici impostate dal test il pannello
//      mostra l'albero, si aggiorna da solo quando un processo del guest
//      crea un file, apre un file, lo modifica e lo salva nel guest, che lo
//      rilegge con cat (modo conservato); una cella di un database SQLite
//      in WAL tenuto aperto dal guest cambiata dal pannello (anteprima della
//      query, SQL eseguito nel guest, tabella riletta dal -wal, valore
//      riletto con sqlite3) e un valore delle SharedPreferences cambiato
//      nella tabella (ADR 0021);
//   3b. il primo avvio salva lo snapshot in OPFS al prompt; il guest scrive
//      sul disco (dd + sync) e lo snapshot si risalva insieme all'overlay
//      (M6, ADR 0016);
//   4. seconda sessione senza snapshot (`snapshot=0`, stesso profilo):
//      avvio da zero, la scrittura della prima sessione c'è (overlay in
//      OPFS), e i blocchi del disco vengono dalla cache OPFS: nessuna lettura
//      Range oltre la prima (dimensione);
//   5. terza sessione: riparte dallo snapshot invece di avviare il kernel
//      (tempo misurato), la console risponde, la scrittura c'è, il gestore
//      dei file si ricollega al demone dello snapshot.
//
// Chrome: VETRO_CHROME, altrimenti i percorsi soliti. Senza Chrome il test
// dice SKIP (non è un test passato) ed esce con 0, o con 1 se
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser.mjs

import { createHash } from 'node:crypto';
import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';

const SIZE = 2 * 1024 * 1024 + 4096;
const WRITE_AT = 1048576;
const TEXT = 'VETRO-OPFS-42';

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
  const modified = disk.slice();
  modified.set(new TextEncoder().encode(TEXT), WRITE_AT);
  const md5After = createHash('md5').update(modified).digest('hex');
  // Rilettura a cache del guest svuotata: i byte vengono dal disco.
  const READ = `echo 3 > /proc/sys/vm/drop_caches; echo L-$(dd if=/dev/vda bs=1 skip=${WRITE_AT} count=${TEXT.length} 2>/dev/null)-F; md5sum /dev/vda`;
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
    let t0 = Date.now();
    let { page, targetId } = await openPage(cdp, url);
    let at = await page.until('# ');
    const coldMs = Date.now() - t0;
    const boot1 = (await page.state()).boot;
    check(boot1?.mode === 'cold', `primo avvio: atteso da zero, ${JSON.stringify(boot1)}`);
    const snap1 = await page.waitFor('primo snapshot', async () => (await page.state()).snapshots[0], 60_000);
    check(snap1.why === 'avvio finito', `primo snapshot: ${JSON.stringify(snap1)}`);
    console.log(`primo avvio (da zero): prompt in ${(coldMs / 1000).toFixed(2)} s; snapshot al prompt ${(snap1.size / 2 ** 20).toFixed(1)} MiB, ` +
      `salvato in ${snap1.saveMs.toFixed(0)} ms + scrittura OPFS ${snap1.writeMs.toFixed(0)} ms`);
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
    at = await page.until('# ', at);

    // Gestore dei file (M8, ADR 0020): il pannello mostra le radici
    // impostate, si aggiorna da solo quando un processo del guest crea un
    // file (inotify), apre un file, lo modifica e lo salva nel guest, che lo
    // rilegge con cat (modo conservato).
    await page.type('mkdir -p /tmp/web && echo prima > /tmp/web/nota.txt && chmod 640 /tmp/web/nota.txt');
    at = await page.until('# ', at);
    const files = (expr) => page.eval(`window.vetroFiles.state()${expr}`);
    await page.waitFor('gestore dei file collegato', async () => (await files('.state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/web'])");
    await page.waitFor('nota.txt nell\'albero', async () => (await files('.shown')).includes('/tmp/web/nota.txt'), 30_000);
    await page.type('echo dal-guest > /tmp/web/nuovo.txt');
    at = await page.until('# ', at);
    await page.waitFor('nuovo.txt comparso da solo', async () => (await files('.shown')).includes('/tmp/web/nuovo.txt'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/nota.txt"]').click()`);
    await page.waitFor('nota.txt aperto', async () => (await page.eval("document.querySelector('#files-content textarea')?.value")) === 'prima\n', 30_000);
    await page.eval(`(() => {
      const a = document.querySelector('#files-content textarea');
      a.value = 'modificato dal pannello\\n';
      a.dispatchEvent(new Event('input'));
      document.getElementById('files-save').click();
    })()`);
    await page.waitFor('salvataggio nel guest', async () => (await files('.message')).startsWith('salvato nel guest'), 30_000);
    await page.type("cat /tmp/web/nota.txt; stat -c 'modo-%a' /tmp/web/nota.txt");
    at = await page.until('modificato dal pannello', at);
    at = await page.until('modo-640', at);
    at = await page.until('# ', at);
    console.log('gestore dei file: albero aggiornato dal vivo, file modificato nel pannello e riletto dal guest con cat (modo 640 conservato)');

    // Modifica di una cella SQLite dal pannello (ADR 0021): database in WAL
    // tenuto aperto da un processo del guest; anteprima della query, SQL
    // eseguito nel guest, tabella riletta con il -wal, valore riletto dal
    // guest con sqlite3.
    await page.type(`sqlite3 -batch -list /tmp/web/app.db "PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'prima'), (2, 'altra');"`);
    at = await page.until('wal', at);
    at = await page.until('# ', at);
    await page.type("(echo 'SELECT 1 FROM t;'; sleep 100000) | sqlite3 /tmp/web/app.db >/dev/null &");
    at = await page.until('# ', at);
    await page.waitFor('app.db nell\'albero', async () => (await files('.shown')).includes('/tmp/web/app.db-shm'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/app.db"]').click()`);
    const cell = (r, c) => `document.querySelector('#files-content td[data-r="${r}"][data-c="${c}"]')`;
    await page.waitFor('tabella SQLite', async () => (await page.eval(`${cell(0, 1)}?.textContent`)) === 'prima', 30_000);
    await page.eval(`${cell(0, 1)}.click()`);
    await page.eval(`(() => {
      const v = document.querySelector('#files-content .fsql-value');
      v.value = 'dal pannello';
      document.querySelector('#files-content .fsql-preview').click();
    })()`);
    const query = await page.eval("document.querySelector('#files-content .fsql-query')?.value");
    check(query === 'UPDATE "t" SET "v" = ?1 WHERE rowid = ?2', `anteprima della query: ${query}`);
    const params = await page.eval("document.querySelector('#files-content .fsql-params')?.textContent");
    check(params === "?1 = 'dal pannello'   ?2 = 1", `parametri dell'anteprima: ${params}`);
    await page.eval("document.querySelector('#files-content .fsql-run').click()");
    await page.waitFor('SQL eseguito nel guest', async () => (await files('.message')).startsWith('eseguito nel guest: 1 righe cambiate'), 30_000)
      .catch(async (e) => {
        throw new Fail(`${e.message}: ${await files('.message')}`);
      });
    await page.waitFor('cella riletta con il WAL', async () => (await page.eval(`${cell(0, 1)}?.textContent`)) === 'dal pannello', 30_000);
    const walNote = await page.eval("document.querySelector('#files-content .fsql .fnote')?.textContent");
    check(/WAL: \d+ frame applicati/.test(walNote), `la tabella non viene dal WAL: ${walNote}`);
    await page.type("sqlite3 -batch -list /tmp/web/app.db 'SELECT v FROM t WHERE id = 1'");
    at = await page.until('dal pannello', at);
    at = await page.until('# ', at);
    console.log('gestore dei file: cella SQLite modificata dal pannello (anteprima, SQL nel guest su un database in WAL aperto), tabella riletta dal -wal, valore riletto dal guest con sqlite3');

    // SharedPreferences: un valore cambiato nella tabella, XML riscritto come Android e salvato.
    await page.type(`printf '%s\\n' "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>" '<map>' '    <int name="avvii" value="3" />' '</map>' > /tmp/web/prefs.xml`);
    at = await page.until('# ', at);
    await page.waitFor('prefs.xml nell\'albero', async () => (await files('.shown')).includes('/tmp/web/prefs.xml'), 30_000);
    await page.eval(`document.querySelector('[data-path="/tmp/web/prefs.xml"]').click()`);
    await page.waitFor('tabella delle SharedPreferences', async () => (await page.eval("document.querySelector('#files-content .fpref-value')?.value")) === '3', 30_000);
    await page.eval(`(() => {
      const v = document.querySelector('#files-content .fpref-value');
      v.value = '42';
      v.dispatchEvent(new Event('change'));
      document.getElementById('files-save').click();
    })()`);
    await page.waitFor('SharedPreferences salvate', async () => (await files('.message')).startsWith('salvato nel guest'), 30_000);
    await page.type("grep -c 'value=\"42\"' /tmp/web/prefs.xml | sed 's/^/conteggio-/'");
    at = await page.until('conteggio-1', at);
    at = await page.until('# ', at);
    console.log('gestore dei file: SharedPreferences cambiate nella tabella, XML riscritto e riletto dal guest');
    await page.type(`printf ${TEXT} | dd of=/dev/vda bs=1 seek=${WRITE_AT} conv=notrunc 2>/dev/null; sync`);
    at = await page.until('# ', at);
    const snap2 = await page.waitFor('snapshot dopo la scrittura', async () => (await page.state()).snapshots.find((x) => x.why === 'dischi cambiati'), 60_000);
    check(snap2.generations[0] > snap1.generations[0], `generazione dell'overlay non cresciuta: ${JSON.stringify([snap1, snap2])}`);
    console.log(`scrittura sul disco: snapshot risalvato con l'overlay alla generazione ${snap2.generations[0]}`);
    const first = ranges.length;
    await cdp.send('Target.closeTarget', { targetId });

    // Seconda sessione senza snapshot: avvio da zero, disco da OPFS.
    ({ page, targetId } = await openPage(cdp, `${url}&snapshot=0`));
    at = await page.until('# ');
    check((await page.state()).boot?.mode === 'cold', 'con snapshot=0 si avvia da zero');
    await page.type(READ);
    at = await page.until(`${md5After}  /dev/vda`, at);
    const text2 = await page.consoleText();
    check(text2.includes(`L-${TEXT}-F`), `scrittura della prima sessione persa:\n${text2.split('\n').slice(-8).join('\n')}`);
    const second = ranges.length - first;
    const status = await page.eval("document.getElementById('status').textContent");
    check(first > 1, `primo avvio: ${first} richieste al disco, attese letture Range`);
    check(second === 1, `riavvio: ${second} richieste al disco (attesa 1, la dimensione): OPFS non usato? stato: ${status}`);
    console.log(`seconda sessione (da zero): la scrittura della prima c'è (overlay in OPFS, md5 ${md5After}); ` +
      `richieste Range: primo avvio ${first}, riavvio ${second} (solo la dimensione)`);
    await cdp.send('Target.closeTarget', { targetId });

    // Terza sessione: dallo snapshot.
    const before3 = ranges.length;
    t0 = Date.now();
    ({ page, targetId } = await openPage(cdp, url));
    const boot3 = await page.waitFor('ripristino dallo snapshot', async () => (await page.state())?.boot, 60_000);
    const restoredMs = Date.now() - t0;
    check(boot3.mode === 'snapshot', `terza sessione: attesa dallo snapshot, ${JSON.stringify(boot3)}`);
    at = (await page.consoleText()).length;
    check((await page.consoleText()).includes('# '), 'la console dello snapshot non è stata rimostrata');
    await page.type(READ);
    at = await page.until(`${md5After}  /dev/vda`, at);
    const answeredMs = Date.now() - t0;
    check((await page.consoleText()).slice(-400).includes(`L-${TEXT}-F`), 'dopo il ripristino la scrittura non c\'è');
    // Il gestore dei file si ricollega al demone dello snapshot (la
    // connessione della prima sessione, rimasta nello snapshot, si chiude).
    await page.waitFor('gestore dei file dopo il ripristino', async () => (await page.eval('window.vetroFiles.state().state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/web'])");
    await page.waitFor('albero dopo il ripristino', async () => (await page.eval('window.vetroFiles.state().shown')).includes('/tmp/web/nota.txt'), 30_000);
    const t = boot3.times;
    console.log(`terza sessione (snapshot): pronta in ${(restoredMs / 1000).toFixed(2)} s dall'apertura della pagina ` +
      `(nel Worker: wasm ${t.wasm.toFixed(0)} ms, kernel e initramfs ${t.files.toFixed(0)} ms, chiave ${t.key.toFixed(0)} ms, ` +
      `lettura OPFS ${t.read.toFixed(0)} ms, ripristino ${t.restore.toFixed(0)} ms; snapshot ${(boot3.size / 2 ** 20).toFixed(1)} MiB), ` +
      `primo comando eseguito a ${(answeredMs / 1000).toFixed(2)} s; primo avvio da zero ${(coldMs / 1000).toFixed(2)} s; ` +
      `richieste Range ${ranges.length - before3}`);
    console.log('app nel browser: ok');
  } finally {
    await srv.close();
    await closeChrome(proc, cdp, profile);
  }
});
