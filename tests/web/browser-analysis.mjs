#!/usr/bin/env node
// M7 and M10: network inspector, timeline and record & replay in the web app, in
// headless Chrome (DevTools protocol, like browser.mjs):
//
//   1. the M3 kernel with the network; from the page console (real keys) the
//      DHCP, then "Record" in the Recording panel;
//   2. the file manager watches /tmp/t; a guest command writes a
//      file: in the timeline the effect "written /tmp/t/nota.txt" is tied to the
//      command;
//   3. `wget` sends a JSON POST to the sinkhole: the request shows up
//      in the inspector (method, host, path, status 200), the detail has the
//      decoded JSON body, and in the timeline the request is tied to the
//      typed command;
//   4. "Stop": keyframes in OPFS; "Download log" really downloads the file
//      (Chrome's download behaviour set by the test), which
//      starts with VETROREC; HAR and pcapng are downloaded too;
//   5. "Replay": identical replay (same state, same instructions as the
//      end of the recording) and the request shows up again in the inspector;
//   6. "go here" on the command in the timeline: the machine stops at
//      that instruction with registers and memory (dump at VBAR_EL1);
//      "Continue" finishes the replay, identical again;
//   7. "Load log" with the downloaded file, then "Replay": identical.
//
// Without Chrome it says SKIP (it is not a passed test), or fails with
// VETRO_REQUIRE_BROWSER=1.
//
//   node tests/web/browser-analysis.mjs

import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, POST_JSON, root, run } from './lib.mjs';

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome not found (VETRO_CHROME): browser test not run';
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

    // 1. Recording (keyframe every 20 M instructions).
    await click('#tabs [data-tab="replay"]');
    await page.eval("document.getElementById('rr-kf').value = '20'");
    await click('#rr-record');
    await page.waitFor('recording started', async () => (await state()).rr?.status.state === 'Recording', 30_000);

    // 2. File manager and timeline.
    await page.waitFor('file manager connected', async () => (await page.eval('window.vetroFiles.state().state')) === 'Ready', 60_000);
    await page.eval("window.vetroFiles.setRoots(['/tmp/t'])");
    await page.waitFor('root watched', async () => (await page.eval('window.vetroFiles.state().shown')).includes('/tmp/t'), 30_000);
    const FILE_CMD = 'echo ciao > /tmp/t/nota.txt';
    await page.type(FILE_CMD);
    at = await page.until('# ', at);

    // 3. wget to the sinkhole.
    await page.type(`${POST_JSON}; echo WGET-$((40+2))`);
    at = await page.until('WGET-42', at);
    at = await page.until('# ', at);
    const req = await page.waitFor('request in the inspector', async () =>
      (await state()).requests?.requests.find((r) => r.path === '/v1/eventi'), 30_000);
    check(req.method === 'POST' && req.host === 'api.vetro.test' && req.status === 200 && req.reqKind === 'json', `riga: ${JSON.stringify(req)}`);
    await click('#tabs [data-tab="net"]');
    await click(`#net-table tr[data-i="${req.i}"]`);
    const detail = await page.waitFor('detail of the request', () => page.eval('window.vetroAnalysis.detail()'), 30_000);
    check(detail.request.body.json?.vetro === 42 && detail.request.body.json.nome === 'prova', `body: ${JSON.stringify(detail.request.body)}`);
    const shown = await page.eval("document.getElementById('net-detail').textContent");
    check(shown.includes('"vetro": 42') && shown.includes('api.vetro.test/v1/eventi'), `detail in the page: ${shown.slice(0, 400)}`);
    const tl = await page.waitFor('timeline with the request and the file', async () => {
      const t = (await state()).timeline;
      const http = t?.effects.find((e) => e.kind === 'http' && e.ref === req.i);
      const file = t?.effects.find((e) => e.kind === 'file' && e.label.includes('/tmp/t/nota.txt'));
      return http && file ? { t, http, file } : null;
    }, 30_000);
    const wgetInput = tl.t.inputs[tl.http.cause];
    check(wgetInput?.label.startsWith('Enter: wget') && wgetInput.label.includes('/v1/eventi'), `cause of the request: ${JSON.stringify(wgetInput)}`);
    const fileInput = tl.t.inputs[tl.file.cause];
    check(fileInput?.label === `Enter: ${FILE_CMD}`, `cause of the write: ${JSON.stringify(fileInput)} (${tl.file.label})`);
    await click('#tabs [data-tab="timeline"]');
    const tlText = await page.eval(`document.querySelector('[data-input="${wgetInput.i}"]')?.textContent ?? ''`);
    check(tlText.includes('POST http://api.vetro.test/v1/eventi → 200'), `timeline in the page: ${tlText.slice(0, 300)}`);
    console.log(`inspector: POST ${req.host}${req.path} → ${req.status}, decoded JSON body; timeline: request tied to ` +
      `"${wgetInput.label.slice(0, 40)}…", write of nota.txt tied to its command`);

    // 4. Stop, keyframes in OPFS, download of the log, of HAR and of pcapng.
    await click('#tabs [data-tab="replay"]');
    await click('#rr-record');
    const rec = await page.waitFor('recording saved', async () => {
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
    check(log.subarray(0, 8).toString() === 'VETROREC', 'the downloaded log does not start with VETROREC');
    await click('#tabs [data-tab="net"]');
    await click('#net-har');
    const har = JSON.parse(readFileSync(await saved('har'), 'utf8'));
    check(har.log.entries.some((e) => e.request.url === 'http://api.vetro.test/v1/eventi'), 'downloaded HAR without the request');
    await click('#net-pcap');
    const pcap = readFileSync(await saved('pcapng'));
    check(pcap.subarray(0, 4).toString('hex') === '0a0d0d0a', 'downloaded pcapng');
    console.log(`recording: ${rec.info.events} events, ${rec.meta.keyframes} keyframes in OPFS; downloaded log ` +
      `(${(log.length / 2 ** 20).toFixed(1)} MiB), HAR and pcapng (${pcap.length} bytes)`);

    // 5. Replay: identical replay.
    await click('#tabs [data-tab="replay"]');
    const replay = async (what) => {
      await page.eval('window.vetroAnalysis.state().replayEnded = null');
      await click('#rr-replay');
      const end = await page.waitFor(what, async () => (await state()).replayEnded, 120_000);
      check(end.status.state === 'Finished', `${what}: ${end.status.state} ${end.status.message}`);
      check(end.steps === rec.info.endSteps, `${what}: finished at ${end.steps}, recording at ${rec.info.endSteps}`);
      return end;
    };
    await replay('replay from the start');
    const again = await page.waitFor('request captured again in the replay', async () =>
      (await state()).requests?.requests.find((r) => r.path === '/v1/eventi'), 30_000);
    check(again.timings.startedUs === req.timings.startedUs, 'the replay request is at another instant');
    const verdict = await page.eval("document.getElementById('rr-status').textContent");
    check(verdict.includes('replay identical'), `state in the page: ${verdict}`);
    console.log(`replay: identical (${rec.info.endSteps - rec.info.startSteps} instructions), the request shows up again at the same guest instant`);

    // 6. Jump to the wget command from the timeline, registers and memory.
    await page.eval('window.vetroAnalysis.state().replayEnded = null');
    await click('#tabs [data-tab="timeline"]');
    await page.waitFor('"go here" button', () => page.eval(`!!document.querySelector('[data-input] [data-goto="${wgetInput.step}"]')`), 30_000);
    await click(`[data-input] [data-goto="${wgetInput.step}"]`);
    const paused = await page.waitFor('stopped at the command', async () => (await state()).rr?.paused, 120_000);
    check(paused.steps >= wgetInput.step && paused.registers.includes('pc ') && paused.registers.includes('vbar_el1'), `registers: ${JSON.stringify(paused)}`);
    const mem = await page.waitFor('memory dump', async () => {
      const t = await page.eval("document.getElementById('rr-mem').textContent");
      return /physical 0x[0-9a-f]+\n[0-9a-f]{16} {2}([0-9a-f]{2} ){15}[0-9a-f]{2}/.test(t) ? t : null;
    }, 30_000);
    await click('#rr-continue');
    const cont = await page.waitFor('end of the continued replay', async () => (await state()).replayEnded, 120_000);
    check(cont.status.state === 'Finished' && cont.steps === rec.info.endSteps, `continue: ${JSON.stringify(cont)}`);
    console.log(`jump to instruction ${wgetInput.step}: stopped at ${paused.steps}, registers and memory (${mem.split('\n')[0]}); continue: identical`);

    // 7. Load the downloaded log and replay it.
    const { result } = await cdp.send('Runtime.evaluate', { expression: "document.getElementById('rr-load')" }, page.s);
    await cdp.send('DOM.setFileInputFiles', { files: [logFile], objectId: result.objectId }, page.s);
    await page.waitFor('log loaded', async () => (await page.eval("document.getElementById('status').textContent")).startsWith('log loaded'), 60_000);
    await replay('replay of the loaded log');
    console.log('log loaded from the downloaded file: identical replay');
    console.log('inspector, timeline and record & replay in the browser: ok');
  } finally {
    await srv.close();
    await closeChrome(proc, cdp, profile);
  }
});
