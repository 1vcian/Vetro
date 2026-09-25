#!/usr/bin/env node
// M8: il gestore dei file via vetro-wasm (ABI 7, `GuestFiles` di
// web/node/vetro.mjs, ADR 0020) sul kernel guest di M3 con virtio-vsock: il
// demone `vetro-files` del guest risponde al JS.
//   - list con proprietario e modo; lettura; file che non c'è (ENOENT);
//   - scrittura dal JS che conserva modo e proprietario, letta dal guest
//     con `cat` e `stat`; file nuovo;
//   - osservazione: un processo del guest scrive un file e l'evento arriva
//     entro 1 s di tempo del guest;
//   - file grande (1,2 MB) scritto e riletto a pezzi, confrontato dal guest
//     con `cmp`;
//   - istruzioni e log uguali in due esecuzioni (anche col JIT).
//
//   node tests/web/files.mjs [--no-jit]

import { DEV, INOTIFY } from '../../web/node/vetro.mjs';
import { check, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const enc = new TextEncoder();
const dec = new TextDecoder();

/** Il contenuto di `seq 1 n` di BusyBox. */
function seq(n) {
  let s = '';
  for (let i = 1; i <= n; i++) s += `${i}\n`;
  return enc.encode(s);
}

async function session(x, kernel) {
  let files = null;
  const events = [];
  const s = new Session(x, kernel, {
    jit,
    machine: { devices: DEV.DEFAULT | DEV.VSOCK },
    onQuantum: () => files?.pump(),
  });
  files = s.m.files();
  files.onEvent = (e) => events.push({ ns: s.m.guestNs, ...e });
  // Aspetta una Promise del gestore facendo girare la macchina.
  const wait = async (p) => {
    let done = false;
    let value;
    let error;
    p.then((v) => { done = true; value = v; }, (e) => { done = true; error = e; });
    const limit = s.m.steps + 6_000_000_000n;
    while (!done) {
      check(s.m.steps < limit, `operazione del gestore non finita:\n${s.tail()}`);
      const stop = await s.quantum();
      check(stop === 'Budget', `${stop} durante un'operazione del gestore`);
      await null;
    }
    if (error) throw error;
    return value;
  };
  // Uscita di un comando fra due marcatori.
  const command = async (cmd) => {
    const from = s.log.length;
    s.m.consoleWrite(`echo VETRO-OUT-""INIZIO; ${cmd}; echo VETRO-OUT-""FINE\n`);
    const end = await s.until('VETRO-OUT-FINE', from);
    await s.until(SHELL_PROMPT, end);
    const t = s.text(from, end);
    return t.slice(t.indexOf('VETRO-OUT-INIZIO\n') + 17, t.length - 14).replace(/\n+$/, '');
  };

  await s.until(SHELL_PROMPT);
  await command('mkdir /tmp/w && echo uno > /tmp/w/a.txt && chown 12:34 /tmp/w/a.txt && chmod 600 /tmp/w/a.txt && seq 1 200000 > /tmp/w/grande');
  const list = await wait(files.list('/tmp/w'));
  check(JSON.stringify(list.map((e) => e.name)) === '["a.txt","grande"]', `list: ${JSON.stringify(list)}`);
  const a = list[0].stat;
  check(a.kind === 'file' && a.mode === 0o100600 && a.uid === 12 && a.gid === 34 && a.size === 4, `stat di a.txt: ${JSON.stringify(a)}`);
  const st = files.status();
  check(st.state === 'Ready' && st.generation === 1 && st.maxChunk >= 262144, `stato: ${JSON.stringify(st)}`);
  const r = await wait(files.read('/tmp/w/a.txt'));
  check(dec.decode(r.data) === 'uno\n' && r.size === 4, `lettura: ${JSON.stringify(r)}`);
  const missing = await wait(files.read('/tmp/w/manca')).then(() => null, (e) => e);
  check(missing?.code === 'ENOENT' && missing.errno === 2, `file che non c'è: ${missing}`);

  const wd = await wait(files.watch('/tmp/w'));
  const t0 = s.m.guestNs;
  const echoFrom = s.log.length;
  s.m.consoleWrite('echo dal-guest > /tmp/w/g.txt\n');
  const limit = s.m.steps + 6_000_000_000n;
  let ev;
  while (!(ev = events.find((e) => e.wd === wd && e.name === 'g.txt' && e.mask & INOTIFY.CLOSE_WRITE))) {
    check(s.m.steps < limit, `evento non arrivato: ${JSON.stringify(events)}`);
    await s.quantum();
  }
  const ms = Number(ev.ns - t0) / 1e6;
  check(ms < 1000, `evento dopo ${ms} ms di tempo del guest`);
  await s.until(SHELL_PROMPT, echoFrom);

  const w = await wait(files.writeFile('/tmp/w/a.txt', enc.encode('scritto dal JS\n'), 0o644));
  check(w.mode === 0o100600 && w.uid === 12 && w.gid === 34 && w.size === 15, `dopo la scrittura: ${JSON.stringify(w)}`);
  let out = await command("cat /tmp/w/a.txt; stat -c '%a %u %g' /tmp/w/a.txt");
  check(out === 'scritto dal JS\n600 12 34', `il guest legge: ${JSON.stringify(out)}`);
  const big = seq(200000);
  await wait(files.writeFile('/tmp/w/copia', big, 0o640));
  out = await command("cmp /tmp/w/grande /tmp/w/copia && echo COPIA-UGUALE; stat -c '%a' /tmp/w/copia");
  check(out === 'COPIA-UGUALE\n640', `copia grande: ${JSON.stringify(out)}`);
  const back = await wait(files.read('/tmp/w/grande'));
  check(back.data.length === big.length && Buffer.from(back.data).equals(Buffer.from(big)), `file grande riletto: ${back.data.length} byte`);
  check(events.some((e) => e.name === 'a.txt' && e.mask & INOTIFY.MOVED_TO), 'evento della scrittura del JS');
  check(events.every((e) => !e.name.startsWith('.vetro-tmp.')), 'eventi dei file temporanei');
  await wait(files.delete('/tmp/w', { recursive: true }));
  out = await command('ls /tmp');
  check(!out.split('\n').includes('w'), `cartella non cancellata: ${out}`);
  await s.poweroff(0);
  return { steps: s.m.steps, log: s.log, ms };
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();
  const a = await session(exports, kernel);
  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `esecuzioni diverse: ${a.steps} e ${b.steps} istruzioni`);
  console.log(`gestore dei file: list, lettura, scrittura che conserva modo e proprietario, evento dopo ${a.ms.toFixed(1)} ms ` +
    `di tempo del guest, 1,2 MB a pezzi; ${a.steps} istruzioni in due esecuzioni uguali${jit ? ' (JIT)' : ''}`);
});
