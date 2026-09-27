#!/usr/bin/env node
// M8: the file manager via vetro-wasm (ABI 7, `GuestFiles` of
// web/node/vetro.mjs, ADR 0020) on the M3 guest kernel with virtio-vsock: the
// guest's `vetro-files` daemon answers JS.
//   - list with owner and mode; read; a file that doesn't exist (ENOENT);
//   - write from JS that preserves mode and owner, read by the guest
//     with `cat` and `stat`; new file;
//   - watch: a guest process writes a file and the event arrives
//     within 1 s of guest time;
//   - large file (1.2 MB) written and read back in chunks, compared by the guest
//     with `cmp`;
//   - editing (ADR 0021): rows of a WAL database kept open by a
//     guest process changed with SQL in the guest (GuestFiles.sql), seen
//     by the page's reader in the -wal and reread by the guest with sqlite3;
//     SharedPreferences rewritten like Android and reread by the guest; a
//     non-UTF-8 name listed and reopened (surrogateescape);
//   - instructions and log equal in two runs (also with the JIT).
//
//   node tests/web/files.mjs [--no-jit]

import { DEV, INOTIFY } from '../../web/node/vetro.mjs';
import { checkPrefValue, parsePrefs, prefsToXml } from '../../web/app/files.mjs';
import { SqliteDb } from '../../web/app/sqlite.mjs';
import { check, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const enc = new TextEncoder();
const dec = new TextDecoder();

/** The output of BusyBox's `seq 1 n`. */
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
  // Waits for a file manager Promise by running the machine.
  const wait = async (p) => {
    let done = false;
    let value;
    let error;
    p.then((v) => { done = true; value = v; }, (e) => { done = true; error = e; });
    const limit = s.m.steps + 6_000_000_000n;
    while (!done) {
      check(s.m.steps < limit, `file manager operation not finished:\n${s.tail()}`);
      const stop = await s.quantum();
      check(stop === 'Budget', `${stop} during a file manager operation`);
      await null;
    }
    if (error) throw error;
    return value;
  };
  // Output of a command between two markers.
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
  check(st.state === 'Ready' && st.generation === 1 && st.maxChunk >= 262144, `state: ${JSON.stringify(st)}`);
  const r = await wait(files.read('/tmp/w/a.txt'));
  check(dec.decode(r.data) === 'uno\n' && r.size === 4, `read: ${JSON.stringify(r)}`);
  const missing = await wait(files.read('/tmp/w/manca')).then(() => null, (e) => e);
  check(missing?.code === 'ENOENT' && missing.errno === 2, `file that doesn't exist: ${missing}`);

  const wd = await wait(files.watch('/tmp/w'));
  const t0 = s.m.guestNs;
  const echoFrom = s.log.length;
  s.m.consoleWrite('echo dal-guest > /tmp/w/g.txt\n');
  const limit = s.m.steps + 6_000_000_000n;
  let ev;
  while (!(ev = events.find((e) => e.wd === wd && e.name === 'g.txt' && e.mask & INOTIFY.CLOSE_WRITE))) {
    check(s.m.steps < limit, `event did not arrive: ${JSON.stringify(events)}`);
    await s.quantum();
  }
  const ms = Number(ev.ns - t0) / 1e6;
  check(ms < 1000, `event after ${ms} ms of guest time`);
  await s.until(SHELL_PROMPT, echoFrom);

  const w = await wait(files.writeFile('/tmp/w/a.txt', enc.encode('scritto dal JS\n'), 0o644));
  check(w.mode === 0o100600 && w.uid === 12 && w.gid === 34 && w.size === 15, `after the write: ${JSON.stringify(w)}`);
  let out = await command("cat /tmp/w/a.txt; stat -c '%a %u %g' /tmp/w/a.txt");
  check(out === 'scritto dal JS\n600 12 34', `the guest reads: ${JSON.stringify(out)}`);
  const big = seq(200000);
  await wait(files.writeFile('/tmp/w/copia', big, 0o640));
  out = await command("cmp /tmp/w/grande /tmp/w/copia && echo COPIA-UGUALE; stat -c '%a' /tmp/w/copia");
  check(out === 'COPIA-UGUALE\n640', `large copy: ${JSON.stringify(out)}`);
  const back = await wait(files.read('/tmp/w/grande'));
  check(back.data.length === big.length && Buffer.from(back.data).equals(Buffer.from(big)), `large file read back: ${back.data.length} bytes`);
  check(events.some((e) => e.name === 'a.txt' && e.mask & INOTIFY.MOVED_TO), 'event of the JS write');
  check(events.every((e) => !e.name.startsWith('.vetro-tmp.')), 'events of the temporary files');
  // ---- Editing (ADR 0021) --------------------------------------------------
  // A WAL database kept open by a guest process: row changed
  // with SQL in the guest, the page's reader sees it in the -wal, the guest's
  // sqlite3 rereads it.
  const db = '/tmp/w/app.db';
  out = await command(`sqlite3 -batch -list ${db} "PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'uno'), (2, 'due');"`);
  check(out === 'wal', `database in WAL: ${out}`);
  const holderFrom = s.log.length;
  s.m.consoleWrite(`(echo 'SELECT 1 FROM t;'; sleep 100000) | sqlite3 ${db} >/dev/null &\n`);
  await s.until(SHELL_PROMPT, holderFrom);
  const openLimit = s.m.steps + 6_000_000_000n;
  while (!(await command('ls /tmp/w')).includes('app.db-shm')) check(s.m.steps < openLimit, 'the database does not open');
  const upd = await wait(files.sql(db, 'UPDATE t SET v = ?1 WHERE rowid = ?2', ['cambiata dal JS', 2], { expect: 1 }));
  check(upd.changes === 1 && upd.columns.length === 0, `UPDATE: ${JSON.stringify(upd, (k, v) => (typeof v === 'bigint' ? `${v}` : v))}`);
  const ins = await wait(files.sql(db, 'INSERT INTO t (v) VALUES (?1)', [{ type: 'text', value: 'tre' }], { expect: 1 }));
  check(ins.lastRowid === 3n, `INSERT: rowid ${ins.lastRowid}`);
  const refused = await wait(files.sql(db, 'DELETE FROM t', [], { expect: 1 })).then(() => null, (e) => e);
  check(refused?.code === 'SQLITE' && refused.sqlite === 19, `DELETE of 3 rows with expect 1: ${refused}`);
  const sel = await wait(files.sql(db, 'SELECT id, v, 1.5, x\'ff\', NULL FROM t ORDER BY id', [], { readonly: true }));
  check(JSON.stringify(sel.rows.map((r) => r.slice(0, 3))) === '[[1,"uno",1.5],[2,"cambiata dal JS",1.5],[3,"tre",1.5]]' && sel.rows[0][3][0] === 0xff && sel.rows[0][4] === null,
    `SELECT: ${JSON.stringify(sel.rows.map((r) => r.map(String)))}`);
  out = await command(`sqlite3 -batch -list ${db} 'SELECT group_concat(v) FROM t'`);
  check(out === 'uno,cambiata dal JS,tre', `the guest rereads: ${out}`);
  const mainFile = await wait(files.read(db));
  const walFile = await wait(files.read(`${db}-wal`));
  const view = new SqliteDb(mainFile.data, walFile.data).rows('t');
  const stale = new SqliteDb(mainFile.data).rows('t').rows.length;
  check(JSON.stringify(view.rows) === '[[1,"uno"],[2,"cambiata dal JS"],[3,"tre"]]' && stale < 3,
    `reader with the WAL: ${JSON.stringify(view.rows)} (without WAL ${stale} rows)`);
  // SharedPreferences: read, changed and rewritten by JS like Android, reread by the guest.
  const prefsPath = '/tmp/w/prefs.xml';
  await command(`printf '%s\\n' "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>" '<map>' '    <int name="avvii" value="3" />' '    <string name="nome">x &amp; y</string>' '</map>' > ${prefsPath} && chown 12:34 ${prefsPath} && chmod 660 ${prefsPath}`);
  const prefs = parsePrefs(dec.decode((await wait(files.read(prefsPath))).data));
  check(prefs?.length === 2 && prefs[1].value === 'x & y', `SharedPreferences read: ${JSON.stringify(prefs)}`);
  prefs[0].value = checkPrefValue('int', '42');
  prefs.push({ type: 'boolean', name: 'nuovo', value: 'true' }, { type: 'set', name: 's', value: ['a', 'b'] });
  const pw = await wait(files.writeFile(prefsPath, enc.encode(prefsToXml(prefs)), 0o600));
  check(pw.mode === 0o100660 && pw.uid === 12, `SharedPreferences: mode and owner ${JSON.stringify(pw)}`);
  out = await command(`grep -c 'value="42"' ${prefsPath}; grep -c '<string>b</string>' ${prefsPath}; tail -n 1 ${prefsPath}`);
  check(out === '1\n1\n</map>', `SharedPreferences reread by the guest: ${JSON.stringify(out)}`);
  // Non-UTF-8 name: listed in surrogateescape, reopened with the same bytes.
  await command("printf 'np' > \"/tmp/w/$(printf 'n\\377')\"");
  const odd = (await wait(files.list('/tmp/w'))).find((e) => e.name.startsWith('n'));
  check(odd?.name === 'n\udcff', `non-UTF-8 name: ${JSON.stringify(odd?.name)}`);
  const oddData = await wait(files.read(`/tmp/w/${odd.name}`));
  check(dec.decode(oddData.data) === 'np', 'file with the non-UTF-8 name reread');
  await wait(files.delete('/tmp/w', { recursive: true }));
  out = await command('ls /tmp');
  check(!out.split('\n').includes('w'), `folder not deleted: ${out}`);
  await s.poweroff(0);
  return { steps: s.m.steps, log: s.log, ms };
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();
  const a = await session(exports, kernel);
  const b = await session(exports, kernel);
  check(a.steps === b.steps && a.log === b.log, `runs differ: ${a.steps} and ${b.steps} instructions`);
  console.log(`file manager: list, read, write preserving mode and owner, event after ${a.ms.toFixed(1)} ms ` +
    `of guest time, 1.2 MB in chunks, SQL in the guest on an open WAL database (seen in the -wal), SharedPreferences ` +
    `rewritten, non-UTF-8 name; ${a.steps} instructions in two identical runs${jit ? ' (JIT)' : ''}`);
});
