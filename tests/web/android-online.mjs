#!/usr/bin/env node
// Android online through the network gateway (ADR 0046), in headless
// Chrome, with the local Node gateway (relay/src/node.mjs) on the real
// internet. Long: runs only with VETRO_ANDROID=1 (on the build VM:
// tools/remote/test.sh -- env VETRO_ANDROID=1 node tests/web/android-online.mjs).
//
// 1. The app with `?gateway=` the local gateway: the prebuilt home-screen
//    snapshot (ADR 0031) restored from R2 (VETRO_ANDROID_MANIFEST, default:
//    the app's image; R2's CORS wants port 8080), then online (the uplink
//    says so, the page's client has a lane).
// 2. The browser app (Browser2, the WebView shell) opens
//    https://example.com/: the DNS answer and an HTTPS connection to its
//    address go through the gateway (bytes both ways, more than a TLS
//    handshake), and the page's text "Example Domain" is on the screen
//    (uiautomator). Screenshot target/aosp/online-browser.png.
// 3. A catalog app (VETRO_ONLINE_APP, default `chromium`) installed from the
//    catalog with the card's path and opened on https://www.wikipedia.org/:
//    a real request through the gateway (DNS for the name, an HTTPS
//    connection with bytes both ways). Screenshot target/aosp/online-app.png.
//
// The gateway's per-connection counters here come from the test's own
// socket wrapper (the gateway itself logs only per-lane totals).
// Measurements in target/aosp/online-measurements.json.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { DEFAULT_MANIFEST } from '../../web/node/android.mjs';
import { CATALOG_URL } from '../../web/node/catalog.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { check, Fail, root, run } from './lib.mjs';
import { nodeSocket, startGateway } from '../../relay/src/node.mjs';
import { answerAddrs, dohResolver, parseQuery } from '../../relay/src/dns.mjs';

if (process.env.VETRO_ANDROID !== '1') {
  console.log('SKIP: long Android online test in Chrome (VETRO_ANDROID=1 to run it)');
  process.exit(0);
}

const APP = process.env.VETRO_ONLINE_APP ?? 'chromium';
const out = join(root, 'target/aosp');
const secs = (ms) => (ms / 1000).toFixed(1);
const ANDROID = "window.vetroAndroid ? window.vetroAndroid.state() : { adb: { state: 'none' } }";
const CATALOG = 'window.vetroCatalog ? window.vetroCatalog.state() : { loaded: false }';
const m = { start: new Date().toISOString(), app: APP };

/** The gateway's connections, as the test sees them (name from the DNS answers). */
const conns = [];
const names = new Map();
const dnsSeen = [];

function counted({ hostname, port }) {
  const s = nodeSocket({ hostname, port });
  const rec = { ip: hostname, port, name: names.get(hostname) ?? null, toRemote: 0, toGuest: 0, at: Date.now() };
  conns.push(rec);
  const up = new TransformStream({ transform(c, ctl) { rec.toRemote += c.length; ctl.enqueue(c); } });
  up.readable.pipeTo(s.writable).catch(() => {});
  const readable = s.readable.pipeThrough(new TransformStream({ transform(c, ctl) { rec.toGuest += c.length; ctl.enqueue(c); } }));
  return { ...s, readable, writable: up.writable };
}

const doh = dohResolver();
async function resolver(q) {
  const a = await doh(q);
  const name = parseQuery(q)?.name;
  dnsSeen.push(name);
  for (const ip of answerAddrs(a)) names.set(ip, name);
  return a;
}

/** A waitFor predicate that runs `fn` at most every `ms` (adb commands are guest inputs). */
function every(ms, fn) {
  let last = -Infinity;
  return async () => {
    if (Date.now() - last < ms) return null;
    last = Date.now();
    return fn();
  };
}

const toName = (name, after) => conns.filter((c) => c.port === 443 && c.name === name && c.at >= after);
const shot = async (page, file) => {
  const png = await page.eval("document.getElementById('screen').toDataURL('image/png')");
  writeFileSync(join(out, file), Buffer.from(png.split(',')[1], 'base64'));
};

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  mkdirSync(out, { recursive: true });
  const srv = await serve({ mounts: appMounts(), port: Number(process.env.VETRO_WEB_PORT ?? 8080) });
  const origin = new URL(srv.url).origin;
  const gw = await startGateway({ port: 0, vars: { TOKEN_SECRET: 'a'.repeat(48), ALLOWED_ORIGINS: origin }, connect: counted, resolver, log: (o) => (m.lanes ??= []).push(o) });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST;
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}&catalog=${encodeURIComponent(process.env.VETRO_CATALOG ?? CATALOG_URL)}&gateway=${encodeURIComponent(gw.url)}`;
  const profile = mkdtempSync(join(tmpdir(), 'vetro-online-chrome-'));
  const { proc, cdp } = await launch(chrome, profile);
  let ok = false;
  try {
    const t0 = Date.now();
    const { page } = await openPage(cdp, url);
    const shell = async (cmd) => (await page.eval(`window.vetroAndroid.shell(${JSON.stringify(cmd)})`)).stdout ?? '';
    const boot = await page.waitFor('prebuilt snapshot restored', async () => {
      const st = await page.state();
      const pb = (await page.eval(ANDROID)).prebuilt;
      if (pb?.state === 'failed') throw new Fail(`prebuilt download failed: ${pb.error}`);
      if (pb?.state === 'missing') throw new Fail(`no prebuilt snapshot for this vetro-wasm: ${pb.reason}`);
      if (st?.stopped) throw new Fail(`machine stopped: ${JSON.stringify(st.stopped)}`);
      return st?.boot;
    }, 60 * 60_000);
    check(boot.mode === 'snapshot', `start from ${boot.mode}, not from a snapshot`);
    m.ready_ms = boot.ms;
    const gwState = await page.waitFor('the uplink online', async () => {
      const g = (await page.state())?.stats?.gateway;
      return g?.machine?.online ? g : null;
    }, 60_000);
    console.log(`ready ${secs(boot.ms)} s after opening; uplink online (${JSON.stringify(gwState.machine)})`);
    m.adb_ms = await page.waitFor('adb connected', async () => ((await page.eval(ANDROID)).adb.state === 'ready' ? Date.now() - t0 : null), 20 * 60_000);
    console.log(`adb ready at ${secs(m.adb_ms)} s`);

    // ---- 2. The browser app loads a real HTTPS page --------------------------
    const tb = Date.now();
    await shell('am start -a android.intent.action.VIEW -d https://example.com/ -n org.chromium.webview_shell/.WebViewBrowserActivity');
    const https = await page.waitFor('an HTTPS exchange with example.com through the gateway', async () => {
      const c = toName('example.com', tb).find((x) => x.toGuest > 3000 && x.toRemote > 500);
      return c ? { ...c } : null;
    }, 20 * 60_000);
    m.browser_https = { ...https, ms: Date.now() - tb };
    console.log(`browser: HTTPS to example.com (${https.ip}): ${https.toRemote} bytes out, ${https.toGuest} in, after ${secs(Date.now() - tb)} s`);
    const seen = await page.waitFor('"Example Domain" on the screen', every(15_000, async () => {
      const x = await shell('uiautomator dump /data/local/tmp/ui.xml >/dev/null 2>&1; grep -o "Example Domain" /data/local/tmp/ui.xml | head -n 1');
      return x.includes('Example Domain') ? true : null;
    }), 20 * 60_000);
    check(seen, 'page text');
    m.browser_ms = Date.now() - tb;
    await shot(page, 'online-browser.png');
    console.log(`browser: "Example Domain" shown ${secs(m.browser_ms)} s after the intent (target/aosp/online-browser.png)`);
    check(dnsSeen.includes('example.com'), `DNS for example.com through the gateway: ${dnsSeen.slice(-20)}`);

    // ---- 3. A catalog app makes a real request ----------------------------------
    await page.waitFor('catalog loaded', async () => ((await page.eval(CATALOG)).loaded ? true : null), 120_000);
    await page.eval('window.vetroCatalog.refresh()');
    const ti = Date.now();
    const st = await page.eval(`window.vetroCatalog.install(${JSON.stringify(APP)})`);
    check(st.phase === 'installed', `install of ${APP}: ${JSON.stringify(st)}`);
    m.install_ms = Date.now() - ti;
    console.log(`${APP}: installed in ${secs(m.install_ms)} s`);
    const pkg = (await page.eval(CATALOG)).apps.find((a) => a.id === APP).package;
    const site = 'www.wikipedia.org';
    if (pkg === 'org.chromium.chrome') {
      // No first-run screens (userdebug image: Chromium reads its command-line file).
      await shell("echo '_ --disable-fre --no-default-browser-check --no-first-run' > /data/local/tmp/chrome-command-line; chmod 644 /data/local/tmp/chrome-command-line; am set-debug-app --persistent org.chromium.chrome");
    }
    const ta = Date.now();
    await shell(`am start -a android.intent.action.VIEW -d https://${site}/ ${pkg}`);
    const req = await page.waitFor(`${pkg}: a request to ${site} through the gateway`, async () => {
      const c = toName(site, ta).find((x) => x.toGuest > 3000 && x.toRemote > 500);
      return c ? { ...c } : null;
    }, 60 * 60_000);
    m.app_request = { ...req, ms: Date.now() - ta };
    await shot(page, 'online-app.png');
    console.log(`${APP}: HTTPS to ${site} (${req.ip}): ${req.toRemote} bytes out, ${req.toGuest} in, ${secs(Date.now() - ta)} s after the intent (target/aosp/online-app.png)`);
    const g = (await page.state()).stats.gateway;
    m.gateway = g;
    m.connections = conns.length;
    console.log(`gateway: ${conns.length} connections, client ${JSON.stringify({ state: g.state, lanes: g.lanes, lanesOpened: g.lanesOpened, framesIn: g.framesIn, framesOut: g.framesOut })}, machine ${JSON.stringify(g.machine)}`);
    ok = true;
  } finally {
    m.end = new Date().toISOString();
    m.ports = [...new Set(conns.map((c) => c.port))];
    writeFileSync(join(out, 'online-measurements.json'), JSON.stringify(m, null, 2));
    console.log('measurements: target/aosp/online-measurements.json');
    await Promise.race([cdp.send('Browser.close').catch(() => {}), new Promise((ok2) => setTimeout(ok2, 3000))]);
    cdp.close();
    try {
      process.kill(-proc.pid, 'SIGKILL');
    } catch {}
    await gw.close();
    await srv.close();
    if (ok) rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    else console.log(`Chrome profile kept: ${profile}`);
  }
});
