#!/usr/bin/env node
// Il disco dell'immagine AOSP di Vetro per il browser (M5, ADR 0028): invece
// di pubblicare target/aosp/disk.img (GPT di 15 GiB, quasi tutto zeri), una
// mappa che ricompone lo stesso disco dagli artefatti già pubblicati:
//
//   out/web/disk.json      la mappa: dimensione del disco ed estensioni
//                          [offset nel disco, lunghezza, file, offset nel file]
//                          (file -1 = zeri, -2 = riempimento con la parola a
//                          32 bit in "offset nel file"); fuori dalle
//                          estensioni il disco è a zero
//   out/web/disk-head.bin  i blocchi non a zero del disco che non stanno nelle
//                          immagini della build: GPT primaria e di riserva,
//                          metadata (ext4 vuoto di mkdisk.sh); pochi KiB
//
// super e userdata vengono dai file sparsi della build (super.img,
// userdata.img, formato sparse di Android): ogni pezzo RAW diventa
// un'estensione che punta dentro il file sparso, FILL un riempimento,
// DONT_CARE zeri. I percorsi dei file sono relativi alla mappa
// (`../super.img`), come su R2 (aosp/<versione>/web/disk.json accanto a
// aosp/<versione>/super.img). Il browser legge i byte con HTTP Range
// (LayoutSource di web/node/disk.mjs).
//
// Uso: node tools/aosp/web-disk.mjs [--no-verify]
// Ingressi: target/aosp/disk.img (tools/aosp/mkdisk.sh), target/aosp/out/.
// Con la verifica (default) il disco ricomposto dalla mappa si confronta
// byte per byte con disk.img.

import { createHash } from 'node:crypto';
import { mkdirSync, openSync, readSync, closeSync, fstatSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { composeRead, parseLayout } from '../../web/node/disk.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const aosp = join(root, 'target/aosp');
const out = join(aosp, 'out');
const webDir = join(out, 'web');
const BLOCK = 4096;

function readAt(fd, offset, length) {
  const buf = Buffer.alloc(length);
  let done = 0;
  while (done < length) {
    const n = readSync(fd, buf, done, length - done, offset + done);
    if (n === 0) break;
    done += n;
  }
  return buf.subarray(0, done);
}

/** Partizioni GPT del disco: [{ name, start, size }] in byte. */
function gpt(fd) {
  const h = readAt(fd, 512, 512);
  if (h.toString('latin1', 0, 8) !== 'EFI PART') throw new Error('disk.img: GPT mancante');
  const entriesLba = Number(h.readBigUInt64LE(72));
  const count = h.readUInt32LE(80);
  const size = h.readUInt32LE(84);
  const t = readAt(fd, entriesLba * 512, count * size);
  const parts = [];
  for (let i = 0; i < count; i++) {
    const e = t.subarray(i * size, (i + 1) * size);
    if (e.subarray(0, 16).every((b) => b === 0)) continue;
    const first = Number(e.readBigUInt64LE(32));
    const last = Number(e.readBigUInt64LE(40));
    const name = e.toString('utf16le', 56, 128).replace(/\0.*$/, '');
    parts.push({ name, start: first * 512, size: (last - first + 1) * 512 });
  }
  return parts;
}

/** Pezzi di un'immagine sparse di Android: [{ at, length, kind, fileOffset|value }]. */
function sparseChunks(path) {
  const fd = openSync(path, 'r');
  const h = readAt(fd, 0, 28);
  if (h.readUInt32LE(0) !== 0xed26ff3a) throw new Error(`${path}: non è un'immagine sparse`);
  const fileHdr = h.readUInt16LE(8);
  const chunkHdr = h.readUInt16LE(10);
  const blk = h.readUInt32LE(12);
  const totalBlocks = h.readUInt32LE(16);
  const chunks = h.readUInt32LE(20);
  let pos = fileHdr;
  let block = 0;
  const out = [];
  for (let i = 0; i < chunks; i++) {
    const c = readAt(fd, pos, chunkHdr);
    const type = c.readUInt16LE(0);
    const blocks = c.readUInt32LE(4);
    const total = c.readUInt32LE(8);
    const length = blocks * blk;
    const data = pos + chunkHdr;
    if (type === 0xcac1) {
      if (total - chunkHdr !== length) throw new Error(`${path}: pezzo RAW ${i} di lunghezza incoerente`);
      out.push({ at: block * blk, length, kind: 'raw', fileOffset: data });
    } else if (type === 0xcac2) {
      out.push({ at: block * blk, length, kind: 'fill', value: readAt(fd, data, 4).readUInt32LE(0) });
    } else if (type !== 0xcac3 && type !== 0xcac4) {
      throw new Error(`${path}: pezzo ${i} di tipo ${type.toString(16)}`);
    }
    if (type !== 0xcac4) block += blocks;
    pos += total;
  }
  closeSync(fd);
  if (block !== totalBlocks) throw new Error(`${path}: ${block} blocchi invece di ${totalBlocks}`);
  return { size: totalBlocks * blk, chunks: out };
}

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');

function sha256File(path) {
  const h = createHash('sha256');
  const fd = openSync(path, 'r');
  const buf = Buffer.alloc(8 << 20);
  for (let at = 0; ; ) {
    const n = readSync(fd, buf, 0, buf.length, at);
    if (!n) break;
    h.update(buf.subarray(0, n));
    at += n;
  }
  closeSync(fd);
  return h.digest('hex');
}

function main() {
  const verify = !process.argv.includes('--no-verify');
  const diskPath = join(aosp, 'disk.img');
  const fd = openSync(diskPath, 'r');
  const diskSize = fstatSync(fd).size;
  const parts = gpt(fd);
  const byName = Object.fromEntries(parts.map((p) => [p.name, p]));
  const images = [
    { part: 'super', file: 'super.img' },
    { part: 'userdata', file: 'userdata.img' },
  ];
  const extents = [];
  // File 0: disk-head.bin; 1..: le immagini sparse.
  const files = [{ path: 'disk-head.bin' }];
  for (const [k, img] of images.entries()) {
    const p = byName[img.part];
    if (!p) throw new Error(`disk.img: partizione ${img.part} mancante`);
    const s = sparseChunks(join(out, img.file));
    if (s.size > p.size) throw new Error(`${img.file}: ${s.size} byte, partizione di ${p.size}`);
    for (const c of s.chunks) {
      if (c.kind === 'raw') extents.push([p.start + c.at, c.length, k + 1, c.fileOffset]);
      else if (c.value !== 0) extents.push([p.start + c.at, c.length, -2, c.value]);
    }
    img.range = [p.start, p.start + p.size];
    files.push({ path: `../${img.file}` });
  }
  // Il resto del disco: blocchi non a zero in disk-head.bin.
  const covered = images.map((i) => i.range).sort((a, b) => a[0] - b[0]);
  const gaps = [];
  let at = 0;
  for (const [s, e] of covered) {
    if (s > at) gaps.push([at, s]);
    at = Math.max(at, e);
  }
  if (at < diskSize) gaps.push([at, diskSize]);
  const head = [];
  let headLen = 0;
  for (const [s, e] of gaps) {
    for (let off = s; off < e; off += 64 * BLOCK) {
      const buf = readAt(fd, off, Math.min(64 * BLOCK, e - off));
      for (let b = 0; b < buf.length; b += BLOCK) {
        const blk = buf.subarray(b, b + BLOCK);
        if (blk.every((x) => x === 0)) continue;
        const last = extents.length && extents[extents.length - 1];
        if (last && last[2] === 0 && last[0] + last[1] === off + b) last[1] += blk.length;
        else extents.push([off + b, blk.length, 0, headLen]);
        head.push(Buffer.from(blk));
        headLen += blk.length;
      }
    }
  }
  extents.sort((a, b) => a[0] - b[0]);
  const headBuf = Buffer.concat(head);
  mkdirSync(webDir, { recursive: true });
  writeFileSync(join(webDir, 'disk-head.bin'), headBuf);
  files[0].size = headBuf.length;
  files[0].sha256 = sha256(headBuf);
  for (const [k, img] of images.entries()) {
    const p = join(out, img.file);
    files[k + 1].size = fstatSync(openSync(p, 'r')).size;
    files[k + 1].sha256 = sha256File(p);
  }
  const layout = {
    format: 'vetro-disk-layout',
    version: 1,
    size: diskSize,
    partitions: parts,
    files,
    extents,
  };
  const text = `${JSON.stringify(layout)}\n`;
  parseLayout(JSON.parse(text));
  writeFileSync(join(webDir, 'disk.json'), text);
  console.log(`disk.json: disco di ${(diskSize / 2 ** 30).toFixed(2)} GiB, ${extents.length} estensioni, disk-head.bin ${headBuf.length} byte`);
  if (verify) {
    const l = parseLayout(JSON.parse(text));
    const fds = files.map((f) => openSync(join(webDir, f.path), 'r'));
    const read = (i, off, len) => readAt(fds[i], off, len);
    const WIN = 64 << 20;
    for (let off = 0; off < diskSize; off += WIN) {
      const len = Math.min(WIN, diskSize - off);
      const want = readAt(fd, off, len);
      const got = composeRead(l, off, len, read);
      if (!want.equals(Buffer.from(got.buffer, got.byteOffset, got.length))) {
        let i = 0;
        while (want[i] === got[i]) i++;
        throw new Error(`disco ricomposto diverso da disk.img al byte ${off + i}`);
      }
    }
    console.log('verifica: il disco ricomposto dalla mappa è uguale a disk.img byte per byte');
  }
  closeSync(fd);
}

main();
