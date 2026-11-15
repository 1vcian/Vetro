#!/usr/bin/env node
// The Node gateway over real WebSockets and TCP, with the page's client
// (web/node/relay.mjs) in front of it (ADR 0048). The "remote servers" are
// local: the gateway's socket factory maps two public test addresses to
// them, so the destination policy still sees public addresses (and
// refuses the private ones before any socket).
//
//   node relay/test/node.mjs

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:net';
import { startGateway, nodeSocket } from '../src/node.mjs';
import { decode, encode, MAX_DATA, WINDOW } from '../src/protocol.mjs';
import { buildQuery, parseQuery } from '../src/dns.mjs';
import { GatewayClient } from '../../web/node/relay.mjs';

const ORIGIN = 'http://127.0.0.1:8080';
const tick = () => new Promise((ok) => setTimeout(ok, 5));
async function until(cond, what, ms = 10_000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await tick();
  }
}

function listen(onConn) {
  const s = createServer({ allowHalfOpen: true }, (c) => {
    c.on('error', () => {});
    onConn(c);
  });
  return new Promise((ok) => s.listen(0, '127.0.0.1', () => ok(s)));
}

// Echo (half close kept: the server ends after the client's FIN), and a
// server that greets and closes.
const echo = await listen((c) => c.pipe(c));
const hello = await listen((c) => c.end('HTTP/1.0 200 OK\r\n\r\nhi'));
const servers = { '93.184.215.14:443': echo.address().port, '93.184.215.15:80': hello.address().port };
const opened = [];
const gw = await startGateway({
  port: 0,
  vars: { TOKEN_SECRET: 's'.repeat(48), ALLOWED_ORIGINS: ORIGIN, LANE_CONNS: '2', LIMIT_LANES: '3' },
  connect: ({ hostname, port }) => {
    opened.push(`${hostname}:${port}`);
    const local = servers[`${hostname}:${port}`];
    return local ? nodeSocket({ hostname: '127.0.0.1', port: local }) : nodeSocket({ hostname, port });
  },
  resolver: async (q) => Uint8Array.from([0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, ...q.subarray(12), 0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14]),
  log: () => {},
});
after(async () => {
  await gw.close();
  echo.close();
  hello.close();
});

/** The machine's side, played by the test: frames out, frames in. */
function machine(origin = ORIGIN) {
  const c = new GatewayClient(gw.url, { origin });
  const got = [];
  const pull = () => {
    for (const f of c.take()) got.push(decode(f));
    return got;
  };
  return { c, got, pull, send: (...frames) => c.fromMachine(frames) };
}

test('echo through the gateway, beyond the window, with half close', async () => {
  const m = machine();
  m.send(encode.connect(1, '93.184.215.14', 443));
  await until(() => m.pull().some((f) => f.type === 'connected' && f.id === 1), 'connected');
  const data = new Uint8Array(700_000).map((_, i) => (i * 31) ^ (i >> 9));
  let sent = 0;
  let credit = WINDOW;
  const back = [];
  let seen = 0;
  let unacked = 0;
  let fin = false;
  const deadline = Date.now() + 20_000;
  while (!m.got.some((f) => f.type === 'shutdown')) {
    assert.ok(Date.now() < deadline, `stuck: sent ${sent}, credit ${credit}, back ${back.length}, ${JSON.stringify(m.c.status())}`);
    while (sent < data.length && credit > 0) {
      const n = Math.min(MAX_DATA, credit, data.length - sent);
      m.send(encode.data(1, data.subarray(sent, sent + n), false));
      sent += n;
      credit -= n;
    }
    if (sent === data.length && !fin) {
      m.send(encode.shutdown(1, false));
      fin = true;
    }
    await tick();
    for (const f of m.pull().slice(seen)) {
      if (f.type === 'ack') credit += f.n;
      if (f.type === 'data') {
        back.push(...f.data);
        unacked += f.data.length;
      }
    }
    seen = m.got.length;
    assert.ok(unacked <= WINDOW, 'the gateway stays within the window');
    if (unacked) {
      m.send(encode.ack(1, unacked, false));
      unacked = 0;
    }
  }
  assert.equal(back.length, data.length);
  assert.deepEqual(Uint8Array.from(back), data);
  m.send(encode.close(1, false, false));
  m.c.close();
});

test('policy refusals before any socket; a server that closes first', async () => {
  const before = opened.length;
  const m = machine();
  m.send(encode.connect(1, '10.0.0.1', 443), encode.connect(2, '93.184.215.14', 22), encode.connect(3, '127.0.0.1', 80));
  await until(() => m.pull().filter((f) => f.type === 'refused').length === 3, 'refusals');
  assert.deepEqual(m.got.map((f) => f.reason), ['address', 'port', 'address']);
  assert.equal(opened.length, before, 'no socket opened');
  m.send(encode.connect(4, '93.184.215.15', 80));
  await until(() => m.pull().some((f) => f.type === 'shutdown' && f.id === 4), 'server FIN');
  const text = Buffer.from(m.got.filter((f) => f.type === 'data').flatMap((f) => [...f.data])).toString();
  assert.equal(text, 'HTTP/1.0 200 OK\r\n\r\nhi');
  m.c.close();
});

test('DNS, lanes on demand, the session lane limit', async () => {
  const m = machine();
  const q = buildQuery('vetro.example', 0x4242);
  m.send(encode.dns(9, q));
  await until(() => m.pull().some((f) => f.type === 'dnsAnswer'), 'DNS answer');
  const a = m.got.find((f) => f.type === 'dnsAnswer');
  assert.equal((a.msg[0] << 8) | a.msg[1], 0x4242);
  assert.equal(parseQuery(q).name, 'vetro.example');
  // Two connections per lane, three lanes per session: the seventh is refused locally.
  for (let id = 10; id < 17; id++) m.send(encode.connect(id, '93.184.215.14', 443));
  await until(() => m.pull().filter((f) => f.type === 'connected' || f.type === 'refused').length >= 7, 'answers');
  const byId = Object.fromEntries(m.got.filter((f) => f.id >= 10).map((f) => [f.id, f.type === 'refused' ? f.reason : f.type]));
  assert.deepEqual(byId, { 10: 'connected', 11: 'connected', 12: 'connected', 13: 'connected', 14: 'connected', 15: 'connected', 16: 'limit' });
  assert.equal(m.c.status().lanes, 3);
  const ledger = [...gw.sessions.values()].find((l) => l.counters().lanes === 3);
  assert.equal(ledger.counters().conns, 6);
  m.c.close();
  await until(() => ledger.counters().lanes === 0 && ledger.counters().conns === 0, 'released at the gateway');
});

test('a lane lost resets its connections; the kill switch; a foreign origin', async () => {
  const m = machine();
  m.send(encode.connect(1, '93.184.215.14', 443));
  await until(() => m.pull().some((f) => f.type === 'connected'), 'connected');
  gw.cfg.enabled = false;
  m.send(encode.data(1, new Uint8Array(10), false));
  await until(() => m.pull().some((f) => f.type === 'close' && f.reset), 'reset after the kill switch');
  assert.equal(m.c.stats.lastBye, 'disabled');
  // While disabled: no session, connections refused locally.
  m.send(encode.connect(2, '93.184.215.14', 443));
  await until(() => m.pull().some((f) => f.id === 2 && f.type === 'refused'), 'refused');
  gw.cfg.enabled = true;
  m.c.close();

  const evil = machine('https://evil.example');
  evil.send(encode.connect(1, '93.184.215.14', 443));
  await until(() => evil.pull().some((f) => f.type === 'refused' && f.reason === 'down'), 'no session for another origin');
  assert.match(evil.c.stats.lastError, /HTTP 403/);
  assert.equal(evil.c.status().state, 'unreachable');
  evil.c.close();
});
