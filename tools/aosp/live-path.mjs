#!/usr/bin/env node
// The owner's path on the app, in headless Chrome, against any URL of the app
// (by default the live site): measures what a user sees (M5/M6).
//
//   node tools/aosp/live-path.mjs [--url URL] [--profile DIR] [--out DIR]
//        [--visit first|second] [--minutes N] [--trace FILE]
//
// One visit: opens the page with the Chrome profile DIR (fresh if it does not
// exist: a first visit downloads the prebuilt snapshot), presses Start, waits
// for the machine to be ready and adb, then follows the path: three taps on
// the home screen, a swipe up to the app drawer, Settings opened from the
// drawer, Home (the swipe up from the bottom edge), a catalog app (Minesweeper)
// installed and opened from the Apps panel, Home. For each action: press to
// the first frame (`vetroState.perf`), press to the screen changing and to it
// settling (canvas sampled every 100 ms), the disk wait and HTTP reads it
// caused (`vetroState.stats`). Every 5 s the page's stats (MIPS, guest time,
// disk wait, HTTP reads and bytes) go to a timeline; the guest's own load
// (`top -b -n1`, /proc/loadavg) is read with the page's adb before and after
// the path (`top` over 10 s). Then the session stays open up to --minutes in total with a tap
// on the home screen every 30 s. Screenshots (canvas PNG) and
// measurements.json go to --out.
//
// --trace FILE: writes the disk blocks the session fetched from the network,
// in order (`stats.diskTrace`, a worker that records it), as a prefetch list
// (docs/specs/guest-image.md, "Prefetch list").

import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { findChrome, launch, openPage } from '../../tests/web/chrome.mjs';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : def;
};
// --jit-profile: the app with ?jitprofile=1 (a build that has it): per action,
// the instruction classes the interpreter and env.simd ran, and the FP state
// that made them miss the JIT's fast paths (ADR 0045).
const JIT_PROFILE = process.argv.includes('--jit-profile');
// --cpu-profile-actions SECS: a Worker CPU profile of the first SECS seconds of
// the app drawer, Settings and the catalog app's opening (ADR 0045).
const CPU_ACTIONS = Number(arg('cpu-profile-actions', 0));
const URL_ = arg('url', 'https://1vcian.me/Vetro/app/?os=android') + (JIT_PROFILE ? '&jitprofile=1' : '');
const PROFILE = arg('profile', '/tmp/vetro-live-profile');
const OUT = arg('out', '/tmp/vetro-live');
const VISIT = arg('visit', existsSync(PROFILE) ? 'second' : 'first');
const MINUTES = Number(arg('minutes', 8));
const TRACE = arg('trace', null);
const CATALOG_APP = arg('app', 'minesweeper');
mkdirSync(OUT, { recursive: true });

const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
const t0 = Date.now();
const secs = () => ((Date.now() - t0) / 1000).toFixed(1);
const log = (s) => console.log(`[${secs()} s] ${s}`);
const result = { url: URL_, visit: VISIT, start: new Date().toISOString(), actions: [], timeline: [], guest: [] };
const save = () => writeFileSync(join(OUT, `measurements-${VISIT}.json`), JSON.stringify(result, null, 2));

const chrome = findChrome();
if (!chrome) throw new Error('Chrome not found (VETRO_CHROME)');
const { proc, cdp } = await launch(chrome, PROFILE);
const { page } = await openPage(cdp, URL_);
const ev = (e) => page.eval(e);

async function shot(name) {
  const url = await ev("document.getElementById('screen').toDataURL('image/png')");
  writeFileSync(join(OUT, `${VISIT}-${name}.png`), Buffer.from(url.split(',')[1], 'base64'));
}

/** Stats now: guest seconds, MIPS, disk wait (s), HTTP reads and MiB. */
async function stats() {
  const s = await ev('window.vetroState?.stats ?? null');
  if (!s) return null;
  const http = s.disks.reduce((a, d) => ({ requests: a.requests + d.http.requests, bytes: a.bytes + d.http.bytes }), { requests: 0, bytes: 0 });
  return { wall: (Date.now() - t0) / 1000, guest: s.guestSecs, steps: s.steps, mips: s.mips, waitS: s.feeder.waitMs / 1000, served: s.feeder.served, fromSource: s.feeder.fromSource,
    fromCache: s.feeder.fromCache, requests: http.requests, mib: http.bytes / 2 ** 20, memory: s.memory / 2 ** 20, aheadMs: s.aheadMs,
    prefetched: s.feeder.prefetched ?? null, jitModules: s.jit?.modules ?? null, jitCompileS: s.jitHost ? s.jitHost.compileMs / 1000 : null,
    // A deferred snapshot save in progress (ADR 0046).
    save: s.save ?? null };
}

let sampling = true;
(async () => {
  while (sampling) {
    const s = await stats().catch(() => null);
    if (s) result.timeline.push(s);
    await sleep(5000);
  }
})();

const SIG = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
  const out = [];
  for (let y = 4; y < c.height; y += 12) for (let x = 4; x < c.width; x += 12) { const o = (y * c.width + x) * 4; out.push((d[o] >> 3) << 10 | (d[o + 1] >> 3) << 5 | d[o + 2] >> 3); }
  return out;
})()`;
const diff = (a, b) => (a && b && a.length === b.length ? a.reduce((n, v, i) => n + (v !== b[i]), 0) / a.length : 1);

/**
 * The JIT profile now (`vetroState.jitProfile`, cumulative, every 5 s), as
 * counts per "section | class": `interp` (interpreter steps), `interp-fp`
 * (their FP state), `simd` (env.simd calls), `simd-fp`.
 */
async function jitProf() {
  if (!JIT_PROFILE) return null;
  const p = await ev('window.vetroState.jitProfile ?? null');
  if (!p) return null;
  const out = {};
  let sec = 'interp';
  for (const line of p.text.split('\n')) {
    if (line.startsWith('env.simd:')) sec = 'simd';
    else if (line.startsWith('FP by state')) sec = sec.startsWith('simd') ? 'simd-fp' : 'interp-fp';
    const m = line.match(/^\s*(\d+)\s+[\d.]+%\s+(.*)$/);
    if (m) out[`${sec} | ${m[2]}`] = Number(m[1]);
    const t = line.match(/instructions with the JIT active: (\d+)/);
    if (t) out[`${sec} | total`] = Number(t[1]);
  }
  return out;
}

/** The `n` largest differences per section between two jitProf() results. */
function profDelta(a, b, n = 40) {
  if (!a || !b) return null;
  const by = {};
  for (const [k, v] of Object.entries(b)) {
    const d = v - (a[k] ?? 0);
    if (d <= 0) continue;
    const [sec, ...rest] = k.split(' | ');
    (by[sec] ??= []).push([rest.join(' | '), d]);
  }
  for (const sec of Object.keys(by)) by[sec] = by[sec].sort((x, y) => y[1] - x[1]).slice(0, n);
  return by;
}

/** Client coordinates of guest pixel (gx, gy). */
async function at(gx, gy) {
  return ev(`(() => { const c = document.getElementById('screen'); c.scrollIntoView({ block: 'center' }); const b = c.getBoundingClientRect();
    return { x: b.left + ${gx} * b.width / c.width, y: b.top + ${gy} * b.height / c.height }; })()`);
}
const mouse = (type, p, buttons = 1) => cdp.send('Input.dispatchMouseEvent', { type, x: p.x, y: p.y, button: 'left', buttons, clickCount: 1 }, page.s);

/**
 * One timed action: `act()` presses; then the canvas is sampled every 100 ms
 * until it has changed (> `minChange` of the grid) and stayed still for
 * `stillMs`, or `limitMs`. Also the disk wait and reads during it.
 */
async function timed(name, act, { minChange = 0.02, stillMs = 2000, limitMs = 90_000, check = null, until = null } = {}) {
  const before = await ev(SIG);
  const s0 = await stats();
  const n0 = await ev('window.vetroState.perf.taps.length');
  const p0 = await jitProf();
  const cpu = CPU_ACTIONS && /drawer|Settings|open (?!app)/.test(name)
    ? profileWorker(name.replace(/\W+/g, '-'), CPU_ACTIONS).catch((e) => log(`no Worker profile: ${e.message}`)) : null;
  const ts = Date.now();
  await act();
  let changed = null;
  let settled = null;
  let last = before;
  let lastChange = ts;
  let done = null;
  for (;;) {
    await sleep(100);
    const now = Date.now();
    const sig = await ev(SIG);
    if (until && done === null && (await ev(until))) done = now - ts;
    if (diff(sig, last) > 0.002) lastChange = now;
    if (changed === null && diff(sig, before) > minChange) changed = now - ts;
    last = sig;
    if (changed !== null && now - lastChange >= stillMs && (!until || done !== null)) {
      settled = lastChange - ts;
      break;
    }
    if (now - ts > limitMs) break;
  }
  if (cpu) await cpu;
  const frame = await ev(`window.vetroState.perf.taps[${n0}]?.frameMs ?? null`);
  const s1 = await stats();
  const a = { name, frameMs: frame, changedMs: changed, doneMs: done, settledMs: settled, diskWaitS: s1 && s0 ? s1.waitS - s0.waitS : null, httpReads: s1 && s0 ? s1.requests - s0.requests : null,
    httpMiB: s1 && s0 ? s1.mib - s0.mib : null, guestS: s1 && s0 ? s1.guest - s0.guest : null, wallS: (Date.now() - ts) / 1000,
    mips: s1 && s0 ? (s1.steps - s0.steps) / 1e6 / ((s1.wall - s0.wall) || 1) : null };
  if (JIT_PROFILE) {
    // The profile is refreshed every 5 s: wait for one after the action.
    await sleep(5500);
    a.jitProfile = profDelta(p0, await jitProf());
  }
  if (check) a.check = await check().catch((e) => `error: ${e.message}`);
  result.actions.push(a);
  log(`${name}: first frame ${frame?.toFixed(0)} ms, changed ${changed} ms${until ? `, done ${done} ms` : ''}, settled ${settled} ms, ${a.mips?.toFixed(1)} MIPS, guest ${a.guestS?.toFixed(1)} s in ${a.wallS.toFixed(1)} s, disk wait ${a.diskWaitS?.toFixed(1)} s, ${a.httpReads} HTTP reads (${a.httpMiB?.toFixed(1)} MiB)${a.check ? `, ${a.check}` : ''}`);
  await shot(name.replace(/\W+/g, '-'));
  save();
  return a;
}

async function tap(gx, gy) {
  const p = await at(gx, gy);
  await mouse('mousePressed', p);
  await sleep(80);
  await mouse('mouseReleased', p, 0);
}

async function swipe(x0, y0, x1, y1, ms = 300) {
  const n = 10;
  await mouse('mousePressed', await at(x0, y0));
  for (let i = 1; i <= n; i++) {
    await sleep(ms / n);
    await mouse('mouseMoved', await at(x0 + (x1 - x0) * i / n, y0 + (y1 - y0) * i / n));
  }
  await mouse('mouseReleased', await at(x1, y1), 0);
}

const sh = async (cmd, ms = 120_000) => {
  const r = await Promise.race([ev(`window.vetroAndroid.shell(${JSON.stringify(cmd)})`), sleep(ms).then(() => ({ stdout: `(timeout ${ms / 1000} s)` }))]);
  return r.stdout ?? '';
};
const focus = async () => (await sh('dumpsys window | grep -m1 mCurrentFocus')).trim();

/**
 * A CPU profile of the machine's Worker for `secs` seconds (a second DevTools
 * client: the page's target, auto-attach to its dedicated Worker, Profiler):
 * the profile in <out>/<visit>-<label>.cpuprofile and the functions with the
 * most self time in the measurements.
 */
async function profileWorker(label, secs) {
  const { readFileSync } = await import('node:fs');
  const [port, path] = readFileSync(join(PROFILE, 'DevToolsActivePort'), 'utf8').trim().split('\n');
  const ws = new WebSocket(`ws://127.0.0.1:${port}${path}`);
  await new Promise((ok, ko) => ((ws.onopen = ok), (ws.onerror = ko)));
  let id = 0;
  const pending = new Map();
  let worker = null;
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) {
      pending.get(m.id)(m);
      pending.delete(m.id);
    } else if (m.method === 'Target.attachedToTarget' && m.params.targetInfo.type === 'worker') worker = m.params.sessionId;
  };
  const send = (method, params = {}, sessionId) => new Promise((ok) => {
    pending.set(++id, ok);
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });
  try {
    const { result: { targetInfos } } = await send('Target.getTargets');
    const target = targetInfos.find((t) => t.type === 'page' && t.url.includes('/app/'));
    const { result: { sessionId } } = await send('Target.attachToTarget', { targetId: target.targetId, flatten: true });
    await send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, sessionId);
    for (let i = 0; i < 50 && !worker; i++) await sleep(100);
    if (!worker) throw new Error('no Worker target');
    await send('Profiler.enable', {}, worker);
    await send('Profiler.setSamplingInterval', { interval: 500 }, worker);
    const s0 = await stats();
    await send('Profiler.start', {}, worker);
    await sleep(secs * 1000);
    const { result: { profile } } = await send('Profiler.stop', {}, worker);
    const s1 = await stats();
    writeFileSync(join(OUT, `${VISIT}-${label}.cpuprofile`), JSON.stringify(profile));
    const self = new Map();
    const dt = (profile.endTime - profile.startTime) / profile.samples.length / 1000;
    const byId = new Map(profile.nodes.map((n) => [n.id, n]));
    for (const sid of profile.samples) {
      const n = byId.get(sid);
      const f = n.callFrame;
      const k = `${f.functionName || '(anonymous)'} ${f.url.split('/').pop()}:${f.lineNumber}`;
      self.set(k, (self.get(k) ?? 0) + dt);
    }
    const total = [...self.values()].reduce((a, b) => a + b, 0);
    const top = [...self.entries()].sort((a, b) => b[1] - a[1]).slice(0, 40).map(([k, ms]) => ({ fn: k, ms: Math.round(ms), pct: +(100 * ms / total).toFixed(1) }));
    result[`profile_${label}`] = { secs, totalMs: Math.round(total), guestS: s1.guest - s0.guest, top };
    log(`Worker profile (${label}, ${secs} s, guest ${(s1.guest - s0.guest).toFixed(1)} s):\n${top.map((t) => `  ${t.pct}% ${t.fn}`).join('\n')}`);
    save();
  } finally {
    ws.close();
  }
}

async function guestLoad(label) {
  const ts = Date.now();
  const load = (await sh('cat /proc/loadavg; top -b -d 10 -n 2 -m 12 -s 9 | tail -n 18')).trim();
  result.guest.push({ label, wall: (Date.now() - t0) / 1000, adbMs: Date.now() - ts, out: load });
  log(`guest load (${label}, adb ${Date.now() - ts} ms):\n${load}`);
  save();
}

try {
  // The page as the user sees it: the zero-choice app starts by itself; the
  // older page (and ?advanced=1) has a setup form with a Start button.
  const tStart = Date.now();
  await page.waitFor('page loaded', () => ev("document.readyState === 'complete' && document.querySelector('select[name=profile]')?.options.length > 0"), 60_000);
  await sleep(1000);
  result.form = await ev(`(() => { const f = document.getElementById('setup'); const e = f.elements;
    return { hidden: f.hidden, os: e.os?.value, profile: e.profile?.value, version: e.androidVersion?.value, width: e.width?.value, height: e.height?.value, ram: e.ramMiB?.value }; })()`);
  log(`form: ${JSON.stringify(result.form)}`);
  if (!result.form.hidden && (await ev("document.getElementById('machine').hidden"))) await ev("document.querySelector('#setup button[type=submit]').click()");
  const boot = await page.waitFor('ready', async () => {
    const st = await ev('window.vetroState');
    if (st?.stopped) throw new Error(`machine stopped: ${JSON.stringify(st.stopped)}`);
    return st?.boot;
  }, 60 * 60_000);
  const pb = await ev('window.vetroAndroid.state().prebuilt ?? null');
  result.ready = { ms: Date.now() - tStart, boot, prebuilt: pb };
  log(`ready after ${((Date.now() - tStart) / 1000).toFixed(1)} s (${boot.mode}, prebuilt ${boot.prebuilt}${pb?.ms ? `, download ${(pb.ms / 1000).toFixed(1)} s` : ''})`);
  const frame = await page.waitFor('first frame', () => ev('window.vetroState.firstFrame'), 120_000);
  result.firstFrameMs = frame;
  await sleep(3000);
  await shot('ready');
  const adbT = Date.now();
  await page.waitFor('adb', async () => (await ev('window.vetroAndroid.state().adb.state')) === 'ready', 15 * 60_000);
  result.adbMs = Date.now() - tStart;
  log(`adb ready ${((Date.now() - tStart) / 1000).toFixed(1)} s after Start (waited ${((Date.now() - adbT) / 1000).toFixed(1)} s)`);
  const W = Number((await ev("document.getElementById('screen').width")));
  const H = Number((await ev("document.getElementById('screen').height")));
  result.screen = { W, H };
  await guestLoad('after ready');
  // --setup CMD: an adb shell command before the path (an A/B of a guest setting).
  if (arg('setup', null)) log(`setup: ${arg('setup')}: ${(await sh(arg('setup'))).trim()}`);
  if (arg('cpu-profile', null)) await profileWorker('idle-home', Number(arg('cpu-profile')));
  // --probe FILE: adb shell commands (one per line) run now, outputs in <out>/<visit>-probe.txt.
  if (arg('probe', null)) {
    const { readFileSync } = await import('node:fs');
    let text = '';
    for (const cmd of readFileSync(arg('probe'), 'utf8').split('\n').filter((l) => l.trim() && !l.startsWith('#'))) {
      const tc = Date.now();
      text += `### ${cmd}\n${await sh(cmd, 300_000)}\n### (${Date.now() - tc} ms)\n`;
    }
    writeFileSync(join(OUT, `${VISIT}-probe.txt`), text);
    log(`probe: ${join(OUT, `${VISIT}-probe.txt`)}`);
    if (process.argv.includes('--probe-only')) throw Object.assign(new Error('probe only'), { done: true });
  }
  log(`focus: ${await focus()}`);

  // 1. Taps on an empty part of the home screen.
  for (let i = 0; i < 3; i++) await timed(`home tap ${i + 1}`, () => tap(W * 0.5, H * 0.45), { minChange: 0, stillMs: 1000, limitMs: 15_000 });
  // 2. The app drawer: a swipe up from the bottom of the home screen. Each
  // step waits (up to 4 min) for its outcome on the screen before the next:
  // the drawer's light background or the home screen's dark teal wallpaper
  // below the middle (Launcher3, the image's wallpaper), Settings covering most of
  // the drawer.
  const LIGHT = `(() => { const c = document.getElementById('screen'); const d = c.getContext('2d').getImageData(c.width >> 1, (c.height * 3) >> 2, 1, 1).data; return (d[0] + d[1] + d[2]) / 3 > 180; })()`;
  const DARK = `(() => { const c = document.getElementById('screen'); const d = c.getContext('2d').getImageData(c.width >> 1, (c.height * 3) >> 2, 1, 1).data; return (d[0] + d[1] + d[2]) / 3 < 130 && d[1] > d[0] + 20 && Math.abs(d[1] - d[2]) < 40; })()`;
  const LONG = 240_000;
  await timed('open app drawer', () => swipe(W * 0.5, H * 0.85, W * 0.5, H * 0.25), { until: LIGHT, limitMs: LONG });
  // 3. Settings from the drawer: its icon's place in Launcher3's grid
  // (--settings X,Y in guest pixels; the light profile's 960x600 by default).
  const [sx, sy] = (arg('settings', '') || `${W * 0.1},${H * 0.473}`).split(',').map(Number);
  await timed('open Settings', () => tap(sx, sy), { minChange: 0.3, limitMs: LONG, check: focus });
  // Home (the user's gesture: swipe up from the bottom edge).
  await timed('home (swipe up)', () => swipe(W * 0.5, H - 2, W * 0.5, H * 0.5, 200), { until: DARK, limitMs: LONG });
  // 4. A catalog app from the Apps panel: Install, then Open.
  const sel = `#catalog-list li[data-app=${CATALOG_APP}] button`;
  await page.waitFor('catalog card', () => ev(`!!document.querySelector(${JSON.stringify(sel)})`), 120_000);
  const ti = Date.now();
  await ev(`document.querySelector(${JSON.stringify(sel)}).click()`);
  await page.waitFor('catalog app installed', async () => (await ev(`document.querySelector(${JSON.stringify(sel)}).textContent`)) === 'Open', 15 * 60_000);
  result.install = { app: CATALOG_APP, ms: Date.now() - ti };
  log(`${CATALOG_APP} installed in ${((Date.now() - ti) / 1000).toFixed(1)} s`);
  await timed(`open ${CATALOG_APP}`, () => ev(`document.querySelector(${JSON.stringify(sel)}).click()`), { minChange: 0.3, check: focus, limitMs: LONG });
  await timed('home (swipe up) 2', () => swipe(W * 0.5, H - 2, W * 0.5, H * 0.5, 200), { until: DARK, limitMs: LONG });
  await guestLoad('after the path');

  // 5. The rest of the session: a tap every 30 s.
  let k = 0;
  while (Date.now() - t0 < MINUTES * 60_000) {
    await sleep(30_000);
    await timed(`idle tap ${++k}`, () => tap(W * 0.5, H * 0.45), { minChange: 0, stillMs: 1000, limitMs: 15_000 });
  }
  await guestLoad('end');
  result.stats = await ev('window.vetroState.stats');
  // The snapshots saved in this session (why, size, ms on the machine's thread
  // and in the saver, ADR 0046).
  result.snapshots = await ev('window.vetroState.snapshots');
  log(`snapshots: ${JSON.stringify(result.snapshots.map(({ why, size, background, saveMs, writeMs, keptMax, at }) => ({ why, size, background, saveMs, writeMs, keptMax, at })))}`);
  if (JIT_PROFILE) result.jitProfile = await ev('window.vetroState.jitProfile?.text ?? null');
  if (TRACE) {
    const trace = result.stats?.diskTrace;
    if (!trace) log('no disk trace in the stats (a worker that records it is needed)');
    else {
      writeFileSync(TRACE, JSON.stringify(trace));
      log(`disk trace: ${trace.map((t) => t.length).join(', ')} blocks -> ${TRACE}`);
    }
    if (result.stats) delete result.stats.diskTrace;
  }
  result.console = (await ev("document.getElementById('console')?.textContent ?? ''")).slice(-20000);
} catch (e) {
  if (!e.done) {
    result.error = String(e.stack ?? e);
    log(`error: ${e.message ?? e}`);
    await shot('error').catch(() => {});
  }
} finally {
  sampling = false;
  result.end = new Date().toISOString();
  save();
  log(`measurements: ${join(OUT, `measurements-${VISIT}.json`)}`);
  await Promise.race([cdp.send('Browser.close').catch(() => {}), sleep(3000)]);
  cdp.close();
  try {
    process.kill(-proc.pid, 'SIGKILL');
  } catch {}
  process.exit(result.error ? 1 : 0);
}
