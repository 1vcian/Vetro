// Dischi virtio-blk di vetro-wasm con i dati procurati dal JS (M5,
// docs/specs/wasm.md, ADR 0014). Non usa API di Node: gira in Node (test)
// e nel browser (Worker dell'app).
//
// Il giro:
//   1. `vetro_run` si ferma con `Blocked`: una richiesta del guest tocca
//      blocchi che la macchina non ha, e il tempo del guest è fermo;
//   2. `DiskFeeder.serve()` prende i blocchi chiesti (`vetro_disk_wanted`),
//      li cerca prima nella cache (OPFS nel browser), poi nella sorgente
//      (HTTP Range su un URL, o un File locale), unendo i blocchi contigui in
//      una sola richiesta, e li consegna (`vetro_disk_fill`);
//   3. il quanto successivo ripete la richiesta, che ora si completa allo
//      stesso numero di istruzioni che con un disco locale.
//
// Le scritture del guest restano nel livello copy-on-write in memoria dentro
// vetro-wasm: la sorgente e la cache non cambiano mai.

/** Una sorgente letta con HTTP Range. */
export class RangeSource {
  #fetch;
  /** Richieste HTTP fatte e byte ricevuti (per i test e la barra di stato). */
  stats = { requests: 0, bytes: 0 };

  constructor(url, { fetch: f = (...a) => globalThis.fetch(...a) } = {}) {
    this.url = url;
    this.#fetch = f;
    this.size = 0;
    this.key = null;
  }

  /**
   * GET con Range; riprova (fino a 3 volte, con attesa crescente) gli errori
   * di rete e le risposte 5xx: una connessione tenuta aperta e chiusa dal
   * server nel frattempo non deve diventare un errore di I/O per il guest.
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

  /** Legge la dimensione (dal Content-Range di una richiesta di 1 byte). */
  async open() {
    const res = await this.#get('bytes=0-0');
    this.stats.requests++;
    await res.arrayBuffer();
    if (res.status !== 206) throw new Error(`${this.url}: il server non risponde a Range (stato ${res.status})`);
    const m = /\/(\d+)$/.exec(res.headers.get('Content-Range') ?? '');
    if (!m) throw new Error(`${this.url}: Content-Range senza dimensione`);
    this.size = Number(m[1]);
    // La chiave della cache cambia se cambia il file sul server.
    const tag = res.headers.get('ETag') ?? res.headers.get('Last-Modified') ?? '';
    this.key = `${this.url}|${this.size}|${tag}`;
    return this;
  }

  /** `length` byte da `offset` (Uint8Array). */
  async read(offset, length) {
    const end = offset + length - 1;
    const res = await this.#get(`bytes=${offset}-${end}`);
    this.stats.requests++;
    const buf = new Uint8Array(await res.arrayBuffer());
    if (res.status !== 206 || buf.length !== length) {
      throw new Error(`${this.url}: byte ${offset}-${end}: stato ${res.status}, ${buf.length} byte`);
    }
    this.stats.bytes += length;
    return buf;
  }
}

/** Una sorgente da un Blob o File (disco scelto dal computer). */
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

/** Cache dei blocchi in memoria (Node, test, o browser senza OPFS). */
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
 * Cache dei blocchi in OPFS (Origin Private File System), solo in un
 * Worker dedicato: `FileSystemSyncAccessHandle` legge e scrive in modo
 * sincrono. Due file per disco, dal nome derivato dalla chiave della
 * sorgente (URL, dimensione, ETag) e dalla dimensione del blocco: `.img`
 * (i blocchi al loro posto, file sparso) e `.map` (un bit per blocco
 * presente). Al riavvio i blocchi già scaricati si leggono da qui.
 */
export class OpfsCache {
  stats = { hits: 0, puts: 0 };

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
    if (n !== length) throw new Error(`OPFS ${this.name}: blocco ${block} corto (${n} byte)`);
    return buf;
  }

  put(block, bytes) {
    this.stats.puts++;
    this.data.write(bytes, { at: block * this.blockSize });
    // Prima i dati, poi il bit: un'interruzione lascia al più un blocco da
    // riscaricare.
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

/** Blocchi contigui uniti in una richiesta, al più. */
const MAX_RUN_BYTES = 8 << 20;

/** Serve i dischi di una `Machine` (web/node/vetro.mjs) dalle loro sorgenti. */
export class DiskFeeder {
  #m;
  disks = [];
  stats = { served: 0, fromCache: 0, fromSource: 0, readahead: 0, failed: 0, waitMs: 0 };

  constructor(machine) {
    this.#m = machine;
  }

  /**
   * Aggiunge un disco dalla sorgente (già aperta). `cache`: MemoryCache,
   * OpfsCache o null; `readahead`: blocchi seguenti da prendere insieme a
   * ogni blocco chiesto. Restituisce l'indice del disco.
   */
  add(source, { cache = null, blockSize = 1 << 20, maxBlocks = 0, readOnly = false, readahead = 0 } = {}) {
    const size = Math.floor(source.size / 512) * 512;
    const index = this.#m.addDisk(size, { blockSize, maxBlocks, readOnly });
    this.disks[index] = { source, cache, blockSize, size, readahead, blocks: Math.ceil(size / blockSize), given: new Set() };
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
   * Procura e consegna i blocchi chiesti; restituisce quanti ne erano stati
   * chiesti (0 = niente da fare). Un errore della sorgente diventa un
   * errore di I/O per il guest.
   */
  async serve() {
    const wanted = this.#m.diskWanted();
    if (!wanted.length) return 0;
    const t0 = performance.now();
    const byDisk = new Map();
    for (const { disk, block } of wanted) {
      if (!byDisk.has(disk)) byDisk.set(disk, new Set());
      byDisk.get(disk).add(block);
    }
    for (const [index, set] of byDisk) {
      const d = this.disks[index];
      if (!d) throw new Error(`disco ${index} sconosciuto al DiskFeeder`);
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
      for (const b of [...set].sort((a, c) => a - c)) {
        if (d.cache?.has(b)) {
          this.#give(index, d, b, d.cache.get(b, this.#blockLen(d, b)));
          this.stats.fromCache++;
        } else {
          missing.push(b);
        }
      }
      // Blocchi contigui in una sola lettura.
      let i = 0;
      while (i < missing.length) {
        let j = i + 1;
        while (j < missing.length && missing[j] === missing[j - 1] + 1 && (j - i + 1) * d.blockSize <= MAX_RUN_BYTES) j++;
        const first = missing[i];
        const run = missing.slice(i, j);
        const offset = first * d.blockSize;
        const length = run.reduce((n, b) => n + this.#blockLen(d, b), 0);
        let bytes;
        try {
          bytes = await d.source.read(offset, length);
        } catch (e) {
          console.error(`vetro: disco ${index}: ${e.message ?? e}`);
          for (const b of run) {
            if (asked.has(b)) {
              this.#m.diskFail(index, b);
              this.stats.failed++;
            }
          }
          i = j;
          continue;
        }
        let at = 0;
        for (const b of run) {
          const n = this.#blockLen(d, b);
          const block = bytes.subarray(at, at + n);
          at += n;
          d.cache?.put(b, block);
          this.#give(index, d, b, block);
          this.stats.fromSource++;
        }
        i = j;
      }
    }
    this.stats.served += wanted.length;
    this.stats.waitMs += performance.now() - t0;
    return wanted.length;
  }
}
