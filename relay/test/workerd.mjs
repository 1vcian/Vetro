#!/usr/bin/env node
// The Cloudflare Worker itself (relay/src/worker.mjs, its Durable Object
// ledger and `cloudflare:sockets`), run locally by `wrangler dev` (workerd,
// the same runtime as Cloudflare's), with the page's client in front.
// Needs internet: it resolves and fetches a real page (example.com, port 80)
// through the Worker, and checks that private destinations are refused.
//
//   node relay/test/workerd.mjs        (after `npm ci --prefix relay`)

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../src/protocol.mjs';
import { answerAddrs, buildQuery } from '../src/dns.mjs';
import { GatewayClient } from '../../web/node/relay.mjs';

const ORIGIN = 'http://127.0.0.1:8080';
const PORT = Number(process.env.RELAY_WORKERD_PORT ?? 8799);
const dir = fileURLToPath(new URL('..', import.meta.url));
const dev = spawn('npx', ['wrangler', 'dev', '--local', '--ip', '127.0.0.1', '--port', String(PORT), '--var', `TOKEN_SECRET:${'w'.repeat(48)}`, '--var', `ALLOWED_ORIGINS:${ORIGIN}`, '--show-interactive-dev-session=false'], { cwd: dir, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
let out = '';
dev.stdout.on('data', (d) => (out += d));
dev.stderr.on('data', (d) => (out += d));
after(() => {
  try {
    process.kill(-dev.pid, 'SIGTERM');
  } catch {}
});

const url = `http://127.0.0.1:${PORT}`;
const tick = () => new Promise((ok) => setTimeout(ok, 20));
async function until(cond, what, ms = 30_000) {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error(`timed out: ${what}\n${out.slice(-2000)}`);
    await tick();
  }
}

test('the Worker under workerd: session, lanes, DNS, a real page, refusals', async () => {
  await until(async () => (await fetch(`${url}/health`).catch(() => null))?.ok, 'wrangler dev up', 90_000);
  assert.equal((await fetch(`${url}/session`, { method: 'POST', headers: { origin: 'https://evil.example' } })).status, 403);
  const c = new GatewayClient(url, { origin: ORIGIN });
  const got = [];
  const pull = () => (got.push(...c.take().map(decode)), got);
  try {
    c.fromMachine([encode.dns(1, buildQuery('example.com', 0x77))]);
    await until(() => pull().some((f) => f.type === 'dnsAnswer'), 'DNS answer');
    const addrs = answerAddrs(got.find((f) => f.type === 'dnsAnswer').msg);
    assert.ok(addrs.length > 0, 'example.com resolves');
    assert.equal(c.status().lanes, 1);

    c.fromMachine([encode.connect(2, '10.0.0.1', 443), encode.connect(3, '169.254.169.254', 80), encode.connect(4, addrs[0], 22)]);
    await until(() => pull().filter((f) => f.type === 'refused').length === 3, 'refusals');
    assert.deepEqual(got.filter((f) => f.type === 'refused').map((f) => f.reason), ['address', 'address', 'port']);

    c.fromMachine([encode.connectName(5, 'example.com', 80)]);
    await until(() => pull().some((f) => f.id === 5 && (f.type === 'connected' || f.type === 'refused')), 'connect by name');
    const answer = got.find((f) => f.id === 5);
    console.log(`# example.com through the Worker: ${answer.type}${answer.reason ? ` (${answer.reason})` : ''}`);
    if (answer.type === 'refused') {
      // example.com may be served from Cloudflare's ranges: the platform refuses (ADR 0046).
      assert.equal(answer.reason, 'platform');
      return;
    }
    c.fromMachine([encode.data(5, new TextEncoder().encode('GET / HTTP/1.0\r\nHost: example.com\r\n\r\n'), false)]);
    await until(() => pull().some((f) => f.id === 5 && f.type === 'shutdown'), 'the page and the server FIN');
    const page = Buffer.from(got.filter((f) => f.id === 5 && f.type === 'data').flatMap((f) => [...f.data])).toString();
    assert.match(page, /^HTTP\/1\.[01] 200/);
    assert.match(page, /Example Domain/);
    c.fromMachine([encode.close(5, false, false)]);
  } finally {
    c.close();
  }
});
