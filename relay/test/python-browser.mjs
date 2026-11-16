#!/usr/bin/env node
// M7, ADR 0050: the web app online through the Python gateway (relay/py), in
// headless Chrome: the first part of tests/web/browser-online.mjs with
// `python3 relay/py/connect.py` in place of the Node gateway. It is the
// page's own path against the hand-written HTTP and WebSocket server: the
// browser's `fetch` (with its CORS preflight and its Origin) and the
// browser's `WebSocket` (masked frames, the extensions it offers and does
// not get), from the app's Worker.
//
// `/app/?os=linux&autostart=1&gateway=<the Python gateway>`: in the guest
// (BusyBox) a name is resolved through the gateway, a short GET and a 600 KB
// GET (beyond the window) arrive whole, a private address is refused. The
// page's stats, the machine's uplink and the gateway's own counters agree.
//
// The page here is served from the loopback address, so Chrome's permission
// for a public page to reach this computer (`loopback-network`, ADR 0050) is
// not involved. The "remote server" is a local HTTP server: the gateway's
// TEST ONLY connect map sends a public test address to it, after the
// destination policy.
//
// Chrome: VETRO_CHROME, otherwise the usual paths (SKIP without it; with
// VETRO_REQUIRE_BROWSER=1 an error).
//
//   node relay/test/python-browser.mjs

import { mkdtempSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, run } from '../../tests/web/lib.mjs';
import { closeChrome, findChrome, launch, openPage } from '../../tests/web/chrome.mjs';
import { startPythonGateway } from './python-gateway.mjs';

const PUBLIC = '93.184.215.14';

function big() {
  const out = Buffer.alloc(600_000);
  for (let i = 0; i < out.length; i++) out[i] = (Math.imul(i, 2246822519) >>> 11) & 0xff;
  return out;
}

/** POSIX `cksum` of `data`, as BusyBox prints it. */
function cksum(data) {
  let crc = 0;
  const feed = (b) => {
    crc = (crc ^ (b << 24)) >>> 0;
    for (let k = 0; k < 8; k++) crc = crc & 0x80000000 ? ((crc << 1) ^ 0x04c11db7) >>> 0 : (crc << 1) >>> 0;
  };
  for (const b of data) feed(b);
  for (let n = data.length; n; n = Math.floor(n / 256)) feed(n & 0xff);
  return `${(~crc) >>> 0} ${data.length}`;
}

run(async () => {
  const chrome = findChrome();
  if (!chrome) {
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail('Chrome not found (VETRO_CHROME)');
    console.log('SKIP: Chrome not found (VETRO_CHROME)');
    return;
  }
  const body = big();
  let requests = 0;
  const http = createServer((req, res) => {
    requests++;
    const data = req.url === '/big' ? body : Buffer.from('VETRO-ONLINE-OK\n');
    res.writeHead(200, { 'content-length': data.length, connection: 'close' });
    res.end(data);
  });
  await new Promise((ok) => http.listen(0, '127.0.0.1', ok));
  const local = `127.0.0.1:${http.address().port}`;
  const srv = await serve({ mounts: appMounts() });
  const gw = await startPythonGateway({
    origins: [new URL(srv.url).origin],
    connectMap: { [`${PUBLIC}:80`]: local, '10.0.0.1:80': local },
    hosts: { '*': PUBLIC },
  });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-python-'));
  const { proc, cdp } = await launch(chrome, profile);
  const gateway = (page) => page.eval('JSON.stringify(window.vetroState?.stats?.gateway ?? null)').then(JSON.parse);
  try {
    const q = new URLSearchParams({ os: 'linux', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1', gateway: gw.url, snapshot: '0' });
    const { page } = await openPage(cdp, `${srv.url}/app/?${q}`);
    let at = await page.until('# ');
    const run1 = async (cmd, marker) => {
      await page.type(cmd);
      at = await page.until(marker, at);
    };
    await run1('udhcpc -i eth0 -n -q >/dev/null 2>&1; echo VETRO-DHCP-$?', 'VETRO-DHCP-0');
    await run1('echo "N"S=$(nslookup vetro.example | grep -c "Address: 93.184.215.14")', 'NS=1');
    await run1('echo "G"ET=$(wget -q -O - http://vetro.example/hello)', 'GET=VETRO-ONLINE-OK');
    await run1('echo "C"K=$(wget -q -O - http://big.example/big | cksum)', `CK=${cksum(body)}`);
    await run1('wget -q -T 20 -O - http://10.0.0.1/ ; echo "R"C=$?', 'RC=1');
    // The page's stats are posted about once a second.
    const g = await page.waitFor('the uplink counters', async () => {
      const x = await gateway(page);
      return x && x.machine.connects === 3 && x.machine.refused === 1 ? x : null;
    }, 30_000).catch(async (e) => {
      throw new Fail(`${e.message}: ${JSON.stringify(await gateway(page))}`);
    });
    check(g.state !== 'unreachable' && g.lanesOpened >= 1 && g.laneFailures === 0 && g.lastError === null, `the page's gateway client: ${JSON.stringify(g)}`);
    check(g.machine.online && g.bytesIn > 600_000, `bytes through the gateway: ${JSON.stringify(g)}`);
    // The gateway's side of the same session.
    const { stats, sessions } = await gw.counters();
    check(stats.sessions >= 1 && stats.sockets === 2 && requests === 2 && stats.refused === 1 && stats.conns_total === 2 && stats.errors === 0, `the gateway's counters: ${JSON.stringify(stats)}, ${requests} requests at the local server`);
    check(stats.bytes_to_guest >= body.length + 16 && stats.dns >= 1, `bytes and DNS at the gateway: ${JSON.stringify(stats)}`);
    check(sessions.length === 1 && sessions[0].ended === null, `one session: ${JSON.stringify(sessions)}`);
    console.log(`online in Chrome through the Python gateway: DNS, a 600 KB GET and a refusal (${g.lanesOpened} lane, ${g.framesIn} frames in, ${g.bytesIn} bytes in; gateway: ${stats.conns_total} connections, ${stats.bytes_to_guest} bytes to the guest)`);
  } finally {
    await closeChrome(proc, cdp, profile);
    await gw.close();
    await srv.close();
    http.close();
  }
});
