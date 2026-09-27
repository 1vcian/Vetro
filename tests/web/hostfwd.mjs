#!/usr/bin/env node
// M5: connections from JS to a TCP service of the guest (vetro-wasm ABI 5,
// `GuestSocket` of web/node/vetro.mjs), the basis for an ADB client in JS.
// In the guest `nc -n -v -l -p 5555 -e cat` (the echo); from JS:
//   - the guest sees the connection arrive from 10.0.2.2;
//   - echo of 200 KB (more than the window and the queue), byte by byte;
//   - orderly close (state Closed/Normal) and release;
//   - a port without a service: Closed/Refused;
//   - instructions and log equal in two runs.
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
    check(m.steps < limit, `echo stuck at ${echoed} bytes:\n${s.tail()}`);
    const stop = await s.quantum();
    check(stop === 'Budget', `${stop} during the echo`);
  }
  const st = sock.state();
  check(st.reason === 'Normal', `close ${JSON.stringify(st)}`);
  const echo = Buffer.concat(got);
  check(echo.length === data.length && echo.equals(Buffer.from(data)), `echo differs (${echo.length} bytes)`);
  sock.release();
  check(sock.state().state === 'Unknown', 'connection not released');
  at = await s.until(SHELL_PROMPT, at);
  const conn = s.text(0, at).split('\n').find((l) => l.startsWith('connect to'));
  check(/^connect to 10\.0\.2\.15:5555 from 10\.0\.2\.2:49152 /.test(conn ?? ''), `nc line: ${conn}`);

  const refused = m.connectGuest(5556);
  while (refused.state().state !== 'Closed') {
    const stop = await s.quantum();
    check(stop === 'Budget', `${stop} while waiting for the refusal`);
  }
  check(refused.state().reason === 'Refused', `port without a service: ${JSON.stringify(refused.state())}`);
  refused.release();
  await s.poweroff(at);
  return { steps: m.steps, log: s.log };
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();
  const a = await session(exports, kernel);
  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `runs differ: ${a.steps} and ${b.steps} instructions`);
  console.log(`hostfwd: 200 KB echo from JS, close, refusal; ${a.steps} instructions in two identical runs`);
});
