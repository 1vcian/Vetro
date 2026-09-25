#!/usr/bin/env node
// Test unitari dei pezzi JS dell'app web (senza kernel né browser):
// server con Range (tools/web-serve.mjs), sorgenti e DiskFeeder
// (web/node/disk.mjs), mappa dei tasti (web/app/keymap.mjs), terminale
// (web/app/terminal.mjs), persistenza (web/node/persist.mjs), lettore
// SQLite e visualizzatori del gestore dei file (web/app/sqlite.mjs,
// web/app/files.mjs, M8), formati dei pannelli di analisi
// (web/app/analysis.mjs, M7/M10).
//
//   node tests/web/unit.mjs

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { BlobSource, DiskFeeder, MemoryCache, RangeSource } from '../../web/node/disk.mjs';
import { parseRange, serve } from '../../tools/web-serve.mjs';
import { absAxis, BUTTONS, evdevCode } from '../../web/app/keymap.mjs';
import { keyToBytes, Terminal } from '../../web/app/terminal.mjs';
import { fromBase64, MemFile, readAll, SnapshotStore, snapshotKey, staleReason, toBase64 } from '../../web/node/persist.mjs';
import { formatValue, isSqlite, parseCreateTable, SqliteDb, varint } from '../../web/app/sqlite.mjs';
import { asText, detectView, hexDump, imageType, modeString, parseHex, sizeString } from '../../web/app/files.mjs';
import { duration, fromB64, guestTime, hexdump } from '../../web/app/analysis.mjs';
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

run(async () => {
  for (const [name, f] of cases) {
    await f();
    count++;
    console.log(`ok: ${name}`);
  }
  console.log(`test unitari web: ${count} ok`);
});
