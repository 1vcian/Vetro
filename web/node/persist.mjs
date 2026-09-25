// Persistenza fra una sessione e l'altra (M6, ADR 0017). Non usa API di
// Node: gira nel Worker dell'app (OPFS) e nei test in Node (file in memoria).
//
// - `DiskOverlay`: l'overlay copy-on-write persistente di un disco. Le
//   scritture del guest stanno nel copy-on-write di vetro-wasm; qui si
//   chiedono le scritture da fare sul file (`vetro_overlay_take`) e si
//   applicano. Il formato del file è quello di `vetro_snapshot::overlay`,
//   uguale per la CLI (`vetro boot --disk=... --overlay=FILE`).
// - `SnapshotStore`: la cache degli snapshot della macchina, un file di
//   byte e uno di metadati per chiave (`snapshotKey`), in OPFS o in memoria.
//   I metadati si scrivono dopo i byte: sono il segno che lo snapshot è
//   completo.
//
// I file sono oggetti con l'interfaccia di `FileSystemSyncAccessHandle`
// (getSize, read, write, truncate, flush, close): quelli veri di OPFS nel
// Worker, `MemFile` altrove.

/** Un file in memoria con l'interfaccia di FileSystemSyncAccessHandle. */
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

  /** Copia del contenuto. */
  bytes() {
    return this.#buf.slice(0, this.#len);
  }
}

/** Il contenuto intero di un file (MemFile o FileSystemSyncAccessHandle). */
export function readAll(file) {
  const out = new Uint8Array(file.getSize());
  if (out.length && file.read(out, { at: 0 }) !== out.length) throw new Error('lettura corta');
  return out;
}

/** Cartella `name` di OPFS (creata se manca). */
export async function opfsDir(name) {
  const root = await navigator.storage.getDirectory();
  return root.getDirectoryHandle(name, { create: true });
}

/** Handle sincrono sul file `name` della cartella OPFS `dir` (solo in un Worker). */
export async function opfsFile(dir, name) {
  const d = typeof dir === 'string' ? await opfsDir(dir) : dir;
  return (await d.getFileHandle(name, { create: true })).createSyncAccessHandle();
}

/** SHA-256 esadecimale di byte o di una stringa. */
export async function sha256Hex(data) {
  const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
  const d = await crypto.subtle.digest('SHA-256', bytes);
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Chiave della cache degli snapshot: SHA-256 del JSON di `parts`, che deve
 * contenere tutto quello che rende uno snapshot applicabile (versione del
 * formato, kernel, initramfs, riga di comando, configurazione della macchina,
 * identità e parametri dei dischi). Le chiavi ordinate: stessa chiave per gli
 * stessi valori.
 */
export async function snapshotKey(parts) {
  const sorted = (v) =>
    v && typeof v === 'object' && !Array.isArray(v)
      ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sorted(v[k])]))
      : Array.isArray(v) ? v.map(sorted) : v;
  return (await sha256Hex(JSON.stringify(sorted(parts)))).slice(0, 32);
}

/** L'overlay persistente del disco `disk` di una Machine su `file`. */
export class DiskOverlay {
  stats = { persists: 0, writes: 0, bytes: 0, ms: 0 };

  /**
   * Legge `file` e apre l'overlay per l'immagine base `identity`: i cluster
   * salvati entrano nel disco. `opened.code`: 'Loaded', 'New', 'Mismatch'
   * (overlay di un'altra base, scartato), 'Corrupt' (scartato).
   */
  static open(machine, disk, file, identity) {
    const opened = machine.overlayOpen(disk, identity, readAll(file));
    if (opened.code === 'NoDisk') throw new Error(`overlay del disco ${disk}: ${opened.message}`);
    return new DiskOverlay(machine, disk, file, opened);
  }

  constructor(machine, disk, file, opened) {
    this.m = machine;
    this.disk = disk;
    this.file = file;
    this.opened = opened;
  }

  /**
   * Scrive nel file le scritture del guest dall'ultima volta (dopo un
   * ripristino, tutti i cluster diversi dal file). Restituisce se ha scritto.
   */
  persist() {
    const p = this.m.overlayTake(this.disk);
    if (!p) return false;
    const t0 = performance.now();
    if (p.truncate !== null) this.file.truncate(p.truncate);
    // Prima i dati, poi l'intestazione (offset 0), con un flush in mezzo.
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

/** Cache degli snapshot: `<chiave>.snap` (byte) e `<chiave>.json` (metadati). */
export class SnapshotStore {
  #dir;
  #mem;

  /** In OPFS (Worker), nella cartella `name`. */
  static async opfs(name = 'vetro-snapshots') {
    return new SnapshotStore(await opfsDir(name), null);
  }

  /** In memoria (Node, test). */
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

  /** { meta, bytes } per la chiave, o null (manca o è incompleto). */
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

  /** Salva i byte, poi i metadati (con `size`). */
  async save(key, meta, bytes) {
    await this.#remove(`${key}.json`);
    await this.#write(`${key}.snap`, bytes);
    await this.#write(`${key}.json`, new TextEncoder().encode(JSON.stringify({ ...meta, size: bytes.length })));
  }

  async remove(key) {
    await this.#remove(`${key}.json`);
    await this.#remove(`${key}.snap`);
  }
}

/**
 * Uno snapshot salvato vale solo con gli overlay dei dischi alla stessa
 * generazione di quando è stato preso (ADR 0017): se il disco è andato
 * avanti dopo, lo snapshot (RAM e cache del guest) non corrisponde più al
 * disco. `overlays[i]` è il DiskOverlay del disco i (o null se il disco non
 * è persistente). Restituisce null se vale, o il motivo.
 */
export function staleReason(meta, overlays) {
  for (const [i, o] of overlays.entries()) {
    if (!o) continue;
    const saved = meta.generations?.[i] ?? null;
    if (saved !== o.generation) return `disco ${i}: overlay alla generazione ${o.generation}, snapshot alla ${saved}`;
  }
  return null;
}

/** Codifica in base64 di byte (per i metadati JSON). */
export function toBase64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

export function fromBase64(text) {
  return Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
}
