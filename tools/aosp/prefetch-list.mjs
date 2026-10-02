#!/usr/bin/env node
// Builds the prefetch list of a prebuilt Android snapshot (ADR 0044) from
// the disk traces of sessions restored from it (tools/aosp/live-path.mjs
// --trace: the blocks each session fetched from the network, in order).
//
//   node tools/aosp/prefetch-list.mjs --key=KEY --manifest=URL --out=DIR TRACE.json...
//
// The blocks of all traces, each at its earliest position in any of them
// (what one session needs first comes first), without duplicates. The disk
// is identified like the app does (SHA-256 of the disk map's text, next to
// the manifest); the block size is the app's (1 MiB). Writes
// DIR/<key>.blocks.json; tools/aosp/upload-snapshot.sh publishes it next to
// the snapshot.

import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join } from 'node:path';
import { PREFETCH_FORMAT, prefetchBlocks } from '../../web/node/disk.mjs';

const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const key = arg('key');
const manifest = arg('manifest');
const out = arg('out', '.');
const blockSize = Number(arg('block-size', 1 << 20));
const traces = process.argv.slice(2).filter((a) => !a.startsWith('--'));
if (!key || !manifest || !traces.length) {
  console.error('usage: prefetch-list.mjs --key=KEY --manifest=URL [--out=DIR] TRACE.json...');
  process.exit(2);
}

const mres = await fetch(manifest);
if (!mres.ok) throw new Error(`${manifest}: status ${mres.status}`);
const layoutUrl = new URL('web/disk.json', manifest).href;
const lres = await fetch(layoutUrl);
if (!lres.ok) throw new Error(`${layoutUrl}: status ${lres.status}`);
const text = await lres.text();
const disk = createHash('sha256').update(text).digest('hex');
const size = Math.floor(JSON.parse(text).size / 512) * 512;

// Earliest position of every block over all the traces (disk 0: the only one).
const first = new Map();
for (const f of traces) {
  const t = JSON.parse(readFileSync(f, 'utf8'));
  const blocks = Array.isArray(t[0]) ? t[0] : t;
  blocks.forEach((b, i) => {
    if (!first.has(b) || first.get(b) > i) first.set(b, i);
  });
}
const blocks = [...first.entries()].sort((a, b) => a[1] - b[1] || a[0] - b[0]).map(([b]) => b);
const list = { format: PREFETCH_FORMAT, version: 1, key, disk, blockSize, blocks, traces: traces.length, made: new Date().toISOString() };
const check = prefetchBlocks(list, { sha256: disk, blockSize, blocks: Math.ceil(size / blockSize) });
if (check.why || check.blocks.length !== blocks.length) throw new Error(`the list does not check: ${check.why ?? 'blocks outside the disk'}`);
const file = join(out, `${key}.blocks.json`);
writeFileSync(file, `${JSON.stringify(list)}\n`);
console.log(`${file}: ${blocks.length} blocks of ${blockSize >> 10} KiB (${(blocks.length * blockSize / 2 ** 20).toFixed(0)} MiB) from ${traces.length} trace(s)`);
