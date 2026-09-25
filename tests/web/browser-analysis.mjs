#!/usr/bin/env node
// M7 e M10: ispettore di rete, timeline e record & replay nell'app web, in
// Chrome headless (protocollo DevTools, come browser.mjs):
//
//   1. il kernel M3 con la rete; dalla console della pagina (tasti veri) il
//      DHCP, poi "Registra" nel pannello Registrazione;
//   2. il gestore dei file osserva /tmp/t; un comando del guest scrive un
//      file: nella timeline l'effetto "scritto /tmp/t/nota.txt" è legato al
//      comando;
//   3. `wget` manda un POST JSON al sinkhole: la richiesta compare
//      nell'ispettore (metodo, host, percorso, stato 200), il dettaglio ha il
//      corpo JSON decodificato, e nella timeline la richiesta è legata al
//      comando digitato;
//   4. "Ferma": keyframe in OPFS; "Scarica log" scarica davvero il file
//      (comportamento dei download di Chrome impostato dal test), che
//      comincia con VETROREC; anche HAR e pcapng si scaricano;
//   5. "Rigioca": replay identico (stesso stato, stesse istruzioni della
//      fine della registrazione) e la richiesta ricompare nell'ispettore;
//   6. "vai qui" sul comando nella timeline: la macchina si ferma a
//      quell'istruzione con registri e memoria (dump a VBAR_EL1);
//      "Continua" finisce il replay, di nuovo identico;
//   7. "Carica log" con il file scaricato, poi "Rigioca": identico.
//
// Senza Chrome dice SKIP (non è un test passato), o fallisce con
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser-analysis.mjs

import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, POST_JSON, root, run } from './lib.mjs';

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome non trovato (VETRO_CHROME): test nel browser non eseguito';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const downloads = join(root, 'target/web-test/downloads');
  rmSync(downloads, { recursive: true, force: true });
  mkdirSync(downloads, { recursive: true });
  const srv = await serve({ mounts: appMounts() });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  try {
    await cdp.send('Browser.setDownloadBehavior', { behavior: 'allow', downloadPath: downloads });
    const q = new URLSearchParams({ cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1', snapshot: '0', persist: '0' });
    const { page } = await openPage(cdp, `${srv.url}/app/?${q}`);
    const state = () => page.eval('window.vetroAnalysis.state()');
    const click = (sel) => page.eval(`document.querySelector(${JSON.stringify(sel)}).click()`);
    let at = await page.until('# ');
    await page.type('udhcpc -i eth0 -n -q; mkdir -p /tmp/t');
    at = await page.until('lease of 10.0.2.15 obtained', at);
    at = await page.until('# ', at);

    // 1. Registrazione (keyframe ogni 20 M istruzioni).
    await click('#tabs [data-tab="replay"]');
    await page.eval("document.getElementById('rr-kf').value = '20'");
    await click('#rr-record');
    await page.waitFor('registrazione partita', async () => (await state()).rr?.status.state === 'Recording', 30_000);

    // 2. Gestore dei file e timeline.
    await page.waitFor('gestore dei file collegato', async () => (await page.eval('window.vetroFiles.state().state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/t'])");
    await page.waitFor('radice osservata', async () => (await page.eval('window.vetroFiles.state().shown')).includes('/tmp/t'), 30_000);
    const FILE_CMD = 'echo ciao > /tmp/t/nota.txt';
    await page.type(FILE_CMD);
    at = await page.until('# ', at);

    // 3. wget verso il sinkhole.
    await page.type(`${POST_JSON}; echo WGET-$((40+2))`);
    at = await page.until('WGET-42', at);
    at = await page.until('# ', at);
    const req = await page.waitFor('richiesta nell\'ispettore', async () =>
      (await state()).requests?.requests.find((r) => r.path === '/v1/eventi'), 30_000);
    check(req.method === 'POST' && req.host === 'api.vetro.test' && req.status === 200 && req.reqKind === 'json', `riga: ${JSON.stringify(req)}`);
    await click('#tabs [data-tab="net"]');
    await click(`#net-table tr[data-i="${req.i}"]`);
    const detail = await page.waitFor('dettaglio della richiesta', () => page.eval('window.vetroAnalysis.detail()'), 30_000);
    check(detail.request.body.json?.vetro === 42 && detail.request.body.json.nome === 'prova', `corpo: ${JSON.stringify(detail.request.body)}`);
    const shown = await page.eval("document.getElementById('net-detail').textContent");
    check(shown.includes('"vetro": 42') && shown.includes('api.vetro.test/v1/eventi'), `dettaglio nella pagina: ${shown.slice(0, 400)}`);
    const tl = await page.waitFor('timeline con la richiesta e il file', async () => {
      const t = (await state()).timeline;
      const http = t?.effects.find((e) => e.kind === 'http' && e.ref === req.i);
      const file = t?.effects.find((e) => e.kind === 'file' && e.label.includes('/tmp/t/nota.txt'));
      return http && file ? { t, http, file } : null;
    }, 30_000);
    const wgetInput = tl.t.inputs[tl.http.cause];
    check(wgetInput?.label.startsWith('Invio: wget') && wgetInput.label.includes('/v1/eventi'), `causa della richiesta: ${JSON.stringify(wgetInput)}`);
    const fileInput = tl.t.inputs[tl.file.cause];
    check(fileInput?.label === `Invio: ${FILE_CMD}`, `causa della scrittura: ${JSON.stringify(fileInput)} (${tl.file.label})`);
    await click('#tabs [data-tab="timeline"]');
    const tlText = await page.eval(`document.querySelector('[data-input="${wgetInput.i}"]')?.textContent ?? ''`);
    check(tlText.includes('POST http://api.vetro.test/v1/eventi → 200'), `timeline nella pagina: ${tlText.slice(0, 300)}`);
    console.log(`ispettore: POST ${req.host}${req.path} → ${req.status}, corpo JSON decodificato; timeline: richiesta legata a ` +
      `"${wgetInput.label.slice(0, 40)}…", scrittura di nota.txt legata al suo comando`);

    // 4. Ferma, keyframe in OPFS, download del log, di HAR e di pcapng.
    await click('#tabs [data-tab="replay"]');
    await click('#rr-record');
    const rec = await page.waitFor('registrazione salvata', async () => {
      const r = (await state()).rr;
      return r?.status.state === 'Idle' && r.info && r.meta ? r : null;
    }, 60_000);
    check(rec.info.events > 10 && rec.meta.keyframes >= 2 && rec.info.sameMachine, `log: ${JSON.stringify(rec.info)} ${JSON.stringify(rec.meta)}`);
    const saved = async (ext) => page.waitFor(`download .${ext}`, async () => {
      const f = readdirSync(downloads).find((n) => n.endsWith(`.${ext}`));
      return f && !existsSync(join(downloads, `${f}.crdownload`)) ? join(downloads, f) : null;
    }, 60_000);
    await click('#rr-download');
    const logFile = await saved('vrec');
    const log = readFileSync(logFile);
    check(log.subarray(0, 8).toString() === 'VETROREC', 'il log scaricato non comincia con VETROREC');
    await click('#tabs [data-tab="net"]');
    await click('#net-har');
    const har = JSON.parse(readFileSync(await saved('har'), 'utf8'));
    check(har.log.entries.some((e) => e.request.url === 'http://api.vetro.test/v1/eventi'), 'HAR scaricato senza la richiesta');
    await click('#net-pcap');
    const pcap = readFileSync(await saved('pcapng'));
    check(pcap.subarray(0, 4).toString('hex') === '0a0d0d0a', 'pcapng scaricato');
    console.log(`registrazione: ${rec.info.events} eventi, ${rec.meta.keyframes} keyframe in OPFS; scaricati log ` +
      `(${(log.length / 2 ** 20).toFixed(1)} MiB), HAR e pcapng (${pcap.length} byte)`);

    // 5. Rigioca: replay identico.
    await click('#tabs [data-tab="replay"]');
    const replay = async (what) => {
      await page.eval('window.vetroAnalysis.state().replayEnded = null');
      await click('#rr-replay');
      const end = await page.waitFor(what, async () => (await state()).replayEnded, 120_000);
      check(end.status.state === 'Finished', `${what}: ${end.status.state} ${end.status.message}`);
      check(end.steps === rec.info.endSteps, `${what}: finito a ${end.steps}, registrazione a ${rec.info.endSteps}`);
      return end;
    };
    await replay('replay dall\'inizio');
    const again = await page.waitFor('richiesta ricatturata nel replay', async () =>
      (await state()).requests?.requests.find((r) => r.path === '/v1/eventi'), 30_000);
    check(again.timings.startedUs === req.timings.startedUs, 'la richiesta del replay è a un altro istante');
    const verdict = await page.eval("document.getElementById('rr-status').textContent");
    check(verdict.includes('replay identico'), `stato nella pagina: ${verdict}`);
    console.log(`replay: identico (${rec.info.endSteps - rec.info.startSteps} istruzioni), la richiesta ricompare allo stesso istante del guest`);

    // 6. Salto al comando wget dalla timeline, registri e memoria.
    await page.eval('window.vetroAnalysis.state().replayEnded = null');
    await click('#tabs [data-tab="timeline"]');
    await page.waitFor('pulsante "vai qui"', () => page.eval(`!!document.querySelector('[data-input] [data-goto="${wgetInput.step}"]')`), 30_000);
    await click(`[data-input] [data-goto="${wgetInput.step}"]`);
    const paused = await page.waitFor('fermo al comando', async () => (await state()).rr?.paused, 120_000);
    check(paused.steps >= wgetInput.step && paused.registers.includes('pc ') && paused.registers.includes('vbar_el1'), `registri: ${JSON.stringify(paused)}`);
    const mem = await page.waitFor('dump della memoria', async () => {
      const t = await page.eval("document.getElementById('rr-mem').textContent");
      return /fisico 0x[0-9a-f]+\n[0-9a-f]{16} {2}([0-9a-f]{2} ){15}[0-9a-f]{2}/.test(t) ? t : null;
    }, 30_000);
    await click('#rr-continue');
    const cont = await page.waitFor('fine del replay continuato', async () => (await state()).replayEnded, 120_000);
    check(cont.status.state === 'Finished' && cont.steps === rec.info.endSteps, `continua: ${JSON.stringify(cont)}`);
    console.log(`salto all'istruzione ${wgetInput.step}: fermo a ${paused.steps}, registri e memoria (${mem.split('\n')[0]}); continua: identico`);

    // 7. Carica il log scaricato e rigioca.
    const { result } = await cdp.send('Runtime.evaluate', { expression: "document.getElementById('rr-load')" }, page.s);
    await cdp.send('DOM.setFileInputFiles', { files: [logFile], objectId: result.objectId }, page.s);
    await page.waitFor('log caricato', async () => (await page.eval("document.getElementById('status').textContent")).startsWith('log caricato'), 60_000);
    await replay('replay del log caricato');
    console.log('log caricato dal file scaricato: replay identico');
    console.log('ispettore, timeline e record & replay nel browser: ok');
  } finally {
    cdp.close();
    const exited = proc.exitCode !== null ? Promise.resolve() : new Promise((ok) => proc.once('exit', ok));
    proc.kill();
    await srv.close();
    await Promise.race([exited, new Promise((ok) => setTimeout(ok, 5000))]);
    rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
});
