// The Python gateway (relay/py, ADR 0050) as a child process, for the tests
// (relay/test/python.mjs, relay/test/python-online.mjs). It runs with its
// TEST ONLY options: `--test-control` (JSON lines on stdout, commands on
// stdin), `--test-connect-map` (public test addresses sent to local servers,
// after the destination policy) and `--test-hosts` (names from a table
// instead of the system resolver).
//
//   VETRO_PYTHON=python3.12 node relay/test/python.mjs   (another interpreter)

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(new URL('../py/connect.py', import.meta.url));

/**
 * Starts `python3 relay/py/connect.py` on a free port. Options:
 * - `origins`: allowed origins besides the live site's (`--origin`);
 * - `connectMap`: { 'ip:port': 'ip:port' } (`--test-connect-map`);
 * - `hosts`: { name: ip | [ip…] }, '*' for any other name (`--test-hosts`);
 * - `env`: more environment variables (LANE_CONNS, LIMIT_*).
 * Resolves to { url, port, page, counters(), setEnabled(on), close() }.
 */
export async function startPythonGateway({ origins = [], connectMap = {}, hosts = null, env = {} } = {}) {
  const args = [SCRIPT, '--port', '0', '--no-browser', '--test-control'];
  for (const o of origins) args.push('--origin', o);
  const map = Object.entries(connectMap).map(([from, to]) => `${from}=${to}`).join(',');
  if (map) args.push('--test-connect-map', map);
  if (hosts) args.push('--test-hosts', Object.entries(hosts).map(([name, a]) => `${name}=${[].concat(a).join('+')}`).join(','));
  const python = process.env.VETRO_PYTHON ?? 'python3';
  const child = spawn(python, args, { env: { ...process.env, ...env }, stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (d) => (stderr += d));
  let gone = null;
  const waiting = [];
  const fail = (why) => {
    gone = new Error(`the Python gateway (${python}) ${why}\n${stderr}`);
    for (const w of waiting.splice(0)) w.no(gone);
  };
  child.on('error', (e) => fail(`did not start: ${e.message}`));
  child.on('exit', (code, signal) => fail(`exited (${signal ?? code})`));
  createInterface({ input: child.stdout }).on('line', (line) => {
    let m;
    try {
      m = JSON.parse(line);
    } catch {
      return;
    }
    const at = waiting.findIndex((w) => w.kind === m.test);
    if (at >= 0) waiting.splice(at, 1)[0].ok(m);
  });
  /** The next JSON line of kind `kind` from the gateway. */
  const next = (kind, ms = 15_000) =>
    new Promise((ok, no) => {
      if (gone) return no(gone);
      const t = setTimeout(() => no(new Error(`the Python gateway did not answer "${kind}" in ${ms} ms\n${stderr}`)), ms);
      waiting.push({ kind, ok: (m) => (clearTimeout(t), ok(m)), no: (e) => (clearTimeout(t), no(e)) });
    });
  const ask = (command, kind) => {
    const answer = next(kind);
    child.stdin.write(`${command}\n`);
    return answer;
  };
  const ready = await next('ready');
  return {
    url: ready.url,
    port: ready.port,
    page: ready.page,
    /** { stats: the gateway's counters, sessions: [each session ledger's counters] }. */
    counters: () => ask('counters', 'counters'),
    /** The kill switch. */
    setEnabled: (on) => ask(on ? 'enable' : 'disable', 'enabled'),
    stderr: () => stderr,
    close: () =>
      new Promise((ok) => {
        if (child.exitCode !== null || child.signalCode !== null) return ok();
        const t = setTimeout(() => child.kill('SIGKILL'), 5000);
        child.once('exit', () => (clearTimeout(t), ok()));
        child.stdin.end('quit\n');
      }),
  };
}
