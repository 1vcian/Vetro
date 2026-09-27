// The prebuilt Android snapshot (ADR 0031). Uses no Node API: it runs in the
// app's Worker (OPFS) and in Node (tests, tools).
//
// A snapshot at the home screen, made once per (image version, vetro-wasm
// snapshot format and machine configuration) by tools/aosp/prebuilt-snapshot.mjs
// and published next to the image on R2:
//
//   <image>/snapshots/<key>.json   info: key, its parts, size, sha256, the
//                                  SHA-256 of every PREBUILT_CHUNK bytes, the
//                                  metadata the app keeps with a snapshot
//   <image>/snapshots/<key>.snap   the snapshot file, as the app stores it
//
// `key` is `snapshotKey(androidKeyParts(...))`, the same key as the app's own
// snapshots in OPFS: the app computes it from the vetro-wasm it runs and looks
// for exactly that file, so an incompatible prebuilt snapshot cannot even be
// found (404 = cold boot). The download goes straight into the snapshot cache
// (SnapshotStore), chunk by chunk, each chunk verified before it is written;
// an interrupted download resumes with an HTTP Range from the last verified
// chunk. The metadata file is written last: only then does the cache see the
// snapshot.

import { androidKeyParts } from './android.mjs';
import { sha256Hex, snapshotKey } from './persist.mjs';

/** Bytes per verified chunk of a prebuilt snapshot (16 MiB). */
export const PREBUILT_CHUNK = 16 << 20;

/** Format of the info file. */
export const PREBUILT_FORMAT = 'vetro-prebuilt-snapshot';

/** URL of the info file of snapshot `key` next to the image's manifest. */
export const prebuiltInfoUrl = (manifestUrl, key) => new URL(`snapshots/${key}.json`, manifestUrl).href;
/** URL of the snapshot bytes. */
export const prebuiltSnapUrl = (manifestUrl, key) => new URL(`snapshots/${key}.snap`, manifestUrl).href;

/**
 * Key of the Android snapshots of machine `m` (built, disks added): see
 * `androidKeyParts`. `layout` is the opened LayoutSource of the disk map.
 * Returns { key, parts }.
 */
export async function androidSnapshotKey(m, { machine, devices, manifest, images, params, layout }) {
  const parts = androidKeyParts({
    format: m.snapshotVersion,
    configHash: m.snapshotConfigHash,
    ramMiB: machine.ramMiB,
    width: machine.width,
    height: machine.height,
    devices,
    version: manifest.version,
    images: images.map((f) => f.sha256),
    params,
    disk: { sha256: layout.sha256, size: Math.floor(layout.size / 512) * 512 },
  });
  return { key: await snapshotKey(parts), parts };
}

/** Checks an info file against the key it was looked up with; returns the reason it is unusable, or null. */
export function prebuiltProblem(info, key) {
  if (!info || info.format !== PREBUILT_FORMAT || info.version !== 1) return 'not a prebuilt snapshot info file';
  if (info.key !== key) return `key ${info.key}, wanted ${key}`;
  if (!Number.isSafeInteger(info.size) || info.size <= 0) return 'bad size';
  if (info.chunk !== PREBUILT_CHUNK || !Array.isArray(info.chunks) || info.chunks.length !== Math.ceil(info.size / PREBUILT_CHUNK)) return 'bad chunk list';
  if (!info.meta || typeof info.meta !== 'object') return 'no metadata';
  return null;
}

/**
 * The info file for `key` next to the manifest: { info } if there is a usable
 * one, { missing: reason } otherwise (404 included: no prebuilt snapshot for
 * this vetro-wasm and image).
 */
export async function findPrebuilt(manifestUrl, key, { fetch: f = (...a) => globalThis.fetch(...a) } = {}) {
  const url = prebuiltInfoUrl(manifestUrl, key);
  let res;
  try {
    res = await f(url, { cache: 'no-cache' });
  } catch (e) {
    return { missing: `${url}: ${e.message ?? e}` };
  }
  if (!res.ok) return { missing: res.status === 404 || res.status === 403 ? 'no prebuilt snapshot for this vetro-wasm and image' : `${url}: status ${res.status}` };
  let info;
  try {
    info = await res.json();
  } catch {
    return { missing: `${url}: not JSON` };
  }
  const problem = prebuiltProblem(info, key);
  return problem ? { missing: `${url}: ${problem}` } : { info, url };
}

/**
 * Downloads the prebuilt snapshot described by `info` from `url` into `file`
 * (FileSystemSyncAccessHandle interface), resuming after the chunks `resume`
 * says are already verified ({ sha256, verified }, or null). Each chunk is
 * checked against its SHA-256 before it is written; after each chunk
 * `saveResume({ sha256, verified })` records progress. Network errors are
 * retried (with a Range from the last verified chunk) up to `retries` times
 * in a row. `onProgress({ loaded, total, verified, resumedFrom })`.
 * Returns { bytes, ms, resumedFrom, retries }.
 */
export async function downloadPrebuilt(info, url, file, { resume = null, saveResume = async () => {}, onProgress = () => {}, fetch: f = (...a) => globalThis.fetch(...a), retries = 6, digest = sha256Hex } = {}) {
  const t0 = performance.now();
  const n = info.chunks.length;
  let verified = resume && resume.sha256 === info.sha256 ? Math.min(resume.verified | 0, n) : 0;
  if (!resume || resume.sha256 !== info.sha256) file.truncate(0);
  const resumedFrom = verified * PREBUILT_CHUNK;
  let got = 0;
  let failures = 0;
  let totalRetries = 0;
  const buf = new Uint8Array(PREBUILT_CHUNK);
  while (verified < n) {
    const start = verified * PREBUILT_CHUNK;
    let fill = 0;
    try {
      const res = await f(url, { headers: start ? { Range: `bytes=${start}-` } : {}, cache: 'no-store' });
      if (start && res.status !== 206) throw Object.assign(new Error(`${url}: no Range support (status ${res.status})`), { fatal: true });
      if (!start && res.status !== 200 && res.status !== 206) throw Object.assign(new Error(`${url}: status ${res.status}`), { fatal: res.status < 500 });
      const reader = res.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        let at = 0;
        while (at < value.length) {
          const want = Math.min(PREBUILT_CHUNK, info.size - verified * PREBUILT_CHUNK);
          const take = Math.min(want - fill, value.length - at);
          buf.set(value.subarray(at, at + take), fill);
          fill += take;
          at += take;
          got += take;
          if (fill === want) {
            const chunk = buf.subarray(0, want);
            const sum = await digest(chunk);
            if (sum !== info.chunks[verified]) {
              await reader.cancel().catch(() => {});
              throw Object.assign(new Error(`chunk ${verified}: sha256 ${sum}, expected ${info.chunks[verified]}`), { chunk: true });
            }
            if (file.write(chunk, { at: verified * PREBUILT_CHUNK }) !== want) throw Object.assign(new Error('short write'), { fatal: true });
            verified++;
            fill = 0;
            failures = 0;
            file.flush();
            await saveResume({ sha256: info.sha256, verified });
            if (verified === n) {
              if (at < value.length) throw Object.assign(new Error(`${url}: longer than ${info.size} bytes`), { fatal: true });
              await reader.cancel().catch(() => {});
              break;
            }
          }
          onProgress({ loaded: verified * PREBUILT_CHUNK + fill, total: info.size, verified, resumedFrom });
        }
        if (verified === n) break;
      }
      if (verified < n) throw new Error(`${url}: connection closed after ${start + got} bytes`);
    } catch (e) {
      if (e.fatal) throw e;
      failures++;
      totalRetries++;
      // A damaged chunk twice in a row is not the network.
      if (failures > retries || (e.chunk && failures > 1)) throw e;
      await new Promise((ok) => setTimeout(ok, Math.min(8000, 250 << failures)));
    }
  }
  if (file.getSize() !== info.size) file.truncate(info.size);
  return { bytes: got, ms: performance.now() - t0, resumedFrom, retries: totalRetries };
}
