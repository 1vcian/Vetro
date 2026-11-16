#!/usr/bin/env node
// M7, ADR 0050: the guest online through the Python gateway (relay/py), the
// same run as tests/web/online.mjs with `python3 relay/py/connect.py` in
// place of the Node gateway: vetro-wasm (relayTake/relayPush), the page's
// GatewayClient (web/node/relay.mjs), real WebSockets to the Python child
// process, real TCP to a local HTTP server. The gateway's TEST ONLY connect
// map sends a public test address to the local server after the destination
// policy (a private address is refused), and its TEST ONLY host table makes
// every name that address.
//
// In the guest (BusyBox): nslookup through the gateway, a short GET and a
// 600 KB GET (beyond the window) with `cksum`, a GET to a private address
// (refused by the gateway: wget fails), then the counters of the three
// sides: the machine's uplink, the page's client, the gateway.
//
//   node relay/test/python-online.mjs [--no-jit]
//   (needs target/guest-kernel and the release vetro-wasm, like tests/web/online.mjs)

import { createServer } from 'node:http';
import { check, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from '../../tests/web/lib.mjs';
import { down, GatewayClient, online } from '../../web/node/relay.mjs';
import { startPythonGateway } from './python-gateway.mjs';

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
  let requests = 0;
  const http = createServer((req, res) => {
    requests++;
    const data = req.url === '/big' ? body : Buffer.from('VETRO-ONLINE-OK\n');
    res.writeHead(200, { 'content-length': data.length, connection: 'close' });
    res.end(data);
  });
  await new Promise((ok) => http.listen(0, '127.0.0.1', ok));
  const local = `127.0.0.1:${http.address().port}`;
  const gw = await startPythonGateway({
    origins: [ORIGIN],
    // The private address is in the map too: the policy refuses it first.
    connectMap: { [`${PUBLIC}:80`]: local, '10.0.0.1:80': local },
    // Every name is the public test address.
    hosts: { '*': PUBLIC },
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
    // The gateway's own counters: two sockets (the refused address never got
    // one), the bytes of both responses, nothing left open but the lane.
    const { stats, sessions } = await gw.counters();
    check(stats.sockets === 2 && requests === 2, `sockets ${stats.sockets}, requests at the local server ${requests}`);
    check(stats.refused === 1 && stats.conns_total === 2 && stats.conns === 0 && stats.lanes === 1 && stats.errors === 0, `gateway ${JSON.stringify(stats)}`);
    check(stats.bytes_to_guest >= body.length + 16 && stats.bytes_to_guest < body.length + 2000, `bytes to the guest ${stats.bytes_to_guest}`);
    check(stats.dns >= 1 && stats.sessions === 1, `DNS and sessions ${JSON.stringify(stats)}`);
    check(sessions.length === 1 && sessions[0].lanes === 1 && sessions[0].conns === 0 && sessions[0].ended === null, `ledger ${JSON.stringify(sessions)}`);
    await s.poweroff(at);
    console.log(`online: DNS, 600 KB GET and a refusal through the Python gateway; ${s.pushed} gateway frames, ${m.steps} instructions`);
  } finally {
    client.close();
    await gw.close();
    http.close();
  }
});
