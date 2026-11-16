#!/usr/bin/env node
// The Python gateway (relay/py, ADR 0050) over real WebSockets and TCP, with
// the page's client (web/node/relay.mjs) in front of it: the scenarios of
// relay/test/node.mjs against `python3 relay/py/connect.py` as a child
// process. The "remote servers" are local: the gateway's TEST ONLY connect
// map sends two public test addresses to them, after the destination
// policy, which still sees the address the guest asked for. Two private
// addresses are in the map as well: if the policy came after the map (or
// not at all) they would connect instead of being refused.
//
//   node relay/test/python.mjs          (VETRO_PYTHON = another interpreter)

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createServer } from 'node:net';
import { fileURLToPath } from 'node:url';
import { startPythonGateway } from './python-gateway.mjs';
import { decode, encode, MAX_DATA, WINDOW } from '../src/protocol.mjs';
import { answerAddrs, buildQuery } from '../src/dns.mjs';
import { issue, verify } from '../src/token.mjs';
import { GatewayClient } from '../../web/node/relay.mjs';

const ORIGIN = 'http://127.0.0.1:8080';
const tick = () => new Promise((ok) => setTimeout(ok, 5));
async function until(cond, what, ms = 10_000) {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await tick();
  }
}

let accepted = 0;
function listen(onConn) {
  const s = createServer({ allowHalfOpen: true }, (c) => {
    accepted++;
    c.on('error', () => {});
    onConn(c);
  });
  return new Promise((ok) => s.listen(0, '127.0.0.1', () => ok(s)));
}

// Echo (half close kept: the server ends after the client's FIN), and a
// server that greets and closes.
const echo = await listen((c) => c.pipe(c));
const hello = await listen((c) => c.end('HTTP/1.0 200 OK\r\n\r\nhi'));
const toEcho = `127.0.0.1:${echo.address().port}`;
const gw = await startPythonGateway({
  origins: [ORIGIN],
  connectMap: {
    '93.184.215.14:443': toEcho,
    '93.184.215.15:80': `127.0.0.1:${hello.address().port}`,
    // Never reached: the policy refuses these before the map is looked at.
    '10.0.0.1:443': toEcho,
    '127.0.0.1:80': toEcho,
    '192.168.1.10:443': toEcho,
  },
  hosts: {
    'vetro.example': '93.184.215.14',
    'rebind.example': ['93.184.215.14', '127.0.0.1'],
    'lan.example': '192.168.1.10',
    'mixed.example': ['192.168.1.10', '93.184.215.14'],
  },
  env: { LANE_CONNS: '2', LIMIT_LANES: '3' },
});
after(async () => {
  await gw.close();
  echo.close();
  hello.close();
});

/**
 * The machine's side, played by the test `t`: frames out, frames in. The
 * client is closed when the test ends (a failed test would otherwise keep
 * the process alive with its timers and sockets).
 */
function machine(t, origin = ORIGIN) {
  const c = new GatewayClient(gw.url, { origin });
  t.after(() => c.close());
  const got = [];
  const pull = () => {
    for (const f of c.take()) got.push(decode(f));
    return got;
  };
  /** Sends a CONNECT and waits for its answer: 'connected' or the refusal's reason. */
  const connect = async (frame) => {
    const { id } = decode(frame);
    c.fromMachine([frame]);
    const answer = (f) => f.id === id && (f.type === 'connected' || f.type === 'refused');
    await until(() => pull().some(answer), `an answer for connection ${id}`);
    const f = got.find(answer);
    return f.type === 'refused' ? f.reason : f.type;
  };
  return { c, got, pull, connect, send: (...frames) => c.fromMachine(frames) };
}

/** Everything the earlier tests opened has been released at the gateway. */
const released = () =>
  until(async () => {
    const c = await gw.counters();
    return c.stats.lanes === 0 && c.stats.conns === 0 && c.sessions.every((s) => s.lanes === 0 && s.conns === 0);
  }, 'released at the gateway');

/** Runs token operations in Python (relay/py/vetro_connect/tokens.py): JSON in, JSON out. */
function pythonTokens(ops) {
  const code = [
    'import json, sys',
    'sys.path.insert(0, sys.argv[1])',
    'from vetro_connect import tokens',
    'out = []',
    'for op in json.load(sys.stdin):',
    '    kind = op.pop("op")',
    '    if kind == "issue":',
    '        out.append(tokens.issue(op.pop("secret"), **op))',
    '    else:',
    '        out.append(tokens.verify(op.pop("secret"), op.pop("token"), origin=op["origin"], now=op["now"], allow_expired=op["allowExpired"]))',
    'print(json.dumps(out))',
  ].join('\n');
  // ASCII only on the pipe, whatever the locale.
  const input = JSON.stringify(ops).replace(/[\u0080-￿]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
  const py = fileURLToPath(new URL('../py', import.meta.url));
  return JSON.parse(execFileSync(process.env.VETRO_PYTHON ?? 'python3', ['-c', code, py], { input, encoding: 'utf8' }));
}

test('tokens: the same as relay/src/token.mjs makes, and each accepts the other\'s', async () => {
  const secret = 'interoperable-secret-'.repeat(2);
  const other = 'another-secret-entirely-'.repeat(2);
  const now = 1_800_000_000;
  const origins = ['https://1vcian.me', 'http://127.0.0.1:8080', 'https://caffè.example'];
  const sessions = origins.map((origin, i) => ({ sid: `c2Vzc2lvbi${i}`, origin, now: now + i, first: now - 100 * i }));
  const fromJs = await Promise.all(sessions.map((s) => issue(secret, s)));
  const fromPython = pythonTokens(sessions.map((s) => ({ op: 'issue', secret, ...s })));
  assert.deepEqual(fromPython, fromJs, 'the same bytes for the same session');
  // The same verdict on each other's tokens: good, another origin, expired,
  // expired but renewing, another secret, tampered.
  const cases = [];
  for (const [i, token] of fromJs.entries()) {
    const { origin } = sessions[i];
    cases.push(
      { secret, token, origin, now: now + 10, allowExpired: false },
      { secret, token, origin: 'https://evil.example', now: now + 10, allowExpired: false },
      { secret, token, origin, now: now + i + 600, allowExpired: false },
      { secret, token, origin, now: now + i + 599, allowExpired: false },
      { secret, token, origin, now: now + 9000, allowExpired: true },
      { secret: other, token, origin, now: now + 10, allowExpired: false },
      { secret, token: token.replace(/\.(.)/, (_, c) => `.${c === 'A' ? 'B' : 'A'}`), origin, now: now + 10, allowExpired: false },
      { secret, token: `${token}.x`, origin, now: now + 10, allowExpired: false },
    );
  }
  const byJs = await Promise.all(cases.map(({ secret: s, token, ...o }) => verify(s, token, o)));
  const byPython = pythonTokens(cases.map((c) => ({ op: 'verify', ...c })));
  assert.deepEqual(byPython, byJs);
  assert.deepEqual(byJs.slice(0, 8).map((p) => p.error ?? p.sid), ['c2Vzc2lvbi0', 'origin', 'expired', 'c2Vzc2lvbi0', 'c2Vzc2lvbi0', 'signature', 'signature', 'malformed']);
});

test('the gateway listens on the loopback address only, and says so', async () => {
  assert.match(gw.url, /^http:\/\/127\.0\.0\.1:\d+$/);
  assert.equal(gw.page, `https://1vcian.me/Vetro/app/?gateway=${gw.url}`);
  const health = await (await fetch(`${gw.url}/health`)).json();
  assert.deepEqual(health, { ok: true, enabled: true });
});

test('echo through the gateway, beyond the window, with half close', async (t) => {
  const m = machine(t);
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
  for (const f of m.got.filter((f) => f.type === 'data')) assert.ok(f.data.length <= MAX_DATA);
  m.send(encode.close(1, false, false));
  m.c.close();
  const { stats } = await gw.counters();
  assert.equal(stats.bytes_to_remote, data.length, 'the gateway counted the bytes out');
  assert.equal(stats.bytes_to_guest, data.length, 'and the bytes back');
});

test('policy refusals before any socket; a server that closes first', async (t) => {
  const before = (await gw.counters()).stats.sockets;
  const reached = accepted;
  const m = machine(t);
  m.send(encode.connect(1, '10.0.0.1', 443), encode.connect(2, '93.184.215.14', 22), encode.connect(3, '127.0.0.1', 80));
  await until(() => m.pull().filter((f) => f.type === 'refused').length === 3, 'refusals');
  assert.deepEqual(m.got.map((f) => f.reason), ['address', 'port', 'address']);
  // Every blocked kind of address a page could aim at the user's network.
  const blocked = ['192.168.1.10', '192.168.1.1', '172.16.0.1', '169.254.169.254', '100.64.0.1', '0.0.0.0', '224.0.0.251', '255.255.255.255', '198.18.0.1'];
  for (const [i, ip] of blocked.entries()) assert.equal(await m.connect(encode.connect(100 + i, ip, 443)), 'address', ip);
  assert.equal((await gw.counters()).stats.sockets, before, 'no socket opened');
  assert.equal(accepted, reached, 'and nothing reached the local servers through the map');
  m.send(encode.connect(4, '93.184.215.15', 80));
  await until(() => m.pull().some((f) => f.type === 'shutdown' && f.id === 4), 'server FIN');
  const text = Buffer.from(m.got.filter((f) => f.type === 'data').flatMap((f) => [...f.data])).toString();
  assert.equal(text, 'HTTP/1.0 200 OK\r\n\r\nhi');
  assert.equal((await gw.counters()).stats.sockets, before + 1);
  m.c.close();
});

test('names: connect by name with every address checked, DNS answers without local addresses', async (t) => {
  const before = (await gw.counters()).stats.sockets;
  const m = machine(t);
  // One at a time: a lane takes two connections here, and the client learns
  // that only with its session.
  assert.equal(await m.connect(encode.connectName(1, 'vetro.example', 443)), 'connected');
  assert.equal(await m.connect(encode.connectName(2, 'rebind.example', 443)), 'address', 'one of its addresses is local');
  assert.equal(await m.connect(encode.connectName(3, 'lan.example', 443)), 'address');
  assert.equal(await m.connect(encode.connectName(4, 'none.example', 443)), 'resolve');
  assert.equal(await m.connect(encode.connectName(5, 'vetro.example', 8080)), 'port');
  assert.equal((await gw.counters()).stats.sockets, before + 1, 'one socket: the name with only public addresses');
  const ask = async (id, name, qid, qtype) => {
    m.send(encode.dns(id, buildQuery(name, qid, qtype)));
    await until(() => m.pull().some((f) => f.type === 'dnsAnswer' && f.id === id), `DNS answer ${name}`);
    return m.got.find((f) => f.type === 'dnsAnswer' && f.id === id).msg;
  };
  const rcode = (a) => a[3] & 0x0f;
  const a = await ask(20, 'vetro.example', 0x4242);
  assert.equal((a[0] << 8) | a[1], 0x4242, 'the query id');
  assert.deepEqual(answerAddrs(a), ['93.184.215.14']);
  // A local address is not handed to the guest; a mixed answer keeps the public one.
  const lan = await ask(21, 'lan.example', 7);
  assert.deepEqual([rcode(lan), answerAddrs(lan)], [0, []]);
  assert.deepEqual(answerAddrs(await ask(22, 'mixed.example', 8)), ['93.184.215.14']);
  // Not type A (AAAA, HTTPS): no records, no error. No such name: the same.
  for (const [id, name, qtype] of [[23, 'vetro.example', 28], [24, 'vetro.example', 65], [25, 'none.example', 1]]) {
    const r = await ask(id, name, 9, qtype);
    assert.deepEqual([rcode(r), answerAddrs(r), (r[6] << 8) | r[7]], [0, [], 0], `${name} type ${qtype}`);
  }
  // A response is not a query: FORMERR.
  const bad = buildQuery('vetro.example', 10);
  bad[2] |= 0x80;
  m.send(encode.dns(26, bad));
  await until(() => m.pull().some((f) => f.type === 'dnsAnswer' && f.id === 26), 'FORMERR');
  assert.equal(rcode(m.got.find((f) => f.id === 26).msg), 1);
  m.c.close();
});

test('DNS, lanes on demand, the session lane limit', async (t) => {
  await released();
  const m = machine(t);
  m.send(encode.dns(9, buildQuery('vetro.example', 0x4242)));
  await until(() => m.pull().some((f) => f.type === 'dnsAnswer'), 'DNS answer');
  const a = m.got.find((f) => f.type === 'dnsAnswer');
  assert.equal((a.msg[0] << 8) | a.msg[1], 0x4242);
  // Two connections per lane, three lanes per session: the seventh is refused locally.
  for (let id = 10; id < 17; id++) m.send(encode.connect(id, '93.184.215.14', 443));
  await until(() => m.pull().filter((f) => f.type === 'connected' || f.type === 'refused').length >= 7, 'answers');
  const byId = Object.fromEntries(m.got.filter((f) => f.id >= 10).map((f) => [f.id, f.type === 'refused' ? f.reason : f.type]));
  assert.deepEqual(byId, { 10: 'connected', 11: 'connected', 12: 'connected', 13: 'connected', 14: 'connected', 15: 'connected', 16: 'limit' });
  assert.equal(m.c.status().lanes, 3);
  const { stats, sessions } = await gw.counters();
  assert.deepEqual([stats.lanes, stats.conns], [3, 6]);
  const ledger = sessions.find((s) => s.lanes === 3);
  assert.equal(ledger.conns, 6);
  // The gateway itself keeps the session's lane limit and each lane's index.
  const session = await (await fetch(`${gw.url}/session`, { method: 'POST', headers: { origin: ORIGIN }, body: '{}' })).json();
  assert.deepEqual([session.lanes, session.laneConns], [3, 2]);
  const upgrade = (query, origin = ORIGIN) =>
    new Promise((ok) => {
      const ws = new WebSocket(`${gw.url.replace('http', 'ws')}/lane?${query}`, { headers: { origin } });
      ws.onopen = () => (ws.close(), ok('open'));
      ws.onerror = () => ok('refused');
    });
  assert.equal(await upgrade(`token=${session.token}&lane=3`), 'refused', 'a lane index beyond the limit');
  assert.equal(await upgrade(`token=${session.token}&lane=0`, 'https://1vcian.me'), 'refused', 'a token of another origin');
  assert.equal(await upgrade(`token=${await issue('s'.repeat(48), { sid: 'x', origin: ORIGIN, now: Math.floor(Date.now() / 1000) })}&lane=0`), 'refused', 'a token signed with another secret');
  assert.equal(await upgrade(`token=${session.token}&lane=0`), 'open', 'and a good one');
  // The gateway's own limits, without the page's client (which keeps them
  // too) in front of it: a lane index is taken once, and a lane carries two
  // connections.
  const raw = new WebSocket(`${gw.url.replace('http', 'ws')}/lane?token=${session.token}&lane=1`, { headers: { origin: ORIGIN } });
  t.after(() => raw.close());
  raw.binaryType = 'arraybuffer';
  const seen = [];
  raw.onmessage = (e) => seen.push(typeof e.data === 'string' ? JSON.parse(e.data) : decode(new Uint8Array(e.data)));
  await until(() => seen.some((x) => x.type === 'hello'), 'hello on a lane opened by hand');
  assert.equal(await upgrade(`token=${session.token}&lane=1`), 'refused', 'a lane index already in use');
  for (const id of [1, 2, 3]) raw.send(encode.connect(id, '93.184.215.14', 443));
  await until(() => seen.filter((x) => x.type === 'connected' || x.type === 'refused').length === 3, 'answers on that lane');
  assert.deepEqual(Object.fromEntries(seen.filter((x) => x.id).map((x) => [x.id, x.reason ?? x.type])), { 1: 'connected', 2: 'connected', 3: 'limit' });
  raw.close();
  m.c.close();
  await released();
});

test('a lane lost resets its connections; the kill switch; a foreign origin', async (t) => {
  t.after(() => gw.setEnabled(true));
  const m = machine(t);
  m.send(encode.connect(1, '93.184.215.14', 443));
  await until(() => m.pull().some((f) => f.type === 'connected'), 'connected');
  assert.deepEqual(await gw.setEnabled(false), { test: 'enabled', enabled: false });
  m.send(encode.data(1, new Uint8Array(10), false));
  await until(() => m.pull().some((f) => f.type === 'close' && f.reset), 'reset after the kill switch');
  assert.equal(m.c.stats.lastBye, 'disabled');
  // While disabled: no session, connections refused locally.
  m.send(encode.connect(2, '93.184.215.14', 443));
  await until(() => m.pull().some((f) => f.id === 2 && f.type === 'refused'), 'refused');
  await gw.setEnabled(true);
  m.c.close();

  const evil = machine(t, 'https://evil.example');
  evil.send(encode.connect(1, '93.184.215.14', 443));
  await until(() => evil.pull().some((f) => f.type === 'refused' && f.reason === 'down'), 'no session for another origin');
  assert.match(evil.c.stats.lastError, /HTTP 403/);
  assert.equal(evil.c.status().state, 'unreachable');
  evil.c.close();
  // Nothing but counters ever came out of the gateway.
  assert.doesNotMatch(gw.stderr(), /93\.184|example|10\.0\.0/);
});
