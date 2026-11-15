#!/usr/bin/env node
// Unit tests of the network gateway's core (ADR 0048): protocol and the
// vectors shared with Rust, destination policy, tokens, session ledger,
// HTTP side, lanes against fake sockets and a fake resolver.
//
//   node relay/test/unit.mjs

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { decode, encode, HEADER, MAX_DATA, OP, WINDOW, withId, frameId } from '../src/protocol.mjs';
import { addressBlocked, BLOCKED, checkDestination, parseIPv4 } from '../src/policy.mjs';
import { issue, verify, TOKEN_TTL_S, SESSION_MAX_S } from '../src/token.mjs';
import { Ledger, limitsFrom, DEFAULT_LIMITS } from '../src/ledger.mjs';
import { answerAddrs, buildQuery, failAnswer, fitUdp, MAX_UDP_ANSWER, parseQuery } from '../src/dns.mjs';
import { checkLane, configFrom, handleHttp } from '../src/gateway.mjs';
import { GRANT, Lane } from '../src/lane.mjs';

const hex = (b) => Buffer.from(b).toString('hex');
const SECRET = 'x'.repeat(40);
const ORIGIN = 'https://vetro.example';

// ---- Protocol ---------------------------------------------------------------

test('the vectors shared with vetro_net::relay', () => {
  const dnsQ = Uint8Array.from([0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
  const dnsA = Uint8Array.from([0x12, 0x34, 0x81, 0x80, 0, 0, 0, 0, 0, 0, 0, 0]);
  const te = new TextEncoder();
  const build = {
    connect: [encode.connect(7, '93.184.215.14', 443), { type: 'connect', id: 7, ip: '93.184.215.14', port: 443 }],
    connect_name: [encode.connectName(8, 'example.com', 80), { type: 'connectName', id: 8, name: 'example.com', port: 80 }],
    data_machine: [encode.data(7, te.encode('hello'), false), { type: 'data', id: 7 }],
    data_gateway: [encode.data(7, te.encode('world'), true), { type: 'data', id: 7, fromGateway: true }],
    shutdown_machine: [encode.shutdown(7, false), { type: 'shutdown', id: 7 }],
    shutdown_gateway: [encode.shutdown(7, true), { type: 'shutdown', id: 7 }],
    close_machine: [encode.close(7, false, false), { type: 'close', id: 7, reset: false }],
    close_gateway: [encode.close(9, true, true), { type: 'close', id: 9, reset: true }],
    ack_machine: [encode.ack(7, 1000, false), { type: 'ack', id: 7, n: 1000 }],
    ack_gateway: [encode.ack(7, WINDOW, true), { type: 'ack', id: 7, n: WINDOW }],
    dns: [encode.dns(3, dnsQ), { type: 'dns', id: 3 }],
    dns_answer: [encode.dnsAnswer(3, dnsA), { type: 'dnsAnswer', id: 3 }],
    connected: [encode.connected(7), { type: 'connected', id: 7 }],
    refused: [encode.refused(9, 'platform'), { type: 'refused', id: 9, reason: 'platform' }],
    online: [encode.online(true), { type: 'online', on: true }],
    down: [encode.down(), { type: 'down', id: 0 }],
    big_id: [encode.data(0xfffffffe, Uint8Array.of(0xff), false), { type: 'data', id: 0xfffffffe }],
  };
  const lines = readFileSync(new URL('./vectors.txt', import.meta.url), 'utf8').split('\n').filter((l) => l && !l.startsWith('#'));
  assert.equal(lines.length, Object.keys(build).length);
  for (const line of lines) {
    const [name, side, h] = line.split(' ');
    const [bytes, want] = build[name];
    assert.equal(hex(bytes), h, name);
    const f = decode(Buffer.from(h, 'hex'));
    assert.equal(f.fromGateway, side === 'gateway', name);
    for (const [k, v] of Object.entries(want)) assert.deepEqual(f[k], v, `${name}.${k}`);
  }
});

test('malformed frames are rejected', () => {
  for (const bad of [
    [],
    [OP.DATA, 0, 0, 0, 1],
    [OP.CLOSE, 0, 0, 0, 1, 2],
    [OP.REFUSED, 0, 0, 0, 1, 9],
    [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80, 0x41],
    [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80, 0x61, 0x2e, 0x2e, 0x62],
    [OP.ONLINE, 0, 0, 0, 1, 1],
    [0x42, 0, 0, 0, 1],
    [OP.CONNECT, 0, 0, 0, 1, 1, 2, 3, 4, 0],
    [OP.DNS, 0, 0, 0, 1, 1, 2, 3],
  ]) {
    assert.equal(decode(Uint8Array.from(bad)), null, JSON.stringify(bad));
  }
  const big = new Uint8Array(HEADER + MAX_DATA + 1);
  big[0] = OP.DATA;
  assert.equal(decode(big), null);
  const f = withId(encode.connected(5), 77);
  assert.equal(frameId(f), 77);
  assert.equal(frameId(encode.connected(5)), 5, 'withId copies');
});

// ---- Destination policy -----------------------------------------------------

test('private, loopback, link-local, metadata and reserved addresses are rejected', () => {
  const u32ToIp = (v) => [24, 16, 8, 0].map((s) => (v >>> s) & 255).join('.');
  for (const [base, len] of BLOCKED) {
    const first = parseIPv4(base);
    const last = (first + 2 ** (32 - len) - 1) >>> 0;
    assert.notEqual(addressBlocked(u32ToIp(first)), null, `${base}/${len} first`);
    assert.notEqual(addressBlocked(u32ToIp(last)), null, `${base}/${len} last`);
  }
  for (const ip of ['169.254.169.254', '127.0.0.1', '0.0.0.0', '255.255.255.255', '10.0.2.2', '100.100.100.200', '192.168.1.1', '172.31.255.255', '198.18.0.1', '224.0.0.251']) {
    assert.notEqual(addressBlocked(ip), null, ip);
  }
  for (const ip of ['9.255.255.255', '11.0.0.0', '100.63.255.255', '100.128.0.0', '172.15.255.255', '172.32.0.0', '198.17.255.255', '198.20.0.0', '223.255.255.255', '1.1.1.1', '93.184.215.14', '8.8.8.8']) {
    assert.equal(addressBlocked(ip), null, ip);
  }
  for (const bad of ['1.2.3', '01.2.3.4', '256.1.1.1', '::1', '1.2.3.4.5', ' 1.2.3.4', 'localhost', '']) {
    assert.notEqual(addressBlocked(bad), null, bad);
  }
});

test('only ports 80 and 443', () => {
  assert.equal(checkDestination('93.184.215.14', 443), null);
  assert.equal(checkDestination('93.184.215.14', 80), null);
  for (const p of [0, 22, 25, 53, 853, 5228, 8080, 8443, 65535]) assert.equal(checkDestination('93.184.215.14', p), 'port', String(p));
  assert.equal(checkDestination('10.0.0.1', 443), 'address');
});

// ---- Tokens -------------------------------------------------------------------

test('tokens: signature, origin, expiry, renewal', async () => {
  const now = 1_000_000;
  const t = await issue(SECRET, { sid: 's1', origin: ORIGIN, now });
  const ok = await verify(SECRET, t, { origin: ORIGIN, now: now + 1 });
  assert.equal(ok.sid, 's1');
  assert.equal(ok.exp, now + TOKEN_TTL_S);
  assert.equal((await verify(SECRET, t, { origin: 'https://evil.example', now })).error, 'origin');
  assert.equal((await verify(SECRET, t, { origin: ORIGIN, now: now + TOKEN_TTL_S })).error, 'expired');
  assert.equal((await verify(SECRET, t, { origin: ORIGIN, now: now + TOKEN_TTL_S, allowExpired: true })).sid, 's1');
  assert.equal((await verify('y'.repeat(40), t, { origin: ORIGIN, now })).error, 'signature');
  const [p, s] = t.split('.');
  const forged = Buffer.from(JSON.stringify({ v: 1, sid: 's2', origin: ORIGIN, iat: now, exp: now + 1e6, first: now })).toString('base64url');
  assert.equal((await verify(SECRET, `${forged}.${s}`, { origin: ORIGIN, now })).error, 'signature');
  for (const bad of ['', 'abc', `${p}.${s}.x`, `${p}.!!`, null, 'a'.repeat(2000)]) {
    assert.equal((await verify(SECRET, bad, { origin: ORIGIN, now })).error, 'malformed', String(bad).slice(0, 20));
  }
  await assert.rejects(issue('short', { sid: 'x', origin: ORIGIN, now }));
});

// ---- Ledger -------------------------------------------------------------------

test('ledger: connections, lanes, bandwidth, total, DNS, kill switch', () => {
  let t = 0;
  let on = true;
  const l = new Ledger({ ...DEFAULT_LIMITS, conns: 2, lanes: 2, rate: 1000, burst: 2000, bytes: 10_000, dnsRate: 1, dnsBurst: 2 }, { now: () => t, enabled: () => on });
  assert.deepEqual(l.openLane(0), { ok: true });
  assert.equal(l.openLane(0).reason, 'lane-in-use');
  assert.deepEqual(l.openLane(1), { ok: true });
  assert.equal(l.openLane(2).reason, 'lanes');
  l.closeLane(1);
  assert.deepEqual(l.openLane(2), { ok: true });
  assert.ok(l.openConn().ok && l.openConn().ok);
  assert.equal(l.openConn().reason, 'conns');
  l.closeConn();
  assert.ok(l.openConn().ok);
  // Bandwidth: the burst at once, then a wait proportional to the debt.
  assert.deepEqual(l.grant(2000), { ok: true, waitMs: 0 });
  assert.deepEqual(l.grant(500), { ok: true, waitMs: 500 });
  t += 500; // refilled 500: back to zero
  assert.deepEqual(l.grant(1000), { ok: true, waitMs: 1000 });
  // Total: 3500 so far; beyond 10 000 the session ends for good.
  assert.equal(l.grant(7000).reason, 'bytes');
  assert.equal(l.openConn().reason, 'bytes');
  // DNS: the burst, then waits, then refusals far beyond the rate.
  const d = new Ledger({ ...DEFAULT_LIMITS, dnsRate: 1, dnsBurst: 2 }, { now: () => t });
  assert.deepEqual([d.dns().waitMs, d.dns().waitMs, d.dns().waitMs, d.dns().waitMs], [0, 0, 1000, 2000]);
  assert.equal(d.dns().reason, 'dns');
  // Kill switch: everything refused at once.
  const k = new Ledger(DEFAULT_LIMITS, { now: () => t, enabled: () => on });
  on = false;
  for (const r of [k.openLane(0), k.openConn(), k.grant(1), k.dns()]) assert.equal(r.reason, 'disabled');
  // Limits from variables.
  assert.equal(limitsFrom({ LIMIT_CONNS: '4', LIMIT_DNS_RATE: '3', LIMIT_BYTES: 'x' }).conns, 4);
  assert.equal(limitsFrom({ LIMIT_DNS_RATE: '3' }).dnsRate, 3);
  assert.equal(limitsFrom({ LIMIT_BYTES: 'x' }).bytes, DEFAULT_LIMITS.bytes);
  // State survives a round trip (Durable Object storage).
  const r = new Ledger(DEFAULT_LIMITS, { now: () => t, state: structuredClone(l.state) });
  assert.equal(r.counters().bytes, 3500);
});

// ---- HTTP side -------------------------------------------------------------------

const cfg = (over = {}) => ({ ...configFrom({ TOKEN_SECRET: SECRET, ALLOWED_ORIGINS: `${ORIGIN}, http://127.0.0.1:8080` }), ...over });
const post = (body, origin = ORIGIN) =>
  new Request('https://gw.example/session', { method: 'POST', headers: origin ? { origin, 'content-type': 'application/json' } : {}, body: body === undefined ? undefined : JSON.stringify(body) });

test('sessions only for the Vetro site, renewal keeps the session while young', async () => {
  assert.deepEqual(cfg().origins, [ORIGIN, 'http://127.0.0.1:8080']);
  assert.equal((await handleHttp(post({}, 'https://evil.example'), cfg())).status, 403);
  assert.equal((await handleHttp(post({}, null), cfg())).status, 403);
  assert.equal((await handleHttp(post({}), cfg({ enabled: false }))).status, 503);
  assert.equal((await handleHttp(new Request('https://gw.example/session', { headers: { origin: ORIGIN } }), cfg())).status, 405);
  assert.equal((await handleHttp(new Request('https://gw.example/other'), cfg())).status, 404);
  const pre = await handleHttp(new Request('https://gw.example/session', { method: 'OPTIONS', headers: { origin: ORIGIN } }), cfg());
  assert.equal(pre.status, 204);
  assert.equal(pre.headers.get('access-control-allow-origin'), ORIGIN);
  const now = 5_000_000_000;
  const r = await handleHttp(post(undefined), cfg(), { now, lanes: 8 });
  assert.equal(r.status, 200);
  assert.equal(r.headers.get('access-control-allow-origin'), ORIGIN);
  const s = await r.json();
  assert.equal(s.lanes, 8);
  assert.equal(s.expires, now + TOKEN_TTL_S * 1000);
  const p = await verify(SECRET, s.token, { origin: ORIGIN, now: now / 1000 });
  // Renewal (even expired): the same session.
  const later = now + (TOKEN_TTL_S + 60) * 1000;
  const r2 = await (await handleHttp(post({ renew: s.token }), cfg(), { now: later })).json();
  const p2 = await verify(SECRET, r2.token, { origin: ORIGIN, now: later / 1000 });
  assert.equal(p2.sid, p.sid);
  assert.equal(p2.first, p.first);
  // Too old, or a bad token: a new session.
  const old = now + SESSION_MAX_S * 1000;
  const r3 = await (await handleHttp(post({ renew: s.token }), cfg(), { now: old })).json();
  assert.notEqual((await verify(SECRET, r3.token, { origin: ORIGIN, now: old / 1000 })).sid, p.sid);
  const r4 = await (await handleHttp(post({ renew: 'garbage' }), cfg(), { now })).json();
  assert.notEqual((await verify(SECRET, r4.token, { origin: ORIGIN, now: now / 1000 })).sid, p.sid);
  assert.equal((await handleHttp(new Request('https://gw.example/session', { method: 'POST', headers: { origin: ORIGIN }, body: '{' }), cfg())).status, 400);
});

test('lane upgrades need a valid token and the same origin', async () => {
  const now = 7_000_000_000;
  const token = await issue(SECRET, { sid: 'abc', origin: ORIGIN, now: now / 1000 });
  const lane = (q, headers = { upgrade: 'websocket', origin: ORIGIN }) => new Request(`https://gw.example/lane?${q}`, { headers });
  assert.deepEqual(await checkLane(lane(`token=${token}&lane=3`), cfg(), { now }), { sid: 'abc', lane: 3 });
  assert.equal((await checkLane(lane(`token=${token}&lane=8`), cfg(), { now })).error, 'lane');
  assert.equal((await checkLane(lane(`token=${token}&lane=x`), cfg(), { now })).error, 'lane');
  assert.equal((await checkLane(lane(`token=${token}&lane=0`, { origin: ORIGIN }), cfg(), { now })).status, 426);
  assert.equal((await checkLane(lane(`token=${token}&lane=0`, { upgrade: 'websocket', origin: 'http://127.0.0.1:8080' }), cfg(), { now })).error, 'origin', 'another allowed origin, not the token one');
  assert.equal((await checkLane(lane(`token=${token}&lane=0`, { upgrade: 'websocket' }), cfg(), { now })).status, 403);
  assert.equal((await checkLane(lane(`token=${token}&lane=0`), cfg(), { now: now + TOKEN_TTL_S * 1000 })).error, 'expired');
  assert.equal((await checkLane(lane('lane=0'), cfg(), { now })).error, 'malformed');
  assert.equal((await checkLane(lane(`token=${token}&lane=0`), cfg({ enabled: false }), { now })).status, 503);
});

// ---- DNS helpers -------------------------------------------------------------------

test('DNS: queries checked, answers parsed, failures and truncation', () => {
  const q = buildQuery('Example.COM', 0x1234);
  assert.deepEqual(parseQuery(q), { id: 0x1234, name: 'example.com', qtype: 1, end: q.length });
  const resp = Uint8Array.from([...q.subarray(0, 2), 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0, ...q.subarray(12),
    0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 12, // CNAME (rdata: a pointer)
    0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14]);
  assert.deepEqual(answerAddrs(resp), ['93.184.215.14']);
  const r = q.slice();
  r[2] |= 0x80; // a response is not a query
  assert.equal(parseQuery(r), null);
  assert.equal(parseQuery(q.subarray(0, 14)), null);
  const f = failAnswer(q, 2);
  assert.equal(f[3] & 0x0f, 2);
  assert.equal(f[2] & 0x80, 0x80);
  assert.deepEqual([...f.subarray(6, 12)], [0, 0, 0, 0, 0, 0]);
  const huge = new Uint8Array(MAX_UDP_ANSWER + 1);
  huge.set(resp.subarray(0, 12));
  const t = fitUdp(q, huge);
  assert.ok(t.length < 512 && (t[2] & 0x02) === 0x02, 'TC');
  assert.equal(fitUdp(q, resp), resp);
});

// ---- Lanes against fake sockets -------------------------------------------------

const tick = () => new Promise((ok) => setTimeout(ok, 0));
async function until(cond, what, ms = 2000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await tick();
  }
}

function fakeSocket({ fail = null } = {}) {
  let ctrl;
  const s = {
    written: [],
    writeClosed: false,
    closed: false,
    reset: false,
    opened: fail ? Promise.reject(fail) : Promise.resolve({}),
    readable: new ReadableStream({ start: (c) => (ctrl = c) }, new ByteLengthQueuingStrategy({ highWaterMark: 16 * 1024 })),
    writable: new WritableStream({
      write: (chunk) => {
        s.written.push(...chunk);
      },
      close: () => {
        s.writeClosed = true;
      },
    }),
    close() {
      s.closed = true;
    },
    push: (b) => ctrl.enqueue(b),
    end: () => ctrl.close(),
  };
  s.opened.catch(() => {});
  s.reset = undefined;
  return s;
}

function lane({ sockets = {}, resolver = async () => { throw new Error('no dns'); }, ledger = new Ledger(), laneConns = 4, now = () => Date.now(), ...rest } = {}) {
  const sent = [];
  const logs = [];
  const opened = [];
  const ws = { closed: null };
  const l = new Lane({
    send: (d) => sent.push(d),
    close: (code, reason) => (ws.closed = { code, reason }),
    connect: ({ hostname, port }) => {
      const s = (sockets[`${hostname}:${port}`] ?? (() => fakeSocket()))();
      opened.push({ hostname, port, s });
      return s;
    },
    resolver,
    ledger,
    laneConns,
    now,
    log: (o) => logs.push(o),
    tickMs: 0,
    ...rest,
  });
  l.start();
  const frames = () => sent.filter((d) => typeof d !== 'string').map(decode);
  const texts = () => sent.filter((d) => typeof d === 'string').map((t) => JSON.parse(t));
  return { l, sent, frames, texts, logs, opened, ws, msg: (b) => l.onMessage(b) };
}

test('hello, refusals by policy and limits', async () => {
  const t = lane({ laneConns: 2, sockets: { '93.184.215.15:443': () => fakeSocket({ fail: new Error('proxy request failed, cannot connect to the specified address') }), '93.184.215.16:443': () => fakeSocket({ fail: Object.assign(new Error('refused'), { code: 'ECONNREFUSED' }) }) } });
  assert.equal(t.texts()[0].type, 'hello');
  assert.equal(t.texts()[0].window, WINDOW);
  await t.msg(encode.connect(1, '10.0.0.1', 443));
  await t.msg(encode.connect(2, '93.184.215.14', 22));
  await t.msg(encode.connect(3, '169.254.169.254', 80));
  await t.msg(encode.connect(4, '93.184.215.15', 443));
  await t.msg(encode.connect(5, '93.184.215.16', 443));
  await t.msg(encode.connect(6, '93.184.215.14', 443));
  await t.msg(encode.connect(7, '93.184.215.14', 80));
  await t.msg(encode.connect(8, '93.184.215.14', 80));
  const r = Object.fromEntries(t.frames().map((f) => [f.id, f.type === 'refused' ? f.reason : f.type]));
  assert.deepEqual(r, { 1: 'address', 2: 'port', 3: 'address', 4: 'platform', 5: 'unreachable', 6: 'connected', 7: 'connected', 8: 'limit' });
  assert.deepEqual(t.opened.map((o) => `${o.hostname}:${o.port}`), ['93.184.215.15:443', '93.184.215.16:443', '93.184.215.14:443', '93.184.215.14:80'], 'no socket for refused destinations');
  // A second CONNECT on a live id is a protocol violation.
  await t.msg(encode.connect(6, '93.184.215.14', 443));
  assert.equal(t.texts().at(-1).reason, 'protocol');
  assert.equal(t.ws.closed.code, 4002);
  assert.ok(t.opened.every((o) => o.s.closed), 'every socket closed with the lane');
});

test('CONNECT by name: resolved, every address checked', async () => {
  const answer = (q, addrs) => {
    const ans = addrs.flatMap((a) => [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, ...a.split('.').map(Number)]);
    return Uint8Array.from([...q.subarray(0, 2), 0x81, 0x80, 0, 1, 0, addrs.length, 0, 0, 0, 0, ...q.subarray(12), ...ans]);
  };
  const table = { 'good.example': ['93.184.215.14'], 'rebind.example': ['93.184.215.14', '127.0.0.1'], 'none.example': [] };
  const resolver = async (q) => answer(q, table[parseQuery(q).name] ?? []);
  const t = lane({ resolver });
  await t.msg(encode.connectName(1, 'good.example', 443));
  await t.msg(encode.connectName(2, 'rebind.example', 443));
  await t.msg(encode.connectName(3, 'none.example', 443));
  await t.msg(encode.connectName(4, 'good.example', 8080));
  const r = Object.fromEntries(t.frames().map((f) => [f.id, f.type === 'refused' ? f.reason : f.type]));
  assert.deepEqual(r, { 1: 'connected', 2: 'address', 3: 'resolve', 4: 'port' });
  assert.equal(t.opened[0].hostname, '93.184.215.14', 'the checked address literal');
});

test('data both ways within the window, acks, half close', async () => {
  const t = lane();
  await t.msg(encode.connect(1, '93.184.215.14', 443));
  const s = t.opened[0].s;
  // Guest → remote: written, then acknowledged.
  const out = new Uint8Array(40_000).map((_, i) => i * 13);
  for (let at = 0; at < out.length; at += MAX_DATA) await t.msg(encode.data(1, out.subarray(at, at + MAX_DATA), false));
  await until(() => s.written.length === out.length, 'written');
  await until(() => t.frames().filter((f) => f.type === 'ack').reduce((a, f) => a + f.n, 0) === out.length, 'acks');
  assert.deepEqual(Uint8Array.from(s.written), out);
  // Remote → guest: no more than WINDOW unacknowledged.
  const big = new Uint8Array(3 * WINDOW + 1000).map((_, i) => i * 7);
  for (let at = 0; at < big.length; at += 50_000) s.push(big.subarray(at, at + 50_000));
  s.end();
  const got = [];
  let acked = 0;
  let seen = 0;
  for (let round = 0; round < 10_000 && got.length < big.length; round++) {
    await tick();
    const data = t.frames().filter((f) => f.type === 'data');
    for (const f of data.slice(seen)) got.push(...f.data);
    seen = data.length;
    assert.ok(got.length - acked <= WINDOW, `in flight ${got.length - acked}`);
    if (got.length > acked) {
      await t.msg(encode.ack(1, got.length - acked, false));
      acked = got.length;
    }
  }
  assert.deepEqual(Uint8Array.from(got), big);
  for (const f of t.frames().filter((f) => f.type === 'data')) assert.ok(f.data.length <= MAX_DATA);
  await until(() => t.frames().some((f) => f.type === 'shutdown'), 'remote FIN');
  // Guest FIN: the socket's write side closes; both done, the socket goes.
  await t.msg(encode.shutdown(1, false));
  await until(() => s.writeClosed, 'write side closed');
  await until(() => t.l.conns.size === 0, 'connection finished');
  // An ACK beyond what was sent is a violation (on a live connection).
  await t.msg(encode.connect(2, '93.184.215.14', 443));
  await t.msg(encode.ack(2, 1, false));
  assert.equal(t.texts().at(-1).reason, 'protocol');
});

test('the client must respect the window', async () => {
  // The socket never finishes writing: nothing is acknowledged.
  const stuck = () => Object.assign(fakeSocket(), { writable: new WritableStream({ write: () => new Promise(() => {}) }) });
  const t = lane({ sockets: { '93.184.215.14:443': stuck } });
  await t.msg(encode.connect(1, '93.184.215.14', 443));
  const chunk = new Uint8Array(MAX_DATA);
  for (let i = 0; i <= WINDOW / MAX_DATA; i++) await t.msg(encode.data(1, chunk, false));
  assert.equal(t.texts().at(-1).reason, 'protocol');
});

test('idle connections, lane life, kill switch, total bytes; only counters in the log', async () => {
  let now = 0;
  const t = lane({ now: () => now, idleMs: 1000, maxLifeMs: 5000 });
  await t.msg(encode.connect(1, '93.184.215.14', 443));
  now = 999;
  t.l.tick();
  assert.equal(t.l.conns.size, 1);
  now = 1000;
  t.l.tick();
  assert.deepEqual(t.frames().at(-1), { type: 'close', id: 1, fromGateway: true, reset: true });
  assert.ok(t.opened[0].s.closed);
  now = 5000;
  t.l.tick();
  assert.equal(t.texts().at(-1).reason, 'expired');
  assert.equal(t.logs.length, 1);
  const log = JSON.stringify(t.logs[0]);
  assert.ok(!/93\.184|example/.test(log), `no destinations in the log: ${log}`);
  assert.deepEqual(Object.keys(t.logs[0]).sort(), ['bytesToGuest', 'bytesToRemote', 'conns', 'dns', 'event', 'platform', 'reason', 'refused', 'secs']);

  let on = true;
  const k = lane({ ledger: new Ledger(DEFAULT_LIMITS, { enabled: () => on }) });
  await k.msg(encode.connect(1, '93.184.215.14', 443));
  on = false;
  await k.msg(encode.data(1, Uint8Array.of(1), false));
  await until(() => k.ws.closed, 'kill switch');
  assert.equal(k.texts().at(-1).reason, 'disabled');

  const b = lane({ ledger: new Ledger({ ...DEFAULT_LIMITS, bytes: GRANT }) });
  await b.msg(encode.connect(1, '93.184.215.14', 443));
  await b.msg(encode.data(1, new Uint8Array(1000), false));
  await until(() => b.frames().some((f) => f.type === 'ack'), 'first grant');
  b.opened[0].s.push(new Uint8Array(GRANT));
  await until(() => b.ws.closed, 'total bytes');
  assert.equal(b.texts().at(-1).reason, 'limit-bytes');
});

test('DNS through the lane', async () => {
  const resolver = async (q) => {
    const n = parseQuery(q).name;
    if (n === 'fail.example') throw new Error('down');
    if (n === 'big.example') return new Uint8Array(2000).fill(0, 0, 12);
    // DoH answers with id 0.
    return Uint8Array.from([0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, ...q.subarray(12), 0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
  };
  const t = lane({ resolver });
  await t.msg(encode.dns(1, buildQuery('ok.example', 0xabcd)));
  await t.msg(encode.dns(2, buildQuery('fail.example', 7)));
  await t.msg(encode.dns(3, buildQuery('big.example', 8)));
  const bad = buildQuery('x.example', 9);
  bad[2] |= 0x80;
  await t.msg(encode.dns(4, bad));
  await until(() => t.frames().length === 4, 'answers');
  const a = Object.fromEntries(t.frames().map((f) => [f.id, f.msg]));
  assert.equal((a[1][0] << 8) | a[1][1], 0xabcd, 'the guest id restored');
  assert.deepEqual(answerAddrs(a[1]), ['1.2.3.4']);
  assert.equal(a[2][3] & 0x0f, 2, 'SERVFAIL');
  assert.equal(a[3][2] & 0x02, 2, 'TC');
  assert.equal(a[4][3] & 0x0f, 1, 'FORMERR');
});
