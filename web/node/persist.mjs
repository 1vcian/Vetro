// Persistence from one session to the next (M6, ADR 0017). Uses no Node
// API: it runs in the app's Worker (OPFS) and in Node tests (in-memory files).
//
// - `DiskOverlay`: a disk's persistent copy-on-write overlay. The guest's
//   writes are in vetro-wasm's copy-on-write layer; here the writes to make
//   to the file are requested (`vetro_overlay_take`) and applied. The file
//   format is `vetro_snapshot::overlay`, the same as the CLI's
//   (`vetro boot --disk=... --overlay=FILE`).
// - `SnapshotStore`: the machine snapshot cache, one file of bytes and one of
//   metadata per key (`snapshotKey`), in OPFS or in memory. The metadata is
//   written after the bytes: it marks the snapshot as complete. A snapshot
//   downloaded from elsewhere (the prebuilt one, ADR 0031) goes through
//   `downloadTarget`.
//
// Files are objects with the `FileSystemSyncAccessHandle` interface
// (getSize, read, write, truncate, flush, close): the real OPFS ones in the
// Worker, `MemFile` elsewhere.

/** An in-memory file with the FileSystemSyncAccessHandle interface. */
export class MemFile {
  #buf;
  #len;

  constructor(bytes = new Uint8Array()) {
    this.#buf = new Uint8Array(Math.max(bytes.length, 4096));
    this.#buf.set(bytes);
    this.#len = bytes.length;
  }

  getSize() {
    return this.#len;
  }

  read(dst, { at = 0 } = {}) {
    const n = Math.max(0, Math.min(dst.length, this.#len - at));
    dst.set(this.#buf.subarray(at, at + n));
    return n;
  }

  write(src, { at = 0 } = {}) {
    const end = at + src.length;
    if (end > this.#buf.length) {
      const b = new Uint8Array(Math.max(end, this.#buf.length * 2));
      b.set(this.#buf.subarray(0, this.#len));
      this.#buf = b;
    }
    if (at > this.#len) this.#buf.fill(0, this.#len, at);
    this.#buf.set(src, at);
    this.#len = Math.max(this.#len, end);
    return src.length;
  }

  truncate(n) {
    if (n > this.#len) this.write(new Uint8Array(n - this.#len), { at: this.#len });
    this.#len = n;
  }

  flush() {}

  close() {}

  /** A copy of the content. */
  bytes() {
    return this.#buf.slice(0, this.#len);
  }
}

/** The whole content of a file (MemFile or FileSystemSyncAccessHandle). */
export function readAll(file) {
  const out = new Uint8Array(file.getSize());
  if (out.length && file.read(out, { at: 0 }) !== out.length) throw new Error('short read');
  return out;
}

/** OPFS directory `name` (created if missing). */
export async function opfsDir(name) {
  const root = await navigator.storage.getDirectory();
  return root.getDirectoryHandle(name, { create: true });
}

/** Synchronous handle on file `name` of OPFS directory `dir` (only in a Worker). */
export async function opfsFile(dir, name) {
  const d = typeof dir === 'string' ? await opfsDir(dir) : dir;
  return (await d.getFileHandle(name, { create: true })).createSyncAccessHandle();
}

/** Hex SHA-256 of bytes or of a string. */
export async function sha256Hex(data) {
  const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
  const d = await crypto.subtle.digest('SHA-256', bytes);
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Snapshot cache key: SHA-256 of the JSON of `parts`, which must contain
 * everything that makes a snapshot applicable (format version, kernel,
 * initramfs, command line, machine configuration, disk identities and
 * parameters; for Android `androidKeyParts`). Keys sorted: the same values
 * give the same key.
 */
export async function snapshotKey(parts) {
  const sorted = (v) =>
    v && typeof v === 'object' && !Array.isArray(v)
      ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sorted(v[k])]))
      : Array.isArray(v) ? v.map(sorted) : v;
  return (await sha256Hex(JSON.stringify(sorted(parts)))).slice(0, 32);
}

/** The persistent overlay of disk `disk` of a Machine on `file`. */
export class DiskOverlay {
  stats = { persists: 0, writes: 0, bytes: 0, ms: 0 };

  /**
   * Reads `file` and opens the overlay for base image `identity`: the saved
   * clusters go into the disk. `opened.code`: 'Loaded', 'New', 'Mismatch'
   * (overlay of another base, discarded), 'Corrupt' (discarded).
   */
  static open(machine, disk, file, identity) {
    const opened = machine.overlayOpen(disk, identity, readAll(file));
    if (opened.code === 'NoDisk') throw new Error(`overlay of disk ${disk}: ${opened.message}`);
    return new DiskOverlay(machine, disk, file, opened);
  }

  constructor(machine, disk, file, opened) {
    this.m = machine;
    this.disk = disk;
    this.file = file;
    this.opened = opened;
  }

  /**
   * Writes to the file the guest's writes since last time (after a restore,
   * every cluster that differs from the file). Returns whether it wrote.
   */
  persist() {
    const p = this.m.overlayTake(this.disk);
    if (!p) return false;
    const t0 = performance.now();
    if (p.truncate !== null) this.file.truncate(p.truncate);
    // Data first, then the header (offset 0), with a flush in between.
    let header = null;
    for (const w of p.writes) {
      if (w.at === 0) {
        header = w;
        continue;
      }
      this.file.write(w.bytes, { at: w.at });
      this.stats.bytes += w.bytes.length;
    }
    if (header) {
      this.file.flush();
      this.file.write(header.bytes, { at: 0 });
      this.stats.bytes += header.bytes.length;
    }
    this.file.flush();
    this.stats.persists++;
    this.stats.writes += p.writes.length;
    this.stats.ms += performance.now() - t0;
    return true;
  }

  get info() {
    return this.m.overlayInfo(this.disk);
  }

  get generation() {
    return this.info.generation;
  }

  close() {
    this.file.close();
  }
}

/** Snapshot cache: `<key>.snap` (bytes) and `<key>.json` (metadata). */
export class SnapshotStore {
  #dir;
  #mem;

  /** In OPFS (Worker), in directory `name`. */
  static async opfs(name = 'vetro-snapshots') {
    return new SnapshotStore(await opfsDir(name), null);
  }

  /** In memory (Node, tests). */
  static memory() {
    return new SnapshotStore(null, new Map());
  }

  constructor(dir, mem) {
    this.#dir = dir;
    this.#mem = mem;
  }

  async #write(name, bytes) {
    if (this.#mem) return void this.#mem.set(name, bytes.slice());
    const h = await opfsFile(this.#dir, name);
    try {
      h.truncate(0);
      h.write(bytes, { at: 0 });
      h.flush();
    } finally {
      h.close();
    }
  }

  async #read(name) {
    if (this.#mem) return this.#mem.get(name)?.slice() ?? null;
    let fh;
    try {
      fh = await this.#dir.getFileHandle(name);
    } catch {
      return null;
    }
    const h = await fh.createSyncAccessHandle();
    try {
      return readAll(h);
    } finally {
      h.close();
    }
  }

  async #remove(name) {
    if (this.#mem) return void this.#mem.delete(name);
    await this.#dir.removeEntry(name).catch(() => {});
  }

  /** { meta, bytes } for the key, or null (missing or incomplete). */
  async load(key) {
    const m = await this.#read(`${key}.json`);
    if (!m) return null;
    let meta;
    try {
      meta = JSON.parse(new TextDecoder().decode(m));
    } catch {
      return null;
    }
    const bytes = await this.#read(`${key}.snap`);
    if (!bytes || bytes.length !== meta.size) return null;
    return { meta, bytes };
  }

  /**
   * Only the metadata of a complete snapshot (the file length matches),
   * without reading its bytes: for large snapshots (Android, hundreds of MiB)
   * the bytes are read with `readInto` or `openReader` right where needed.
   */
  async loadMeta(key) {
    const m = await this.#read(`${key}.json`);
    if (!m) return null;
    let meta;
    try {
      meta = JSON.parse(new TextDecoder().decode(m));
    } catch {
      return null;
    }
    if (this.#mem) return this.#mem.get(`${key}.snap`)?.length === meta.size ? meta : null;
    try {
      const h = await (await this.#dir.getFileHandle(`${key}.snap`)).createSyncAccessHandle();
      const size = h.getSize();
      h.close();
      return size === meta.size ? meta : null;
    } catch {
      return null;
    }
  }

  /**
   * A reader of snapshot `key`: `{ size, readAt(view, offset), close() }`
   * (synchronous, for `Machine.snapshotRestoreStream`).
   */
  async openReader(key) {
    if (this.#mem) {
      const all = this.#mem.get(`${key}.snap`);
      return { size: all.length, readAt: (view, at) => view.set(all.subarray(at, at + view.length)), close: () => {} };
    }
    const h = await (await this.#dir.getFileHandle(`${key}.snap`)).createSyncAccessHandle();
    return {
      size: h.getSize(),
      readAt: (view, at) => {
        if (h.read(view, { at }) !== view.length) throw new Error('snapshot: short read');
      },
      close: () => h.close(),
    };
  }

  /** Reads the bytes of snapshot `key` into `view` (`meta.size` long). */
  async readInto(key, view) {
    if (this.#mem) {
      view.set(this.#mem.get(`${key}.snap`));
      return;
    }
    const h = await (await this.#dir.getFileHandle(`${key}.snap`)).createSyncAccessHandle();
    try {
      if (h.read(view, { at: 0 }) !== view.length) throw new Error('snapshot: short read');
    } finally {
      h.close();
    }
  }

  /**
   * Like `save`, with the bytes produced in chunks: `produce(write)` calls
   * `write(bytes, offset)` for each chunk (synchronously) and returns the
   * total length (see `Machine.snapshotSaveTo`).
   */
  async saveStream(key, meta, produce) {
    let size;
    if (!this.#mem) {
      // Into a new file, then in place of the old one: if the save fails
      // (memory exhausted, page closed) the previous snapshot stays.
      const fh = await this.#dir.getFileHandle(`${key}.new`, { create: true });
      if (typeof fh.move === 'function') {
        const h = await fh.createSyncAccessHandle();
        try {
          h.truncate(0);
          size = produce((b, at) => {
            if (h.write(b, { at }) !== b.length) throw new Error('snapshot: short write to OPFS');
          });
          h.flush();
        } finally {
          h.close();
        }
        await this.#remove(`${key}.json`);
        await this.#remove(`${key}.snap`);
        await fh.move(`${key}.snap`);
        await this.#write(`${key}.json`, new TextEncoder().encode(JSON.stringify({ ...meta, size })));
        return size;
      }
      await this.#remove(`${key}.new`);
    }
    await this.#remove(`${key}.json`);
    if (this.#mem) {
      const parts = [];
      size = produce((b, at) => parts.push([at, b.slice()]));
      const all = new Uint8Array(size);
      for (const [at, b] of parts) all.set(b, at);
      this.#mem.set(`${key}.snap`, all);
    } else {
      const h = await opfsFile(this.#dir, `${key}.snap`);
      try {
        h.truncate(0);
        size = produce((b, at) => {
          if (h.write(b, { at }) !== b.length) throw new Error('snapshot: short write to OPFS');
        });
        h.flush();
      } finally {
        h.close();
      }
    }
    await this.#write(`${key}.json`, new TextEncoder().encode(JSON.stringify({ ...meta, size })));
    return size;
  }

  /**
   * Where a snapshot downloaded from elsewhere (the prebuilt one, ADR 0031)
   * is written: `{ file, resume, saveResume(state), finish(meta), close() }`.
   * `file` is the snapshot file itself; the metadata is removed first and
   * written by `finish` (with `size`), so the cache sees the snapshot only
   * when it is complete. `resume` is the last state given to `saveResume`
   * (kept in `<key>.part.json`), for resuming an interrupted download.
   */
  async downloadTarget(key) {
    const partName = `${key}.part.json`;
    const raw = await this.#read(partName);
    let resume = null;
    try {
      resume = raw ? JSON.parse(new TextDecoder().decode(raw)) : null;
    } catch {}
    await this.#remove(`${key}.json`);
    let file;
    if (this.#mem) {
      const f = new MemFile(this.#mem.get(`${key}.snap`) ?? new Uint8Array());
      const save = () => this.#mem.set(`${key}.snap`, f.bytes());
      file = { getSize: () => f.getSize(), read: (d, o) => f.read(d, o), write: (b, o) => f.write(b, o), truncate: (n) => f.truncate(n), flush: save, close: save };
    } else {
      file = await opfsFile(this.#dir, `${key}.snap`);
    }
    // Without a resume state the file content is unknown: start over.
    if (!resume) file.truncate(0);
    return {
      file,
      resume,
      saveResume: (state) => this.#write(partName, new TextEncoder().encode(JSON.stringify(state))),
      finish: async (meta) => {
        file.flush();
        const size = file.getSize();
        file.close();
        await this.#write(`${key}.json`, new TextEncoder().encode(JSON.stringify({ ...meta, size })));
        await this.#remove(partName);
        return size;
      },
      close: () => file.close(),
    };
  }

  /** Saves the bytes, then the metadata (with `size`). */
  async save(key, meta, bytes) {
    await this.#remove(`${key}.json`);
    await this.#write(`${key}.snap`, bytes);
    await this.#write(`${key}.json`, new TextEncoder().encode(JSON.stringify({ ...meta, size: bytes.length })));
  }

  async remove(key) {
    await this.#remove(`${key}.json`);
    await this.#remove(`${key}.snap`);
    await this.#remove(`${key}.part.json`);
  }
}

/**
 * A saved snapshot is valid only with the disk overlays at the generation
 * they had when it was taken (ADR 0017): if a disk moved on afterwards, the
 * snapshot (RAM and guest caches) no longer matches it. `overlays[i]` is disk
 * i's DiskOverlay (or null if the disk is not persistent). Returns null if
 * valid, or the reason.
 */
export function staleReason(meta, overlays) {
  for (const [i, o] of overlays.entries()) {
    if (!o) continue;
    const saved = meta.generations?.[i] ?? null;
    if (saved !== o.generation) return `disk ${i}: overlay at generation ${o.generation}, snapshot at ${saved}`;
  }
  return null;
}

/** Base64 of bytes (for JSON metadata). */
export function toBase64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

export function fromBase64(text) {
  return Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
}
