#!/usr/bin/env node
// M7: ispettore di rete e timeline input→effetti via vetro-wasm (ABI 8,
// ADR 0023) sul kernel guest di M3 con la rete (sinkhole):
//   - `wget` di BusyBox manda un POST JSON e un POST form al sinkhole: la
//     lista dell'ispettore ha metodo, host, percorso, stato, dimensioni e
//     tempi; il dettaglio ha intestazioni e corpi decodificati (JSON come
//     valore, form come coppie);
//   - HAR (JSON valido con le due voci) e pcapng (intestazione, frame);
//   - timeline: la riga della console con il comando è l'ingresso, e le
//     richieste HTTP e le domande DNS che seguono sono attribuite a lei
//     (finestra di 3 s); l'eco della console al tasto;
//   - due esecuzioni danno le stesse istruzioni, lo stesso log, lo stesso
//     HAR e la stessa timeline (anche col JIT).
//
//   node tests/web/inspector.mjs [--no-jit]

import { DEV } from '../../web/node/vetro.mjs';
import { check, guestKernel, loadVetro, POST_JSON, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const POST_FORM = "wget -q -O /dev/null --post-data 'a=1&b=due+parole' http://form.vetro.test/modulo";

async function session(x, kernel) {
  const s = new Session(x, kernel, { jit, machine: { devices: DEV.DEFAULT } });
  check(s.m.capture(true), 'cattura non accesa: la macchina non ha la rete');
  let at = await s.until(SHELL_PROMPT);
  [, at] = await s.command('udhcpc -i eth0 -n -q', at);
  check(s.text().includes('lease of 10.0.2.15 obtained'), `DHCP:\n${s.tail()}`);
  // Un tasto solo (debole) prima del comando: non deve "causare" la rete.
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
  check(reqs.length === 2, `richieste: ${JSON.stringify(reqs)}`);
  const [j, f] = reqs;
  check(j.method === 'POST' && j.host === 'api.vetro.test' && j.path === '/v1/eventi' && j.status === 200,
    `prima richiesta: ${JSON.stringify(j)}`);
  check(j.reqKind === 'json' && j.reqBytes === 27 && j.timings.totalUs > 0 && j.timings.dnsUs !== null && j.timings.connectUs !== null,
    `corpo e tempi della prima: ${JSON.stringify(j)}`);
  check(f.host === 'form.vetro.test' && f.path === '/modulo' && f.reqKind === 'form', `seconda richiesta: ${JSON.stringify(f)}`);
  check(a.list.dns.some((d) => d.name === 'api.vetro.test' && d.addrs.length === 1), `DNS: ${JSON.stringify(a.list.dns)}`);
  const d0 = a.detail[0];
  check(d0.request.body.json?.vetro === 42 && d0.request.body.json.nome === 'prova', `JSON decodificato: ${JSON.stringify(d0.request.body)}`);
  check(d0.request.headers.some(([k, v]) => k.toLowerCase() === 'content-type' && v === 'application/json'), 'intestazione Content-Type');
  check(d0.response.status === 200 && d0.response.body.kind === 'vuoto', `risposta: ${JSON.stringify(d0.response)}`);
  check(JSON.stringify(a.detail[1].request.body.fields) === JSON.stringify([['a', '1'], ['b', 'due parole']]),
    `form decodificato: ${JSON.stringify(a.detail[1].request.body)}`);
  const har = JSON.parse(a.har);
  check(har.log.entries.length === 2 && har.log.entries[0].request.postData.text === '{"vetro":42,"nome":"prova"}', 'HAR');
  const magic = Buffer.from(a.pcap.subarray(0, 4)).toString('hex');
  check(magic === '0a0d0d0a' && a.stats.frames > 10 && a.pcap.length > a.stats.bytes, `pcapng: ${magic}, ${JSON.stringify(a.stats)}`);

  // Timeline: le richieste e il DNS sono del comando giusto.
  const t = a.timeline;
  const cause = (e) => (e.cause === null ? null : t.inputs[e.cause]);
  const http = t.effects.filter((e) => e.kind === 'http');
  check(http.length === 2, `effetti http: ${JSON.stringify(http)}`);
  check(cause(http[0])?.label === `Invio: ${POST_JSON}` && http[0].ref === 0, `causa della prima: ${JSON.stringify(cause(http[0]))}`);
  check(cause(http[1])?.label === `Invio: ${POST_FORM}` && http[1].ref === 1, `causa della seconda: ${JSON.stringify(cause(http[1]))}`);
  const dns = t.effects.filter((e) => e.kind === 'dns' && e.label.includes('api.vetro.test'));
  check(dns.length === 2 && dns.every((e) => cause(e)?.label === `Invio: ${POST_JSON}`), `DNS nella timeline: ${JSON.stringify(dns)}`);
  const hash = t.inputs.find((i) => i.label === '#');
  check(hash?.weak === true, `tasto singolo: ${JSON.stringify(hash)}`);
  const echo = t.effects.find((e) => e.kind === 'console' && e.cause === hash.i);
  check(echo?.label.includes('#'), `eco del tasto: ${JSON.stringify(t.effects.slice(0, 5))}`);
  check(!t.effects.some((e) => e.kind !== 'console' && e.cause === hash.i), 'il tasto singolo ha causato effetti di rete');

  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `esecuzioni diverse: ${a.steps} e ${b.steps} istruzioni`);
  check(a.har === b.har && JSON.stringify(a.timeline) === JSON.stringify(b.timeline) && Buffer.compare(a.pcap, b.pcap) === 0,
    'HAR, pcapng o timeline diversi fra due esecuzioni');
  console.log(`ispettore: ${reqs.length} richieste (JSON e form decodificati), ${a.list.dns.length} domande DNS, ${a.stats.frames} frame; ` +
    `HAR e pcapng; timeline con ${t.inputs.length} ingressi e ${t.effects.length} effetti, rete attribuita al comando; ` +
    `${a.steps} istruzioni in due esecuzioni uguali${jit ? ' (JIT)' : ''}`);
});
