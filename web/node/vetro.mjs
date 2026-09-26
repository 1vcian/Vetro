// Caricatore di vetro-wasm: istanzia il modulo con i suoi import e avvolge
// l'API C di docs/specs/wasm.md. Non usa API di Node: va bene anche nel
// browser (i byte del .wasm li passa chi chiama).

import { JitEngine } from './jit-engine.mjs';

export const ABI_VERSION = 12;
/** Codici di vetro_run. */
export const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented', 'Blocked'];

/** Bit dei dispositivi di vetro_machine_new_with. */
export const DEV = { GPU: 1, KEYBOARD: 2, TABLET: 4, MULTITOUCH: 8, NET: 16, VSOCK: 32, DEFAULT: 1 | 2 | 4 | 16 };
/** Bit dei dischi. */
export const DISK = { READ_ONLY: 1 };
/** Dispositivi di vetro_input_events. */
export const INPUT = { KEYBOARD: 0, POINTER: 1 };
/** Stati e motivi di chiusura di vetro_net_state (GuestSocket.state). */
export const NET_STATE = ['Unknown', 'Connecting', 'Open', 'Closed'];
/** Codici di vetro_snapshot_restore (0 = riuscito). */
export const RESTORE = [null, 'BadMagic', 'Version', 'Config', 'Corrupt'];
/** Codici di vetro_overlay_open. */
export const OVERLAY = ['Loaded', 'New', 'Mismatch', 'Corrupt', 'NoDisk'];
export const NET_REASON = [null, 'Normal', 'GuestReset', 'RemoteReset', 'Refused', 'Timeout'];
/** Operazioni di vetro_files_request (gestore dei file, ABI 7; SQL con l'ABI 9). */
export const FILES_OP = { STAT: 1, LIST: 2, READ: 3, WRITE: 4, MKDIR: 5, CREATE: 6, DELETE: 7, RENAME: 8, WATCH: 9, UNWATCH: 10, SQL: 11 };
/** Tipi degli ingressi della timeline (vetro_timeline_input, InputKind di vetro-analysis). */
export const TIMELINE_INPUT = { KEY: 0, POINTER: 1, TOUCH: 2, CONSOLE: 3, FILES: 4, POWER: 5, DISPLAY: 6, OTHER: 7 };
/** Tipi degli effetti della timeline (vetro_timeline_effect, EffectKind). */
export const TIMELINE_EFFECT = { HTTP: 0, DNS: 1, TLS: 2, FILE: 3, CONSOLE: 4 };
/** Stati di vetro_rr_status. */
export const RR_STATE = ['Idle', 'Recording', 'Replaying', 'Finished', 'Diverged'];
/** Codici di vetro_replay_start (0 = riuscito). */
export const REPLAY_START = [null, 'NoLog', 'KeyframeMissing', 'Refused'];
/** Stati di vetro_files_status. */
export const FILES_STATUS = ['None', 'Connecting', 'Ready'];
/** Bit degli eventi di inotify (GuestFiles.onEvent). */
export const INOTIFY = {
  MODIFY: 0x2, ATTRIB: 0x4, CLOSE_WRITE: 0x8, MOVED_FROM: 0x40, MOVED_TO: 0x80, CREATE: 0x100, DELETE: 0x200,
  DELETE_SELF: 0x400, MOVE_SELF: 0x800, Q_OVERFLOW: 0x4000, IGNORED: 0x8000, ISDIR: 0x40000000,
};

const utf8 = new TextDecoder();
const toUtf8 = new TextEncoder();

/**
 * Byte di un percorso del guest da una stringa in *surrogateescape* (ADR
 * 0021): i surrogati solitari U+DC80..U+DCFF tornano i byte 0x80..0xFF che
 * non erano UTF-8 valido, il resto è UTF-8.
 */
export function pathBytes(path) {
  if (!/[\udc80-\udcff]/.test(path)) return toUtf8.encode(path);
  const out = [];
  let run = '';
  const flush = () => {
    for (const b of toUtf8.encode(run)) out.push(b);
    run = '';
  };
  for (let i = 0; i < path.length; i++) {
    const c = path.charCodeAt(i);
    const lone = c >= 0xdc80 && c <= 0xdcff && !(i > 0 && path.charCodeAt(i - 1) >= 0xd800 && path.charCodeAt(i - 1) <= 0xdbff);
    if (lone) {
      flush();
      out.push(c - 0xdc00);
    } else run += path[i];
  }
  flush();
  return new Uint8Array(out);
}

/** Una stringa in surrogateescape dai byte di un percorso (l'inverso di pathBytes). */
export function pathString(bytes) {
  const strict = new TextDecoder('utf-8', { fatal: true });
  try {
    return strict.decode(bytes);
  } catch {
    // Byte per byte: le sequenze UTF-8 valide restano, gli altri byte diventano surrogati.
    let s = '';
    let i = 0;
    while (i < bytes.length) {
      const b = bytes[i];
      const n = b < 0x80 ? 1 : b >= 0xc2 && b <= 0xdf ? 2 : b >= 0xe0 && b <= 0xef ? 3 : b >= 0xf0 && b <= 0xf4 ? 4 : 0;
      if (n > 0 && i + n <= bytes.length) {
        try {
          s += strict.decode(bytes.subarray(i, i + n));
          i += n;
          continue;
        } catch {}
      }
      s += String.fromCharCode(0xdc00 + b);
      i++;
    }
    return s;
  }
}

/** Un nome con i byte non UTF-8 (surrogati solitari) mostrati come \xNN. */
export function displayName(s) {
  return s.replace(/[\udc80-\udcff]/g, (c, i) => {
    const prev = i > 0 ? s.charCodeAt(i - 1) : 0;
    return prev >= 0xd800 && prev <= 0xdbff ? c : `\\x${(c.charCodeAt(0) - 0xdc00).toString(16).padStart(2, '0')}`;
  });
}

/** Tipi dei valori SQL nel protocollo del gestore dei file (ADR 0021). */
const SQLV = { NULL: 0, INT: 1, REAL: 2, TEXT: 3, BLOB: 4 };

/**
 * SQL e parametri nel formato di vetro-wasm (`proto::encode_sql_args`).
 * Un parametro è null, un bigint o un numero intero (INTEGER), un numero
 * non intero (REAL), una stringa (TEXT), un Uint8Array (BLOB), un booleano
 * (0/1), o esplicito: { type: 'integer'|'real'|'text'|'blob'|'null', value }.
 */
export function encodeSqlArgs(sql, params = []) {
  const parts = [];
  let len = 0;
  const push = (u8) => {
    parts.push(u8);
    len += u8.length;
  };
  const u32 = (v) => {
    const b = new Uint8Array(4);
    new DataView(b.buffer).setUint32(0, v, true);
    return b;
  };
  const text = toUtf8.encode(sql);
  push(u32(text.length));
  push(text);
  const n = new Uint8Array(2);
  new DataView(n.buffer).setUint16(0, params.length, true);
  push(n);
  for (const p of params) {
    let type;
    let value = p;
    if (p !== null && typeof p === 'object' && !(p instanceof Uint8Array)) {
      type = p.type;
      value = p.value;
    } else if (p === null || p === undefined) type = 'null';
    else if (typeof p === 'bigint' || typeof p === 'boolean' || (typeof p === 'number' && Number.isInteger(p))) type = 'integer';
    else if (typeof p === 'number') type = 'real';
    else if (typeof p === 'string') type = 'text';
    else if (p instanceof Uint8Array) type = 'blob';
    else throw new Error(`parametro SQL non valido: ${p}`);
    if (type === 'null') push(new Uint8Array([SQLV.NULL]));
    else if (type === 'integer') {
      const b = new Uint8Array(9);
      b[0] = SQLV.INT;
      new DataView(b.buffer).setBigInt64(1, BigInt.asIntN(64, BigInt(typeof value === 'boolean' ? Number(value) : value)), true);
      push(b);
    } else if (type === 'real') {
      const b = new Uint8Array(9);
      b[0] = SQLV.REAL;
      new DataView(b.buffer).setFloat64(1, Number(value), true);
      push(b);
    } else if (type === 'text' || type === 'blob') {
      const bytes = type === 'text' ? toUtf8.encode(String(value)) : value;
      push(new Uint8Array([type === 'text' ? SQLV.TEXT : SQLV.BLOB]));
      push(u32(bytes.length));
      push(bytes);
    } else throw new Error(`tipo di parametro SQL ${type}`);
  }
  const out = new Uint8Array(len);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** Un valore SQL dal JSON di vetro-wasm: null, Number o BigInt, String, Uint8Array. */
export function sqlValue(v) {
  if (v === null) return null;
  const [t, x] = v;
  if (t === 'i') {
    const b = BigInt(x);
    return b >= BigInt(Number.MIN_SAFE_INTEGER) && b <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(b) : b;
  }
  if (t === 'f') return Number(x === 'inf' ? Infinity : x === '-inf' ? -Infinity : x);
  if (t === 't') return x;
  return new Uint8Array(x.match(/../g)?.map((h) => parseInt(h, 16)) ?? []);
}

/** Istanzia vetro-wasm dai byte del .wasm: { exports, jit }. */
export async function instantiate(wasmBytes) {
  const jit = new JitEngine();
  let exports = null;
  const imports = {
    vetro_host: {
      panic: (ptr, len) => {
        const msg = utf8.decode(new Uint8Array(exports.memory.buffer, ptr >>> 0, len));
        console.error(`vetro-wasm: panic: ${msg}`);
      },
    },
    vetro_jit: jit.imports(),
  };
  const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
  exports = instance.exports;
  jit.attach(exports);
  const abi = exports.vetro_abi_version();
  if (abi !== ABI_VERSION) throw new Error(`vetro-wasm: API ${abi}, attesa ${ABI_VERSION}`);
  return { exports, jit };
}

/** Copia `bytes` in un buffer nuovo della memoria del modulo: [ptr, len]. */
export function copyIn(x, bytes) {
  if (bytes.length === 0) return [0, 0];
  // Puntatori come u32: oltre 2 GiB un i32 di WASM arriva negativo.
  const ptr = x.vetro_alloc(bytes.length) >>> 0;
  if (ptr === 0) throw new Error(`vetro_alloc(${bytes.length}) fallita`);
  // Vista presa dopo l'allocazione: la memoria può essere cresciuta.
  new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

/** Una macchina di vetro-wasm. */
export class Machine {
  #x;
  #vm;
  #buf;
  #cap = 64 * 1024;

  /**
   * ramSize/nowSecs/seed: BigInt, 0n = i valori di MachineConfig::default.
   * devices: bit di DEV (default: GPU, tastiera e tablet, come
   * `Devices::default`); width/height: risoluzione iniziale della GPU
   * (0 = 1280x800).
   */
  constructor(x, { ramSize = 0n, nowSecs = 0n, seed = 0n, devices = DEV.DEFAULT, width = 0, height = 0 } = {}) {
    this.#x = x;
    this.#vm = x.vetro_machine_new_with(ramSize, nowSecs, seed, devices, width, height);
    this.#buf = x.vetro_alloc(this.#cap) >>> 0;
  }

  /** Buffer di lavoro di `n` byte (dentro il buffer della console). */
  #scratch(n) {
    if (n > this.#cap) throw new Error(`buffer di lavoro troppo piccolo (${n} > ${this.#cap})`);
    return this.#buf;
  }

  // ---- Display (virtio-gpu) -------------------------------------------

  /** { width, height } dello scanout, o null se spento. */
  displaySize(scanout = 0) {
    const v = this.#x.vetro_display_size(this.#vm, scanout);
    return v === 0n ? null : { width: Number(v >> 32n), height: Number(v & 0xffffffffn) };
  }

  /** Aggiornamenti dello scanout (Number): se non cambia, niente da ridisegnare. */
  displayUpdates(scanout = 0) {
    return Number(this.#x.vetro_display_updates(this.#vm, scanout));
  }

  /**
   * Vista sui pixel RGBA dello scanout nella memoria del modulo (valida fino
   * alla prossima esecuzione), o null.
   */
  displayPixels(scanout = 0) {
    const size = this.displaySize(scanout);
    if (!size) return null;
    const ptr = this.#x.vetro_display_ptr(this.#vm, scanout) >>> 0;
    return new Uint8Array(this.#x.memory.buffer, ptr, size.width * size.height * 4);
  }

  /** Rettangolo cambiato dall'ultima chiamata { x, y, width, height }, o null. */
  displayTakeDirty(scanout = 0) {
    const p = this.#scratch(16);
    if (!this.#x.vetro_display_take_dirty(this.#vm, scanout, p)) return null;
    const [x, y, width, height] = new Uint32Array(this.#x.memory.buffer, p, 4);
    return { x, y, width, height };
  }

  /**
   * I pixel RGBA del rettangolo `r` in un ArrayBuffer nuovo (da trasferire
   * a un altro thread), righe da `r.width * 4` byte.
   */
  displayCopy(r, scanout = 0) {
    const size = this.displaySize(scanout);
    const src = this.displayPixels(scanout);
    const out = new Uint8ClampedArray(r.width * r.height * 4);
    for (let y = 0; y < r.height; y++) {
      const at = ((r.y + y) * size.width + r.x) * 4;
      out.set(src.subarray(at, at + r.width * 4), y * r.width * 4);
    }
    return out;
  }

  /** Risoluzione chiesta per lo scanout (ingresso dell'host). */
  displayResize(width, height, scanout = 0) {
    return this.#x.vetro_display_resize(this.#vm, scanout, width, height) === 1;
  }

  /** Cursore { resource, x, y, hotX, hotY, updates }, o null senza GPU. */
  cursor(scanout = 0) {
    const p = this.#scratch(24);
    if (!this.#x.vetro_cursor_state(this.#vm, scanout, p)) return null;
    const [resource, x, y, hotX, hotY, updates] = new Uint32Array(this.#x.memory.buffer, p, 6);
    return { resource, x, y, hotX, hotY, updates };
  }

  /** Immagine del cursore, copia 64x64 RGBA (Uint8ClampedArray), o null. */
  cursorImage(scanout = 0) {
    const ptr = this.#x.vetro_cursor_image(this.#vm, scanout) >>> 0;
    return ptr ? new Uint8ClampedArray(this.#x.memory.buffer, ptr, 64 * 64 * 4).slice() : null;
  }

  // ---- Ingressi (virtio-input, GPIO) ------------------------------------

  /** Tasto Linux (KEY_*) premuto o rilasciato; false se non c'è la tastiera. */
  key(code, down) {
    return this.#x.vetro_input_key(this.#vm, code, down ? 1 : 0) === 1;
  }

  /** Posizione assoluta del tablet (0..32767). */
  pointerMove(x, y) {
    return this.#x.vetro_input_abs(this.#vm, x, y) === 1;
  }

  /** Pulsante del puntatore (BTN_LEFT = 0x110, ...). */
  pointerButton(code, down) {
    return this.#x.vetro_input_button(this.#vm, code, down ? 1 : 0) === 1;
  }

  /** Contatto del touchscreen: pos = [x, y] (0..32767) o null per toglierlo. */
  touch(slot, pos) {
    const [x, y] = pos ?? [0, 0];
    return this.#x.vetro_input_touch(this.#vm, slot, x, y, pos ? 1 : 0) === 1;
  }

  /** Eventi evdev grezzi [[type, code, value], ...] su INPUT.KEYBOARD o INPUT.POINTER. */
  inputEvents(device, events) {
    const p = this.#scratch(events.length * 12);
    new Uint32Array(this.#x.memory.buffer, p, events.length * 3).set(events.flat().map((v) => v >>> 0));
    return this.#x.vetro_input_events(this.#vm, device, p, events.length) === 1;
  }

  /** LED della tastiera accesi dal guest (bit LED_*). */
  get leds() {
    return this.#x.vetro_input_leds(this.#vm);
  }

  /** Livello di una linea del GPIO; senza `line`, il tasto di accensione. */
  gpio(level, line = this.#x.vetro_power_key_line()) {
    this.#x.vetro_gpio_input(this.#vm, line, level ? 1 : 0);
  }

  // ---- Dischi (virtio-blk) ----------------------------------------------

  /**
   * Disco con i dati dal JS a blocchi (vedi web/node/disk.mjs): size in
   * byte (Number o BigInt), blockSize potenza di due >= 512, maxBlocks
   * blocchi in memoria (0 = nessun limite). Restituisce l'indice.
   */
  addDisk(size, { blockSize = 1 << 20, maxBlocks = 0, readOnly = false } = {}) {
    const i = this.#x.vetro_disk_add(this.#vm, BigInt(size), blockSize, maxBlocks, readOnly ? DISK.READ_ONLY : 0);
    if (i < 0) throw new Error(`vetro_disk_add: ${this.#message()}`);
    return i;
  }

  /** Disco con tutto il contenuto in memoria (sempre pronto). */
  addDiskMem(bytes, { readOnly = false } = {}) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const i = x.vetro_disk_add_mem(this.#vm, p, n, readOnly ? DISK.READ_ONLY : 0);
    if (n) x.vetro_free(p, n);
    if (i < 0) throw new Error(`vetro_disk_add_mem: ${this.#message()}`);
    return i;
  }

  /** Blocchi chiesti: [{ disk, block }] (block Number). */
  diskWanted() {
    const x = this.#x;
    const out = [];
    const cap = 256;
    const p = this.#scratch(cap * 16);
    for (;;) {
      const n = x.vetro_disk_wanted(this.#vm, p, cap);
      const v = new BigUint64Array(x.memory.buffer, p, 2 * n);
      for (let k = 0; k < n; k++) out.push({ disk: Number(v[2 * k]), block: Number(v[2 * k + 1]) });
      if (n < cap) return out;
    }
  }

  /** Consegna un blocco (Uint8Array); lancia se rifiutato. */
  diskFill(disk, block, bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_disk_fill(this.#vm, disk, BigInt(block), p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 0) throw new Error(`vetro_disk_fill(${disk}, ${block}, ${n} byte): codice ${r}`);
  }

  /** Il blocco non si può avere: il guest riceve un errore di I/O. */
  diskFail(disk, block) {
    this.#x.vetro_disk_fail(this.#vm, disk, BigInt(block));
  }

  /** Contatori del disco, o null. */
  diskStats(disk) {
    const names = ['size', 'blockSize', 'cachedBlocks', 'misses', 'fills', 'evictions', 'failures', 'dirtyClusters'];
    const p = this.#scratch(8 * names.length);
    const n = this.#x.vetro_disk_stats(this.#vm, disk, p, names.length);
    if (!n) return null;
    const v = new BigUint64Array(this.#x.memory.buffer, p, names.length);
    return Object.fromEntries(names.map((k, i) => [k, Number(v[i])]));
  }

  // ---- Rete: connessioni verso i servizi del guest (ABI 5) ---------------

  /**
   * Apre una connessione TCP verso `port` del guest (10.0.2.15), che la vede
   * arrivare dal gateway 10.0.2.2, come `hostfwd` di QEMU (per esempio adbd
   * sulla 5555). Il SYN parte al prossimo `run`. Lancia senza rete.
   */
  connectGuest(port) {
    const id = this.#x.vetro_net_connect(this.#vm, port);
    if (id === 0n) throw new Error(`vetro_net_connect(${port}): rete assente o porta non valida`);
    return new GuestSocket(this.#x, this.#vm, id);
  }

  // ---- Gestore dei file (ABI 7, ADR 0020) --------------------------------

  /**
   * Il client del gestore dei file verso il demone `vetro-files` del guest
   * (serve DEV.VSOCK). Le richieste partono e le risposte arrivano con
   * `pump()`, da chiamare fra un quanto e l'altro. Lancia senza vsock.
   */
  files(port = 0) {
    return new GuestFiles(this.#x, this.#vm, port);
  }

  // ---- Snapshot (ABI 4, ADR 0015) ---------------------------------------

  /** Versione del formato degli snapshot (da mettere nelle chiavi delle cache). */
  get snapshotVersion() {
    return this.#x.vetro_snapshot_version();
  }

  /**
   * Snapshot della macchina intera, copiato fuori dalla memoria del modulo
   * (Uint8Array). Leggere prima la console: l'uscita non ancora letta dal JS
   * non entra.
   */
  snapshotSave() {
    const x = this.#x;
    const n = x.vetro_snapshot_save(this.#vm) >>> 0;
    const ptr = x.vetro_snapshot_ptr(this.#vm) >>> 0;
    const out = new Uint8Array(x.memory.buffer, ptr, n).slice();
    x.vetro_snapshot_clear(this.#vm);
    return out;
  }

  /**
   * Snapshot senza copiarlo nel JS: `use(view)` riceve la vista sui byte
   * nella memoria del modulo (da non usare dopo il ritorno) e può scriverli
   * altrove (OPFS); restituisce quello che restituisce `use`. Con Android lo
   * snapshot è di centinaia di MiB: una copia in più conta (ADR 0028).
   */
  async snapshotSaveWith(use) {
    const x = this.#x;
    const n = x.vetro_snapshot_save(this.#vm) >>> 0;
    const ptr = x.vetro_snapshot_ptr(this.#vm) >>> 0;
    try {
      return await use(new Uint8Array(x.memory.buffer, ptr, n));
    } finally {
      x.vetro_snapshot_clear(this.#vm);
    }
  }

  /**
   * Come `snapshotRestore`, con i byte scritti da `fill(view)` direttamente
   * in un buffer di `n` byte della memoria del modulo (per esempio letti da
   * OPFS), senza una copia nel JS.
   */
  async snapshotRestoreWith(n, fill) {
    const x = this.#x;
    const ptr = x.vetro_alloc(n) >>> 0;
    if (!ptr) throw Object.assign(new Error(`vetro_alloc(${n}) fallita: memoria del modulo esaurita`), { code: 'Memory' });
    try {
      await fill(new Uint8Array(x.memory.buffer, ptr, n));
      const r = x.vetro_snapshot_restore(this.#vm, ptr, n);
      if (r !== 0) {
        const e = new Error(`vetro_snapshot_restore: ${RESTORE[r] ?? r}: ${this.#message()}`);
        e.code = RESTORE[r] ?? String(r);
        throw e;
      }
    } finally {
      x.vetro_free(ptr, n);
    }
  }

  /** Byte della memoria lineare del modulo (RAM del guest compresa). */
  get memoryBytes() {
    return this.#x.memory.buffer.byteLength;
  }

  /**
   * Ripristina uno snapshot su questa macchina, costruita come quella salvata
   * (stessi dispositivi, stessi dischi aggiunti nello stesso ordine). Lancia
   * un Error con `code` ('BadMagic', 'Version', 'Config', 'Corrupt') e il
   * motivo; con i primi tre la macchina non è cambiata.
   */
  snapshotRestore(bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_snapshot_restore(this.#vm, p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 0) {
      const e = new Error(`vetro_snapshot_restore: ${RESTORE[r] ?? r}: ${this.#message()}`);
      e.code = RESTORE[r] ?? String(r);
      throw e;
    }
  }

  // ---- Overlay persistente dei dischi (ABI 6, ADR 0017) -----------------

  /**
   * Apre l'overlay del disco `disk` dal contenuto del file (`bytes`, vuoto se
   * non c'è) per l'immagine base `identity` (stringa). Restituisce
   * { code: 'Loaded' | 'New' | 'Mismatch' | 'Corrupt' | 'NoDisk', message }.
   * Con 'Mismatch' e 'Corrupt' l'overlay è scartato: la prossima
   * `overlayTake` tronca il file.
   */
  overlayOpen(disk, identity, bytes) {
    const x = this.#x;
    const id = copyIn(x, toUtf8.encode(identity));
    const data = copyIn(x, bytes);
    const r = x.vetro_overlay_open(this.#vm, disk, ...id, ...data);
    for (const [p, n] of [id, data]) if (n) x.vetro_free(p, n);
    return { code: OVERLAY[r] ?? String(r), message: r >= 2 ? this.#message() : '' };
  }

  /**
   * Scritture da fare sul file dell'overlay del disco `disk` per salvarci le
   * scritture del guest fatte finora: { truncate: Number | null, writes:
   * [{ at: Number, bytes: Uint8Array }] } in ordine (l'intestazione per
   * ultima), o null se non c'è niente di nuovo.
   */
  overlayTake(disk) {
    const x = this.#x;
    const n = x.vetro_overlay_take(this.#vm, disk) >>> 0;
    if (n === 0) return null;
    const ptr = x.vetro_overlay_ptr(this.#vm) >>> 0;
    const buf = new Uint8Array(x.memory.buffer, ptr, n).slice();
    x.vetro_overlay_clear(this.#vm);
    const v = new DataView(buf.buffer);
    const t = v.getBigUint64(0, true);
    const count = v.getUint32(8, true);
    const writes = [];
    let at = 12;
    for (let k = 0; k < count; k++) {
      const off = Number(v.getBigUint64(at, true));
      const len = v.getUint32(at + 8, true);
      writes.push({ at: off, bytes: buf.subarray(at + 12, at + 12 + len) });
      at += 12 + len;
    }
    return { truncate: t === 0xffffffffffffffffn ? null : Number(t), writes };
  }

  /** Contatori dell'overlay del disco, o null senza overlay. */
  overlayInfo(disk) {
    const names = ['generation', 'clusters', 'slots', 'damaged', 'fileLength'];
    const p = this.#scratch(8 * names.length);
    const n = this.#x.vetro_overlay_info(this.#vm, disk, p, names.length);
    if (!n) return null;
    const v = new BigUint64Array(this.#x.memory.buffer, p, names.length);
    return Object.fromEntries(names.map((k, i) => [k, Number(v[i])]));
  }

  // ---- Ispettore di rete e timeline (ABI 8, ADR 0023) ---------------------

  /** L'ultimo risultato di Rust (buffer dei risultati), copiato; poi liberato. */
  #result(n) {
    const x = this.#x;
    if (!n) return new Uint8Array();
    const ptr = x.vetro_result_ptr(this.#vm) >>> 0;
    const out = new Uint8Array(x.memory.buffer, ptr, n >>> 0).slice();
    x.vetro_result_clear(this.#vm);
    return out;
  }

  #json(n) {
    return n ? JSON.parse(utf8.decode(this.#result(n))) : null;
  }

  #u64s(fn, names, ...args) {
    const x = this.#x;
    const p = this.#scratch(8 * names.length);
    const n = fn(this.#vm, ...args, p, names.length);
    const v = new BigUint64Array(x.memory.buffer, p, names.length);
    return n ? Object.fromEntries(names.map((k, i) => [k, Number(v[i])])) : null;
  }

  /** Accende o spegne la cattura dei frame di virtio-net; false senza rete. */
  capture(on = true) {
    return this.#x.vetro_capture_set(this.#vm, on ? 1 : 0) === 1;
  }

  captureClear() {
    this.#x.vetro_capture_clear(this.#vm);
  }

  /** { on, frames, bytes, dropped }. */
  captureStats() {
    const s = this.#u64s(this.#x.vetro_capture_stats, ['on', 'frames', 'bytes', 'dropped']);
    return { ...s, on: s.on === 1 };
  }

  /** La lista dell'ispettore: { frames, requests: [...], dns: [...], tls: [...] } (docs/specs/analysis.md). */
  inspectRequests() {
    return this.#json(this.#x.vetro_inspect_requests(this.#vm));
  }

  /** Il dettaglio della richiesta `index` ({ row, request, response }), o null. */
  inspectRequest(index) {
    return this.#json(this.#x.vetro_inspect_request(this.#vm, index));
  }

  /** L'HAR 1.2 della cattura (stringa); epochUs: µs Unix del tempo 0 del guest. */
  inspectHar(epochUs = 0) {
    return utf8.decode(this.#result(this.#x.vetro_inspect_har(this.#vm, BigInt(epochUs))));
  }

  /** Il pcapng della cattura (Uint8Array). */
  inspectPcapng(epochUs = 0) {
    return this.#result(this.#x.vetro_inspect_pcapng(this.#vm, BigInt(epochUs)));
  }

  /** Annota un ingresso dell'utente (kind in TIMELINE_INPUT) all'istruzione corrente. */
  timelineInput(kind, label, weak = false) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(label));
    x.vetro_timeline_input(this.#vm, kind, weak ? 1 : 0, p, n);
    if (n) x.vetro_free(p, n);
  }

  /** Annota un effetto (kind in TIMELINE_EFFECT) all'istruzione corrente. */
  timelineEffect(kind, label) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(label));
    const ok = x.vetro_timeline_effect(this.#vm, kind, p, n) === 1;
    if (n) x.vetro_free(p, n);
    return ok;
  }

  /** La timeline { windowUs, inputs, effects, ... } con finestra `windowUs` (0 = 3 s). */
  timeline(windowUs = 0) {
    return this.#json(this.#x.vetro_timeline_json(this.#vm, BigInt(windowUs)));
  }

  /** Cambia quando la timeline cambia (BigInt). */
  timelineVersion() {
    return this.#x.vetro_timeline_version(this.#vm);
  }

  timelineClear() {
    this.#x.vetro_timeline_clear(this.#vm);
  }

  // ---- Record & replay (ABI 8, ADR 0019 e 0023) ------------------------------

  /** Registra da qui, con un keyframe ogni `keyframeEvery` istruzioni (il primo subito). */
  recordStart(keyframeEvery = 200_000_000) {
    this.#x.vetro_record_start(this.#vm, BigInt(keyframeEvery));
  }

  /** Finisce la registrazione (il log resta nella macchina); false se non si registrava. */
  recordStop() {
    return this.#x.vetro_record_stop(this.#vm) === 1;
  }

  /** { state (RR_STATE), progress, events, keyframes, startSteps, endSteps, hasLog, message }. */
  rrStatus() {
    const x = this.#x;
    const names = ['progress', 'events', 'keyframes', 'startSteps', 'endSteps', 'hasLog'];
    const p = this.#scratch(8 * names.length);
    const code = x.vetro_rr_status(this.#vm, p, names.length);
    const v = new BigUint64Array(x.memory.buffer, p, names.length);
    const out = Object.fromEntries(names.map((k, i) => [k, Number(v[i])]));
    out.hasLog = out.hasLog === 1;
    out.state = RR_STATE[code];
    out.message = out.state === 'Diverged' ? this.#message() : '';
    return out;
  }

  /** Il file del log con i keyframe presenti (Uint8Array, vuoto senza log). */
  logEncode() {
    return this.#result(this.#x.vetro_log_encode(this.#vm));
  }

  /** Carica un file di log (al posto del log che c'era); lancia se non è valido. */
  logLoad(bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_log_load(this.#vm, p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 0) throw new Error(`log non valido: ${this.#message()}`);
  }

  /** { startSteps, endSteps, events, keyframes, keyframeEvery, jit, sameMachine, eventsBytes }, o null. */
  logInfo() {
    const s = this.#u64s(this.#x.vetro_log_info, ['startSteps', 'endSteps', 'events', 'keyframes', 'keyframeEvery', 'jit', 'sameMachine', 'eventsBytes']);
    return s && { ...s, jit: s.jit === 1, sameMachine: s.sameMachine === 1 };
  }

  /** { step, consoleLen, consoleHash, size, present } del keyframe `index`, o null. */
  logKeyframe(index) {
    const s = this.#u64s(this.#x.vetro_log_keyframe, ['step', 'consoleLen', 'consoleHash', 'size', 'present'], index);
    return s && { ...s, present: s.present === 1 };
  }

  /** Sposta fuori i byte del keyframe `index` (Uint8Array; vuota se già fuori). */
  logKeyframeTake(index) {
    return this.#result(this.#x.vetro_log_keyframe_take(this.#vm, index));
  }

  /** Rimette i byte del keyframe `index`; lancia se rifiutati. */
  logKeyframePut(index, bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_log_keyframe_put(this.#vm, index, p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 1) throw new Error(`keyframe ${index} rifiutato (${n} byte)`);
  }

  /** Indice del keyframe da cui parte il replay verso l'istruzione `step`, -1 se nessuno. */
  logKeyframeFor(step) {
    return this.#x.vetro_log_keyframe_for(this.#vm, BigInt(step));
  }

  /** Gli eventi del log: [{ i, step, kind, label, weak, user }]. */
  logEvents() {
    return this.#json(this.#x.vetro_log_events(this.#vm)) ?? [];
  }

  /**
   * Replay del log dall'ultimo keyframe non oltre `step` (che dev'essere
   * presente). Lancia un Error con `code` ('NoLog', 'KeyframeMissing',
   * 'Refused') e il motivo.
   */
  replayStart(step = 0) {
    const r = this.#x.vetro_replay_start(this.#vm, BigInt(step));
    if (r !== 0) {
      const code = REPLAY_START[r] ?? String(r);
      throw Object.assign(new Error(`replay: ${code}: ${this.#message()}`), { code });
    }
  }

  /** I registri al punto raggiunto (testo di Machine::registers_text). */
  registersText() {
    return utf8.decode(this.#result(this.#x.vetro_registers_text(this.#vm)));
  }

  /** `len` byte all'indirizzo virtuale `va` (BigInt o Number): { bytes } o { fault } (BigInt). */
  readVirt(va, len) {
    const x = this.#x;
    const p = len ? x.vetro_alloc(len) >>> 0 : 0;
    const f = x.vetro_alloc(8) >>> 0;
    const ok = x.vetro_read_virt(this.#vm, BigInt(va), p, len, f) === 1;
    const out = ok ? { bytes: new Uint8Array(x.memory.buffer, p, len).slice() } : { fault: new BigUint64Array(x.memory.buffer, f, 1)[0] };
    if (len) x.vetro_free(p, len);
    x.vetro_free(f, 8);
    return out;
  }

  /** Indirizzo fisico di `va` (BigInt), o null se non è mappato. */
  translate(va) {
    const pa = this.#x.vetro_translate(this.#vm, BigInt(va));
    return pa === 0xffffffffffffffffn ? null : pa;
  }

  /** `len` byte di RAM all'indirizzo fisico `pa`, o null fuori dalla RAM. */
  readPhys(pa, len) {
    const x = this.#x;
    const p = x.vetro_alloc(Math.max(len, 1)) >>> 0;
    const ok = x.vetro_read_phys(this.#vm, BigInt(pa), p, len) === 1;
    const out = ok ? new Uint8Array(x.memory.buffer, p, len).slice() : null;
    x.vetro_free(p, Math.max(len, 1));
    return out;
  }

  #message() {
    const x = this.#x;
    const ptr = x.vetro_message_ptr(this.#vm) >>> 0;
    return utf8.decode(new Uint8Array(x.memory.buffer, ptr, x.vetro_message_len(this.#vm)));
  }

  /** Kernel, initramfs (o null) e riga di comando; lancia in caso di errore. */
  loadLinux(image, initrd, cmdline) {
    const x = this.#x;
    const bufs = [copyIn(x, image), copyIn(x, initrd ?? new Uint8Array()), copyIn(x, toUtf8.encode(cmdline))];
    const code = x.vetro_load_linux(this.#vm, ...bufs.flat());
    for (const [p, n] of bufs) x.vetro_free(p, n);
    if (code !== 0) throw new Error(`vetro_load_linux: codice ${code}: ${this.#message()}`);
  }

  /**
   * Avvio da immagini Android (ABI 12, ADR 0018): `boot` (boot.img),
   * `vendorBoot`, `initBoot` (Uint8Array o null), `params` (parametri del
   * bootloader: gli `androidboot.*` vanno nel bootconfig), `recovery`.
   * Restituisce la descrizione di kernel e ramdisk; lancia in caso di errore.
   */
  loadAndroid({ boot, vendorBoot = null, initBoot = null, params = '', recovery = false }) {
    const x = this.#x;
    const empty = new Uint8Array();
    const bufs = [copyIn(x, boot), copyIn(x, vendorBoot ?? empty), copyIn(x, initBoot ?? empty), copyIn(x, toUtf8.encode(params))];
    const code = x.vetro_load_android(this.#vm, ...bufs.flat(), recovery ? 1 : 0);
    for (const [p, n] of bufs) if (n) x.vetro_free(p, n);
    const msg = this.#message();
    if (code !== 0) throw new Error(`vetro_load_android: codice ${code}: ${msg}`);
    return msg;
  }

  /** Esegue al più `budget` istruzioni; restituisce il motivo dell'arresto. */
  run(budget) {
    const x = this.#x;
    const code = x.vetro_run(this.#vm, BigInt(budget));
    if (STOP[code] === 'Unimplemented') {
      const pc = x.vetro_unimplemented_pc(this.#vm).toString(16);
      const raw = (x.vetro_unimplemented_raw(this.#vm) >>> 0).toString(16).padStart(8, '0');
      return `Unimplemented { pc: 0x${pc}, raw: 0x${raw}, what: ${JSON.stringify(this.#message())} }`;
    }
    return STOP[code] ?? `codice ${code}`;
  }

  /** L'uscita della console dall'ultima lettura (Uint8Array). */
  consoleRead() {
    const x = this.#x;
    const parts = [];
    let total = 0;
    for (;;) {
      const n = x.vetro_console_read(this.#vm, this.#buf, this.#cap);
      if (n === 0) break;
      parts.push(new Uint8Array(x.memory.buffer, this.#buf, n).slice());
      total += n;
    }
    if (parts.length === 1) return parts[0];
    const out = new Uint8Array(total);
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    return out;
  }

  /** Scrive sulla console, come dalla tastiera. */
  consoleWrite(text) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(text));
    x.vetro_console_write(this.#vm, p, n);
    x.vetro_free(p, n);
  }

  /**
   * Attiva il JIT della modalità sistema (ADR 0013): `threshold` ingressi
   * prima di tradurre un blocco, `batch` blocchi per modulo. Il risultato
   * non cambia, solo la velocità.
   */
  setJit(threshold = 64, batch = 16) {
    this.#x.vetro_machine_set_jit(this.#vm, threshold, batch);
  }

  /** Contatori del JIT (`SysJitStats`), o null senza JIT. */
  jitStats() {
    const x = this.#x;
    const names = ['jitSteps', 'runs', 'resolves', 'calls', 'blocks', 'modules', 'reused', 'invalidatedPages', 'faults',
      'svcs', 'stops', 'epochs', 'tlbFlushes', 'tlbFills', 'resets', 'yields'];
    const p = x.vetro_alloc(8 * names.length) >>> 0;
    const n = x.vetro_jit_stats(this.#vm, p, names.length);
    const v = new BigUint64Array(x.memory.buffer, p, names.length);
    const out = n ? Object.fromEntries(names.map((k, i) => [k, Number(v[i])])) : null;
    x.vetro_free(p, 8 * names.length);
    return out;
  }

  /** Istruzioni eseguite (BigInt). */
  get steps() {
    return this.#x.vetro_steps(this.#vm);
  }

  /** Tempo del guest in ns (BigInt). */
  get guestNs() {
    return this.#x.vetro_guest_ns(this.#vm);
  }

  free() {
    this.#x.vetro_free(this.#buf, this.#cap);
    this.#x.vetro_machine_free(this.#vm);
  }
}

/**
 * Una connessione dal JS verso un servizio TCP del guest (vedi
 * `Machine.connectGuest`). Sincrona: `send` mette in coda, `recv` legge ciò
 * che è arrivato; i byte si muovono mentre la macchina esegue (`run`), quindi
 * chi la usa alterna i due, come la console. Scrivere, leggere byte pronti,
 * chiudere sono ingressi della macchina (da registrare per il replay); lo
 * stato no.
 */
export class GuestSocket {
  #x;
  #vm;
  #buf;
  #cap = 64 * 1024;

  constructor(x, vm, id) {
    this.#x = x;
    this.#vm = vm;
    /** Id della connessione nello stack (BigInt). */
    this.id = id;
    this.#buf = x.vetro_alloc(this.#cap) >>> 0;
  }

  /**
   * { state, reason, readable, writable, guestEof, unsent }: state in
   * NET_STATE, reason in NET_REASON (null finché è aperta), guestEof vero
   * quando il guest ha chiuso il suo verso e tutto è stato letto.
   */
  state() {
    const x = this.#x;
    const code = x.vetro_net_state(this.#vm, this.id, this.#buf, 5);
    if (code === 0) return { state: 'Unknown', reason: null, readable: 0, writable: 0, guestEof: false, unsent: 0 };
    const [reason, readable, writable, eof, unsent] = new Uint32Array(x.memory.buffer, this.#buf, 5);
    return { state: NET_STATE[code], reason: NET_REASON[reason], readable, writable, guestEof: eof === 1, unsent };
  }

  /** Mette in coda byte (Uint8Array) per il guest; restituisce quanti ne ha presi. */
  send(bytes) {
    let sent = 0;
    while (sent < bytes.length) {
      const n = Math.min(this.#cap, bytes.length - sent);
      new Uint8Array(this.#x.memory.buffer, this.#buf, n).set(bytes.subarray(sent, sent + n));
      const k = this.#x.vetro_net_send(this.#vm, this.id, this.#buf, n);
      sent += k;
      if (k < n) break;
    }
    return sent;
  }

  /** I byte arrivati dal guest (Uint8Array, vuota se non ce ne sono). */
  recv() {
    const x = this.#x;
    const parts = [];
    let total = 0;
    for (;;) {
      const n = x.vetro_net_recv(this.#vm, this.id, this.#buf, this.#cap);
      if (n === 0) break;
      parts.push(new Uint8Array(x.memory.buffer, this.#buf, n).slice());
      total += n;
    }
    if (parts.length === 1) return parts[0];
    const out = new Uint8Array(total);
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    return out;
  }

  /** Chiude il verso JS→guest (FIN dopo i byte in coda). */
  shutdown() {
    this.#x.vetro_net_shutdown(this.#vm, this.id);
  }

  /** Interrompe la connessione (RST al guest). */
  abort() {
    this.#x.vetro_net_abort(this.#vm, this.id);
  }

  /** Dimentica la connessione (se è viva la interrompe) e libera il buffer. */
  release() {
    if (!this.#buf) return;
    this.#x.vetro_net_release(this.#vm, this.id);
    this.#x.vetro_free(this.#buf, this.#cap);
    this.#buf = 0;
  }
}

/**
 * Il gestore dei file dal JS (vedi `Machine.files`): operazioni sui file del
 * guest attraverso il demone `vetro-files` su virtio-vsock (ADR 0020).
 * Ogni operazione restituisce una Promise che si risolve (o fallisce con un
 * Error con `code`, per esempio 'ENOENT', ed `errno`) durante un `pump()`.
 * Gli eventi di inotify delle osservazioni arrivano a `onEvent({ wd, mask,
 * cookie, name })`. Chiedere, mandare e leggere sono ingressi della macchina
 * (registrati per il replay); `status()` no.
 */
export class GuestFiles {
  #x;
  #vm;
  #pending = new Map();
  /** Callback degli eventi di inotify. */
  onEvent = null;

  constructor(x, vm, port = 0) {
    this.#x = x;
    this.#vm = vm;
    if (x.vetro_files_open(vm, port) !== 1) throw new Error('vetro_files_open: la macchina non ha virtio-vsock (DEV.VSOCK)');
  }

  /** { state: 'None' | 'Connecting' | 'Ready', pending, generation, maxChunk, selinux }. */
  status() {
    const x = this.#x;
    const p = x.vetro_alloc(16) >>> 0;
    const code = x.vetro_files_status(this.#vm, p, 4);
    const [pending, generation, maxChunk, flags] = new Uint32Array(x.memory.buffer, p, 4);
    x.vetro_free(p, 16);
    return { state: FILES_STATUS[code], pending, generation, maxChunk, selinux: (flags & 1) === 1 };
  }

  #request(op, path, b = null, x = 0n, y = 0n) {
    const ex = this.#x;
    const a = copyIn(ex, pathBytes(path));
    const bb = copyIn(ex, typeof b === 'string' ? pathBytes(b) : b ?? new Uint8Array());
    const id = ex.vetro_files_request(this.#vm, op, ...a, ...bb, BigInt(x), BigInt(y));
    for (const [p, n] of [a, bb]) if (n) ex.vetro_free(p, n);
    if (id === 0) return Promise.reject(new Error(`vetro_files_request(${op}, ${path}): rifiutata`));
    return new Promise((ok, ko) => this.#pending.set(id, { ok, ko }));
  }

  /** Metadati: { kind, mode, uid, gid, size, mtime, mtimeNs, nlink, link, selinux }. */
  stat(path) {
    return this.#request(FILES_OP.STAT, path).then((r) => r.stat);
  }

  /** Le voci della cartella: [{ name, stat }], in ordine di nome. */
  list(path) {
    return this.#request(FILES_OP.LIST, path).then((r) => r.entries);
  }

  /** { size, data: Uint8Array }: `length` byte da `offset` (null = fino alla fine). */
  read(path, offset = 0, length = null) {
    return this.#request(FILES_OP.READ, path, null, offset, length === null ? 0xffffffffffffffffn : length)
      .then((r) => ({ size: r.size, data: r.data }));
  }

  /** Sostituisce il file (scrittura atomica; proprietario, modo e xattr restano). Restituisce i nuovi metadati. */
  writeFile(path, bytes, mode = 0o644) {
    return this.#request(FILES_OP.WRITE, path, bytes, mode).then((r) => r.stat);
  }

  mkdir(path, mode = 0o755) {
    return this.#request(FILES_OP.MKDIR, path, null, mode).then(() => undefined);
  }

  create(path, mode = 0o644) {
    return this.#request(FILES_OP.CREATE, path, null, mode).then(() => undefined);
  }

  delete(path, { recursive = false } = {}) {
    return this.#request(FILES_OP.DELETE, path, null, recursive ? 1 : 0).then(() => undefined);
  }

  rename(from, to) {
    return this.#request(FILES_OP.RENAME, from, to).then(() => undefined);
  }

  /** Osserva una cartella con inotify: l'id (wd) degli eventi. */
  watch(path) {
    return this.#request(FILES_OP.WATCH, path).then((r) => r.wd);
  }

  unwatch(wd) {
    return this.#request(FILES_OP.UNWATCH, '/', null, wd).then(() => undefined);
  }

  /**
   * SQL sul database SQLite `path`, nel guest con il motore vero e come il
   * proprietario del file (ADR 0021): istruzioni in una transazione (tranne
   * `readonly`), `params` legati a ?1, ?2, ... (vedi encodeSqlArgs); con
   * `expect` un numero diverso di righe cambiate annulla tutto. Restituisce
   * { changes, lastRowid (BigInt), truncated, columns, rows }; un rifiuto
   * di SQLite è un errore con code 'SQLITE' e sqlite (il codice).
   */
  sql(path, sql, params = [], { expect = null, readonly = false } = {}) {
    const x = expect === null ? 0xffffffffffffffffn : BigInt(expect);
    return this.#request(FILES_OP.SQL, path, encodeSqlArgs(sql, params), x, readonly ? 1 : 0).then((r) => ({
      changes: r.changes,
      lastRowid: BigInt(r.lastRowid),
      truncated: r.truncated,
      columns: r.columns,
      rows: r.rows.map((row) => row.map(sqlValue)),
    }));
  }

  /** Fa avanzare il client e consegna risposte ed eventi; restituisce quanti messaggi. */
  pump() {
    const x = this.#x;
    x.vetro_files_pump(this.#vm);
    let count = 0;
    for (;;) {
      const n = x.vetro_files_take(this.#vm) >>> 0;
      if (n === 0) return count;
      count++;
      const ptr = x.vetro_files_ptr(this.#vm) >>> 0;
      const buf = new Uint8Array(x.memory.buffer, ptr, n);
      const jl = buf[0] | (buf[1] << 8) | (buf[2] << 16) | (buf[3] << 24);
      const msg = JSON.parse(utf8.decode(buf.subarray(4, 4 + jl)));
      if (msg.kind === 'event') {
        this.onEvent?.(msg);
        continue;
      }
      if (msg.type === 'data') msg.data = buf.slice(4 + jl, 4 + jl + msg.length);
      const p = this.#pending.get(msg.op);
      if (!p) continue;
      this.#pending.delete(msg.op);
      if (msg.ok) p.ok(msg);
      else {
        const e = new Error(msg.error);
        e.code = msg.code;
        e.errno = msg.errno;
        if (msg.sqlite !== undefined) e.sqlite = msg.sqlite;
        p.ko(e);
      }
    }
  }

  /** Chiude la connessione; le richieste in corso falliscono al prossimo pump (se ce n'è uno). */
  close() {
    this.#x.vetro_files_close(this.#vm);
    for (const p of this.#pending.values()) p.ko(Object.assign(new Error('gestore dei file chiuso'), { code: 'CLOSED' }));
    this.#pending.clear();
  }
}
