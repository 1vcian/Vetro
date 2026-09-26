#!/usr/bin/env node
// Test unitari dei pezzi JS dell'app web (senza kernel né browser):
// server con Range (tools/web-serve.mjs), sorgenti e DiskFeeder
// (web/node/disk.mjs), mappa dei tasti (web/app/keymap.mjs), terminale
// (web/app/terminal.mjs), persistenza (web/node/persist.mjs), lettore
// SQLite (anche con il WAL) e visualizzatori del gestore dei file
// (web/app/sqlite.mjs, web/app/files.mjs, M8), SQL e SharedPreferences
// del pannello, nomi non UTF-8 e argomenti SQL di vetro.mjs (ADR 0021),
// formati dei pannelli di analisi (web/app/analysis.mjs, M7/M10), the disk
// rebuilt from a map, Android boot phases and APK manifests
// (web/node/disk.mjs, android.mjs, apk.mjs, M5/M6).
//
//   node tests/web/unit.mjs

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { BlobSource, composePlan, composeRead, DiskFeeder, LayoutSource, MemoryCache, parseLayout, RangeSource } from '../../web/node/disk.mjs';
import { BootProgress, gridColors, isHome, PHASES } from '../../web/node/android.mjs';
import { apkInfo, parseAxml, zipEntries } from '../../web/node/apk.mjs';
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
import { check, root, run } from './lib.mjs';

const eq = (a, b, what) => check(JSON.stringify(a) === JSON.stringify(b), `${what}: ${JSON.stringify(a)} invece di ${JSON.stringify(b)}`);
let count = 0;
const cases = [];
const test = (name, f) => cases.push([name, f]);

test('parseRange', () => {
  eq(parseRange(undefined, 100), null, 'senza Range');
  eq(parseRange('bytes=0-9', 100), { start: 0, end: 9 }, 'intervallo');
  eq(parseRange('bytes=90-', 100), { start: 90, end: 99 }, 'aperto');
  eq(parseRange('bytes=-10', 100), { start: 90, end: 99 }, 'suffisso');
  eq(parseRange('bytes=95-200', 100), { start: 95, end: 99 }, 'oltre la fine');
  eq(parseRange('bytes=100-', 100), 'invalid', 'dalla fine');
  eq(parseRange('bytes=5-2', 100), 'invalid', 'rovesciato');
  eq(parseRange('bytes=0-1,4-5', 100), 'invalid', 'multiplo');
  eq(parseRange('items=0-1', 100), 'invalid', 'unità');
});

test('server: Range, HEAD, isolamento, percorsi', async () => {
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
    eq(r.status, 206, 'stato');
    eq(r.headers.get('content-range'), 'bytes 10-19/1000', 'Content-Range');
    eq([...new Uint8Array(await r.arrayBuffer())], [...data.subarray(10, 20)], 'byte');
    eq(r.headers.get('cross-origin-opener-policy'), 'same-origin', 'COOP');
    eq(r.headers.get('cross-origin-embedder-policy'), 'require-corp', 'COEP');
    r = await fetch(`${srv.url}/x.bin`);
    eq([r.status, (await r.arrayBuffer()).byteLength], [200, 1000], 'intero');
    r = await fetch(`${srv.url}/x.bin`, { headers: { Range: 'bytes=5000-' } });
    eq([r.status, r.headers.get('content-range')], [416, 'bytes */1000'], '416');
    r = await fetch(`${srv.url}/x.bin`, { method: 'HEAD' });
    eq([r.status, r.headers.get('content-length'), r.headers.get('accept-ranges')], [200, '1000', 'bytes'], 'HEAD');
    r = await fetch(`${srv.url}/w/`);
    eq([r.status, r.headers.get('content-type'), await r.text()], [200, 'text/html; charset=utf-8', '<p>ciao</p>'], 'indice');
    r = await fetch(`${srv.url}/w/..%2fsegreto.txt`);
    eq(r.status, 404, 'fuori dalla radice');
    r = await fetch(`${srv.url}/altro`);
    eq(r.status, 404, 'non montato');

    // Sorgenti.
    const src = await new RangeSource(`${srv.url}/w/a.bin`).open();
    eq(src.size, 1000, 'dimensione dal Content-Range');
    check(src.key.includes('|1000|"'), `chiave con ETag: ${src.key}`);
    eq([...(await src.read(995, 5))], [...data.subarray(995)], 'lettura');
    eq(src.stats, { requests: 2, bytes: 5 }, 'contatori');
    let failed = false;
    await src.read(990, 20).catch(() => (failed = true));
    check(failed, 'una lettura oltre la fine deve fallire');
    failed = false;
    await new RangeSource(`${srv.url}/nessuno`).open().catch(() => (failed = true));
    check(failed, 'una sorgente senza file deve fallire');
  } finally {
    await srv.close();
  }
  const blob = new BlobSource(new Blob([new Uint8Array([1, 2, 3, 4])]), 'b');
  eq([...(await blob.read(1, 2))], [2, 3], 'BlobSource');
});

/** Una Machine finta: registra le consegne e dà blocchi chiesti a comando. */
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
    if (this.fail) throw new Error('rete giù');
    return new Uint8Array(length).map((_, i) => ((offset + i) >> 12) & 0xff);
  }
}

test('DiskFeeder: cache, blocchi contigui, lettura anticipata, errori', async () => {
  const m = new FakeMachine();
  const f = new DiskFeeder(m);
  const src = new FakeSource(10 * 4096 + 700); // 10 blocchi e uno corto (512 dopo l'arrotondamento)
  const cache = new MemoryCache();
  cache.put(3, new Uint8Array(4096).fill(33));
  eq(f.add(src, { cache, blockSize: 4096, readahead: 1 }), 0, 'indice');
  eq(m.opt, { size: 10 * 4096 + 512, blockSize: 4096, maxBlocks: 0, readOnly: false }, 'disco aggiunto');
  eq(await f.serve(), 0, 'niente da fare');
  m.wanted = [{ disk: 0, block: 1 }, { disk: 0, block: 2 }, { disk: 0, block: 3 }, { disk: 0, block: 10 }];
  eq(await f.serve(), 4, 'chiesti');
  // 1 e 2 insieme, 3 dalla cache, 4 in anticipo (dopo 3), 10 (ultimo, corto).
  eq(src.reads, [[4096, 2 * 4096], [4 * 4096, 4096], [10 * 4096, 512]], 'letture');
  eq(m.filled.map((x) => x[1]).sort((a, b) => a - b), [1, 2, 3, 4, 10], 'consegnati');
  eq(m.filled.find((x) => x[1] === 3)[3], 33, 'il blocco 3 viene dalla cache');
  eq(m.filled.find((x) => x[1] === 10)[2], 512, 'ultimo blocco corto');
  check(cache.has(1) && cache.has(4) && cache.has(10), 'blocchi scaricati messi in cache');
  eq(f.stats.fromCache, 1, 'da cache');
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
  eq(bad.failed, [[0, 0]], 'solo il blocco chiesto fallisce (non quello in anticipo)');
});

test('keymap', () => {
  eq([evdevCode('KeyA'), evdevCode('Enter'), evdevCode('Space'), evdevCode('ArrowUp'), evdevCode('MetaLeft')], [30, 28, 57, 103, 125], 'codici');
  eq([evdevCode('Digit1'), evdevCode('Digit0'), evdevCode('F12'), evdevCode('NumpadEnter')], [2, 11, 88, 96], 'altri codici');
  eq(evdevCode('toString'), undefined, 'niente prototipo');
  eq(evdevCode('Nessuno'), undefined, 'sconosciuto');
  eq([BUTTONS[0], BUTTONS[1], BUTTONS[2]], [0x110, 0x112, 0x111], 'pulsanti');
  eq([absAxis(0), absAxis(1), absAxis(0.5), absAxis(-1), absAxis(2)], [0, 32767, 16384, 0, 32767], 'assi');
});

test('terminale', () => {
  const replies = [];
  const t = new Terminal({ onReply: (s) => replies.push(s) });
  const feed = (s) => t.feed(new TextEncoder().encode(s));
  feed('abc\r\nriga due\rRIGA\n');
  eq(t.lines, ['abc', 'RIGA due', ''], 'CR e LF');
  feed('~ # \x1b[6n');
  eq(replies, ['\x1b[3;5R'], 'risposta a ESC[6n');
  feed('xyz\b\b\x1b[K!');
  eq(t.lines[2], '~ # x!', 'backspace e cancellazione');
  feed('\x1b[1;32mverde\x1b[0m\x1b]0;titolo\x07.');
  eq(t.lines[2], '~ # x!verde.', 'colori e OSC ignorati');
  const e = new TextEncoder().encode('è');
  t.feed(e.subarray(0, 1));
  t.feed(e.subarray(1));
  eq(t.lines[2].endsWith('è'), true, 'UTF-8 spezzato');
  feed('\ta');
  eq(t.lines[2].length, 17, 'tabulazione');
  const k = (key, o = {}) => keyToBytes({ key, ctrlKey: false, altKey: false, metaKey: false, ...o });
  eq([k('a'), k('Enter'), k('Backspace'), k('ArrowUp'), k('c', { ctrlKey: true }), k('Shift'), k('v', { metaKey: true })],
    ['a', '\r', '\x7f', '\x1b[A', '\x03', null, null], 'tasti');
});

test('persistenza: MemFile, cache degli snapshot, chiavi', async () => {
  const f = new MemFile();
  f.write(new Uint8Array([1, 2, 3]), { at: 5000 });
  eq(f.getSize(), 5003, 'scrittura oltre la fine');
  eq([...readAll(f).subarray(4998)], [0, 0, 1, 2, 3], 'buco a zero');
  f.truncate(4999);
  f.truncate(5001);
  eq([...readAll(f).subarray(4998)], [0, 0, 0], 'troncato e riallungato a zero');
  const buf = new Uint8Array(4);
  eq(f.read(buf, { at: 4999 }), 2, 'lettura corta alla fine');

  const store = SnapshotStore.memory();
  eq(await store.load('k'), null, 'chiave assente');
  await store.save('k', { generations: [3, null], console: toBase64(new Uint8Array([0, 255, 10])) }, new Uint8Array([9, 8, 7]));
  const r = await store.load('k');
  eq([...r.bytes], [9, 8, 7], 'byte riletti');
  eq(r.meta.size, 3, 'dimensione nei metadati');
  eq([...fromBase64(r.meta.console)], [0, 255, 10], 'console in base64');
  eq(staleReason(r.meta, [{ generation: 3 }, null]), null, 'stessa generazione: vale');
  check(staleReason(r.meta, [{ generation: 4 }, null])?.includes('generazione 4'), 'disco andato avanti: non vale');
  await store.remove('k');
  eq(await store.load('k'), null, 'tolto');

  const a = await snapshotKey({ v: 2, disks: [{ id: 'x', size: 1 }], ram: 1024 });
  const b = await snapshotKey({ ram: 1024, disks: [{ size: 1, id: 'x' }], v: 2 });
  const c = await snapshotKey({ ram: 1024, disks: [{ size: 1, id: 'x' }], v: 3 });
  eq(a === b && a !== c && a.length === 32, true, 'chiave stabile rispetto all\'ordine, diversa per un valore');
});

test('SQLite: varint e CREATE TABLE', () => {
  eq(varint([0x05], 0).map(String), ['5', '1'], 'un byte');
  eq(varint([0x81, 0x00], 0).map(String), ['128', '2'], 'due byte');
  eq(varint([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], 0).map(String), ['18446744073709551615', '9'], 'nove byte');
  eq(parseCreateTable('CREATE TABLE t (id INTEGER PRIMARY KEY, "a b" TEXT, [c] INT, `d`, CHECK (c > 0))'),
    { columns: ['id', 'a b', 'c', 'd'], rowidAlias: 0, withoutRowid: false, pk: [0] }, 'alias del rowid e virgolette');
  eq(parseCreateTable('CREATE TABLE t (id INT PRIMARY KEY, x)').rowidAlias, -1, 'INT non è un alias');
  eq(parseCreateTable('CREATE TABLE t (k TEXT, v, PRIMARY KEY (k)) WITHOUT ROWID'),
    { columns: ['k', 'v'], rowidAlias: -1, withoutRowid: true, pk: [0] }, 'WITHOUT ROWID');
  eq(formatValue(new Uint8Array([0, 255])), "x'00ff' (2 byte)", 'BLOB');
  eq(formatValue(null), 'NULL', 'NULL');
});

test('SQLite: database vero (tests/web/testdata/prova.sqlite)', () => {
  const bytes = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/prova.sqlite')));
  check(isSqlite(bytes) && !isSqlite(bytes.subarray(0, 50)), 'riconoscimento');
  const db = new SqliteDb(bytes);
  eq(db.pageSize, 1024, 'pagine');
  eq(db.tables().map((t) => t.name), ['valori', 'molte righe', 'prefs'], 'tabelle');
  const v = db.rows('valori');
  eq(v.columns, ['id', 'nome', 'n', 'x', 'dati'], 'colonne');
  eq(v.rows.length, 7, 'righe');
  eq(v.rows[0], [1, 'nullo', null, null, null], 'NULL');
  eq(v.rows[1][2] === 0 && v.rows[1][3] === 1.5 && [...v.rows[1][4]].join() === '0,255', true, 'zero, reale, BLOB');
  eq(v.rows[2][2] === 1 && v.rows[2][3] === -2.25 && v.rows[2][4].length === 0, true, 'uno, BLOB vuoto');
  check(v.rows[3][2] === 9007199254740993n, `intero a 8 byte oltre 2^53: ${v.rows[3][2]}`);
  eq([v.rows[4][2], v.rows[4][3]], [-300000, 1e300], 'negativo a 3 byte, reale grande');
  eq(v.rows[5][1], 'àèìòù €', 'UTF-8');
  eq(v.rows[6][1], 'L'.repeat(5000), 'testo con pagine di overflow');
  const m = db.rows('molte righe', 1000);
  eq(m.columns, ['rowid', 'chiave', 'valore'], 'rowid senza alias');
  eq(m.rows.length, 600, 'b-tree a più livelli');
  eq(m.rows[599], [600, 'riga-0599', 599 * 599], 'ultima riga');
  eq(db.rows('molte righe', 10).rows.length, 10, 'limite');
  eq(db.rows('prefs').rows, [['anna', 'lingua', 'en'], ['anna', 'tema', 'scuro'], ['bruno', 'lingua', 'it']], 'WITHOUT ROWID in ordine di chiave');
  let err = null;
  try {
    new SqliteDb(new Uint8Array(200));
  } catch (e) {
    err = e.message;
  }
  eq(err, 'non è un database SQLite 3', 'file che non è SQLite');
});

test('SQLite: WAL (tests/web/testdata/wal.sqlite e -wal)', () => {
  const bytes = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/wal.sqlite')));
  const wal = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/wal.sqlite-wal')));
  // Senza WAL: com'era all'ultimo checkpoint.
  const old = new SqliteDb(bytes);
  eq(old.rows('t').rows, [[1, 'base-1'], [2, 'base-2'], [3, 'base-3'], [4, 'base-4'], [5, 'base-5']], 'senza WAL');
  eq(old.tables().map((t) => t.name), ['t'], 'tabelle senza WAL');
  const db = new SqliteDb(bytes, wal);
  check(db.walFrames > 0 && db.pageCount > old.pageCount, `WAL applicato: ${db.walFrames} frame, ${db.pageCount} pagine`);
  const t = db.rows('t');
  eq(t.rows, [[1, 'base-1'], [2, 'dal-wal'], [3, 'base-3'], [4, 'base-4'], [6, 'nuova']], 'righe con il WAL (come sqlite3)');
  eq(t.rowids, [1, 2, 3, 4, 6], 'rowid');
  eq(t.types[0], ['integer', 'text'], 'tipi');
  eq(db.tables().map((x) => x.name), ['t', 'altra'], 'tabella creata nel WAL');
  eq(db.rows('altra').rows.map((r) => r[1].length), [900, 900, 900, 900], 'righe della tabella nuova');
  // Il frame rovinato in coda non conta; un WAL tagliato a metà del primo
  // frame non vale; un WAL di un'altra dimensione di pagina si ignora.
  const w = walPages(wal, 1024);
  check((wal.length - 32) / (24 + 1024) === w.frames + 1, `frame validi ${w.frames} su ${(wal.length - 32) / 1048}`);
  eq(walPages(wal.subarray(0, 32 + 600), 1024), null, 'frame tagliato');
  eq(walPages(wal, 4096), null, 'altra pagina');
  const bad = wal.slice();
  bad[40] ^= 1;
  eq(walPages(bad, 1024), null, 'salt rovinato nel primo frame');
});

test('SQLite: SQL delle modifiche del pannello', () => {
  const db = new SqliteDb(new Uint8Array(readFileSync(join(root, 'tests/web/testdata/prova.sqlite'))));
  const v = db.rows('valori');
  eq(v.types[1], ['integer', 'text', 'integer', 'real', 'blob'], 'tipi di una riga');
  const u = updateCellSql(v, 1, 3, { type: 'real', value: 2 });
  eq(u.sql, 'UPDATE "valori" SET "x" = ?1 WHERE rowid = ?2', 'UPDATE con alias');
  eq(u.params[1], 2, 'rowid');
  const m = db.rows('molte righe');
  eq(updateCellSql(m, 0, 2, 'z').sql, 'UPDATE "molte righe" SET "valore" = ?1 WHERE rowid = ?2', 'senza alias: colonna spostata dal rowid');
  let err = null;
  try {
    updateCellSql(m, 0, 0, 1);
  } catch (e) {
    err = e.message;
  }
  check(err?.includes('rowid'), 'il rowid senza alias non si cambia');
  const pr = db.rows('prefs');
  const d = deleteRowSql(pr, 0);
  eq([d.sql, d.params], ['DELETE FROM "prefs" WHERE "utente" IS ?1 AND "chiave" IS ?2', ['anna', 'lingua']], 'WITHOUT ROWID: chiave');
  eq(deleteRowSql(v, 0), { sql: 'DELETE FROM "valori" WHERE rowid = ?1', params: [1] }, 'DELETE');
  eq(insertRowSql(v.table, { nome: 'n', x: 1.5 }), { sql: 'INSERT INTO "valori" ("nome", "x") VALUES (?1, ?2)', params: ['n', 1.5] }, 'INSERT');
  eq(insertRowSql({ name: 'a"b' }, {}).sql, 'INSERT INTO "a""b" DEFAULT VALUES', 'INSERT vuoto e virgolette');
});

test('SharedPreferences: XML di Android', () => {
  // Un file come lo scrive Android (FastXmlSerializer), con tutti i tipi.
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
  eq(p.map((e) => e.type), ['string', 'int', 'long', 'float', 'boolean', 'set', 'set', 'null', 'string'], 'tipi');
  eq(p[0].value, 'Vetro & <co> "x" \nriga', 'entità');
  eq(p[5].value, ['a', 'b c'], 'set');
  eq(prefsToXml(p), xml, 'riletto e riscritto: stessi byte');
  eq(prefsToXml([]), "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map />\n", 'mappa vuota');
  eq(prefsToXml([{ type: 'string', name: 't', value: 'fine\n' }]).split('\n')[2], '    <string name="t">fine&#10;    </string>', 'testo che finisce con \\n (come FastXmlSerializer)');
  eq(parsePrefs('<a/>'), null, 'radice diversa da map');
  eq(parsePrefs('<map><int-array name="x" num="0" /></map>'), null, 'tipo non gestito');
  eq(parsePrefs('<!-- c --><map>\n<string name="a"><![CDATA[<x>]]></string></map>')[0].value, '<x>', 'commento e CDATA');
  for (const bad of ['<map>', '<map><int name="a" value="1"></map>', '<map a=1/>', '<map>&nope;</map>', '<map/><x/>']) {
    let e = null;
    try {
      parseXml(bad);
    } catch (x) {
      e = x.message;
    }
    check(e?.startsWith('XML non valido'), `XML rotto accettato: ${bad}`);
  }
  eq(['7', '+7', '-2147483648'].map((x) => checkPrefValue('int', x)), ['7', '7', '-2147483648'], 'int');
  eq(checkPrefValue('long', '9223372036854775807'), '9223372036854775807', 'long');
  eq(['1', '1.5f', '0.1', '1e10', '-0', '1e-5', '3.4028235e38', 'NaN'].map((x) => checkPrefValue('float', x)),
    ['1.0', '1.5', '0.1', '1.0E10', '-0.0', '1.0E-5', '3.4028235E38', 'NaN'], 'float come Float.toString');
  eq([javaFloatString(100), javaFloatString(1234567), javaFloatString(0.001), javaFloatString(1 / 3)], ['100.0', '1234567.0', '0.001', '0.33333334'], 'Float.toString');
  for (const [t, x] of [['int', '2147483648'], ['int', '1.0'], ['long', '9223372036854775808'], ['float', 'abc'], ['boolean', 'True']]) {
    let e = null;
    try {
      checkPrefValue(t, x);
    } catch (y) {
      e = y;
    }
    check(e !== null, `${t} ${x} accettato`);
  }
});

test('nomi non UTF-8 e argomenti SQL (vetro.mjs)', () => {
  const raw = new Uint8Array([0x2f, 0x61, 0xff, 0x62, 0xc3, 0xa0, 0xc3, 0xed, 0xb2, 0x80]);
  const s = pathString(raw);
  eq(s, '/a\udcffbà\udcc3\udced\udcb2\udc80', 'surrogateescape');
  eq([...pathBytes(s)], [...raw], 'andata e ritorno');
  eq(displayName(s), '/a\\xffbà\\xc3\\xed\\xb2\\x80', 'mostrato con \\xNN');
  eq([...pathBytes('😀/à')], [...new TextEncoder().encode('😀/à')], 'UTF-8 con coppie di surrogati');
  eq(displayName('😀'), '😀', 'coppia di surrogati intatta');
  eq(JSON.parse('"a\\udcff"'), 'a\udcff', 'JSON di vetro-wasm');
  // Stessi byte di proto::encode_sql_args (test Rust sql_e_nomi / wasm).
  eq([...encodeSqlArgs('S', [1n, 'x'])], [1, 0, 0, 0, 83, 2, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 1, 0, 0, 0, 120], 'formato');
  const all = encodeSqlArgs('', [null, 2, 2.5, true, new Uint8Array([7]), { type: 'real', value: 3 }, { type: 'integer', value: -1n }]);
  eq([...all.subarray(4, 6)], [7, 0], 'numero di parametri');
  eq([all[6], all[7], all[16], all[25], all[34]], [0, 1, 2, 1, 4], 'tipi dedotti');
  check(sqlValue(['i', '9223372036854775807']) === 9223372036854775807n, 'intero grande');
  eq([sqlValue(['i', '-3']), sqlValue(['f', '1.0']), sqlValue(['f', 'inf']), sqlValue(['t', 'x']), sqlValue(null)], [-3, 1, null, 'x', null], 'valori');
  eq(sqlValue(['f', '-inf']), -Infinity, 'meno infinito');
  eq([...sqlValue(['b', '00ab'])], [0, 0xab], 'BLOB');
});

test('gestore dei file: visualizzatori', () => {
  const enc = new TextEncoder();
  const bytes = new Uint8Array(40).map((_, i) => (i * 37) & 0xff);
  const dump = hexDump(bytes);
  eq(dump.split('\n').length, 3, 'righe da 16 byte');
  eq(dump.split('\n')[0], '00000000  00 25 4a 6f 94 b9 de 03 28 4d 72 97 bc e1 06 2b  |.%Jo....(Mr....+|', 'prima riga');
  eq([...parseHex(dump)], [...bytes], 'andata e ritorno');
  eq([...parseHex('00000000  41 42 |AB|\n00000002  43 ff 00\n')], [0x41, 0x42, 0x43, 0xff, 0x00], 'byte aggiunti');
  let err = null;
  try {
    parseHex('00000000  41 4g');
  } catch (e) {
    err = e.message;
  }
  eq(err, 'riga 1: "4g" non è un byte esadecimale', 'errore con la riga');
  eq(modeString('file', 0o100640), '-rw-r-----', 'modo di un file');
  eq(modeString('dir', 0o40755), 'drwxr-xr-x', 'modo di una cartella');
  eq(sizeString(2048), '2.0 KiB', 'dimensione');
  eq(asText(enc.encode('ciao')), 'ciao', 'testo');
  eq(asText(new Uint8Array([0xc3])), null, 'UTF-8 non valido');
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
  ], ['json', 'json', 'xml', 'xml', 'text', 'hex', 'image', 'sqlite'], 'riconoscimento');
});

test('pannelli di analisi: tempi, durate, dump', () => {
  eq(guestTime(1_234_567), '1.234 s', 'tempo del guest');
  eq(guestTime(5), '0.000 s', 'tempo piccolo');
  eq([duration(null), duration(999), duration(1500), duration(25_000), duration(3_200_000)], ['–', '999 µs', '1.50 ms', '25.0 ms', '3.20 s'], 'durate');
  eq([...fromB64('AAH/')], [0, 1, 255], 'base64');
  const d = hexdump(new Uint8Array([0x41, 0x00, 0x7f, 0x42, ...new Array(14).fill(0x2e)]), 0xffff800080010800n);
  eq(d.split('\n'), [
    'ffff800080010800  41 00 7f 42 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e 2e  A..B............',
    'ffff800080010810  2e 2e                                            ..',
  ], 'dump esadecimale');
  eq(hexdump(new Uint8Array(40), 0n, 16).split('\n').at(-1), '… altri 24 byte', 'dump tagliato');
});

test('ispettore: celle dei corpi e del tipo', () => {
  eq([bodyCell(0, 'vuoto'), bodyCell(12, 'json'), bodyCell(2048, 'binario'), bodyCell(5, '-'), bodyCell(null, '-'), bodyCell(undefined)],
    ['0 B', '12 B json', '2.0 KiB binario', '5 B', '–', '–'], 'corpi');
  const t = (r) => typeText(r).text;
  eq([
    t({ mime: 'application/json', status: 200, respBytes: 2, respKind: 'json' }),
    t({ mime: null, status: 200, respBytes: 0, respKind: 'vuoto' }),
    t({ mime: null, status: 200, respBytes: 7, respKind: 'testo' }),
    t({ mime: null, status: null, respBytes: 0, respKind: '-' }),
  ], ['application/json', 'vuoto', 'testo', '–'], 'tipo');
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

test('APK: ZIP and binary manifest (testdata/tocco-manifest.axml)', async () => {
  const axml = new Uint8Array(readFileSync(join(root, 'tests/web/testdata/tocco-manifest.axml')));
  const els = parseAxml(axml);
  eq(els[0].name, 'manifest', 'first element');
  eq(els[0].attrs.package, 'it.vetro.tocco', 'package');
  // A ZIP with the manifest compressed (deflate) and an uncompressed file.
  const deflated = new Uint8Array(await new Response(new Blob([axml]).stream().pipeThrough(new CompressionStream('deflate-raw'))).arrayBuffer());
  const zip = makeZip([['classes.dex', new TextEncoder().encode('dex\n035'), 0], ['AndroidManifest.xml', deflated, 8, axml.length]]);
  eq([...zipEntries(zip).keys()], ['classes.dex', 'AndroidManifest.xml'], 'ZIP files');
  eq(await apkInfo(zip), { package: 'it.vetro.tocco', versionName: '1.0', versionCode: 1, label: 'Tocco', launcher: 'it.vetro.tocco.Main' }, 'information');
  let threw = false;
  try {
    await apkInfo(new Uint8Array(100));
  } catch {
    threw = true;
  }
  check(threw, 'not a ZIP: rejected');
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

/** A minimal ZIP: [name, data, method, uncompressed length]. */
function makeZip(files) {
  const parts = [];
  const central = [];
  let at = 0;
  const enc = new TextEncoder();
  for (const [name, data, method, size = data.length] of files) {
    const n = enc.encode(name);
    const h = new Uint8Array(30 + n.length);
    const v = new DataView(h.buffer);
    v.setUint32(0, 0x04034b50, true);
    v.setUint16(8, method, true);
    v.setUint32(18, data.length, true);
    v.setUint32(22, size, true);
    v.setUint16(26, n.length, true);
    h.set(n, 30);
    const c = new Uint8Array(46 + n.length);
    const cv = new DataView(c.buffer);
    cv.setUint32(0, 0x02014b50, true);
    cv.setUint16(10, method, true);
    cv.setUint32(20, data.length, true);
    cv.setUint32(24, size, true);
    cv.setUint16(28, n.length, true);
    cv.setUint32(42, at, true);
    c.set(n, 46);
    parts.push(h, data);
    central.push(c);
    at += h.length + data.length;
  }
  const cdSize = central.reduce((s, c) => s + c.length, 0);
  const e = new Uint8Array(22);
  const ev = new DataView(e.buffer);
  ev.setUint32(0, 0x06054b50, true);
  ev.setUint16(8, files.length, true);
  ev.setUint16(10, files.length, true);
  ev.setUint32(12, cdSize, true);
  ev.setUint32(16, at, true);
  const all = [...parts, ...central, e];
  const out = new Uint8Array(all.reduce((s, x) => s + x.length, 0));
  let o = 0;
  for (const x of all) {
    out.set(x, o);
    o += x.length;
  }
  return out;
}

run(async () => {
  for (const [name, f] of cases) {
    await f();
    count++;
    console.log(`ok: ${name}`);
  }
  console.log(`test unitari web: ${count} ok`);
});
