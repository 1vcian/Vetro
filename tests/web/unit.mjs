#!/usr/bin/env node
// Unit tests of the JS pieces of the web app (without a kernel or browser):
// server with Range (tools/web-serve.mjs), sources and DiskFeeder
// (web/node/disk.mjs), key map (web/app/keymap.mjs), terminal
// (web/app/terminal.mjs), persistence (web/node/persist.mjs), SQLite
// reader (also with the WAL) and file manager viewers
// (web/app/sqlite.mjs, web/app/files.mjs, M8), panel SQL and SharedPreferences,
// non-UTF-8 names and SQL arguments of vetro.mjs (ADR 0021),
// formats of the analysis panels (web/app/analysis.mjs, M7/M10), the disk
// rebuilt from a map, Android boot phases and APK manifests
// (web/node/disk.mjs, android.mjs, apk.mjs, M5/M6), the resource table and
// icon of an APK, the app catalog's parsing, minimum image version, SHA-256
// check, download and install states (web/node/catalog.mjs, ADR 0033).
//
//   node tests/web/unit.mjs

import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { inline, linkTarget, markdownToHtml, slug } from '../../tools/pages/markdown.mjs';
import { join } from 'node:path';
import { BlobSource, composePlan, composeRead, DiskFeeder, LayoutSource, MemoryCache, parseLayout, RangeSource } from '../../web/node/disk.mjs';
import { ANDROID_MACHINE, ANDROID_PARAMS, ANDROID_VERSIONS, BootProgress, colorSeen, DEFAULT_MANIFEST, gridColors, isHome, PHASES } from '../../web/node/android.mjs';
import { apkIcon, apkInfo, parseArsc, parseAxml, resolveResource, zipEntries } from '../../web/node/apk.mjs';
import {
  CATALOG_FORMAT, downloadApk, imageRelease, imageSatisfies, initialState, nextState, parseCatalog, parseEntry, parsePackages, sizeText, STATES, verifyApk,
} from '../../web/node/catalog.mjs';
import {
  DEFAULT_PROFILE, parseProfile, parseProfileReport, PROFILE_REPORT, profileAdbCommands, profileAndroidParams, profileBootParams, ProfileError,
  profileMachine, profileMismatches, profileUrl, STARTER_PROFILES,
} from '../../web/node/profiles.mjs';
import { parseRange, serve } from '../../tools/web-serve.mjs';
import { absAxis, BUTTONS, evdevCode } from '../../web/app/keymap.mjs';
import { keyToBytes, Terminal } from '../../web/app/terminal.mjs';
import { fromBase64, MemFile, readAll, SnapshotStore, snapshotKey, staleReason, toBase64 } from '../../web/node/persist.mjs';
import { deleteRowSql, formatValue, insertRowSql, isSqlite, parseCreateTable, SqliteDb, updateCellSql, varint, walPages } from '../../web/app/sqlite.mjs';
import {
  asText, checkPrefValue, detectView, displayName, hexDump, imageType, javaFloatString, modeString, parseHex, parsePrefs, parseXml,
  prefsToXml, sizeString,
} from '../../web/app/files.mjs';
import { encodeSqlArgs, pathBytes, pathString, sqlValue } from '../../web/node/vetro.mjs';
import { bodyCell, duration, fromB64, guestTime, hexdump, typeText } from '../../web/app/analysis.mjs';
import { check, makeZip, root, run } from './lib.mjs';
import { downloadPrebuilt, findPrebuilt, PREBUILT_CHUNK, PREBUILT_FORMAT, prebuiltInfoUrl, prebuiltProblem, prebuiltSnapUrl } from '../../web/node/prebuilt.mjs';

const eq = (a, b, what) => check(JSON.stringify(a) === JSON.stringify(b), `${what}: ${JSON.stringify(a)} instead of ${JSON.stringify(b)}`);
let count = 0;
const cases = [];
const test = (name, f) => cases.push([name, f]);

test('parseRange', () => {
  eq(parseRange(undefined, 100), null, 'without Range');
  eq(parseRange('bytes=0-9', 100), { start: 0, end: 9 }, 'range');
  eq(parseRange('bytes=90-', 100), { start: 90, end: 99 }, 'open');
  eq(parseRange('bytes=-10', 100), { start: 90, end: 99 }, 'suffisso');
  eq(parseRange('bytes=95-200', 100), { start: 95, end: 99 }, 'beyond the end');
  eq(parseRange('bytes=100-', 100), 'invalid', 'from the end');
  eq(parseRange('bytes=5-2', 100), 'invalid', 'rovesciato');
  eq(parseRange('bytes=0-1,4-5', 100), 'invalid', 'multiple');
  eq(parseRange('items=0-1', 100), 'invalid', 'unit');
});

test('server: Range, HEAD, isolation, paths', async () => {
  const dir = join(root, 'target/web-test');
  mkdirSync(join(dir, 'www'), { recursive: true });
  const data = new Uint8Array(1000).map((_, i) => i % 251);
  writeFileSync(join(dir, 'www/a.bin'), data);
  writeFileSync(join(dir, 'www/index.html'), '<p>ciao</p>');
  writeFileSync(join(dir, 'segreto.txt'), 'no');
  const log = [];
  const srv = await serve({ mounts: [['/w/', join(dir, 'www')], ['/x.bin', join(dir, 'www/a.bin')]], onRequest: (r) => log.push(r) });
  try {
    let r = await fetch(`${srv.url}/w/a.bin`, { headers: { Range: 'bytes=10-19' } });
    eq(r.status, 206, 'status');
    eq(r.headers.get('content-range'), 'bytes 10-19/1000', 'Content-Range');
    eq([...new Uint8Array(await r.arrayBuffer())], [...data.subarray(10, 20)], 'byte');
    eq(r.headers.get('cross-origin-opener-policy'), 'same-origin', 'COOP');
    eq(r.headers.get('cross-origin-embedder-policy'), 'require-corp', 'COEP');
    r = await fetch(`${srv.url}/x.bin`);
    eq([r.status, (await r.arrayBuffer()).byteLength], [200, 1000], 'whole');
    r = await fetch(`${srv.url}/x.bin`, { headers: { Range: 'bytes=5000-' } });
    eq([r.status, r.headers.get('content-range')], [416, 'bytes */1000'], '416');
    r = await fetch(`${srv.url}/x.bin`, { method: 'HEAD' });
    eq([r.status, r.headers.get('content-length'), r.headers.get('accept-ranges')], [200, '1000', 'bytes'], 'HEAD');
    r = await fetch(`${srv.url}/w/`);
    eq([r.status, r.headers.get('content-type'), await r.text()], [200, 'text/html; charset=utf-8', '<p>ciao</p>'], 'index');
    r = await fetch(`${srv.url}/w/..%2fsegreto.txt`);
    eq(r.status, 404, 'outside the root');
    r = await fetch(`${srv.url}/altro`);
    eq(r.status, 404, 'not mounted');

    // Sources.
    const src = await new RangeSource(`${srv.url}/w/a.bin`).open();
    eq(src.size, 1000, 'size from the Content-Range');
    check(src.key.includes('|1000|"'), `key with ETag: ${src.key}`);
    eq([...(await src.read(995, 5))], [...data.subarray(995)], 'read');
    eq(src.stats, { requests: 2, bytes: 5 }, 'counters');
    let failed = false;
    await src.read(990, 20).catch(() => (failed = true));
    check(failed, 'a read beyond the end must fail');
    failed = false;
    await new RangeSource(`${srv.url}/nessuno`).open().catch(() => (failed = true));
    check(failed, 'a source without a file must fail');
  } finally {
    await srv.close();
  }
  const blob = new BlobSource(new Blob([new Uint8Array([1, 2, 3, 4])]), 'b');
  eq([...(await blob.read(1, 2))], [2, 3], 'BlobSource');
});

/** A fake Machine: records the deliveries and gives requested blocks on command. */
class FakeMachine {
  wanted = [];
  filled = [];
  failed = [];
  addDisk(size, opt) {
    this.opt = { size, ...opt };
    return 0;
  }
  diskWanted() {
    return this.wanted.splice(0);
  }
  diskFill(disk, block, bytes) {
    this.filled.push([disk, block, bytes.length, bytes[0]]);
  }
  diskFail(disk, block) {
    this.failed.push([disk, block]);
  }
}

class FakeSource {
  reads = [];
  constructor(size, fail = false) {
    this.size = size;
    this.fail = fail;
  }
  async read(offset, length) {
    this.reads.push([offset, length]);
    if (this.fail) throw new Error('network down');
    return new Uint8Array(length).map((_, i) => ((offset + i) >> 12) & 0xff);
  }
}

test('DiskFeeder: cache, contiguous blocks, read-ahead, errors', async () => {
  const m = new FakeMachine();
  const f = new DiskFeeder(m);
  const src = new FakeSource(10 * 4096 + 700); // 10 blocks and a short one (512 after rounding)
  const cache = new MemoryCache();
  cache.put(3, new Uint8Array(4096).fill(33));
  eq(f.add(src, { cache, blockSize: 4096, readahead: 1 }), 0, 'index');
  eq(m.opt, { size: 10 * 4096 + 512, blockSize: 4096, maxBlocks: 0, readOnly: false }, 'disk added');
  eq(await f.serve(), 0, 'nothing to do');
  m.wanted = [{ disk: 0, block: 1 }, { disk: 0, block: 2 }, { disk: 0, block: 3 }, { disk: 0, block: 10 }];
  eq(await f.serve(), 4, 'requested');
  // 1 and 2 together, 3 from the cache, 4 ahead (after 3), 10 (last, short).
  eq(src.reads, [[4096, 2 * 4096], [4 * 4096, 4096], [10 * 4096, 512]], 'reads');
  eq(m.filled.map((x) => x[1]).sort((a, b) => a - b), [1, 2, 3, 4, 10], 'delivered');
  eq(m.filled.find((x) => x[1] === 3)[3], 33, 'block 3 comes from the cache');
  eq(m.filled.find((x) => x[1] === 10)[2], 512, 'short last block');
  check(cache.has(1) && cache.has(4) && cache.has(10), 'downloaded blocks put in the cache');
  eq(f.stats.fromCache, 1, 'from cache');
  eq(f.stats.readahead, 1, 'in anticipo');

  const bad = new FakeMachine();
  const g = new DiskFeeder(bad);
  const orig = console.error;
  console.error = () => {};
  try {
    g.add(new FakeSource(8192, true), { blockSize: 4096, readahead: 1 });
    bad.wanted = [{ disk: 0, block: 0 }];
    await g.serve();
  } finally {
    console.error = orig;
  }
  eq(bad.failed, [[0, 0]], 'only the requested block fails (not the read-ahead one)');
});

test('keymap', () => {
  eq([evdevCode('KeyA'), evdevCode('Enter'), evdevCode('Space'), evdevCode('ArrowUp'), evdevCode('MetaLeft')], [30, 28, 57, 103, 125], 'codes');
  eq([evdevCode('Digit1'), evdevCode('Digit0'), evdevCode('F12'), evdevCode('NumpadEnter')], [2, 11, 88, 96], 'other codes');
  eq(evdevCode('toString'), undefined, 'no prototype');
  eq(evdevCode('Nessuno'), undefined, 'unknown');
  eq([BUTTONS[0], BUTTONS[1], BUTTONS[2]], [0x110, 0x112, 0x111], 'buttons');
  eq([absAxis(0), absAxis(1), absAxis(0.5), absAxis(-1), absAxis(2)], [0, 32767, 16384, 0, 32767], 'assi');
});

test('terminal', () => {
  const replies = [];
  const t = new Terminal({ onReply: (s) => replies.push(s) });
  const feed = (s) => t.feed(new TextEncoder().encode(s));
  feed('abc\r\nriga due\rRIGA\n');
  eq(t.lines, ['abc', 'RIGA due', ''], 'CR e LF');
  feed('~ # \x1b[6n');
  eq(replies, ['\x1b[3;5R'], 'reply to ESC[6n');
  feed('xyz\b\b\x1b[K!');
  eq(t.lines[2], '~ # x!', 'backspace and erase');
  feed('\x1b[1;32mverde\x1b[0m\x1b]0;titolo\x07.');
  eq(t.lines[2], '~ # x!verde.', 'colours and OSC ignored');
  const e = new TextEncoder().encode('è');
  t.feed(e.subarray(0, 1));
  t.feed(e.subarray(1));
  eq(t.lines[2].endsWith('è'), true, 'split UTF-8');
  feed('\ta');
  eq(t.lines[2].length, 17, 'tabulazione');
  const k = (key, o = {}) => keyToBytes({ key, ctrlKey: false, altKey: false, metaKey: false, ...o });
  eq([k('a'), k('Enter'), k('Backspace'), k('ArrowUp'), k('c', { ctrlKey: true }), k('Shift'), k('v', { metaKey: true })],
    ['a', '\r', '\x7f', '\x1b[A', '\x03', null, null], 'keys');
});

test('persistence: MemFile, snapshot cache, keys', async () => {
  const f = new MemFile();
  f.write(new Uint8Array([1, 2, 3]), { at: 5000 });
  eq(f.getSize(), 5003, 'write beyond the end');
  eq([...readAll(f).subarray(4998)], [0, 0, 1, 2, 3], 'zeroed hole');
  f.truncate(4999);
  f.truncate(5001);
  eq([...readAll(f).subarray(4998)], [0, 0, 0], 'truncated and extended again with zeros');
  const buf = new Uint8Array(4);
  eq(f.read(buf, { at: 4999 }), 2, 'short read at the end');

  const store = SnapshotStore.memory();
  eq(await store.load('k'), null, 'missing key');
  await store.save('k', { generations: [3, null], console: toBase64(new Uint8Array([0, 255, 10])) }, new Uint8Array([9, 8, 7]));
  const r = await store.load('k');
  eq([...r.bytes], [9, 8, 7], 'bytes read back');
  eq(r.meta.size, 3, 'size in the metadata');
  eq([...fromBase64(r.meta.console)], [0, 255, 10], 'console in base64');
  eq(staleReason(r.meta, [{ generation: 3 }, null]), null, 'same generation: valid');
  check(staleReason(r.meta, [{ generation: 4 }, null])?.includes('generation 4'), 'disk moved on: not valid');
  await store.remove('k');
  eq(await store.load('k'), null, 'removed');

  const a = await snapshotKey({ v: 2, disks: [{ id: 'x', size: 1 }], ram: 1024 });
  const b = await snapshotKey({ ram: 1024, disks: [{ size: 1, id: 'x' }], v: 2 });
  const c = await snapshotKey({ ram: 1024, disks: [{ size: 1, id: 'x' }], v: 3 });
  eq(a === b && a !== c && a.length === 32, true, 'key stable with respect to order, different for one value');
});

test('SQLite: varint and CREATE TABLE', () => {
  eq(varint([0x05], 0).map(String), ['5', '1'], 'un byte');
  eq(varint([0x81, 0x00], 0).map(String), ['128', '2'], 'due byte');
  eq(varint([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], 0).map(String), ['18446744073709551615', '9'], 'nine bytes');
  eq(parseCreateTable('CREATE TABLE t (id INTEGER PRIMARY KEY, "a b" TEXT, [c] INT, `d`, CHECK (c > 0))'),
    { columns: ['id', 'a b', 'c', 'd'], rowidAlias: 0, withoutRowid: false, pk: [0] }, 'rowid alias and quotes');
  eq(parseCreateTable('CREATE TABLE t (id INT PRIMARY KEY, x)').rowidAlias, -1, 'INT is not an alias');
  eq(parseCreateTable('CREATE TABLE t (k TEXT, v, PRIMARY KEY (k)) WITHOUT ROWID'),
    { columns: ['k', 'v'], rowidAlias: -1, withoutRowid: true, pk: [0] }, 'WITHOUT ROWID');
  eq(formatValue(new Uint8Array([0, 255])), "x'00ff' (2 bytes)", 'BLOB');
  eq(formatValue(null), 'NULL', 'NULL');
});

test('SQLite: real database (tests/web/testdata/prova.sqlite)', () => {
  const bytes = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/prova.sqlite')));
  check(isSqlite(bytes) && !isSqlite(bytes.subarray(0, 50)), 'riconoscimento');
  const db = new SqliteDb(bytes);
  eq(db.pageSize, 1024, 'pages');
  eq(db.tables().map((t) => t.name), ['valori', 'molte righe', 'prefs'], 'tables');
  const v = db.rows('valori');
  eq(v.columns, ['id', 'nome', 'n', 'x', 'dati'], 'columns');
  eq(v.rows.length, 7, 'righe');
  eq(v.rows[0], [1, 'nullo', null, null, null], 'NULL');
  eq(v.rows[1][2] === 0 && v.rows[1][3] === 1.5 && [...v.rows[1][4]].join() === '0,255', true, 'zero, real, BLOB');
  eq(v.rows[2][2] === 1 && v.rows[2][3] === -2.25 && v.rows[2][4].length === 0, true, 'one, empty BLOB');
  check(v.rows[3][2] === 9007199254740993n, `8-byte integer beyond 2^53: ${v.rows[3][2]}`);
  eq([v.rows[4][2], v.rows[4][3]], [-300000, 1e300], '3-byte negative, large real');
  eq(v.rows[5][1], 'àèìòù €', 'UTF-8');
  eq(v.rows[6][1], 'L'.repeat(5000), 'text with overflow pages');
  const m = db.rows('molte righe', 1000);
  eq(m.columns, ['rowid', 'chiave', 'valore'], 'rowid without an alias');
  eq(m.rows.length, 600, 'multi-level b-tree');
  eq(m.rows[599], [600, 'riga-0599', 599 * 599], 'last row');
  eq(db.rows('molte righe', 10).rows.length, 10, 'limit');
  eq(db.rows('prefs').rows, [['anna', 'lingua', 'en'], ['anna', 'tema', 'scuro'], ['bruno', 'lingua', 'it']], 'WITHOUT ROWID in key order');
  let err = null;
  try {
    new SqliteDb(new Uint8Array(200));
  } catch (e) {
    err = e.message;
  }
  eq(err, 'not an SQLite 3 database', 'file that is not SQLite');
});

test('SQLite: WAL (tests/web/testdata/wal.sqlite and -wal)', () => {
  const bytes = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/wal.sqlite')));
  const wal = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/wal.sqlite-wal')));
  // Without WAL: as it was at the last checkpoint.
  const old = new SqliteDb(bytes);
  eq(old.rows('t').rows, [[1, 'base-1'], [2, 'base-2'], [3, 'base-3'], [4, 'base-4'], [5, 'base-5']], 'without WAL');
  eq(old.tables().map((t) => t.name), ['t'], 'tables without WAL');
  const db = new SqliteDb(bytes, wal);
  check(db.walFrames > 0 && db.pageCount > old.pageCount, `WAL applied: ${db.walFrames} frames, ${db.pageCount} pages`);
  const t = db.rows('t');
  eq(t.rows, [[1, 'base-1'], [2, 'dal-wal'], [3, 'base-3'], [4, 'base-4'], [6, 'nuova']], 'rows with the WAL (like sqlite3)');
  eq(t.rowids, [1, 2, 3, 4, 6], 'rowid');
  eq(t.types[0], ['integer', 'text'], 'types');
  eq(db.tables().map((x) => x.name), ['t', 'altra'], 'table created in the WAL');
  eq(db.rows('altra').rows.map((r) => r[1].length), [900, 900, 900, 900], 'rows of the new table');
  // The damaged frame at the end doesn't count; a WAL cut halfway through the first
  // frame is not valid; a WAL with another page size is ignored.
  const w = walPages(wal, 1024);
  check((wal.length - 32) / (24 + 1024) === w.frames + 1, `valid frames ${w.frames} of ${(wal.length - 32) / 1048}`);
  eq(walPages(wal.subarray(0, 32 + 600), 1024), null, 'frame tagliato');
  eq(walPages(wal, 4096), null, 'another page size');
  const bad = wal.slice();
  bad[40] ^= 1;
  eq(walPages(bad, 1024), null, 'damaged salt in the first frame');
});

test('SQLite: SQL of the panel edits', () => {
  const db = new SqliteDb(new Uint8Array(readFileSync(join(root, 'tests/web/testdata/prova.sqlite'))));
  const v = db.rows('valori');
  eq(v.types[1], ['integer', 'text', 'integer', 'real', 'blob'], 'types of a row');
  const u = updateCellSql(v, 1, 3, { type: 'real', value: 2 });
  eq(u.sql, 'UPDATE "valori" SET "x" = ?1 WHERE rowid = ?2', 'UPDATE with alias');
  eq(u.params[1], 2, 'rowid');
  const m = db.rows('molte righe');
  eq(updateCellSql(m, 0, 2, 'z').sql, 'UPDATE "molte righe" SET "valore" = ?1 WHERE rowid = ?2', 'without an alias: column moved by the rowid');
  let err = null;
  try {
    updateCellSql(m, 0, 0, 1);
  } catch (e) {
    err = e.message;
  }
  check(err?.includes('rowid'), 'the rowid without an alias cannot be changed');
  const pr = db.rows('prefs');
  const d = deleteRowSql(pr, 0);
  eq([d.sql, d.params], ['DELETE FROM "prefs" WHERE "utente" IS ?1 AND "chiave" IS ?2', ['anna', 'lingua']], 'WITHOUT ROWID: key');
  eq(deleteRowSql(v, 0), { sql: 'DELETE FROM "valori" WHERE rowid = ?1', params: [1] }, 'DELETE');
  eq(insertRowSql(v.table, { nome: 'n', x: 1.5 }), { sql: 'INSERT INTO "valori" ("nome", "x") VALUES (?1, ?2)', params: ['n', 1.5] }, 'INSERT');
  eq(insertRowSql({ name: 'a"b' }, {}).sql, 'INSERT INTO "a""b" DEFAULT VALUES', 'empty INSERT and quotes');
});

test('SharedPreferences: Android XML', () => {
  // A file as Android writes it (FastXmlSerializer), with all the types.
  const xml = "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map>\n" +
    '    <string name="nome">Vetro &amp; &lt;co&gt; &quot;x&quot; &#10;riga</string>\n' +
    '    <int name="avvii" value="3" />\n' +
    '    <long name="ultimo" value="-9223372036854775808" />\n' +
    '    <float name="scala" value="1.5" />\n' +
    '    <boolean name="primo" value="true" />\n' +
    '    <set name="etichette">\n        <string>a</string>\n        <string>b c</string>\n    </set>\n' +
    '    <set name="vuoto" />\n' +
    '    <null name="niente" />\n' +
    '    <string name="vuota"></string>\n' +
    '</map>\n';
  const p = parsePrefs(xml);
  eq(p.map((e) => e.type), ['string', 'int', 'long', 'float', 'boolean', 'set', 'set', 'null', 'string'], 'types');
  eq(p[0].value, 'Vetro & <co> "x" \nriga', 'entities');
  eq(p[5].value, ['a', 'b c'], 'set');
  eq(prefsToXml(p), xml, 'read back and rewritten: same bytes');
  eq(prefsToXml([]), "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map />\n", 'empty map');
  eq(prefsToXml([{ type: 'string', name: 't', value: 'fine\n' }]).split('\n')[2], '    <string name="t">fine&#10;    </string>', 'text ending with \\n (like FastXmlSerializer)');
  eq(parsePrefs('<a/>'), null, 'root other than map');
  eq(parsePrefs('<map><int-array name="x" num="0" /></map>'), null, 'unhandled type');
  eq(parsePrefs('<!-- c --><map>\n<string name="a"><![CDATA[<x>]]></string></map>')[0].value, '<x>', 'commento e CDATA');
  for (const bad of ['<map>', '<map><int name="a" value="1"></map>', '<map a=1/>', '<map>&nope;</map>', '<map/><x/>']) {
    let e = null;
    try {
      parseXml(bad);
    } catch (x) {
      e = x.message;
    }
    check(e?.startsWith('invalid XML'), `broken XML accepted: ${bad}`);
  }
  eq(['7', '+7', '-2147483648'].map((x) => checkPrefValue('int', x)), ['7', '7', '-2147483648'], 'int');
  eq(checkPrefValue('long', '9223372036854775807'), '9223372036854775807', 'long');
  eq(['1', '1.5f', '0.1', '1e10', '-0', '1e-5', '3.4028235e38', 'NaN'].map((x) => checkPrefValue('float', x)),
    ['1.0', '1.5', '0.1', '1.0E10', '-0.0', '1.0E-5', '3.4028235E38', 'NaN'], 'float like Float.toString');
  eq([javaFloatString(100), javaFloatString(1234567), javaFloatString(0.001), javaFloatString(1 / 3)], ['100.0', '1234567.0', '0.001', '0.33333334'], 'Float.toString');
  for (const [t, x] of [['int', '2147483648'], ['int', '1.0'], ['long', '9223372036854775808'], ['float', 'abc'], ['boolean', 'True']]) {
    let e = null;
    try {
      checkPrefValue(t, x);
    } catch (y) {
      e = y;
    }
    check(e !== null, `${t} ${x} accepted`);
  }
});

test('non-UTF-8 names and SQL arguments (vetro.mjs)', () => {
  const raw = new Uint8Array([0x2f, 0x61, 0xff, 0x62, 0xc3, 0xa0, 0xc3, 0xed, 0xb2, 0x80]);
  const s = pathString(raw);
  eq(s, '/a\udcffbà\udcc3\udced\udcb2\udc80', 'surrogateescape');
  eq([...pathBytes(s)], [...raw], 'andata e ritorno');
  eq(displayName(s), '/a\\xffbà\\xc3\\xed\\xb2\\x80', 'shown with \\xNN');
  eq([...pathBytes('😀/à')], [...new TextEncoder().encode('😀/à')], 'UTF-8 with surrogate pairs');
  eq(displayName('😀'), '😀', 'surrogate pair intact');
  eq(JSON.parse('"a\\udcff"'), 'a\udcff', 'vetro-wasm JSON');
  // Same bytes as proto::encode_sql_args (Rust tests sql_e_nomi / wasm).
  eq([...encodeSqlArgs('S', [1n, 'x'])], [1, 0, 0, 0, 83, 2, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 1, 0, 0, 0, 120], 'format');
  const all = encodeSqlArgs('', [null, 2, 2.5, true, new Uint8Array([7]), { type: 'real', value: 3 }, { type: 'integer', value: -1n }]);
  eq([...all.subarray(4, 6)], [7, 0], 'number of parameters');
  eq([all[6], all[7], all[16], all[25], all[34]], [0, 1, 2, 1, 4], 'inferred types');
  check(sqlValue(['i', '9223372036854775807']) === 9223372036854775807n, 'large integer');
  eq([sqlValue(['i', '-3']), sqlValue(['f', '1.0']), sqlValue(['f', 'inf']), sqlValue(['t', 'x']), sqlValue(null)], [-3, 1, null, 'x', null], 'values');
  eq(sqlValue(['f', '-inf']), -Infinity, 'minus infinity');
  eq([...sqlValue(['b', '00ab'])], [0, 0xab], 'BLOB');
});

test('file manager: viewers', () => {
  const enc = new TextEncoder();
  const bytes = new Uint8Array(40).map((_, i) => (i * 37) & 0xff);
  const dump = hexDump(bytes);
  eq(dump.split('\n').length, 3, 'righe da 16 byte');
  eq(dump.split('\n')[0], '00000000  00 25 4a 6f 94 b9 de 03 28 4d 72 97 bc e1 06 2b  |.%Jo....(Mr....+|', 'first line');
  eq([...parseHex(dump)], [...bytes], 'andata e ritorno');
  eq([...parseHex('00000000  41 42 |AB|\n00000002  43 ff 00\n')], [0x41, 0x42, 0x43, 0xff, 0x00], 'bytes added');
  let err = null;
  try {
    parseHex('00000000  41 4g');
  } catch (e) {
    err = e.message;
  }
  eq(err, 'line 1: "4g" is not a hexadecimal byte', 'error with the line');
  eq(modeString('file', 0o100640), '-rw-r-----', 'mode of a file');
  eq(modeString('dir', 0o40755), 'drwxr-xr-x', 'mode of a folder');
  eq(sizeString(2048), '2.0 KiB', 'size');
  eq(asText(enc.encode('ciao')), 'ciao', 'text');
  eq(asText(new Uint8Array([0xc3])), null, 'invalid UTF-8');
  eq(imageType(new Uint8Array([0x89, 0x50, 0x4e, 0x47, 13, 10])), 'image/png', 'PNG');
  const sqlite = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/prova.sqlite')));
  eq([
    detectView('a.json', enc.encode('{"a": 1}')),
    detectView('dati', enc.encode(' [1, 2]')),
    detectView('prefs.xml', enc.encode('<?xml version="1.0"?><map/>')),
    detectView('x', enc.encode('<map><int name="n" value="1" /></map>')),
    detectView('note.txt', enc.encode('{ non json')),
    detectView('bin', new Uint8Array([1, 0, 2])),
    detectView('img', new Uint8Array([0xff, 0xd8, 0xff, 0xe0])),
    detectView('app.db', sqlite),
  ], ['json', 'json', 'xml', 'xml', 'text', 'hex', 'image', 'sqlite'], 'detection');
});

test('analysis panels: times, durations, dump', () => {
  eq(guestTime(1_234_567), '1.234 s', 'guest time');
  eq(guestTime(5), '0.000 s', 'small time');
  eq([duration(null), duration(999), duration(1500), duration(25_000), duration(3_200_000)], ['–', '999 µs', '1.50 ms', '25.0 ms', '3.20 s'], 'durations');
  eq([...fromB64('AAH/')], [0, 1, 255], 'base64');
  const d = hexdump(new Uint8Array([0x41, 0x00, 0x7f, 0x42, ...new Array(14).fill(0x2e)]), 0xffff800080010800n);
  eq(d.split('\n'), [
    'ffff800080010800  41 00 7f 42 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e  A..B............',
    'ffff800080010810  2e 2e                                            ..',
  ], 'hex dump');
  eq(hexdump(new Uint8Array(40), 0n, 16).split('\n').at(-1), '… 24 more bytes', 'truncated dump');
});

test('inspector: body and type cells', () => {
  eq([bodyCell(0, 'empty'), bodyCell(12, 'json'), bodyCell(2048, 'binary'), bodyCell(5, '-'), bodyCell(null, '-'), bodyCell(undefined)],
    ['0 B', '12 B json', '2.0 KiB binary', '5 B', '–', '–'], 'bodies');
  const t = (r) => typeText(r).text;
  eq([
    t({ mime: 'application/json', status: 200, respBytes: 2, respKind: 'json' }),
    t({ mime: null, status: 200, respBytes: 0, respKind: 'empty' }),
    t({ mime: null, status: 200, respBytes: 7, respKind: 'text' }),
    t({ mime: null, status: null, respBytes: 0, respKind: '-' }),
  ], ['application/json', 'empty', 'text', '–'], 'type');
});

test('disk map: extents, fills, holes, LayoutSource over HTTP', async () => {
  const dir = join(root, 'target/web-test/layout');
  mkdirSync(join(dir, 'web'), { recursive: true });
  const head = new Uint8Array(3000).map((_, i) => (i * 7 + 1) & 0xff);
  const big = new Uint8Array(20000).map((_, i) => (i * 13 + 5) & 0xff);
  writeFileSync(join(dir, 'web/head.bin'), head);
  writeFileSync(join(dir, 'big.img'), big);
  const layout = {
    format: 'vetro-disk-layout', version: 1, size: 65536,
    files: [{ path: 'head.bin', size: head.length }, { path: '../big.img', size: big.length }],
    extents: [[0, 1000, 0, 0], [1000, 500, 0, 2000], [4096, 8192, 1, 100], [12288, 4096, 1, 8292], [20000, 6, -2, 0x04030201], [30000, 100, -1, 0], [60000, 5536, 1, 1000]],
  };
  // The expected disk, byte by byte.
  const want = new Uint8Array(65536);
  want.set(head.subarray(0, 1000), 0);
  want.set(head.subarray(2000, 2500), 1000);
  want.set(big.subarray(100, 100 + 8192), 4096);
  want.set(big.subarray(8292, 8292 + 4096), 12288);
  for (let k = 20000; k < 20006; k++) want[k] = [1, 2, 3, 4][k & 3];
  want.set(big.subarray(1000, 1000 + 5536), 60000);
  writeFileSync(join(dir, 'web/disk.json'), JSON.stringify(layout));
  const l = parseLayout(layout);
  // Two extents contiguous in the disk and in the file: a single piece.
  eq(composePlan(l, 4096, 12288), [{ at: 0, length: 12288, file: 1, fileOffset: 100 }], 'pieces merged');
  const files = [head, big];
  for (const [off, len] of [[0, 65536], [999, 3], [4000, 200], [19990, 20], [59999, 5537], [30000, 100]]) {
    const got = composeRead(l, off, len, (f, o, n) => files[f].subarray(o, o + n));
    eq(Buffer.compare(Buffer.from(got), Buffer.from(want.subarray(off, off + len))), 0, `composeRead ${off}+${len}`);
  }
  for (const [bad, what] of [
    [{ ...layout, extents: [[0, 10, 0, 0], [5, 10, 0, 0]] }, 'overlapping'],
    [{ ...layout, extents: [[65530, 10, 0, 0]] }, 'beyond the disk'],
    [{ ...layout, extents: [[0, 10, 0, 2995]] }, 'beyond the file'],
    [{ ...layout, extents: [[0, 10, 5, 0]] }, 'unknown file'],
    [{ ...layout, version: 2 }, 'version'],
  ]) {
    let threw = false;
    try {
      parseLayout(bad);
    } catch {
      threw = true;
    }
    check(threw, `map rejected: ${what}`);
  }
  const srv = await serve({ mounts: [['/l/', dir]] });
  try {
    const src = await new LayoutSource(`${srv.url}/l/web/disk.json`).open();
    eq(src.size, 65536, 'size from the map');
    check(src.key.startsWith(`layout:${srv.url}/l/web/disk.json|65536|`), 'key with the map URL and hash');
    const got = await src.read(0, 65536);
    eq(Buffer.compare(Buffer.from(got), Buffer.from(want)), 0, 'whole disk over HTTP Range');
    // With the DiskFeeder and 4 KiB blocks: the same bytes.
    const fed = new Map();
    const machine = { addDisk: () => 0, diskWanted: () => [...Array(16).keys()].map((b) => ({ disk: 0, block: b })), diskFill: (_d, b, bytes) => fed.set(b, bytes.slice()), diskFail: () => { throw new Error('failed'); } };
    const feeder = new DiskFeeder(machine);
    feeder.add(src, { blockSize: 4096 });
    await feeder.serve();
    for (let b = 0; b < 16; b++) eq(Buffer.compare(Buffer.from(fed.get(b)), Buffer.from(want.subarray(b * 4096, (b + 1) * 4096))), 0, `block ${b}`);
    // A server file differing from the map: rejected on open.
    writeFileSync(join(dir, 'big.img'), big.subarray(0, 100));
    let threw = false;
    try {
      await new LayoutSource(`${srv.url}/l/web/disk.json`).open();
    } catch {
      threw = true;
    }
    check(threw, 'file shorter than the map: rejected');
  } finally {
    await srv.close();
  }
});

test('Android boot phases (BootProgress)', () => {
  const p = new BootProgress();
  eq(p.phase, null, 'no phase at the start');
  eq(p.feed('[    0.000000][    T0] Booting Linux on physical CPU 0x0\n[    1.1][    T1] Run /init as ', 1).map((e) => e.phase), ['kernel'], 'split line');
  eq(p.feed('init process\n', 2).map((e) => e.phase), ['init'], 'line completed');
  // A zygote line without the second stage: skipped phases count as seen.
  eq(p.feed("[   84.3][    T1] init: starting service 'zygote'...\r\n", 84).map((e) => e.phase), ['init2', 'zygote'], 'skipped phases');
  eq(p.feed("init: starting service 'zygote'...\n", 90).length, 0, 'phase already seen');
  eq(p.feed("init: Control message: Processed ctl.start for 'idmap2d' from pid: 794 (system_server)\n", 193).map((e) => e.phase), ['surfaceflinger', 'system_server'], 'system_server');
  eq(p.feed('init: processing action (persist.sys.zram_enabled=1 && sys-boot-completed-set) from (x)\n', 585).map((e) => e.phase), ['booted'], 'boot finished');
  eq([p.phase, p.label, p.events.length, p.events.at(-1).guestSecs], ['booted', 'boot finished', PHASES.length - 1, 585], 'end of the boot');
  // The home screen is marked by whoever has adb (focused window).
  check(isHome('  mCurrentFocus=Window{5d2 u0 com.android.launcher3/com.android.launcher3.uioverlay.QuickstepLauncher}') && !isHome('  mCurrentFocus=Window{a1 u0 com.android.settings/com.android.settings.FallbackHome}'), 'home screen recognised from the launcher');
  eq(p.mark('home', 1600).map((e) => e.phase), ['home'], 'home screen marked');
  eq(p.mark('home', 1700).length, 0, 'home screen already marked');
  eq([p.phase, p.events.length], ['home', PHASES.length], 'final state');
  // Home screen drawn: distinct colours on the grid.
  const img = new Uint8Array(64 * 32 * 4);
  eq(gridColors(img, 64, 32), 1, 'black screen: one colour');
  for (let i = 0; i < 64 * 32; i++) img.set([i & 255, (i >> 3) & 255, 7], i * 4);
  eq(gridColors(img, 64, 32), 8, '16-pixel grid: 4x2 samples');
});

test('Android image versions and app colours (ADR 0032)', () => {
  eq(DEFAULT_MANIFEST, ANDROID_VERSIONS[0].manifest, 'the first version is the default');
  check(DEFAULT_MANIFEST.endsWith('/aosp/android-15.0.0_r36-BP1A.250505.005.D1-f08b79e/manifest.json'), `default image f08b79e: ${DEFAULT_MANIFEST}`);
  for (const old of ['8b519e5', '64fcd35', 'bd09e2f']) {
    check(ANDROID_VERSIONS.some((v) => v.version.endsWith(`-${old}`) && v.manifest.endsWith(`/aosp/${v.version}/manifest.json`)), `${old} still selectable`);
  }
  // The test app's blue: the right order only (the default image converts to BGRX).
  const blue = [0x15, 0x65, 0xc0];
  check(colorSeen([0x15, 0x65, 0xc0], blue) && colorSeen([0x1a, 0x60, 0xc6], blue), 'blue, also within the tolerance');
  check(!colorSeen([0xc0, 0x65, 0x15], blue), 'red and blue swapped is not the app colour any more');
  check(!colorSeen(null, blue) && !colorSeen([0x15, 0x65, 0xd0], blue), 'no pixel, or too far');
});

test('device profiles: starters, boot parameters, adb commands, rejections (ADR 0035)', () => {
  const load = (id) => parseProfile(readFileSync(join(root, 'web/app/profiles', `${id}.json`), 'utf8'));
  // The same strings as the Rust twin (vetro_machine::profile tests, EXPECTED).
  const expected = {
    light: 'androidboot.lcd_density=180',
    default: '',
    phone: 'androidboot.lcd_density=320 androidboot.serialno=VETROPHONE01 androidboot.hardware.sku=phone',
    'small-phone': 'androidboot.serialno=VETROSMALL01 androidboot.hardware.sku=small-phone',
    tablet: 'androidboot.lcd_density=213 androidboot.serialno=VETROTABLET1 androidboot.hardware.sku=tablet',
  };
  eq(STARTER_PROFILES, Object.keys(expected), 'starter list');
  eq(DEFAULT_PROFILE, 'light', 'the app\'s default profile (ADR 0039)');
  const light = load('light');
  eq(profileMachine(light), { ...ANDROID_MACHINE, width: 960, height: 600 }, 'light machine');
  eq(profileAdbCommands(light), [], 'light adb commands');
  for (const id of STARTER_PROFILES) {
    const p = load(id);
    eq(p.id, id, `${id}: id`);
    eq(profileAndroidParams(p), expected[id], `${id}: androidboot parameters`);
    eq(profileBootParams(p), [ANDROID_PARAMS, expected[id]].filter(Boolean).join(' '), `${id}: bootloader parameters`);
  }
  // The default profile keeps the prebuilt snapshot's key (ADR 0031): same machine, same parameters, no adb commands.
  const def = load('default');
  eq(profileMachine(def), ANDROID_MACHINE, 'default machine');
  eq(profileBootParams(def), ANDROID_PARAMS, 'default parameters');
  eq(profileAdbCommands(def), [], 'default adb commands');
  const phone = load('phone');
  eq(profileMachine(phone), { ...ANDROID_MACHINE, ramMiB: 2048, width: 720, height: 1280 }, 'phone machine');
  eq(profileAdbCommands(phone), ['cmd alarm set-timezone UTC', "settings put global device_name 'Vetro Phone'"], 'phone adb commands');
  eq(profileMachine(load('small-phone')).ramMiB, 1536, 'small phone RAM');
  // What the guest reports, read back.
  const report = parseProfileReport('size=720x1280\ndensity=320\nserial=VETROPHONE01\nsku=phone\ntimezone=UTC\ndeviceName=Vetro Phone\n');
  eq(profileMismatches(phone, report), [], 'matching report');
  eq(profileMismatches(phone, { ...report, size: '1280x800' }), ['size: "1280x800" instead of "720x1280"'], 'wrong size');
  check(PROFILE_REPORT.includes('wm size') && PROFILE_REPORT.includes('ro.sf.lcd_density'), 'report command');
  // Defaults, locale and time zone.
  const minimal = { vetroProfile: 1, id: 'x', name: 'X', screen: { width: 720, height: 1280, density: 320 }, ramMiB: 2048 };
  const m = parseProfile(JSON.stringify(minimal));
  eq([m.locale, m.timezone, m.device], ['en-US', null, { name: null, serial: 'VETRO00001', sku: null }], 'image defaults');
  eq(profileAndroidParams(m), 'androidboot.lcd_density=320', 'only the density');
  eq(profileAdbCommands(parseProfile({ ...minimal, locale: 'it-IT', timezone: 'Europe/Rome' })),
    ['cmd alarm set-timezone Europe/Rome', 'su 0 setprop persist.sys.locale it-IT'], 'locale and time zone');
  for (const ok of ['en', 'en-US', 'zh-Hant-TW', 'es-419', 'sr-Latn']) parseProfile({ ...minimal, locale: ok });
  for (const ok of ['UTC', 'Europe/Rome', 'America/Argentina/Buenos_Aires', 'Etc/GMT+3']) parseProfile({ ...minimal, timezone: ok });
  // Rejections: the field of the error (the same cases as the Rust tests).
  const error = (x) => {
    try {
      parseProfile(x);
    } catch (e) {
      check(e instanceof ProfileError, `not a ProfileError: ${e}`);
      return e;
    }
    return { field: 'accepted', message: '' };
  };
  const field = (x) => error(x).field;
  const v2 = error({ ...minimal, vetroProfile: 2 });
  eq(v2.field, 'vetroProfile', 'newer version');
  check(/needs a newer Vetro/.test(v2.message), `newer version message: ${v2.message}`);
  const { vetroProfile, ...noVersion } = minimal;
  check(vetroProfile === 1, 'minimal version');
  const cases = [
    ['[1]', '(file)'], ['{', '(file)'], [noVersion, 'vetroProfile'], [{ ...minimal, vetroProfile: '1' }, 'vetroProfile'], [{ ...minimal, vetroProfile: 0 }, 'vetroProfile'],
    [{ ...minimal, colour: 1 }, 'colour'], [{ ...minimal, screen: { ...minimal.screen, dpi: 1 } }, 'screen.dpi'],
    [{ ...minimal, id: 'Phone' }, 'id'], [{ ...minimal, id: undefined }, 'id'], [{ ...minimal, name: "it's" }, 'name'],
    [{ ...minimal, screen: { ...minimal.screen, width: 721 } }, 'screen.width'], [{ ...minimal, screen: { ...minimal.screen, width: 100 } }, 'screen.width'],
    [{ ...minimal, screen: { ...minimal.screen, height: 1280.5 } }, 'screen.height'], [{ ...minimal, screen: { width: 720, density: 320 } }, 'screen.height'],
    [{ ...minimal, screen: { ...minimal.screen, density: 1000 } }, 'screen.density'],
    [{ ...minimal, ramMiB: 2000 }, 'ramMiB'], [{ ...minimal, ramMiB: 8192 }, 'ramMiB'], [{ ...minimal, ramMiB: '2048' }, 'ramMiB'],
    [{ ...minimal, locale: 'english' }, 'locale'], [{ ...minimal, timezone: 'Rome' }, 'timezone'], [{ ...minimal, timezone: 'Europe/Rome; reboot' }, 'timezone'],
    [{ ...minimal, device: { serial: 'VETRO 1' } }, 'device.serial'], [{ ...minimal, device: { sku: 'a b' } }, 'device.sku'],
    [{ ...minimal, device: { name: '$(reboot)' } }, 'device.name'], [{ ...minimal, device: { model: 'x' } }, 'device.model'],
  ];
  for (const [x, f] of cases) eq(field(typeof x === 'string' ? x : JSON.stringify(x)), f, `rejected ${typeof x === 'string' ? x : JSON.stringify(x)}`);
  eq(profileUrl('phone', 'https://example.org/app/'), 'https://example.org/app/profiles/phone.json', 'starter URL');
});

test('user guide Markdown (tools/pages/markdown.mjs)', () => {
  eq(inline('**bold**, _it_, *em*, `a<b>` and [x](phone.md#touch) ![alt](images/a.jpg) snake_case_name'),
    '<strong>bold</strong>, <em>it</em>, <em>em</em>, <code>a&lt;b&gt;</code> and <a href="phone.html#touch">x</a> <img src="images/a.jpg" alt="alt" loading="lazy"> snake_case_name', 'inline');
  eq([linkTarget('README.md'), linkTarget('faq.md#memory'), linkTarget('https://x.org/a.md'), linkTarget('#top'), linkTarget('../x/README.md#a')],
    ['index.html', 'faq.html#memory', 'https://x.org/a.md', '#top', '../x/index.html#a'], 'link targets');
  eq(slug("What's in the browser? (OPFS)"), 'whats-in-the-browser-opfs', 'slug');
  const md = [
    '# Getting started', '', 'One line', 'and two.', '', '## A `code` heading', '',
    '- one', '- two', '  continued', '  - nested', '- three', '', '1. first', '2. second', '',
    '> **Note:** quoted', '', '| a | b |', '|---|---|', '| 1 | `x|` |', '', '```sh', 'echo <hi>', '```', '', '---',
  ].join('\n');
  const { html, title } = markdownToHtml(md);
  eq(title, 'Getting started', 'title');
  eq(html.split('\n'), [
    '<h1 id="getting-started">Getting started</h1>',
    '<p>One line and two.</p>',
    '<h2 id="a-code-heading">A <code>code</code> heading</h2>',
    '<ul><li>one</li><li>two continued<ul><li>nested</li></ul></li><li>three</li></ul>',
    '<ol><li>first</li><li>second</li></ol>',
    '<blockquote><p><strong>Note:</strong> quoted</p></blockquote>',
    '<div class="table"><table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td><code>x|</code></td></tr></tbody></table></div>',
    '<pre><code class="language-sh">echo &lt;hi&gt;</code></pre>',
    '<hr>',
  ], 'blocks');
  // Every page of the guide converts, and every relative link points at a page or image that exists.
  const dir = join(root, 'docs/user');
  const pages = readdirSync(dir).filter((f) => f.endsWith('.md'));
  check(pages.includes('README.md') && pages.length >= 10, `docs/user: ${pages}`);
  const guide = new Map(pages.map((f) => [linkTarget(f), markdownToHtml(readFileSync(join(dir, f), 'utf8'))]));
  const ids = (name) => new Set([...guide.get(name).html.matchAll(/ id="([^"]+)"/g)].map((m) => m[1]));
  for (const [name, { html: h, title: t }] of guide) {
    check(t, `${name}: no level-1 heading`);
    for (const [, href, anchor] of h.matchAll(/(?:href|src)="([^"#]*)(?:#([^"]*))?"/g)) {
      if (/^[a-z]+:/.test(href) || href.startsWith('../')) continue;
      const target = href || name;
      check(guide.has(target) || existsSync(join(dir, target)), `${name}: broken link ${href}`);
      if (anchor) check(ids(target).has(anchor), `${name}: no #${anchor} in ${target}`);
    }
  }
});

test('APK: ZIP and binary manifest (testdata/tocco-manifest.axml)', async () => {
  const axml = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/tocco-manifest.axml')));
  const els = parseAxml(axml);
  eq(els[0].name, 'manifest', 'first element');
  eq(els[0].attrs.package, 'it.vetro.tocco', 'package');
  // A ZIP with the manifest compressed (deflate) and an uncompressed file.
  const deflated = new Uint8Array(await new Response(new Blob([axml]).stream().pipeThrough(new CompressionStream('deflate-raw'))).arrayBuffer());
  const zip = makeZip([['classes.dex', new TextEncoder().encode('dex\n035'), 0], ['AndroidManifest.xml', deflated, 8, axml.length]]);
  eq([...zipEntries(zip).keys()], ['classes.dex', 'AndroidManifest.xml'], 'ZIP files');
  eq(await apkInfo(zip), { package: 'it.vetro.tocco', versionName: '1.0', versionCode: 1, label: 'Tocco', launcher: 'it.vetro.tocco.Main', minSdk: 21, targetSdk: 35, icon: null, abis: [] }, 'information');
  const withLib = makeZip([['AndroidManifest.xml', axml, 0], ['lib/arm64-v8a/libx.so', new Uint8Array(4), 0], ['lib/armeabi-v7a/libx.so', new Uint8Array(4), 0]]);
  eq((await apkInfo(withLib)).abis, ['arm64-v8a', 'armeabi-v7a'], 'native code ABIs');
  let threw = false;
  try {
    await apkInfo(new Uint8Array(100));
  } catch {
    threw = true;
  }
  check(threw, 'not a ZIP: rejected');
});

/**
 * A synthetic resources.arsc: package 0x7f with type 1 (four configurations:
 * dense, 16-bit offsets, sparse, and one without the entry) and type 2 whose
 * entry 0 is a reference to 0x7f010000 and entry 1 a compact entry.
 */
const u8 = (n) => new Uint8Array(n);
const joinBytes = (parts) => {
  const out = u8(parts.reduce((a, p) => a + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
};

/** A UTF-8 string pool chunk (resources.arsc and binary XML). */
function stringPoolChunk(strings) {
  const enc = new TextEncoder();
  const data = [];
  const offsets = [];
  let at = 0;
  for (const str of strings) {
    const b = enc.encode(str);
    const e = joinBytes([new Uint8Array([str.length, b.length]), b, u8(1)]);
    offsets.push(at);
    data.push(e);
    at += e.length;
  }
  let body = joinBytes(data);
  if (body.length % 4) body = joinBytes([body, u8(4 - (body.length % 4))]);
  const pool = u8(28 + 4 * strings.length + body.length);
  const pv = new DataView(pool.buffer);
  pv.setUint16(0, 0x0001, true);
  pv.setUint16(2, 28, true);
  pv.setUint32(4, pool.length, true);
  pv.setUint32(8, strings.length, true);
  pv.setUint32(16, 0x100, true);
  pv.setUint32(20, 28 + 4 * strings.length, true);
  offsets.forEach((o, i) => pv.setUint32(28 + 4 * i, o, true));
  pool.set(body, 28 + 4 * strings.length);
  return pool;
}

/**
 * A binary XML (AXML): `tree` is [name, { attr: [dataType, data] }, children].
 * Attribute values are typed (no raw strings), like compiled resources.
 */
function makeAxml(tree) {
  const strings = [];
  const str = (x) => (strings.includes(x) ? strings.indexOf(x) : strings.push(x) - 1);
  const chunks = [];
  const walk = ([name, attrs, children = []]) => {
    const list = Object.entries(attrs);
    const c = u8(36 + 20 * list.length);
    const v = new DataView(c.buffer);
    v.setUint16(0, 0x0102, true);
    v.setUint16(2, 16, true);
    v.setUint32(4, c.length, true);
    v.setUint32(12, 0xffffffff, true);
    v.setUint32(16, 0xffffffff, true);
    v.setUint32(20, str(name), true);
    v.setUint16(24, 20, true);
    v.setUint16(26, 20, true);
    v.setUint16(28, list.length, true);
    list.forEach(([k, [type, data]], i) => {
      const a = 36 + 20 * i;
      v.setUint32(a, 0xffffffff, true);
      v.setUint32(a + 4, str(k), true);
      v.setUint32(a + 8, 0xffffffff, true);
      v.setUint16(a + 12, 8, true);
      v.setUint8(a + 15, type);
      v.setUint32(a + 16, data >>> 0, true);
    });
    chunks.push(c);
    for (const ch of children) walk(ch);
    const e = u8(24);
    const ev = new DataView(e.buffer);
    ev.setUint16(0, 0x0103, true);
    ev.setUint16(2, 16, true);
    ev.setUint32(4, 24, true);
    ev.setUint32(16, 0xffffffff, true);
    ev.setUint32(20, str(name), true);
    chunks.push(e);
  };
  walk(tree);
  const body = joinBytes([stringPoolChunk(strings), ...chunks]);
  const head = u8(8);
  const hv = new DataView(head.buffer);
  hv.setUint16(0, 0x0003, true);
  hv.setUint16(2, 8, true);
  hv.setUint32(4, 8 + body.length, true);
  return joinBytes([head, body]);
}

function makeArsc() {
  const strings = ['res/mdpi.png', 'res/xxxhdpi.webp', 'res/anydpi.xml', 'res/hdpi.png', 'res/compact.png'];
  const join = joinBytes;
  const pool = stringPoolChunk(strings);
  // A type chunk: entries [dataType, data] or ['compact', dataType, data] (null = no entry).
  const typeChunk = (id, density, entries, flags = 0) => {
    const HS = 20 + 64;
    const count = entries.length;
    const present = entries.map((e, i) => [i, e]).filter(([, e]) => e);
    const table = flags & 1 ? 4 * present.length : flags & 2 ? 2 * count : 4 * count;
    const start = HS + ((table + 3) & ~3);
    const size = start + 16 * present.length;
    const b = u8(size);
    const v = new DataView(b.buffer);
    v.setUint16(0, 0x0201, true);
    v.setUint16(2, HS, true);
    v.setUint32(4, size, true);
    v.setUint8(8, id);
    v.setUint8(9, flags);
    v.setUint32(12, flags & 1 ? present.length : count, true);
    v.setUint32(16, start, true);
    v.setUint32(20, 64, true);
    v.setUint16(20 + 14, density, true);
    let k = 0;
    entries.forEach((e, i) => {
      const off = e ? 16 * present.findIndex(([j]) => j === i) : -1;
      if (flags & 1) {
        if (e) {
          v.setUint16(HS + 4 * k, i, true);
          v.setUint16(HS + 4 * k + 2, off / 4, true);
          k++;
        }
      } else if (flags & 2) v.setUint16(HS + 2 * i, e ? off / 4 : 0xffff, true);
      else v.setUint32(HS + 4 * i, e ? off : 0xffffffff, true);
      if (!e) return;
      const p = start + off;
      if (e[0] === 'compact') {
        v.setUint16(p, 0, true);
        v.setUint16(p + 2, 0x0008 | (e[1] << 8), true);
        v.setUint32(p + 4, e[2], true);
      } else {
        v.setUint16(p, 8, true);
        v.setUint16(p + 8, 8, true);
        v.setUint8(p + 11, e[0]);
        v.setUint32(p + 12, e[1], true);
      }
    });
    return b;
  };
  const types = join([
    typeChunk(1, 160, [[3, 0], [3, 2]]),
    typeChunk(1, 640, [[3, 1], null], 2),
    typeChunk(1, 240, [null, [3, 3]], 1),
    typeChunk(1, 0xfffe, [[3, 2]]),
    typeChunk(2, 0, [[1, 0x7f010000], ['compact', 3, 4]]),
  ]);
  const pkg = u8(288 + types.length);
  const kv = new DataView(pkg.buffer);
  kv.setUint16(0, 0x0200, true);
  kv.setUint16(2, 288, true);
  kv.setUint32(4, pkg.length, true);
  kv.setUint32(8, 0x7f, true);
  pkg.set(types, 288);
  const all = u8(12 + pool.length + pkg.length);
  const av = new DataView(all.buffer);
  av.setUint16(0, 0x0002, true);
  av.setUint16(2, 12, true);
  av.setUint32(4, all.length, true);
  av.setUint32(8, 1, true);
  all.set(pool, 12);
  all.set(pkg, 12 + pool.length);
  return all;
}

test('APK: resources.arsc and the icon (catalog tool, ADR 0033)', async () => {
  const arsc = makeArsc();
  const table = parseArsc(arsc);
  eq(table.strings.length, 5, 'global strings');
  const icon = resolveResource(table, 0x7f010000).map((r) => [r.density, r.string]);
  eq(icon, [[160, 'res/mdpi.png'], [640, 'res/xxxhdpi.webp'], [0xfffe, 'res/anydpi.xml']], 'id 0x7f010000: dense, 16-bit offsets, anydpi');
  eq(resolveResource(table, 0x7f010001).map((r) => [r.density, r.string]), [[160, 'res/anydpi.xml'], [240, 'res/hdpi.png']], 'id 0x7f010001: missing in one config, sparse in another');
  eq(resolveResource(table, 0x7f020000).map((r) => r.string), ['res/mdpi.png', 'res/xxxhdpi.webp', 'res/anydpi.xml'], 'a reference is followed');
  eq(resolveResource(table, 0x7f020001).map((r) => r.string), ['res/compact.png'], 'compact entry (Android 14)');
  eq(resolveResource(table, 0x7f030000), [], 'unknown type');
  // The icon: the densest raster present in the ZIP, never the XML.
  // Recognised by content, not by name (shrunk APKs have no extensions).
  const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const webp = new TextEncoder().encode('RIFF\x04\x00\x00\x00WEBP');
  const zip = makeZip([['resources.arsc', arsc, 0], ['res/mdpi.png', png, 0], ['res/xxxhdpi.webp', webp, 0], ['res/anydpi.xml', new Uint8Array(2), 0]]);
  const got = await apkIcon(zip, { icon: 0x7f010000 });
  eq([got.path, got.type, got.density, [...got.bytes]], ['res/xxxhdpi.webp', 'image/webp', 640, [...webp]], 'icon: highest density raster');
  const notImage = makeZip([['resources.arsc', arsc, 0], ['res/mdpi.png', png, 0], ['res/xxxhdpi.webp', new Uint8Array(12), 0]]);
  eq((await apkIcon(notImage, { icon: 0x7f010000 })).path, 'res/mdpi.png', 'icon: a file that is no image is skipped');
  const onlyMdpi = makeZip([['resources.arsc', arsc, 0], ['res/mdpi.png', png, 0]]);
  eq((await apkIcon(onlyMdpi, { icon: 0x7f010000 })).path, 'res/mdpi.png', 'icon: files missing from the ZIP are skipped');
  eq(await apkIcon(makeZip([['resources.arsc', arsc, 0]]), { icon: 0x7f010000 }), null, 'icon: no raster, null');
  eq(await apkIcon(zip, { icon: null }), null, 'icon: none in the manifest');
  // Adaptive icon (id 0x7f010001 = res/anydpi.xml, its raster res/hdpi.png
  // absent): a colour background and a raster foreground (0x7f010000) become an SVG.
  const adaptive = (bg) => makeAxml(['adaptive-icon', {}, [['background', { drawable: bg }], ['foreground', { drawable: [0x01, 0x7f010000] }]]]);
  eq(parseAxml(adaptive([0x1c, 0xff112233]))[2].attrs.drawable, 0x7f010000, 'binary XML builder');
  const zipA = (xml) => makeZip([['resources.arsc', arsc, 0], ['res/anydpi.xml', xml, 0], ['res/xxxhdpi.webp', webp, 0]]);
  const svg = await apkIcon(zipA(adaptive([0x1c, 0xff112233])), { icon: 0x7f010001 });
  const text = new TextDecoder().decode(svg.bytes);
  eq([svg.type, svg.path], ['image/svg+xml', 'res/anydpi.xml'], 'adaptive icon: an SVG');
  check(text.includes('fill="#112233"') && text.includes(`data:image/webp;base64,${btoa(String.fromCharCode(...webp))}`) && text.includes('viewBox="18 18 72 72"'), `adaptive icon SVG: ${text}`);
  // A foreground that is itself XML without a raster (a vector): no icon.
  const vector = makeAxml(['adaptive-icon', {}, [['foreground', { drawable: [0x01, 0x7f010001] }]]]);
  eq(await apkIcon(zipA(vector), { icon: 0x7f010001 }), null, 'adaptive icon with a vector foreground: null');
  const white = new TextDecoder().decode((await apkIcon(zipA(adaptive([0x01, 0x0106000b])), { icon: 0x7f010001 })).bytes);
  check(white.includes('fill="#ffffff"'), 'adaptive icon: a framework colour (android:color/white) as background');
});

/** A valid catalog entry (the fields of catalog/v1.json). */
const catalogEntry = (over = {}) => ({
  id: 'flowit', name: 'Flowit', package: 'com.bytehamster.flowitgame', version: '4.3', versionCode: 403, apk: 'apks/flowit/flowit-403.apk',
  size: 3135618, sha256: 'a'.repeat(64), license: 'GPL-3.0-only', source: 'https://f-droid.org/packages/com.bytehamster.flowitgame/',
  icon: 'icons/flowit-403.png', description: 'Block puzzle', minImage: 'android-15.0.0_r36', advanced: false, ...over,
});

test('catalog: parsing and validation (ADR 0033)', () => {
  const base = 'https://r2.example/catalog/v1.json';
  const e = parseEntry(catalogEntry(), base);
  eq([e.apk, e.icon, e.origin, e.advanced], ['https://r2.example/catalog/apks/flowit/flowit-403.apk', 'https://r2.example/catalog/icons/flowit-403.png', null, false], 'URLs resolved against the catalog');
  const bad = [
    [{ id: 'Flowit' }, 'id'], [{ package: 'flowit' }, 'package'], [{ size: 0 }, 'size'], [{ size: 1.5 }, 'size'], [{ sha256: 'xyz' }, 'sha256'],
    [{ license: 'GPL 3' }, 'license'], [{ source: 'http://x.example/' }, 'source'], [{ apk: 'ftp://x/a.apk' }, 'apk'], [{ minImage: '15' }, 'minImage'],
    [{ advanced: 'yes' }, 'advanced'], [{ name: '' }, 'name'], [{ description: undefined }, 'description'], [{ versionCode: '403' }, 'versionCode'],
  ];
  for (const [over, field] of bad) {
    let msg = null;
    try {
      parseEntry(catalogEntry(over), base);
    } catch (err) {
      msg = err.message;
    }
    check(msg?.includes(field), `bad ${field} not rejected (${msg})`);
  }
  eq(parseEntry(catalogEntry({ license: 'MIT OR Apache-2.0', icon: null, minImage: null, sha256: 'A'.repeat(64) }), base).sha256, 'a'.repeat(64), 'SPDX expression, no icon, sha256 lowercased');
  const apps = [catalogEntry(), catalogEntry({ id: 'x', sha256: '1' }), catalogEntry({ id: 'dup' }), catalogEntry({ id: 'two', package: 'org.example.two', advanced: true }), 7];
  const c = parseCatalog(JSON.stringify({ format: CATALOG_FORMAT, updated: '2026-09-27', apps, extra: 1 }), base);
  eq(c.apps.map((a) => a.id), ['flowit', 'two'], 'valid entries kept in order');
  eq(c.problems.length, 3, 'bad, duplicate and non-object entries named');
  for (const [json, what] of [[{ format: 2, apps: [] }, 'format'], [{ format: 1 }, 'app list'], ['null', 'not an object']]) {
    let threw = false;
    try {
      parseCatalog(typeof json === 'string' ? json : JSON.stringify(json), base);
    } catch {
      threw = true;
    }
    check(threw, `catalog without ${what} accepted`);
  }
});

test('catalog: minimum image version', () => {
  eq(imageRelease('android-15.0.0_r36-BP1A.250505.005.D1-bd09e2f'), [15, 0, 0, 36], 'release of an image version');
  eq(imageRelease('android-15.0.0_r36'), [15, 0, 0, 36], 'bare release');
  eq(imageRelease('local-build'), null, 'not a release');
  const img = 'android-15.0.0_r36-BP1A.250505.005.D1-bd09e2f';
  const mins = [null, 'android-15.0.0_r36', 'android-15.0.0_r35', 'android-14.0.0_r50', 'android-15.0.0_r37', 'android-15.1.0_r1', 'android-16.0.0_r1'];
  eq(mins.map((m) => imageSatisfies(m, img)), [true, true, true, true, false, false, false], 'comparison by release');
  eq([imageSatisfies('android-16.0.0_r1', null), imageSatisfies('android-16.0.0_r1', 'my-image')], [true, true], 'unknown image version: pm decides');
});

test('catalog: SHA-256 check and verified download', async () => {
  const apk = new TextEncoder().encode('PK\x03\x04 not really an APK, but bytes');
  const entry = { id: 't', apk: 'https://r2.example/t.apk', size: apk.length, sha256: createHash('sha256').update(apk).digest('hex') };
  await verifyApk(apk, entry);
  const fails = async (p, what, text) => {
    let msg = null;
    try {
      await p;
    } catch (err) {
      msg = err.message;
    }
    check(msg?.includes(text), `${what}: ${msg}`);
  };
  const other = apk.slice();
  other[5] ^= 1;
  await fails(verifyApk(other, entry), 'one byte changed', 'SHA-256');
  await fails(verifyApk(apk.subarray(1), entry), 'shorter', 'bytes instead of');
  // A fake fetch with a body in pieces.
  const fakeFetch = (body, status = 200) => async () => new Response(new ReadableStream({
    start(ctl) {
      for (let at = 0; at < body.length; at += 7) ctl.enqueue(body.slice(at, at + 7));
      ctl.close();
    },
  }), { status });
  const seen = [];
  const got = await downloadApk(entry, { fetch: fakeFetch(apk), onProgress: (p) => seen.push(p.loaded) });
  eq([...got], [...apk], 'downloaded bytes');
  check(seen[0] === 0 && seen.at(-1) === apk.length && seen.every((x, i) => i === 0 || x > seen[i - 1]), `progress: ${seen}`);
  await fails(downloadApk(entry, { fetch: fakeFetch(other) }), 'tampered download', 'SHA-256');
  await fails(downloadApk(entry, { fetch: fakeFetch(new Uint8Array(apk.length + 10)) }), 'longer download', 'longer');
  await fails(downloadApk(entry, { fetch: fakeFetch(apk.subarray(3)) }), 'truncated download', 'bytes instead of');
  await fails(downloadApk(entry, { fetch: fakeFetch(apk, 404) }), 'status 404', 'status 404');
});

test('catalog: install states and installed packages', () => {
  eq(STATES, ['absent', 'downloading', 'installing', 'installed', 'failed'], 'states');
  const e = { id: 'flowit', package: 'com.bytehamster.flowitgame', size: 1000, versionCode: 403 };
  let st = initialState();
  const step = (ev) => (st = nextState(st, ev, e));
  const same = (ev, what) => {
    const before = st;
    check(nextState(st, ev, e) === before, `${what}: state changed`);
  };
  same({ type: 'progress', loaded: 5 }, 'progress while absent');
  same({ type: 'installed' }, 'installed while absent');
  same({ type: 'failed', error: 'x' }, 'failure while absent');
  step({ type: 'download', total: 1000 });
  eq([st.phase, st.fraction], ['downloading', 0], 'download started');
  same({ type: 'download' }, 'second download while downloading');
  same({ type: 'packages', packages: new Map() }, 'package list while busy');
  step({ type: 'progress', loaded: 250 });
  eq([st.loaded, st.fraction], [250, 0.25], 'progress');
  step({ type: 'downloaded' });
  eq([st.phase, st.fraction], ['installing', null], 'verified: installing');
  step({ type: 'pushing', fraction: 0.5 });
  eq(st.fraction, 0.5, 'push progress');
  step({ type: 'installed', versionCode: 403 });
  eq([st.phase, st.installedCode], ['installed', 403], 'installed');
  step({ type: 'packages', packages: new Map() });
  eq(st.phase, 'absent', 'removed from the device: absent again');
  step({ type: 'download' });
  step({ type: 'failed', error: 'SHA-256 differs' });
  eq([st.phase, st.error], ['failed', 'SHA-256 differs'], 'failure while downloading');
  step({ type: 'packages', packages: new Map() });
  eq(st.phase, 'failed', 'a failure stays until retried');
  step({ type: 'download' });
  eq([st.phase, st.error], ['downloading', null], 'retry from failed');
  step({ type: 'downloaded' });
  step({ type: 'failed', error: 'INSTALL_FAILED' });
  eq(st.phase, 'failed', 'failure while installing');
  const pk = parsePackages('package:com.android.settings versionCode:35\npackage:com.bytehamster.flowitgame versionCode:402\r\npackage:org.old\nnoise\n');
  eq([...pk], [['com.android.settings', 35], ['com.bytehamster.flowitgame', 402], ['org.old', null]], 'pm list packages');
  st = nextState(initialState(), { type: 'packages', packages: pk }, e);
  eq([st.phase, st.outdated, st.installedCode], ['absent', true, 402], 'older version installed: Update');
  st = nextState(initialState(), { type: 'packages', packages: new Map([[e.package, 403]]) }, e);
  eq(st.phase, 'installed', 'same version installed');
  st = nextState(initialState(), { type: 'packages', packages: new Map([[e.package, null]]) }, e);
  eq(st.phase, 'installed', 'installed, version unknown');
  eq([sizeText(900), sizeText(1_468_006), sizeText(75_509_560), sizeText(381_736_755)], ['1 KiB', '1.4 MiB', '72 MiB', '364 MiB'], 'sizes');
});

test('snapshot cache: metadata and reading into a given buffer', async () => {
  const store = SnapshotStore.memory();
  await store.save('k', { why: 'prova' }, new Uint8Array([5, 6, 7, 8]));
  const meta = await store.loadMeta('k');
  eq([meta.size, meta.why], [4, 'prova'], 'metadata');
  const view = new Uint8Array(4);
  await store.readInto('k', view);
  eq([...view], [5, 6, 7, 8], 'bytes in the buffer');
  eq(await store.loadMeta('altro'), null, 'missing key');
});

test('prebuilt snapshot: lookup, verified download, retries, resume, damage (ADR 0031)', async () => {
  const dir = join(root, 'target/web-test/prebuilt/snapshots');
  mkdirSync(dir, { recursive: true });
  const size = 2 * PREBUILT_CHUNK + 12345;
  const data = new Uint8Array(size);
  let x = 1;
  for (let i = 0; i < size; i += 4) {
    x = (x * 1103515245 + 12345) >>> 0;
    data[i] = x >>> 24;
  }
  const key = 'k'.repeat(32);
  const chunks = [];
  for (let at = 0; at < size; at += PREBUILT_CHUNK) chunks.push(createHash('sha256').update(data.subarray(at, at + PREBUILT_CHUNK)).digest('hex'));
  const info = { format: PREBUILT_FORMAT, version: 1, key, size, sha256: createHash('sha256').update(data).digest('hex'), chunk: PREBUILT_CHUNK, chunks, meta: { why: 'prebuilt', steps: '7' } };
  writeFileSync(join(dir, `${key}.snap`), data);
  writeFileSync(join(dir, `${key}.json`), JSON.stringify(info));
  const bad = data.slice();
  bad[PREBUILT_CHUNK + 5] ^= 1;
  writeFileSync(join(dir, 'bad.snap'), bad);
  const log = [];
  const srv = await serve({ mounts: [['/img/', join(root, 'target/web-test/prebuilt')]], onRequest: (r) => log.push(r) });
  const manifest = `${srv.url}/img/manifest.json`;
  /** A fetch whose bodies break after `after` bytes (the first `times` times). */
  const breaking = (after, times) => async (url, opts) => {
    const res = await fetch(url, opts);
    if (times-- <= 0) return res;
    const reader = res.body.getReader();
    let sent = 0;
    return new Response(new ReadableStream({
      async pull(c) {
        const { done, value } = await reader.read();
        if (done) return c.close();
        if (sent + value.length > after) {
          reader.cancel();
          return c.error(new TypeError('network error'));
        }
        sent += value.length;
        c.enqueue(value);
      },
    }), { status: res.status, headers: res.headers });
  };
  try {
    eq(prebuiltInfoUrl(manifest, key), `${srv.url}/img/snapshots/${key}.json`, 'info URL next to the manifest');
    const none = await findPrebuilt(manifest, 'x'.repeat(32));
    check(!none.info && /no prebuilt snapshot/.test(none.missing), `404: ${JSON.stringify(none)}`);
    const found = await findPrebuilt(manifest, key);
    check(found.info?.size === size, `found: ${JSON.stringify(found.missing)}`);
    check(prebuiltProblem(info, 'y'.repeat(32))?.includes('key'), 'another key is refused');
    check(prebuiltProblem({ ...info, chunks: chunks.slice(1) }, key) !== null, 'chunk list of the wrong length');
    const url = prebuiltSnapUrl(manifest, key);

    // A break in the middle: retried with a Range from the verified chunk.
    let store = SnapshotStore.memory();
    let t = await store.downloadTarget(key);
    let seen = 0;
    const r1 = await downloadPrebuilt(info, url, t.file, { resume: t.resume, saveResume: t.saveResume, fetch: breaking(PREBUILT_CHUNK + (1 << 20), 1), onProgress: (p) => (seen = p.loaded) });
    eq([r1.retries, seen <= size], [1, true], 'one retry');
    await t.finish(info.meta);
    const meta = await store.loadMeta(key);
    eq([meta.size, meta.why], [size, 'prebuilt'], 'metadata written at the end');
    const got = new Uint8Array(size);
    await store.readInto(key, got);
    check(Buffer.compare(got, data) === 0, 'same bytes after a retry');
    check(log.some((r) => r.range === `bytes=${PREBUILT_CHUNK}-`), `range from the first unverified chunk: ${JSON.stringify(log.map((r) => r.range))}`);

    // Interrupted for good, then resumed in a new session.
    store = SnapshotStore.memory();
    t = await store.downloadTarget(key);
    let err = null;
    await downloadPrebuilt(info, url, t.file, { resume: t.resume, saveResume: t.saveResume, fetch: breaking(PREBUILT_CHUNK + (1 << 20), 99), retries: 0 }).catch((e) => (err = e));
    check(err, 'gives up without retries');
    t.close();
    eq(await store.loadMeta(key), null, 'no snapshot before it is complete');
    t = await store.downloadTarget(key);
    eq(t.resume, { sha256: info.sha256, verified: 1 }, 'resume state');
    log.length = 0;
    const r2 = await downloadPrebuilt(info, url, t.file, { resume: t.resume, saveResume: t.saveResume });
    eq(r2.resumedFrom, PREBUILT_CHUNK, 'resumed after the verified chunk');
    eq(r2.bytes, size - PREBUILT_CHUNK, 'only the rest downloaded');
    await t.finish(info.meta);
    await store.readInto(key, got);
    check(Buffer.compare(got, data) === 0, 'same bytes after resuming');

    // A damaged chunk is never written.
    store = SnapshotStore.memory();
    t = await store.downloadTarget(key);
    err = null;
    await downloadPrebuilt(info, `${srv.url}/img/snapshots/bad.snap`, t.file, { resume: t.resume, saveResume: t.saveResume }).catch((e) => (err = e));
    check(/chunk 1: sha256/.test(err?.message), `damaged chunk: ${err?.message}`);
    eq(t.file.getSize(), PREBUILT_CHUNK, 'only the good chunk written');
  } finally {
    await srv.close();
  }
});

run(async () => {
  for (const [name, f] of cases) {
    await f();
    count++;
    console.log(`ok: ${name}`);
  }
  console.log(`web unit tests: ${count} ok`);
});
