// Caricatore di vetro-wasm: istanzia il modulo con i suoi import e avvolge
// l'API C di docs/specs/wasm.md. Non usa API di Node: va bene anche nel
// browser (i byte del .wasm li passa chi chiama).

import { JitEngine } from './jit-engine.mjs';

export const ABI_VERSION = 6;
/** Codici di vetro_run. */
export const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented', 'Blocked'];

/** Bit dei dispositivi di vetro_machine_new_with. */
export const DEV = { GPU: 1, KEYBOARD: 2, TABLET: 4, MULTITOUCH: 8, NET: 16, DEFAULT: 1 | 2 | 4 | 16 };
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

const utf8 = new TextDecoder();
const toUtf8 = new TextEncoder();

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
  setJit(threshold = 16, batch = 16) {
    this.#x.vetro_machine_set_jit(this.#vm, threshold, batch);
  }

  /** Contatori del JIT (`SysJitStats`), o null senza JIT. */
  jitStats() {
    const x = this.#x;
    const names = ['jitSteps', 'runs', 'resolves', 'calls', 'blocks', 'modules', 'reused', 'invalidatedPages', 'faults',
      'svcs', 'stops', 'epochs', 'tlbFlushes', 'tlbFills', 'resets'];
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
