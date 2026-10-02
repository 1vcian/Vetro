// vetro-wasm virtio-blk disks with the data obtained by JS (M5,
// docs/specs/wasm.md, ADR 0014). It doesn't use Node APIs: it runs in Node (tests)
// and in the browser (the app's Worker).
//
// The round trip:
//   1. `vetro_run` stops with `Blocked`: a guest request touches
//      blocks the machine doesn't have, and guest time is stopped;
//   2. `DiskFeeder.serve()` takes the requested blocks (`vetro_disk_wanted`),
//      looks for them first in the cache (OPFS in the browser), then in the source
//      (HTTP Range on a URL, or a local File), merging contiguous blocks into
//      a single request (up to DEMAND_PARALLEL requests in flight), and
//      delivers them (`vetro_disk_fill`);
//   3. the next quantum repeats the request, which now completes at the
//      same instruction count as with a local disk.
//
// The guest's writes stay in the in-memory copy-on-write layer inside
// vetro-wasm: the source and the cache never change.
//
// A prefetch list (ADR 0044) fills the cache ahead of the guest, in the
// background: the blocks a restored Android snapshot reads first. Only the
// cache: what the machine sees, and when, does not change (a block is still
// delivered when the guest asks for it), so replay stays exact.

/** A source read with HTTP Range. */
export class RangeSource {
  #fetch;
  /** HTTP requests made and bytes received (for the tests and the status bar). */
  stats = { requests: 0, bytes: 0 };

  constructor(url, { fetch: f = (...a) => globalThis.fetch(...a) } = {}) {
    this.url = url;
    this.#fetch = f;
    this.size = 0;
    this.key = null;
  }

  /**
   * GET with Range; retries (up to 3 times, with increasing waits) network
   * errors and 5xx responses: a connection kept open and closed by the
   * server in the meantime must not become an I/O error for the guest.
   */
  async #get(range) {
    for (let attempt = 0; ; attempt++) {
      try {
        const res = await this.#fetch(this.url, { headers: { Range: range } });
        if (res.status < 500 || attempt >= 2) return res;
        await res.arrayBuffer();
      } catch (e) {
        if (attempt >= 2) throw e;
      }
      await new Promise((ok) => setTimeout(ok, 100 << attempt));
    }
  }

  /** Reads the size (from the Content-Range of a 1-byte request). */
  async open() {
    const res = await this.#get('bytes=0-0');
    this.stats.requests++;
    await res.arrayBuffer();
    if (res.status !== 206) throw new Error(`${this.url}: the server does not answer Range requests (status ${res.status})`);
    const m = /\/(\d+)$/.exec(res.headers.get('Content-Range') ?? '');
    if (!m) throw new Error(`${this.url}: Content-Range without a size`);
    this.size = Number(m[1]);
    // The cache key changes if the file on the server changes.
    const tag = res.headers.get('ETag') ?? res.headers.get('Last-Modified') ?? '';
    this.key = `${this.url}|${this.size}|${tag}`;
    return this;
  }

  /** `length` bytes from `offset` (Uint8Array). */
  async read(offset, length) {
    const end = offset + length - 1;
    const res = await this.#get(`bytes=${offset}-${end}`);
    this.stats.requests++;
    const buf = new Uint8Array(await res.arrayBuffer());
    if (res.status !== 206 || buf.length !== length) {
      throw new Error(`${this.url}: bytes ${offset}-${end}: status ${res.status}, ${buf.length} bytes`);
    }
    this.stats.bytes += length;
    return buf;
  }
}

/** A source from a Blob or File (disk chosen from the computer). */
export class BlobSource {
  stats = { requests: 0, bytes: 0 };

  constructor(blob, name = blob.name ?? 'blob') {
    this.blob = blob;
    this.size = blob.size;
    this.key = `file:${name}|${blob.size}|${blob.lastModified ?? ''}`;
  }

  async open() {
    return this;
  }

  async read(offset, length) {
    this.stats.requests++;
    this.stats.bytes += length;
    return new Uint8Array(await this.blob.slice(offset, offset + length).arrayBuffer());
  }
}

// ---- Disk rebuilt from a map (M5, ADR 0028) ---------------------------------

/**
 * Checks a disk map (`tools/aosp/web-disk.mjs`: `{ format:
 * 'vetro-disk-layout', version: 1, size, files: [{ path, size, sha256 }],
 * extents: [[offset, length, file, file offset | word]] }`) and prepares it
 * for `composePlan`. Extents sorted, non-overlapping, inside the disk and the
 * files; file -1 = zeros, -2 = fill with the 32-bit word (little endian);
 * outside the extents, zeros.
 */
export function parseLayout(l) {
  if (l?.format !== 'vetro-disk-layout' || l.version !== 1) throw new Error('disk map: unknown format');
  const size = l.size;
  if (!Number.isSafeInteger(size) || size <= 0) throw new Error('disk map: invalid size');
  const files = l.files ?? [];
  let end = 0;
  const starts = new Float64Array(l.extents.length);
  for (const [i, e] of l.extents.entries()) {
    const [at, len, file, off] = e;
    if (![at, len, file, off].every(Number.isSafeInteger) || len <= 0 || at < end || at + len > size) {
      throw new Error(`disk map: invalid extent ${i}`);
    }
    if (file >= 0) {
      if (file >= files.length) throw new Error(`disk map: extent ${i}: unknown file ${file}`);
      if (files[file].size !== undefined && off + len > files[file].size) throw new Error(`disk map: extent ${i} beyond the end of ${files[file].path}`);
    } else if (file !== -1 && file !== -2) throw new Error(`disk map: extent ${i}: type ${file}`);
    starts[i] = at;
    end = at + len;
  }
  return { size, files, extents: l.extents, starts };
}

/**
 * The pieces of `[offset, offset+length)`: [{ at, length, file, fileOffset }]
 * for bytes from files (contiguous in the same file = a single piece) and
 * [{ at, length, fill, shift }] for fills; `at` relative to `offset`.
 * Uncovered bytes are zeros.
 */
export function composePlan(layout, offset, length) {
  const { extents, starts } = layout;
  // First extent that can touch the range.
  let lo = 0;
  let hi = starts.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (starts[mid] <= offset) lo = mid + 1;
    else hi = mid;
  }
  const plan = [];
  const stop = offset + length;
  for (let i = Math.max(0, lo - 1); i < extents.length && extents[i][0] < stop; i++) {
    const [at, len, file, off] = extents[i];
    const s = Math.max(at, offset);
    const e = Math.min(at + len, stop);
    if (s >= e || file === -1) continue;
    if (file === -2) {
      // The word repeats from the start of the extent.
      plan.push({ at: s - offset, length: e - s, fill: off >>> 0, shift: (s - at) & 3 });
      continue;
    }
    const fileOffset = off + (s - at);
    const last = plan[plan.length - 1];
    if (last && last.file === file && last.at + last.length === s - offset && last.fileOffset + last.length === fileOffset) last.length += e - s;
    else plan.push({ at: s - offset, length: e - s, file, fileOffset });
  }
  return plan;
}

function applyFill(out, p) {
  const word = new Uint8Array(4);
  new DataView(word.buffer).setUint32(0, p.fill, true);
  for (let k = 0; k < p.length; k++) out[p.at + k] = word[(p.shift + k) & 3];
}

/** `length` bytes from disk `offset` with a synchronous `read(file, offset, length)`. */
export function composeRead(layout, offset, length, read) {
  const out = new Uint8Array(length);
  for (const p of composePlan(layout, offset, length)) {
    if (p.fill !== undefined) applyFill(out, p);
    else out.set(read(p.file, p.fileOffset, p.length), p.at);
  }
  return out;
}

/**
 * A source rebuilt from a map (`tools/aosp/web-disk.mjs`): bytes come with
 * HTTP Range from the map's files (URLs relative to the map), holes are
 * zeros. `key` (block cache, overlay, snapshots) = the map URL + the SHA-256
 * of its text: if a file changes, the map changes.
 */
export class LayoutSource {
  #fetch;
  constructor(url, { fetch: f = (...a) => globalThis.fetch(...a) } = {}) {
    this.url = url;
    this.#fetch = f;
    this.size = 0;
    this.key = null;
    this.sources = [];
  }

  get stats() {
    const s = { requests: 1, bytes: 0 };
    for (const r of this.sources) {
      s.requests += r.stats.requests;
      s.bytes += r.stats.bytes;
    }
    return s;
  }

  async open() {
    const res = await this.#fetch(this.url);
    if (!res.ok) throw new Error(`${this.url}: status ${res.status}`);
    const text = await res.text();
    this.layout = parseLayout(JSON.parse(text));
    this.size = this.layout.size;
    const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
    const hex = [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
    this.key = `layout:${this.url}|${this.size}|${hex}`;
    /** SHA-256 of the map's text: the disk's identity wherever it is served from (snapshot keys, ADR 0031). */
    this.sha256 = hex;
    this.sources = this.layout.files.map((f) => new RangeSource(new URL(f.path, this.url).href, { fetch: this.#fetch }));
    await Promise.all(this.sources.map(async (s, i) => {
      await s.open();
      const want = this.layout.files[i].size;
      if (want !== undefined && s.size !== want) throw new Error(`${s.url}: ${s.size} bytes, the map wants ${want}`);
    }));
    return this;
  }

  async read(offset, length) {
    const out = new Uint8Array(length);
    const plan = composePlan(this.layout, offset, length);
    await Promise.all(plan.map(async (p) => {
      if (p.fill !== undefined) applyFill(out, p);
      else out.set(await this.sources[p.file].read(p.fileOffset, p.length), p.at);
    }));
    return out;
  }
}

/** In-memory block cache (Node, tests, or a browser without OPFS). */
export class MemoryCache {
  #m = new Map();
  stats = { hits: 0, puts: 0 };

  has(block) {
    return this.#m.has(block);
  }

  get(block) {
    this.stats.hits++;
    return this.#m.get(block);
  }

  put(block, bytes) {
    this.stats.puts++;
    this.#m.set(block, bytes.slice());
  }

  get size() {
    return this.#m.size;
  }
}

async function hashName(text) {
  const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(d).slice(0, 16)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Block cache in OPFS (Origin Private File System), only in a
 * dedicated Worker: `FileSystemSyncAccessHandle` reads and writes
 * synchronously. Two files per disk, with the name derived from the key of the
 * source (URL, size, ETag) and from the block size: `.img`
 * (the blocks in their place, a sparse file) and `.map` (one bit per block
 * present). At restart the blocks already downloaded are read from here.
 */
export class OpfsCache {
  stats = { hits: 0, puts: 0 };
  /** Kept between sessions: worth filling ahead (DiskFeeder.prefetch). */
  persistent = true;

  static async open(key, blockSize, blocks) {
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle('vetro-disks', { create: true });
    const name = await hashName(`${key}|${blockSize}`);
    const data = await (await dir.getFileHandle(`${name}.img`, { create: true })).createSyncAccessHandle();
    const map = await (await dir.getFileHandle(`${name}.map`, { create: true })).createSyncAccessHandle();
    const bits = new Uint8Array(Math.ceil(blocks / 8));
    if (map.getSize() >= bits.length) map.read(bits, { at: 0 });
    return new OpfsCache(data, map, bits, blockSize, name);
  }

  constructor(data, map, bits, blockSize, name) {
    this.data = data;
    this.map = map;
    this.bits = bits;
    this.blockSize = blockSize;
    this.name = name;
  }

  has(block) {
    return (this.bits[block >> 3] & (1 << (block & 7))) !== 0;
  }

  get(block, length) {
    this.stats.hits++;
    const buf = new Uint8Array(length);
    const n = this.data.read(buf, { at: block * this.blockSize });
    if (n !== length) throw new Error(`OPFS ${this.name}: block ${block} short (${n} bytes)`);
    return buf;
  }

  put(block, bytes) {
    this.stats.puts++;
    this.data.write(bytes, { at: block * this.blockSize });
    // Data first, then the bit: an interruption leaves at most one block to
    // download again.
    this.data.flush();
    this.bits[block >> 3] |= 1 << (block & 7);
    this.map.write(this.bits.subarray(block >> 3, (block >> 3) + 1), { at: block >> 3 });
    this.map.flush();
  }

  close() {
    this.data.close();
    this.map.close();
  }
}

/** Contiguous blocks merged into one request, at most. */
const MAX_RUN_BYTES = 8 << 20;
/** Requests in flight at once for the blocks the guest waits for. */
const DEMAND_PARALLEL = 6;
/**
 * Requests in flight at once for a prefetch list, and their size at most:
 * small enough that a block the guest asks for meanwhile does not queue
 * behind much (HTTP/2 shares the link between them).
 */
const PREFETCH_PARALLEL = 2;
const PREFETCH_RUN_BYTES = 4 << 20;
/** Entries of a prefetch list taken together (sorted, contiguous ones merged). */
const PREFETCH_BATCH = 8;
/** Entries of the disk trace kept per disk. */
const TRACE_MAX = 8192;

/** Format of a prefetch list (`snapshots/<key>.blocks.json` next to a prebuilt snapshot). */
export const PREFETCH_FORMAT = 'vetro-prefetch';

/**
 * Checks a prefetch list (`{ format, version: 1, disk, blockSize, blocks }`)
 * against the disk it is used for (`{ sha256, blockSize, blocks }`): returns
 * `{ blocks }` (in order, duplicates and blocks outside the disk dropped) or
 * `{ why }` it does not apply.
 */
export function prefetchBlocks(list, { sha256, blockSize, blocks }) {
  if (list?.format !== PREFETCH_FORMAT || list.version !== 1) return { why: 'not a prefetch list' };
  if (list.disk !== sha256) return { why: 'for another disk' };
  if (list.blockSize !== blockSize) return { why: `for ${list.blockSize}-byte blocks, not ${blockSize}` };
  const seen = new Set();
  const out = [];
  for (const b of list.blocks ?? []) {
    if (Number.isSafeInteger(b) && b >= 0 && b < blocks && !seen.has(b)) {
      seen.add(b);
      out.push(b);
    }
  }
  return { blocks: out };
}

/** Runs of contiguous blocks (`sorted`), each at most `max` blocks. */
function runsOf(sorted, max) {
  const runs = [];
  for (const b of sorted) {
    const last = runs[runs.length - 1];
    if (last && b === last[last.length - 1] + 1 && last.length < max) last.push(b);
    else runs.push([b]);
  }
  return runs;
}

/** `fn` over `items`, at most `n` at a time. */
async function parallel(items, n, fn) {
  let next = 0;
  const lane = async () => {
    while (next < items.length) await fn(items[next++]);
  };
  await Promise.all(Array.from({ length: Math.min(n, items.length) }, lane));
}

/**
 * Serves the disks of a `Machine` (web/node/vetro.mjs) from their sources.
 *
 * The blocks the guest waits for are fetched with up to DEMAND_PARALLEL
 * requests in flight (contiguous ones merged). A prefetch list (`prefetch`)
 * is fetched in the background into the block cache only, so the machine's
 * memory does not grow: a later request finds the block there. A block
 * already on its way is awaited, not fetched twice. `trace` keeps, per disk,
 * the blocks fetched from the source in order: the material of a prefetch
 * list (tools/aosp/live-path.mjs --trace).
 */
export class DiskFeeder {
  #m;
  disks = [];
  stats = { served: 0, fromCache: 0, fromSource: 0, readahead: 0, failed: 0, waitMs: 0, prefetched: 0, prefetchPending: 0, prefetchWaitMs: 0 };
  /** Demand fetches running (the prefetch waits for them to end). */
  #demand = 0;
  #idle = [];

  constructor(machine) {
    this.#m = machine;
  }

  /**
   * Adds a disk from the source (already open). `cache`: MemoryCache,
   * OpfsCache or null; `readahead`: following blocks to fetch together with
   * every requested block. Returns the disk index.
   */
  add(source, { cache = null, blockSize = 1 << 20, maxBlocks = 0, readOnly = false, readahead = 0 } = {}) {
    const size = Math.floor(source.size / 512) * 512;
    const index = this.#m.addDisk(size, { blockSize, maxBlocks, readOnly });
    this.disks[index] = { source, cache, blockSize, size, readahead, blocks: Math.ceil(size / blockSize), given: new Set(), inflight: new Map(), trace: [] };
    return index;
  }

  #blockLen(d, b) {
    return Math.min(d.blockSize, d.size - b * d.blockSize);
  }

  #give(index, d, b, bytes) {
    this.#m.diskFill(index, b, bytes);
    d.given.add(b);
  }

  /**
   * Reads `run` (contiguous blocks) from the source into the cache; each
   * block gets an in-flight promise (its bytes) until it arrives. Resolves to
   * the blocks' bytes in order.
   */
  #fetchRun(d, run) {
    const offset = run[0] * d.blockSize;
    const length = run.reduce((n, b) => n + this.#blockLen(d, b), 0);
    const all = d.source.read(offset, length).then((bytes) => {
      let at = 0;
      return run.map((b) => {
        const n = this.#blockLen(d, b);
        const block = bytes.subarray(at, at + n);
        at += n;
        d.cache?.put(b, block);
        if (d.trace.length < TRACE_MAX) d.trace.push(b);
        return block;
      });
    });
    run.forEach((b, k) => {
      const p = all.then((out) => out[k]);
      p.catch(() => {}).finally(() => d.inflight.delete(b));
      d.inflight.set(b, p);
    });
    return all;
  }

  /**
   * Obtains and delivers the requested blocks; returns how many had been
   * requested (0 = nothing to do). A source error becomes an
   * I/O error for the guest.
   */
  async serve() {
    const wanted = this.#m.diskWanted();
    if (!wanted.length) return 0;
    const t0 = performance.now();
    this.#demand++;
    try {
      const byDisk = new Map();
      for (const { disk, block } of wanted) {
        if (!byDisk.has(disk)) byDisk.set(disk, new Set());
        byDisk.get(disk).add(block);
      }
      for (const [index, set] of byDisk) await this.#serveDisk(index, set);
      this.stats.served += wanted.length;
      this.stats.waitMs += performance.now() - t0;
    } finally {
      if (--this.#demand === 0) for (const ok of this.#idle.splice(0)) ok();
    }
    return wanted.length;
  }

  async #serveDisk(index, set) {
    const d = this.disks[index];
    if (!d) throw new Error(`disk ${index} unknown to the DiskFeeder`);
    const asked = new Set(set);
    for (const b of asked) {
      for (let k = 1; k <= d.readahead && b + k < d.blocks; k++) {
        if (!d.given.has(b + k) && !set.has(b + k)) {
          set.add(b + k);
          this.stats.readahead++;
        }
      }
    }
    const missing = [];
    const coming = [];
    for (const b of [...set].sort((a, c) => a - c)) {
      if (d.cache?.has(b)) {
        this.#give(index, d, b, d.cache.get(b, this.#blockLen(d, b)));
        this.stats.fromCache++;
      } else if (d.inflight.has(b)) {
        // Already on its way (prefetch): wait for it instead of asking again.
        if (asked.has(b)) coming.push(b);
      } else {
        missing.push(b);
      }
    }
    const fail = (b, e) => {
      console.error(`vetro: disk ${index}: ${e.message ?? e}`);
      if (asked.has(b)) {
        this.#m.diskFail(index, b);
        this.stats.failed++;
      }
    };
    const fetchRun = async (run) => {
      try {
        const blocks = await this.#fetchRun(d, run);
        run.forEach((b, k) => {
          this.#give(index, d, b, blocks[k]);
          this.stats.fromSource++;
        });
      } catch (e) {
        run.forEach((b) => fail(b, e));
      }
    };
    const wait = async (b) => {
      const tw = performance.now();
      let bytes = null;
      try {
        bytes = await d.inflight.get(b);
      } catch {
        // The prefetch failed: ask again, as the guest's own request.
      }
      this.stats.prefetchWaitMs += performance.now() - tw;
      if (bytes) {
        this.#give(index, d, b, bytes);
        this.stats.fromCache++;
      } else await fetchRun([b]);
    };
    const runs = runsOf(missing, Math.max(1, Math.floor(MAX_RUN_BYTES / d.blockSize)));
    await Promise.all([parallel(runs, DEMAND_PARALLEL, fetchRun), ...coming.map(wait)]);
  }

  /**
   * Fetches `blocks` of disk `index` (a prefetch list, in order) into its
   * cache in the background, behind the guest's own requests: PREFETCH_BATCH
   * entries at a time (sorted, contiguous ones merged up to
   * PREFETCH_RUN_BYTES), PREFETCH_PARALLEL requests in flight, none started
   * while the guest waits for a block. Only with a persistent cache (`cache.persistent`, OPFS): blocks already there
   * are skipped, so a later session costs nothing. An error stops the
   * prefetch, nothing else. Resolves to the blocks fetched.
   */
  async prefetch(index, blocks, { stop = () => false } = {}) {
    const d = this.disks[index];
    if (!d?.cache?.persistent) return 0;
    const todo = blocks.filter((b) => b < d.blocks && !d.cache.has(b) && !d.given.has(b));
    const batches = [];
    for (let i = 0; i < todo.length; i += PREFETCH_BATCH) batches.push(todo.slice(i, i + PREFETCH_BATCH).sort((a, c) => a - c));
    this.stats.prefetchPending += todo.length;
    let fetched = 0;
    let failed = null;
    await parallel(batches, PREFETCH_PARALLEL, async (batch) => {
      for (const run of runsOf(batch, Math.max(1, Math.floor(PREFETCH_RUN_BYTES / d.blockSize)))) {
        while (!failed && !stop() && this.#demand > 0) await new Promise((ok) => this.#idle.push(ok));
        if (!failed && !stop()) {
          const fresh = run.filter((b) => !d.cache.has(b) && !d.given.has(b) && !d.inflight.has(b));
          for (const r of runsOf(fresh, run.length)) {
            try {
              await this.#fetchRun(d, r);
              fetched += r.length;
              this.stats.prefetched += r.length;
            } catch (e) {
              failed ??= e;
            }
          }
        }
        this.stats.prefetchPending -= run.length;
      }
    });
    if (failed) console.error(`vetro: disk ${index}: prefetch stopped: ${failed.message ?? failed}`);
    return fetched;
  }
}
