#!/usr/bin/env node
// M7, ADR 0046: the guest online through the network gateway, everything in
// Node as the app's Worker does it: vetro-wasm (relayTake/relayPush), the
// page's GatewayClient (web/node/relay.mjs), the Node gateway
// (relay/src/node.mjs) over real WebSockets, real TCP to a local HTTP
// server. The gateway's socket factory maps a public test address to the
// local server, so the destination policy still applies (a private
// address is refused).
//
// In the guest (BusyBox): nslookup through the gateway, a short GET and a
// 600 KB GET (beyond the window) with `cksum`, a GET to a private address
// (refused by the gateway: wget fails), then the counters of both sides.
//
//   node tests/web/online.mjs [--no-jit]     (needs `npm ci --prefix relay`)

import { createServer } from 'node:http';
import { check, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';
import { down, GatewayClient, online } from '../../web/node/relay.mjs';
import { nodeSocket, startGateway } from '../../relay/src/node.mjs';

const jit = !process.argv.includes('--no-jit');
const ORIGIN = 'http://127.0.0.1:8080';
const PUBLIC = '93.184.215.14';

function big() {
  const out = Buffer.alloc(600_000);
  for (let i = 0; i < out.length; i++) out[i] = (Math.imul(i, 2246822519) >>> 11) & 0xff;
  return out;
}

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

const tick = () => new Promise((ok) => setImmediate(ok));
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

/** A Session whose quanta let the event loop run and carry the gateway's frames. */
class OnlineSession extends Session {
  constructor(x, kernel, client) {
    super(x, kernel, { jit });
    this.client = client;
    this.pushed = 0;
  }

  pump() {
    const out = this.m.relayTake();
    if (out.length) this.client.fromMachine(out);
    for (const f of this.client.take()) {
      check(this.m.relayPush(f) === 'Done', 'a gateway frame rejected');
      this.pushed++;
    }
  }

  async quantum() {
    this.pump();
    await tick();
    // While the guest waits on the gateway, let the real network catch up
    // instead of letting guest time run ahead.
    const st = this.m.relayState();
    if (st && (st.connections > 0 || st.outbox > 0)) await sleep(1);
    this.pump();
    return super.quantum();
  }
}

run(async () => {
  const body = big();
  const http = createServer((req, res) => {
    const data = req.url === '/big' ? body : Buffer.from('VETRO-ONLINE-OK\n');
    res.writeHead(200, { 'content-length': data.length, connection: 'close' });
    res.end(data);
  });
  await new Promise((ok) => http.listen(0, '127.0.0.1', ok));
  const local = http.address().port;
  const gw = await startGateway({
    port: 0,
    vars: { TOKEN_SECRET: 'o'.repeat(48), ALLOWED_ORIGINS: ORIGIN },
    connect: ({ hostname, port }) => nodeSocket(hostname === PUBLIC && port === 80 ? { hostname: '127.0.0.1', port: local } : { hostname, port }),
    // Every *.example name is the public test address.
    resolver: async (q) => Uint8Array.from([0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, ...q.subarray(12), 0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, ...PUBLIC.split('.').map(Number)]),
    log: () => {},
  });
  const client = new GatewayClient(gw.url, { origin: ORIGIN });
  try {
    const { exports } = await loadVetro();
    const s = new OnlineSession(exports, guestKernel(), client);
    const m = s.m;
    check(m.relayPush(down()) === 'Done' && m.relayPush(online(true)) === 'Done', 'online');
    check(m.relayState().online, 'the uplink is online');
    let at = await s.until(SHELL_PROMPT);
    [, at] = await s.command('udhcpc -i eth0 -n -q', at);
    let from = at;
    [, at] = await s.command('nslookup vetro.example', at);
    check(s.text(from, at).includes(`Address: ${PUBLIC}`), `nslookup through the gateway:\n${s.tail()}`);
    from = at;
    [, at] = await s.command('echo "G"ET=$(wget -q -O - http://vetro.example/hello)', at);
    check(s.text(from, at).includes('GET=VETRO-ONLINE-OK'), `GET:\n${s.tail()}`);
    from = at;
    [, at] = await s.command('echo "C"K=$(wget -q -O - http://big.example/big | cksum)', at);
    check(s.text(from, at).includes(`CK=${cksum(body)}`), `600 KB GET (${cksum(body)}):\n${s.tail()}`);
    from = at;
    [, at] = await s.command('wget -q -T 20 -O - http://10.0.0.1/ ; echo "R"C=$?', at);
    check(s.text(from, at).includes('RC=1'), `a private address is refused:\n${s.tail()}`);
    [, at] = await s.command('sleep 5', at);
    const st = m.relayState();
    check(st.connects === 3 && st.refused === 1 && st.connections === 0, `uplink counters ${JSON.stringify(st)}`);
    const cs = client.status();
    check(cs.state === 'online' && cs.lanesOpened === 1 && cs.conns === 0, `client ${JSON.stringify(cs)}`);
    await s.poweroff(at);
    console.log(`online: DNS, 600 KB GET and a refusal through the Node gateway; ${s.pushed} gateway frames, ${m.steps} instructions`);
  } finally {
    client.close();
    await gw.close();
    http.close();
  }
});
