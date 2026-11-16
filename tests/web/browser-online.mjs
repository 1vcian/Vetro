#!/usr/bin/env node
// M7, ADR 0048: the web app online through the network gateway, in headless
// Chrome. tests/web/online.mjs drives the same pieces in Node; this is the
// page's own path: the Worker's GatewayClient with the browser's `fetch` and
// `WebSocket` (which set Origin by themselves), the gateway's CORS answer to
// the page's origin, lanes opened from a Worker.
//
//   1. `/app/?os=linux&autostart=1&gateway=<the local Node gateway>`: the
//      status says the network is online; in the guest (BusyBox) a name is
//      resolved through the gateway, a short GET and a 600 KB GET (beyond the
//      window) arrive whole, a private address is refused. The page's stats
//      agree with the machine's uplink: one lane, no lane failure, three
//      connections, one refusal.
//   2. A second session from the snapshot the page saved at the prompt, with
//      the uplink online in it: the Worker drops the old connections (DOWN)
//      and goes online again; a GET arrives.
//   3. `&net=offline`: no gateway client, the sinkhole answers as before.
//
// The "remote server" is a local HTTP server: the gateway's socket factory
// maps a public test address to it, so the destination policy still applies
// and the test needs no internet.
//
// Chrome: VETRO_CHROME, otherwise the usual paths (SKIP without it; with
// VETRO_REQUIRE_BROWSER=1 an error).
//
//   node tests/web/browser-online.mjs     (needs `npm ci --prefix relay`)

import { mkdtempSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { appMounts, serve } from '../../tools/web-serve.mjs';
import { check, Fail, run } from './lib.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';
import { nodeSocket, startGateway } from '../../relay/src/node.mjs';

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
  const http = createServer((req, res) => {
    const data = req.url === '/big' ? body : Buffer.from('VETRO-ONLINE-OK\n');
    res.writeHead(200, { 'content-length': data.length, connection: 'close' });
    res.end(data);
  });
  await new Promise((ok) => http.listen(0, '127.0.0.1', ok));
  const local = http.address().port;
  const srv = await serve({ mounts: appMounts() });
  const origin = new URL(srv.url).origin;
  const gw = await startGateway({
    port: 0,
    vars: { TOKEN_SECRET: 'b'.repeat(48), ALLOWED_ORIGINS: origin },
    connect: ({ hostname, port }) => nodeSocket(hostname === PUBLIC && port === 80 ? { hostname: '127.0.0.1', port: local } : { hostname, port }),
    // Every *.example name is the public test address.
    resolver: async (q) => Uint8Array.from([0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, ...q.subarray(12), 0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, ...PUBLIC.split('.').map(Number)]),
    log: () => {},
  });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-online-'));
  const { proc, cdp } = await launch(chrome, profile);
  const gateway = (page) => page.eval('JSON.stringify(window.vetroState?.stats?.gateway ?? null)').then(JSON.parse);
  /** Waits for the page's stats to satisfy `pred` (they are posted about once a second). */
  const gatewayWhen = (page, what, pred) => page.waitFor(what, async () => {
    const g = await gateway(page);
    return g && pred(g) ? g : null;
  }, 30_000).catch(async (e) => {
    throw new Fail(`${e.message}: ${JSON.stringify(await gateway(page))}`);
  });
  try {
    // ---- 1. Online from a cold boot ---------------------------------------
    const q = new URLSearchParams({ os: 'linux', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1', gateway: gw.url });
    const url = `${srv.url}/app/?${q}`;
    let { page, targetId } = await openPage(cdp, url);
    let at = await page.until('# ');
    const run1 = async (cmd, marker) => {
      await page.type(cmd);
      at = await page.until(marker, at);
    };
    check(!(await page.eval("document.getElementById('net-hint').checkVisibility()")), 'the offline hint is shown with a gateway');
    await run1('udhcpc -i eth0 -n -q >/dev/null 2>&1; echo VETRO-DHCP-$?', 'VETRO-DHCP-0');
    await run1('echo "N"S=$(nslookup vetro.example | grep -c "Address: 93.184.215.14")', 'NS=1');
    await run1('echo "G"ET=$(wget -q -O - http://vetro.example/hello)', 'GET=VETRO-ONLINE-OK');
    await run1('echo "C"K=$(wget -q -O - http://big.example/big | cksum)', `CK=${cksum(body)}`);
    await run1('wget -q -T 20 -O - http://10.0.0.1/ ; echo "R"C=$?', 'RC=1');
    const g = await gatewayWhen(page, 'the uplink counters', (x) => x.machine.connects === 3 && x.machine.refused === 1);
    check(g.state !== 'unreachable' && g.lanesOpened >= 1 && g.laneFailures === 0 && g.lastError === null, `the page's gateway client: ${JSON.stringify(g)}`);
    check(g.machine.online && g.bytesIn > 600_000, `bytes through the gateway: ${JSON.stringify(g)}`);
    console.log(`online in Chrome: DNS, a 600 KB GET and a refusal through the gateway (${g.lanesOpened} lane, ${g.framesIn} frames in, ${g.bytesIn} bytes in)`);

    // ---- 2. From the snapshot saved online --------------------------------
    await page.waitFor('snapshot at the prompt', async () => (await page.state()).snapshots[0], 120_000);
    await cdp.send('Target.closeTarget', { targetId });
    ({ page, targetId } = await openPage(cdp, url));
    const boot = await page.waitFor('restore', async () => (await page.state())?.boot, 120_000);
    check(boot.mode === 'snapshot', `second session from ${boot.mode}`);
    at = 0;
    // The snapshot is the one at the prompt, before the address was asked for.
    await run1('udhcpc -i eth0 -n -q >/dev/null 2>&1; echo VETRO-DHCP-$?', 'VETRO-DHCP-0');
    await run1('echo "G"ET2=$(wget -q -O - http://again.example/hello)', 'GET2=VETRO-ONLINE-OK');
    const g2 = await gatewayWhen(page, 'the uplink after the restore', (x) => x.machine.online && x.lanesOpened >= 1);
    check(g2.laneFailures === 0 && g2.lastError === null, `after the restore: ${JSON.stringify(g2)}`);
    console.log('second session from the snapshot: online again, a GET arrives');
    await cdp.send('Target.closeTarget', { targetId });

    // ---- 3. Offline by request --------------------------------------------
    const off = new URLSearchParams({ os: 'linux', cmdline: 'console=ttyAMA0 vetro.noautotest', autostart: '1', gateway: gw.url, net: 'offline', snapshot: '0' });
    ({ page, targetId } = await openPage(cdp, `${srv.url}/app/?${off}`));
    at = await page.until('# ');
    await run1('udhcpc -i eth0 -n -q >/dev/null 2>&1; echo VETRO-DHCP-$?', 'VETRO-DHCP-0');
    // The sinkhole answers every name with a fake address and every GET with 200.
    await run1('echo "N"S=$(nslookup vetro.example | grep -c "Address: 198.1[89]")', 'NS=1');
    await run1('wget -q -O /dev/null http://vetro.example/hello; echo "R"C=$?', 'RC=0');
    check((await gateway(page)) === null, `&net=offline: a gateway client exists: ${JSON.stringify(await gateway(page))}`);
    console.log('&net=offline: no gateway client, the sinkhole answers');
  } finally {
    await closeChrome(proc, cdp, profile);
    await gw.close();
    await srv.close();
    http.close();
  }
});
