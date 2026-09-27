#!/usr/bin/env node
// M7: network inspector and input→effects timeline via vetro-wasm (ABI 8,
// ADR 0023) on the M3 guest kernel with the network (sinkhole):
//   - BusyBox's `wget` sends a JSON POST and a form POST to the sinkhole: the
//     inspector list has method, host, path, status, sizes and
//     timings; the detail has headers and decoded bodies (JSON as a
//     value, form as pairs);
//   - HAR (valid JSON with the two entries) and pcapng (header, frames);
//   - timeline: the console line with the command is the input, and the
//     HTTP requests and DNS queries that follow are attributed to it
//     (3 s window); the console echo of the key;
//   - two runs give the same instructions, the same log, the same
//     HAR and the same timeline (also with the JIT).
//
//   node tests/web/inspector.mjs [--no-jit]

import { DEV } from '../../web/node/vetro.mjs';
import { check, guestKernel, loadVetro, POST_JSON, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const POST_FORM = "wget -q -O /dev/null --post-data 'a=1&b=due+parole' http://form.vetro.test/modulo";

async function session(x, kernel) {
  const s = new Session(x, kernel, { jit, machine: { devices: DEV.DEFAULT } });
  check(s.m.capture(true), 'capture not on: the machine has no network');
  let at = await s.until(SHELL_PROMPT);
  [, at] = await s.command('udhcpc -i eth0 -n -q', at);
  check(s.text().includes('lease of 10.0.2.15 obtained'), `DHCP:\n${s.tail()}`);
  // A single (weak) key before the command: it must not "cause" the network.
  s.m.consoleWrite('#');
  await s.quantum();
  s.m.consoleWrite('\u0015');
  [, at] = await s.command(POST_JSON, at);
  [, at] = await s.command(POST_FORM, at);
  const list = s.m.inspectRequests();
  const har = s.m.inspectHar(0);
  const pcap = s.m.inspectPcapng(0);
  const timeline = s.m.timeline(0);
  const detail = list.requests.map((r) => s.m.inspectRequest(r.i));
  const stats = s.m.captureStats();
  await s.poweroff(at);
  return { steps: s.m.steps, log: s.log, list, har, pcap, timeline, detail, stats };
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();
  const a = await session(exports, kernel);

  const reqs = a.list.requests;
  check(reqs.length === 2, `requests: ${JSON.stringify(reqs)}`);
  const [j, f] = reqs;
  check(j.method === 'POST' && j.host === 'api.vetro.test' && j.path === '/v1/eventi' && j.status === 200,
    `first request: ${JSON.stringify(j)}`);
  check(j.reqKind === 'json' && j.reqBytes === 27 && j.timings.totalUs > 0 && j.timings.dnsUs !== null && j.timings.connectUs !== null,
    `body and timings of the first: ${JSON.stringify(j)}`);
  check(f.host === 'form.vetro.test' && f.path === '/modulo' && f.reqKind === 'form', `second request: ${JSON.stringify(f)}`);
  check(a.list.dns.some((d) => d.name === 'api.vetro.test' && d.addrs.length === 1), `DNS: ${JSON.stringify(a.list.dns)}`);
  const d0 = a.detail[0];
  check(d0.request.body.json?.vetro === 42 && d0.request.body.json.nome === 'prova', `decoded JSON: ${JSON.stringify(d0.request.body)}`);
  check(d0.request.headers.some(([k, v]) => k.toLowerCase() === 'content-type' && v === 'application/json'), 'Content-Type header');
  check(d0.response.status === 200 && d0.response.body.kind === 'empty', `response: ${JSON.stringify(d0.response)}`);
  check(JSON.stringify(a.detail[1].request.body.fields) === JSON.stringify([['a', '1'], ['b', 'due parole']]),
    `decoded form: ${JSON.stringify(a.detail[1].request.body)}`);
  const har = JSON.parse(a.har);
  check(har.log.entries.length === 2 && har.log.entries[0].request.postData.text === '{"vetro":42,"nome":"prova"}', 'HAR');
  const magic = Buffer.from(a.pcap.subarray(0, 4)).toString('hex');
  check(magic === '0a0d0d0a' && a.stats.frames > 10 && a.pcap.length > a.stats.bytes, `pcapng: ${magic}, ${JSON.stringify(a.stats)}`);

  // Timeline: the requests and the DNS belong to the right command.
  const t = a.timeline;
  const cause = (e) => (e.cause === null ? null : t.inputs[e.cause]);
  const http = t.effects.filter((e) => e.kind === 'http');
  check(http.length === 2, `http effects: ${JSON.stringify(http)}`);
  check(cause(http[0])?.label === `Enter: ${POST_JSON}` && http[0].ref === 0, `cause of the first: ${JSON.stringify(cause(http[0]))}`);
  check(cause(http[1])?.label === `Enter: ${POST_FORM}` && http[1].ref === 1, `cause of the second: ${JSON.stringify(cause(http[1]))}`);
  const dns = t.effects.filter((e) => e.kind === 'dns' && e.label.includes('api.vetro.test'));
  check(dns.length === 2 && dns.every((e) => cause(e)?.label === `Enter: ${POST_JSON}`), `DNS in the timeline: ${JSON.stringify(dns)}`);
  const hash = t.inputs.find((i) => i.label === '#');
  check(hash?.weak === true, `single key: ${JSON.stringify(hash)}`);
  const echo = t.effects.find((e) => e.kind === 'console' && e.cause === hash.i);
  check(echo?.label.includes('#'), `echo of the key: ${JSON.stringify(t.effects.slice(0, 5))}`);
  check(!t.effects.some((e) => e.kind !== 'console' && e.cause === hash.i), 'the single key caused network effects');

  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `runs differ: ${a.steps} and ${b.steps} instructions`);
  check(a.har === b.har && JSON.stringify(a.timeline) === JSON.stringify(b.timeline) && Buffer.compare(a.pcap, b.pcap) === 0,
    'HAR, pcapng or timeline differ between two runs');
  console.log(`inspector: ${reqs.length} requests (JSON and form decoded), ${a.list.dns.length} DNS queries, ${a.stats.frames} frames; ` +
    `HAR and pcapng; timeline with ${t.inputs.length} inputs and ${t.effects.length} effects, network attributed to the command; ` +
    `${a.steps} instructions in two identical runs${jit ? ' (JIT)' : ''}`);
});
