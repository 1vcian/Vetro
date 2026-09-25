#!/usr/bin/env node
// M5: connessioni dal JS verso un servizio TCP del guest (vetro-wasm ABI 5,
// `GuestSocket` di web/node/vetro.mjs), la base per un client ADB in JS.
// Nel guest `nc -n -v -l -p 5555 -e cat` (l'eco); dal JS:
//   - il guest vede la connessione arrivare da 10.0.2.2;
//   - eco di 200 KB (più della finestra e della coda), byte per byte;
//   - chiusura ordinata (stato Closed/Normal) e rilascio;
//   - una porta senza servizio: Closed/Refused;
//   - istruzioni e log uguali in due esecuzioni.
//
//   node tests/web/hostfwd.mjs [--no-jit]

import { check, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');

function payload() {
  const out = new Uint8Array(200_000);
  for (let i = 0; i < out.length; i++) out[i] = Number((BigInt(i) * 2654435761n) & 0xffffffffn) >>> 9;
  return out;
}

async function session(x, kernel) {
  const s = new Session(x, kernel, { jit });
  const m = s.m;
  let at = await s.until(SHELL_PROMPT);
  [, at] = await s.command('udhcpc -i eth0 -n -q', at);
  check(s.text(0, at).includes('udhcpc: bound eth0 10.0.2.15'), `DHCP:\n${s.tail()}`);

  m.consoleWrite('nc -n -v -l -p 5555 -e cat\n');
  await s.until('listening on', at);
  const data = payload();
  const sock = m.connectGuest(5555);
  let sent = 0;
  const got = [];
  let echoed = 0;
  let shut = false;
  const limit = m.steps + 6_000_000_000n;
  for (;;) {
    sent += sock.send(data.subarray(sent));
    const r = sock.recv();
    if (r.length) {
      got.push(r);
      echoed += r.length;
    }
    if (!shut && echoed >= data.length) {
      sock.shutdown();
      shut = true;
    }
    if (sock.state().state === 'Closed') break;
    check(m.steps < limit, `eco ferma a ${echoed} byte:\n${s.tail()}`);
    const stop = await s.quantum();
    check(stop === 'Budget', `${stop} durante l'eco`);
  }
  const st = sock.state();
  check(st.reason === 'Normal', `chiusura ${JSON.stringify(st)}`);
  const echo = Buffer.concat(got);
  check(echo.length === data.length && echo.equals(Buffer.from(data)), `eco diversa (${echo.length} byte)`);
  sock.release();
  check(sock.state().state === 'Unknown', 'connessione non rilasciata');
  at = await s.until(SHELL_PROMPT, at);
  const conn = s.text(0, at).split('\n').find((l) => l.startsWith('connect to'));
  check(/^connect to 10\.0\.2\.15:5555 from 10\.0\.2\.2:49152 /.test(conn ?? ''), `riga di nc: ${conn}`);

  const refused = m.connectGuest(5556);
  while (refused.state().state !== 'Closed') {
    const stop = await s.quantum();
    check(stop === 'Budget', `${stop} in attesa del rifiuto`);
  }
  check(refused.state().reason === 'Refused', `porta senza servizio: ${JSON.stringify(refused.state())}`);
  refused.release();
  await s.poweroff(at);
  return { steps: m.steps, log: s.log };
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();
  const a = await session(exports, kernel);
  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `esecuzioni diverse: ${a.steps} e ${b.steps} istruzioni`);
  console.log(`hostfwd: eco di 200 KB dal JS, chiusura, rifiuto; ${a.steps} istruzioni in due esecuzioni uguali`);
});
