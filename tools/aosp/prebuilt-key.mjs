#!/usr/bin/env node
// The prebuilt Android snapshot key of a vetro-wasm build (ADR 0031), and
// whether R2 has that snapshot: the site build's guard, so the site never
// points at a snapshot its vetro-wasm cannot restore.
//
//   node tools/aosp/prebuilt-key.mjs [--wasm=FILE] [--manifest=URL] [--write=FILE] [--require]
//
// Builds the app's Android machine (ANDROID_MACHINE, ANDROID_DISK) with the
// given vetro-wasm, without booting it, and computes the key exactly as the
// Worker does (androidSnapshotKey: snapshot format, configuration hash,
// machine, image version and boot image hashes, disk map). Then looks for
// <image>/snapshots/<key>.json next to the manifest and checks it.
// --write: the hint for the app ({ manifest, key, size, sha256, url }), only
// if the snapshot is there; otherwise the file is removed. --require: exit 1
// if it is not there (without it, a warning).

import { createHash } from 'node:crypto';
import { readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DEV, instantiate, Machine } from '../../web/node/vetro.mjs';
import { DiskFeeder } from '../../web/node/disk.mjs';
import { ANDROID_DISK, ANDROID_MACHINE, ANDROID_PARAMS, DEFAULT_MANIFEST, machineDevices } from '../../web/node/android.mjs';
import { androidSnapshotKey, findPrebuilt, prebuiltSnapUrl } from '../../web/node/prebuilt.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '../..');
const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const wasmPath = arg('wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm'));
const manifestUrl = arg('manifest', DEFAULT_MANIFEST);
const write = arg('write', null);
const require_ = process.argv.includes('--require');

/** The key of `wasm` for the image at `manifestUrl`: { key, parts }. */
export async function prebuiltKey(wasm, manifestUrl, f = fetch) {
  const manifest = await (await f(manifestUrl)).json();
  const images = ['boot.img', 'vendor_boot.img', 'init_boot.img'].map((p) => {
    const x = manifest.files.find((y) => y.path === p);
    if (!x) throw new Error(`manifest without ${p}`);
    return x;
  });
  const mapUrl = new URL('web/disk.json', manifestUrl).href;
  const res = await f(mapUrl);
  if (!res.ok) throw new Error(`${mapUrl}: status ${res.status}`);
  const text = await res.text();
  const layout = { sha256: createHash('sha256').update(text).digest('hex'), size: JSON.parse(text).size };
  const { exports } = await instantiate(wasm);
  const devices = machineDevices(DEV, ANDROID_MACHINE);
  const m = new Machine(exports, { ramSize: BigInt(ANDROID_MACHINE.ramMiB) << 20n, devices, width: ANDROID_MACHINE.width, height: ANDROID_MACHINE.height });
  try {
    new DiskFeeder(m).add({ size: layout.size, key: '' }, ANDROID_DISK);
    return await androidSnapshotKey(m, { machine: ANDROID_MACHINE, devices, manifest, images, params: ANDROID_PARAMS, layout });
  } finally {
    m.free();
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const { key, parts } = await prebuiltKey(readFileSync(wasmPath), manifestUrl);
  console.log(`key ${key}: format ${parts.format}, configuration ${parts.config}, image ${parts.android}`);
  const found = await findPrebuilt(manifestUrl, key);
  if (found.info) {
    const { info } = found;
    console.log(`prebuilt snapshot: ${found.url} (${(info.size / 2 ** 20).toFixed(0)} MiB, sha256 ${info.sha256})`);
    if (write) {
      writeFileSync(write, `${JSON.stringify({ manifest: manifestUrl, key, size: info.size, sha256: info.sha256, url: prebuiltSnapUrl(manifestUrl, key) }, null, 1)}\n`);
      console.log(`hint: ${write}`);
    }
  } else {
    if (write) rmSync(write, { force: true });
    const msg = `no prebuilt snapshot for this vetro-wasm (${found.missing}): the app will cold boot Android; make one with tools/aosp/prebuilt-snapshot.mjs and tools/aosp/upload-snapshot.sh`;
    if (require_) {
      console.error(`ERROR: ${msg}`);
      process.exit(1);
    }
    console.log(`${process.env.GITHUB_ACTIONS ? '::warning title=prebuilt Android snapshot::' : 'warning: '}${msg}`);
  }
}
