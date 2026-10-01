// vetro-wasm loader: instantiates the module with its imports and wraps
// the C API of docs/specs/wasm.md. It doesn't use Node APIs: it works in the
// browser too (the bytes of the .wasm are passed by the caller).

import { JitEngine } from './jit-engine.mjs';

export const ABI_VERSION = 15;
/** Codes of vetro_run. */
export const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented', 'Blocked'];

/** Device bits of vetro_machine_new_with. */
/** GPU_3D: gfxstream GLES over virtio-gpu 3D, drawn by WebGL2 (ADR 0037). */
export const DEV = { GPU: 1, KEYBOARD: 2, TABLET: 4, MULTITOUCH: 8, NET: 16, VSOCK: 32, GPU_3D: 64, DEFAULT: 1 | 2 | 4 | 16 };
/** Disk bits. */
export const DISK = { READ_ONLY: 1 };
/** Devices of vetro_input_events. */
export const INPUT = { KEYBOARD: 0, POINTER: 1 };
/** States and close reasons of vetro_net_state (GuestSocket.state). */
export const NET_STATE = ['Unknown', 'Connecting', 'Open', 'Closed'];
/** Codes of vetro_snapshot_restore (0 = success). */
export const RESTORE = [null, 'BadMagic', 'Version', 'Config', 'Corrupt'];
/** Codes of vetro_overlay_open. */
export const OVERLAY = ['Loaded', 'New', 'Mismatch', 'Corrupt', 'NoDisk'];
export const NET_REASON = [null, 'Normal', 'GuestReset', 'RemoteReset', 'Refused', 'Timeout'];
/** Operations of vetro_files_request (file manager, ABI 7; SQL with ABI 9). */
export const FILES_OP = { STAT: 1, LIST: 2, READ: 3, WRITE: 4, MKDIR: 5, CREATE: 6, DELETE: 7, RENAME: 8, WATCH: 9, UNWATCH: 10, SQL: 11 };
/** Timeline input kinds (vetro_timeline_input, InputKind of vetro-analysis). */
export const TIMELINE_INPUT = { KEY: 0, POINTER: 1, TOUCH: 2, CONSOLE: 3, FILES: 4, POWER: 5, DISPLAY: 6, OTHER: 7 };
/** Timeline effect kinds (vetro_timeline_effect, EffectKind). */
export const TIMELINE_EFFECT = { HTTP: 0, DNS: 1, TLS: 2, FILE: 3, CONSOLE: 4 };
/** States of vetro_rr_status. */
export const RR_STATE = ['Idle', 'Recording', 'Replaying', 'Finished', 'Diverged'];
/** Codes of vetro_replay_start (0 = success). */
export const REPLAY_START = [null, 'NoLog', 'KeyframeMissing', 'Refused'];
/** States of vetro_files_status. */
export const FILES_STATUS = ['None', 'Connecting', 'Ready'];
/** inotify event bits (GuestFiles.onEvent). */
export const INOTIFY = {
  MODIFY: 0x2, ATTRIB: 0x4, CLOSE_WRITE: 0x8, MOVED_FROM: 0x40, MOVED_TO: 0x80, CREATE: 0x100, DELETE: 0x200,
  DELETE_SELF: 0x400, MOVE_SELF: 0x800, Q_OVERFLOW: 0x4000, IGNORED: 0x8000, ISDIR: 0x40000000,
};

const utf8 = new TextDecoder();
const toUtf8 = new TextEncoder();
/** Bytes of a snapshot header (vetro_snapshot::HEADER_LEN). */
export const SNAPSHOT_HEADER_LEN = 36;
/** Where the chunks of vetro_snapshot_save_stream go (import vetro_host.snapshot_write). */
let snapshotSink = null;
/** The WebGL2 executor of the gfxstream op stream (web/app/gl.mjs), or null: batches then run nowhere (reads return zeros). */
let glExecutor = null;
/** Sets the executor the import vetro_host.gl_execute hands the batches to (ADR 0037). */
export function setGlExecutor(e) {
  glExecutor = e;
}
/** Where those of vetro_snapshot_restore_stream come from (import vetro_host.snapshot_read). */
let snapshotSource = null;

/**
 * Bytes of a guest path from a string in *surrogateescape* (ADR
 * 0021): the lone surrogates U+DC80..U+DCFF become again the bytes 0x80..0xFF that
 * were not valid UTF-8, the rest is UTF-8.
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

/** A surrogateescape string from the bytes of a path (the inverse of pathBytes). */
export function pathString(bytes) {
  const strict = new TextDecoder('utf-8', { fatal: true });
  try {
    return strict.decode(bytes);
  } catch {
    // Byte by byte: valid UTF-8 sequences stay, the other bytes become surrogates.
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

/** A name with the non-UTF-8 bytes (lone surrogates) shown as \xNN. */
export function displayName(s) {
  return s.replace(/[\udc80-\udcff]/g, (c, i) => {
    const prev = i > 0 ? s.charCodeAt(i - 1) : 0;
    return prev >= 0xd800 && prev <= 0xdbff ? c : `\\x${(c.charCodeAt(0) - 0xdc00).toString(16).padStart(2, '0')}`;
  });
}

/** Types of SQL values in the file manager protocol (ADR 0021). */
const SQLV = { NULL: 0, INT: 1, REAL: 2, TEXT: 3, BLOB: 4 };

/**
 * SQL and parameters in vetro-wasm's format (`proto::encode_sql_args`).
 * A parameter is null, a bigint or an integer number (INTEGER), a non-integer
 * number (REAL), a string (TEXT), a Uint8Array (BLOB), a boolean
 * (0/1), or explicit: { type: 'integer'|'real'|'text'|'blob'|'null', value }.
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
    else throw new Error(`invalid SQL parameter: ${p}`);
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
    } else throw new Error(`SQL parameter type ${type}`);
  }
  const out = new Uint8Array(len);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** A SQL value from vetro-wasm's JSON: null, Number or BigInt, String, Uint8Array. */
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

/**
 * Instantiates vetro-wasm from the .wasm bytes: { exports, jit }.
 * `jitBudget`: bytes of JIT modules between two resets (CODE_BUDGET in
 * jit-engine.mjs).
 */
export async function instantiate(wasmBytes, { jitBudget } = {}) {
  const jit = new JitEngine(jitBudget ? { budget: jitBudget } : {});
  let exports = null;
  const imports = {
    vetro_host: {
      panic: (ptr, len) => {
        const msg = utf8.decode(new Uint8Array(exports.memory.buffer, ptr >>> 0, len));
        console.error(`vetro-wasm: panic: ${msg}`);
      },
      snapshot_read: (ptr, cap) => {
        if (!snapshotSource) throw new Error('vetro_host.snapshot_read outside Machine.snapshotRestoreStream');
        return snapshotSource(ptr >>> 0, cap >>> 0);
      },
      snapshot_write: (ptr, len) => {
        if (!snapshotSink) throw new Error('vetro_host.snapshot_write outside Machine.snapshotSaveTo');
        snapshotSink(ptr >>> 0, len >>> 0);
      },
      gl_execute: (wp, wn, bp, bn, op, on) => {
        if (!glExecutor) return;
        const buf = exports.memory.buffer;
        glExecutor.execute(new Uint32Array(buf, wp >>> 0, wn >>> 0), new Uint8Array(buf, bp >>> 0, bn >>> 0), new Uint8Array(buf, op >>> 0, on >>> 0));
      },
    },
    vetro_jit: jit.imports(),
  };
  const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
  exports = instance.exports;
  jit.attach(exports);
  const abi = exports.vetro_abi_version();
  if (abi !== ABI_VERSION) throw new Error(`vetro-wasm: API ${abi}, expected ${ABI_VERSION}`);
  return { exports, jit };
}

/** Copies `bytes` into a new buffer in the module's memory: [ptr, len]. */
export function copyIn(x, bytes) {
  if (bytes.length === 0) return [0, 0];
  // Pointers as u32: beyond 2 GiB a WASM i32 arrives negative.
  const ptr = x.vetro_alloc(bytes.length) >>> 0;
  if (ptr === 0) throw new Error(`vetro_alloc(${bytes.length}) fallita`);
  // View taken after the allocation: the memory may have grown.
  new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

/** A vetro-wasm machine. */
export class Machine {
  #x;
  #vm;
  #buf;
  #cap = 64 * 1024;

  /**
   * ramSize/nowSecs/seed: BigInt, 0n = the values of MachineConfig::default.
   * devices: bits of DEV (default: GPU, keyboard and tablet, like
   * `Devices::default`); width/height: initial GPU resolution
   * (0 = 1280x800).
   */
  constructor(x, { ramSize = 0n, nowSecs = 0n, seed = 0n, devices = DEV.DEFAULT, width = 0, height = 0 } = {}) {
    this.#x = x;
    this.#vm = x.vetro_machine_new_with(ramSize, nowSecs, seed, devices, width, height);
    this.#buf = x.vetro_alloc(this.#cap) >>> 0;
  }

  /** Work buffer of `n` bytes (inside the console buffer). */
  #scratch(n) {
    if (n > this.#cap) throw new Error(`work buffer too small (${n} > ${this.#cap})`);
    return this.#buf;
  }

  // ---- Display (virtio-gpu) -------------------------------------------

  /** { width, height } of the scanout, or null if off. */
  displaySize(scanout = 0) {
    const v = this.#x.vetro_display_size(this.#vm, scanout);
    return v === 0n ? null : { width: Number(v >> 32n), height: Number(v & 0xffffffffn) };
  }

  /** Updates of the scanout (Number): if it doesn't change, nothing to redraw. */
  displayUpdates(scanout = 0) {
    return Number(this.#x.vetro_display_updates(this.#vm, scanout));
  }

  /**
   * View on the RGBA pixels of the scanout in the module's memory (valid until
   * the next run), or null.
   */
  displayPixels(scanout = 0) {
    const size = this.displaySize(scanout);
    if (!size) return null;
    const ptr = this.#x.vetro_display_ptr(this.#vm, scanout) >>> 0;
    return new Uint8Array(this.#x.memory.buffer, ptr, size.width * size.height * 4);
  }

  /** Rectangle changed since the last call { x, y, width, height }, or null. */
  displayTakeDirty(scanout = 0) {
    const p = this.#scratch(16);
    if (!this.#x.vetro_display_take_dirty(this.#vm, scanout, p)) return null;
    const [x, y, width, height] = new Uint32Array(this.#x.memory.buffer, p, 4);
    return { x, y, width, height };
  }

  /**
   * The RGBA pixels of rectangle `r` in a new ArrayBuffer (to transfer
   * to another thread), rows of `r.width * 4` bytes.
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

  /** The scanout shows a 3D resource (drawn by the WebGL2 executor, ADR 0037). */
  displayIs3d(scanout = 0) {
    return this.#x.vetro_display_is_3d(this.#vm, scanout) === 1;
  }

  /**
   * Reads back the 3D scanout (host read, not seen by the guest): the RGBA
   * view of `displayPixels`, or null.
   */
  displayRead3d(scanout = 0) {
    const ptr = this.#x.vetro_display_read_3d(this.#vm, scanout) >>> 0;
    return ptr ? this.displayPixels(scanout) : null;
  }

  /** Gives the machine's gfxstream renderer the executor set with setGlExecutor: false without GPU_3D. */
  glEnable() {
    return this.#x.vetro_gl_enable(this.#vm) === 1;
  }

  /** gfxstream counters { calls, batches, presents, readbackBytes, unhandled }, or null. */
  glStats() {
    const p = this.#scratch(40);
    const n = this.#x.vetro_gl_stats(this.#vm, p, 5);
    if (n < 5) return null;
    const v = new BigUint64Array(this.#x.memory.buffer, p, 5);
    return { calls: Number(v[0]), batches: Number(v[1]), presents: Number(v[2]), readbackBytes: Number(v[3]), unhandled: Number(v[4]) };
  }

  /** New lines of the gfxstream decoder's log (unhandled calls…). */
  glTakeLog() {
    const n = this.#x.vetro_gl_take_log(this.#vm);
    return n ? this.#message().split('\n') : [];
  }

  /** Resolution requested for the scanout (host input). */
  displayResize(width, height, scanout = 0) {
    return this.#x.vetro_display_resize(this.#vm, scanout, width, height) === 1;
  }

  /** Cursor { resource, x, y, hotX, hotY, updates }, or null without a GPU. */
  cursor(scanout = 0) {
    const p = this.#scratch(24);
    if (!this.#x.vetro_cursor_state(this.#vm, scanout, p)) return null;
    const [resource, x, y, hotX, hotY, updates] = new Uint32Array(this.#x.memory.buffer, p, 6);
    return { resource, x, y, hotX, hotY, updates };
  }

  /** Cursor image, a 64x64 RGBA copy (Uint8ClampedArray), or null. */
  cursorImage(scanout = 0) {
    const ptr = this.#x.vetro_cursor_image(this.#vm, scanout) >>> 0;
    return ptr ? new Uint8ClampedArray(this.#x.memory.buffer, ptr, 64 * 64 * 4).slice() : null;
  }

  // ---- Inputs (virtio-input, GPIO) --------------------------------------

  /** Linux key (KEY_*) pressed or released; false if there is no keyboard. */
  key(code, down) {
    return this.#x.vetro_input_key(this.#vm, code, down ? 1 : 0) === 1;
  }

  /** Absolute position of the tablet (0..32767). */
  pointerMove(x, y) {
    return this.#x.vetro_input_abs(this.#vm, x, y) === 1;
  }

  /** Pointer button (BTN_LEFT = 0x110, ...). */
  pointerButton(code, down) {
    return this.#x.vetro_input_button(this.#vm, code, down ? 1 : 0) === 1;
  }

  /** Touchscreen contact: pos = [x, y] (0..32767) or null to lift it. */
  touch(slot, pos) {
    const [x, y] = pos ?? [0, 0];
    return this.#x.vetro_input_touch(this.#vm, slot, x, y, pos ? 1 : 0) === 1;
  }

  /** Raw evdev events [[type, code, value], ...] on INPUT.KEYBOARD or INPUT.POINTER. */
  inputEvents(device, events) {
    const p = this.#scratch(events.length * 12);
    new Uint32Array(this.#x.memory.buffer, p, events.length * 3).set(events.flat().map((v) => v >>> 0));
    return this.#x.vetro_input_events(this.#vm, device, p, events.length) === 1;
  }

  /** Keyboard LEDs lit by the guest (LED_* bits). */
  get leds() {
    return this.#x.vetro_input_leds(this.#vm);
  }

  /** Level of a GPIO line; without `line`, the power button. */
  gpio(level, line = this.#x.vetro_power_key_line()) {
    this.#x.vetro_gpio_input(this.#vm, line, level ? 1 : 0);
  }

  // ---- Disks (virtio-blk) -----------------------------------------------

  /**
   * Disk with the data from JS in blocks (see web/node/disk.mjs): size in
   * bytes (Number or BigInt), blockSize a power of two >= 512, maxBlocks
   * blocks in memory (0 = no limit). Returns the index.
   */
  addDisk(size, { blockSize = 1 << 20, maxBlocks = 0, readOnly = false } = {}) {
    const i = this.#x.vetro_disk_add(this.#vm, BigInt(size), blockSize, maxBlocks, readOnly ? DISK.READ_ONLY : 0);
    if (i < 0) throw new Error(`vetro_disk_add: ${this.#message()}`);
    return i;
  }

  /** Disk with all its contents in memory (always ready). */
  addDiskMem(bytes, { readOnly = false } = {}) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const i = x.vetro_disk_add_mem(this.#vm, p, n, readOnly ? DISK.READ_ONLY : 0);
    if (n) x.vetro_free(p, n);
    if (i < 0) throw new Error(`vetro_disk_add_mem: ${this.#message()}`);
    return i;
  }

  /** Requested blocks: [{ disk, block }] (block Number). */
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

  /** Delivers a block (Uint8Array); throws if refused. */
  diskFill(disk, block, bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_disk_fill(this.#vm, disk, BigInt(block), p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 0) throw new Error(`vetro_disk_fill(${disk}, ${block}, ${n} byte): codice ${r}`);
  }

  /** The block can't be obtained: the guest gets an I/O error. */
  diskFail(disk, block) {
    this.#x.vetro_disk_fail(this.#vm, disk, BigInt(block));
  }

  /** Disk counters, or null. */
  diskStats(disk) {
    const names = ['size', 'blockSize', 'cachedBlocks', 'misses', 'fills', 'evictions', 'failures', 'dirtyClusters'];
    const p = this.#scratch(8 * names.length);
    const n = this.#x.vetro_disk_stats(this.#vm, disk, p, names.length);
    if (!n) return null;
    const v = new BigUint64Array(this.#x.memory.buffer, p, names.length);
    return Object.fromEntries(names.map((k, i) => [k, Number(v[i])]));
  }

  // ---- Network: connections to the guest's services (ABI 5) ---------------

  /**
   * Opens a TCP connection to the guest's `port` (10.0.2.15), which sees it
   * arrive from the gateway 10.0.2.2, like QEMU's `hostfwd` (for example adbd
   * on 5555). The SYN leaves at the next `run`. Throws without a network.
   */
  connectGuest(port) {
    const id = this.#x.vetro_net_connect(this.#vm, port);
    if (id === 0n) throw new Error(`vetro_net_connect(${port}): no network or invalid port`);
    return new GuestSocket(this.#x, this.#vm, id);
  }

  // ---- File manager (ABI 7, ADR 0020) --------------------------------------

  /**
   * The file manager client towards the guest's `vetro-files` daemon
   * (needs DEV.VSOCK). Requests leave and responses arrive with
   * `pump()`, to be called between one quantum and the next. Throws without vsock.
   */
  files(port = 0) {
    return new GuestFiles(this.#x, this.#vm, port);
  }

  // ---- Snapshot (ABI 4, ADR 0015) ---------------------------------------

  /** Snapshot format version (to put in the cache keys). */
  get snapshotVersion() {
    return this.#x.vetro_snapshot_version();
  }

  /**
   * Configuration hash carried by this machine's snapshots (ABI 13, ADR
   * 0031), a BigInt: read after the disks are added, for snapshot keys.
   */
  /**
   * Compression of the next snapshots (ABI 13, ADR 0031): 'fast' (default)
   * or 'small' (for snapshots that are downloaded: slower to save, about a
   * third smaller). Restoring accepts both.
   */
  set snapshotLevel(level) {
    const code = { fast: 0, small: 1 }[level];
    if (code === undefined || this.#x.vetro_snapshot_set_level(this.#vm, code) !== 0) throw new Error(`snapshot level ${level}`);
  }

  get snapshotConfigHash() {
    return BigInt.asUintN(64, this.#x.vetro_snapshot_config_hash(this.#vm));
  }

  /**
   * Snapshot of the whole machine, copied out of the module's memory
   * (Uint8Array). Read the console first: the output not yet read by JS
   * is not included.
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
   * Chunked snapshot (ABI 12, ADR 0028), never held whole in the module's
   * memory or in JS: `write(bytes, offset)` (synchronous: it is called from
   * inside vetro-wasm) receives the file's chunks in order, then the header at
   * offset 0; `bytes` is a view not to keep after returning. Returns the file
   * length. With Android the snapshot is hundreds of MiB and would not fit
   * whole in wasm32's 4 GiB.
   */
  snapshotSaveTo(write) {
    const x = this.#x;
    let at = SNAPSHOT_HEADER_LEN;
    snapshotSink = (ptr, len) => {
      write(new Uint8Array(x.memory.buffer, ptr, len), at);
      at += len;
    };
    let total;
    try {
      total = Number(x.vetro_snapshot_save_stream(this.#vm));
    } finally {
      snapshotSink = null;
    }
    const head = new Uint8Array(x.memory.buffer, x.vetro_snapshot_ptr(this.#vm) >>> 0, SNAPSHOT_HEADER_LEN).slice();
    x.vetro_snapshot_clear(this.#vm);
    write(head, 0);
    if (at !== total) throw new Error(`chunked snapshot: ${at} bytes instead of ${total}`);
    return total;
  }

  /**
   * Like `snapshotRestore`, with the bytes written by `fill(view)` straight
   * into an `n`-byte buffer in the module's memory (read from OPFS, for
   * example), without a copy in JS.
   */
  async snapshotRestoreWith(n, fill) {
    const x = this.#x;
    const ptr = x.vetro_alloc(n) >>> 0;
    if (!ptr) throw Object.assign(new Error(`vetro_alloc(${n}) failed: module memory exhausted`), { code: 'Memory' });
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

  /**
   * Chunked restore (ABI 12, ADR 0028): `readAt(view, offset)` (synchronous)
   * fills `view` with the file's bytes from `offset`; `size` is the file
   * length. Only the part before the RAM goes into the module's memory
   * (devices and copy-on-write); the RAM arrives in 1 MiB chunks, so a buffer
   * as large as the snapshot does not fragment the memory of whoever wants to
   * save again later. Throws like `snapshotRestore`.
   */
  snapshotRestoreStream(size, readAt) {
    const x = this.#x;
    const small = new Uint8Array(12);
    let at = SNAPSHOT_HEADER_LEN;
    for (;;) {
      if (at + 12 > size) throw Object.assign(new Error('snapshot: RAM section not found'), { code: 'Corrupt' });
      readAt(small, at);
      const tag = String.fromCharCode(...small.subarray(0, 4));
      if (tag === 'RAM ') break;
      at += 12 + Number(new DataView(small.buffer).getBigUint64(4, true));
    }
    const headLen = at + 12;
    const ptr = x.vetro_alloc(headLen) >>> 0;
    if (!ptr) throw Object.assign(new Error(`vetro_alloc(${headLen}) failed: module memory exhausted`), { code: 'Memory' });
    let pos = headLen;
    snapshotSource = (p, cap) => {
      const n = Math.min(cap, size - pos);
      if (n > 0) readAt(new Uint8Array(x.memory.buffer, p, n), pos);
      pos += n;
      return n;
    };
    try {
      readAt(new Uint8Array(x.memory.buffer, ptr, headLen), 0);
      const r = x.vetro_snapshot_restore_stream(this.#vm, ptr, headLen);
      if (r !== 0) {
        const e = new Error(`vetro_snapshot_restore_stream: ${RESTORE[r] ?? r}: ${this.#message()}`);
        e.code = RESTORE[r] ?? String(r);
        throw e;
      }
    } finally {
      snapshotSource = null;
      x.vetro_free(ptr, headLen);
    }
  }

  /** Bytes of the module's linear memory (guest RAM included). */
  get memoryBytes() {
    return this.#x.memory.buffer.byteLength;
  }

  /**
   * Restores a snapshot onto this machine, built like the saved one
   * (same devices, same disks added in the same order). Throws
   * an Error with `code` ('BadMagic', 'Version', 'Config', 'Corrupt') and the
   * reason; with the first three the machine has not changed.
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

  // ---- Persistent disk overlay (ABI 6, ADR 0017) ----------------------------

  /**
   * Opens the overlay of disk `disk` from the file contents (`bytes`, empty if
   * it doesn't exist) for the base image `identity` (string). Returns
   * { code: 'Loaded' | 'New' | 'Mismatch' | 'Corrupt' | 'NoDisk', message }.
   * With 'Mismatch' and 'Corrupt' the overlay is discarded: the next
   * `overlayTake` truncates the file.
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
   * Writes to make to the overlay file of disk `disk` to save in it the
   * guest's writes made so far: { truncate: Number | null, writes:
   * [{ at: Number, bytes: Uint8Array }] } in order (the header
   * last), or null if there is nothing new.
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

  /** Overlay counters of the disk, or null without an overlay. */
  overlayInfo(disk) {
    const names = ['generation', 'clusters', 'slots', 'damaged', 'fileLength'];
    const p = this.#scratch(8 * names.length);
    const n = this.#x.vetro_overlay_info(this.#vm, disk, p, names.length);
    if (!n) return null;
    const v = new BigUint64Array(this.#x.memory.buffer, p, names.length);
    return Object.fromEntries(names.map((k, i) => [k, Number(v[i])]));
  }

  // ---- Network inspector and timeline (ABI 8, ADR 0023) ---------------------

  /** The last result from Rust (result buffer), copied; then freed. */
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

  /** Turns the capture of the virtio-net frames on or off; false without a network. */
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

  /** The inspector list: { frames, requests: [...], dns: [...], tls: [...] } (docs/specs/analysis.md). */
  inspectRequests() {
    return this.#json(this.#x.vetro_inspect_requests(this.#vm));
  }

  /** The detail of request `index` ({ row, request, response }), or null. */
  inspectRequest(index) {
    return this.#json(this.#x.vetro_inspect_request(this.#vm, index));
  }

  /** The HAR 1.2 of the capture (string); epochUs: Unix µs of guest time 0. */
  inspectHar(epochUs = 0) {
    return utf8.decode(this.#result(this.#x.vetro_inspect_har(this.#vm, BigInt(epochUs))));
  }

  /** The pcapng of the capture (Uint8Array). */
  inspectPcapng(epochUs = 0) {
    return this.#result(this.#x.vetro_inspect_pcapng(this.#vm, BigInt(epochUs)));
  }

  /** Annotates a user input (kind in TIMELINE_INPUT) at the current instruction. */
  timelineInput(kind, label, weak = false) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(label));
    x.vetro_timeline_input(this.#vm, kind, weak ? 1 : 0, p, n);
    if (n) x.vetro_free(p, n);
  }

  /** Annotates an effect (kind in TIMELINE_EFFECT) at the current instruction. */
  timelineEffect(kind, label) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(label));
    const ok = x.vetro_timeline_effect(this.#vm, kind, p, n) === 1;
    if (n) x.vetro_free(p, n);
    return ok;
  }

  /** The timeline { windowUs, inputs, effects, ... } with window `windowUs` (0 = 3 s). */
  timeline(windowUs = 0) {
    return this.#json(this.#x.vetro_timeline_json(this.#vm, BigInt(windowUs)));
  }

  /** Changes when the timeline changes (BigInt). */
  timelineVersion() {
    return this.#x.vetro_timeline_version(this.#vm);
  }

  timelineClear() {
    this.#x.vetro_timeline_clear(this.#vm);
  }

  // ---- Record & replay (ABI 8, ADR 0019 e 0023) ------------------------------

  /** Records from here, with a keyframe every `keyframeEvery` instructions (the first one immediately). */
  recordStart(keyframeEvery = 200_000_000) {
    this.#x.vetro_record_start(this.#vm, BigInt(keyframeEvery));
  }

  /** Ends the recording (the log stays in the machine); false if it wasn't recording. */
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

  /** The log file with the keyframes present (Uint8Array, empty without a log). */
  logEncode() {
    return this.#result(this.#x.vetro_log_encode(this.#vm));
  }

  /** Loads a log file (replacing the log that was there); throws if it is invalid. */
  logLoad(bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_log_load(this.#vm, p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 0) throw new Error(`invalid log: ${this.#message()}`);
  }

  /** { startSteps, endSteps, events, keyframes, keyframeEvery, jit, sameMachine, eventsBytes }, o null. */
  logInfo() {
    const s = this.#u64s(this.#x.vetro_log_info, ['startSteps', 'endSteps', 'events', 'keyframes', 'keyframeEvery', 'jit', 'sameMachine', 'eventsBytes']);
    return s && { ...s, jit: s.jit === 1, sameMachine: s.sameMachine === 1 };
  }

  /** { step, consoleLen, consoleHash, size, present } of keyframe `index`, or null. */
  logKeyframe(index) {
    const s = this.#u64s(this.#x.vetro_log_keyframe, ['step', 'consoleLen', 'consoleHash', 'size', 'present'], index);
    return s && { ...s, present: s.present === 1 };
  }

  /** Moves out the bytes of keyframe `index` (Uint8Array; empty if already out). */
  logKeyframeTake(index) {
    return this.#result(this.#x.vetro_log_keyframe_take(this.#vm, index));
  }

  /** Puts back the bytes of keyframe `index`; throws if refused. */
  logKeyframePut(index, bytes) {
    const x = this.#x;
    const [p, n] = copyIn(x, bytes);
    const r = x.vetro_log_keyframe_put(this.#vm, index, p, n);
    if (n) x.vetro_free(p, n);
    if (r !== 1) throw new Error(`keyframe ${index} refused (${n} bytes)`);
  }

  /** Index of the keyframe from which the replay towards instruction `step` starts, -1 if none. */
  logKeyframeFor(step) {
    return this.#x.vetro_log_keyframe_for(this.#vm, BigInt(step));
  }

  /** The log events: [{ i, step, kind, label, weak, user }]. */
  logEvents() {
    return this.#json(this.#x.vetro_log_events(this.#vm)) ?? [];
  }

  /**
   * Replay of the log from the last keyframe not beyond `step` (which must be
   * present). Throws an Error with `code` ('NoLog', 'KeyframeMissing',
   * 'Refused') and the reason.
   */
  replayStart(step = 0) {
    const r = this.#x.vetro_replay_start(this.#vm, BigInt(step));
    if (r !== 0) {
      const code = REPLAY_START[r] ?? String(r);
      throw Object.assign(new Error(`replay: ${code}: ${this.#message()}`), { code });
    }
  }

  /** The registers at the point reached (text of Machine::registers_text). */
  registersText() {
    return utf8.decode(this.#result(this.#x.vetro_registers_text(this.#vm)));
  }

  /** `len` bytes at virtual address `va` (BigInt or Number): { bytes } or { fault } (BigInt). */
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

  /** Physical address of `va` (BigInt), or null if it is not mapped. */
  translate(va) {
    const pa = this.#x.vetro_translate(this.#vm, BigInt(va));
    return pa === 0xffffffffffffffffn ? null : pa;
  }

  /** `len` bytes of RAM at physical address `pa`, or null outside the RAM. */
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

  /** Kernel, initramfs (or null) and command line; throws on error. */
  loadLinux(image, initrd, cmdline) {
    const x = this.#x;
    const bufs = [copyIn(x, image), copyIn(x, initrd ?? new Uint8Array()), copyIn(x, toUtf8.encode(cmdline))];
    const code = x.vetro_load_linux(this.#vm, ...bufs.flat());
    for (const [p, n] of bufs) x.vetro_free(p, n);
    if (code !== 0) throw new Error(`vetro_load_linux: codice ${code}: ${this.#message()}`);
  }

  /**
   * Boot from Android images (ABI 12, ADR 0018): `boot` (boot.img),
   * `vendorBoot`, `initBoot` (Uint8Array or null), `params` (bootloader
   * parameters: `androidboot.*` go into the bootconfig), `recovery`. Returns
   * the description of kernel and ramdisks; throws on error.
   */
  loadAndroid({ boot, vendorBoot = null, initBoot = null, params = '', recovery = false }) {
    const x = this.#x;
    const empty = new Uint8Array();
    const bufs = [copyIn(x, boot), copyIn(x, vendorBoot ?? empty), copyIn(x, initBoot ?? empty), copyIn(x, toUtf8.encode(params))];
    const code = x.vetro_load_android(this.#vm, ...bufs.flat(), recovery ? 1 : 0);
    for (const [p, n] of bufs) if (n) x.vetro_free(p, n);
    const msg = this.#message();
    if (code !== 0) throw new Error(`vetro_load_android: code ${code}: ${msg}`);
    return msg;
  }

  /** Runs at most `budget` instructions; returns the reason for stopping. */
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

  /** The console output since the last read (Uint8Array). */
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

  /** Writes to the console, as from the keyboard. */
  consoleWrite(text) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(text));
    x.vetro_console_write(this.#vm, p, n);
    x.vetro_free(p, n);
  }

  /**
   * Turns on the system-mode JIT (ADR 0013): `threshold` entries
   * before translating a block, `batch` blocks per module. The result
   * doesn't change, only the speed.
   */
  setJit(threshold = 64, batch = 16, { profile = false, names = false } = {}) {
    // Bit 0: instruction classes of the interpreter; bit 1: region functions
    // named after their guest PC (V8 CPU profiles, ADR 0041).
    const flags = (profile ? 1 : 0) | (names ? 2 : 0);
    if (flags) this.#x.vetro_machine_set_jit_with(this.#vm, threshold, batch, flags);
    else this.#x.vetro_machine_set_jit(this.#vm, threshold, batch);
  }

  /** With `setJit(..., { profile: true })`: the report of the interpreter's instruction classes (text), or null. */
  jitProfile(n = 40) {
    const len = this.#x.vetro_jit_profile(this.#vm, n);
    return len ? utf8.decode(this.#result(len)) : null;
  }

  /** Machine measurement counters: where the steps go (interpreter, WFI), device services. */
  perf() {
    return this.#u64s(this.#x.vetro_perf, ['interpSteps', 'wfiSteps', 'wfis', 'syncs', 'services']);
  }

  /** JIT counters (`SysJitStats`), or null without JIT. */
  jitStats() {
    const x = this.#x;
    const names = ['jitSteps', 'runs', 'resolves', 'calls', 'blocks', 'modules', 'reused', 'invalidatedPages', 'faults',
      'svcs', 'stops', 'epochs', 'tlbFlushes', 'tlbFills', 'resets', 'yields', 'hostLds', 'hostSts', 'epochsRegs', 'epochsTlbi',
      'epochsCode', 'wasmBytes', 'baseSwitches', 'tlbiPartial', 'jcProbes', 'memoHits', 'regimeSwitches', 'evictions', 'evictedModules', 'dispatches'];
    const p = x.vetro_alloc(8 * names.length) >>> 0;
    const n = x.vetro_jit_stats(this.#vm, p, names.length);
    const v = new BigUint64Array(x.memory.buffer, p, names.length);
    // Older builds write fewer counters: only the first `n`.
    const out = n ? Object.fromEntries(names.slice(0, n).map((k, i) => [k, Number(v[i])])) : null;
    x.vetro_free(p, 8 * names.length);
    return out;
  }

  /** Instructions executed (BigInt). */
  get steps() {
    return this.#x.vetro_steps(this.#vm);
  }

  /** Guest time in ns (BigInt). */
  get guestNs() {
    return this.#x.vetro_guest_ns(this.#vm);
  }

  free() {
    this.#x.vetro_free(this.#buf, this.#cap);
    this.#x.vetro_machine_free(this.#vm);
  }
}

/**
 * A connection from JS to a TCP service of the guest (see
 * `Machine.connectGuest`). Synchronous: `send` queues, `recv` reads what
 * has arrived; the bytes move while the machine runs (`run`), so
 * the user alternates the two, like the console. Writing, reading ready bytes,
 * closing are machine inputs (to be recorded for replay); the
 * state isn't.
 */
export class GuestSocket {
  #x;
  #vm;
  #buf;
  #cap = 64 * 1024;

  constructor(x, vm, id) {
    this.#x = x;
    this.#vm = vm;
    /** Id of the connection in the stack (BigInt). */
    this.id = id;
    this.#buf = x.vetro_alloc(this.#cap) >>> 0;
  }

  /**
   * { state, reason, readable, writable, guestEof, unsent }: state in
   * NET_STATE, reason in NET_REASON (null while it is open), guestEof true
   * when the guest has closed its direction and everything has been read.
   */
  state() {
    const x = this.#x;
    const code = x.vetro_net_state(this.#vm, this.id, this.#buf, 5);
    if (code === 0) return { state: 'Unknown', reason: null, readable: 0, writable: 0, guestEof: false, unsent: 0 };
    const [reason, readable, writable, eof, unsent] = new Uint32Array(x.memory.buffer, this.#buf, 5);
    return { state: NET_STATE[code], reason: NET_REASON[reason], readable, writable, guestEof: eof === 1, unsent };
  }

  /** Queues bytes (Uint8Array) for the guest; returns how many it took. */
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

  /** The bytes arrived from the guest (Uint8Array, empty if there are none). */
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

  /** Closes the JS→guest direction (FIN after the queued bytes). */
  shutdown() {
    this.#x.vetro_net_shutdown(this.#vm, this.id);
  }

  /** Aborts the connection (RST to the guest). */
  abort() {
    this.#x.vetro_net_abort(this.#vm, this.id);
  }

  /** Forgets the connection (if it is alive it aborts it) and frees the buffer. */
  release() {
    if (!this.#buf) return;
    this.#x.vetro_net_release(this.#vm, this.id);
    this.#x.vetro_free(this.#buf, this.#cap);
    this.#buf = 0;
  }
}

/**
 * The file manager from JS (see `Machine.files`): operations on the guest's
 * files through the `vetro-files` daemon over virtio-vsock (ADR 0020).
 * Every operation returns a Promise that resolves (or fails with an
 * Error with `code`, for example 'ENOENT', and `errno`) during a `pump()`.
 * The inotify events of the watches arrive at `onEvent({ wd, mask,
 * cookie, name })`. Requesting, sending and reading are machine inputs
 * (recorded for replay); `status()` isn't.
 */
export class GuestFiles {
  #x;
  #vm;
  #pending = new Map();
  /** Callback for the inotify events. */
  onEvent = null;

  constructor(x, vm, port = 0) {
    this.#x = x;
    this.#vm = vm;
    if (x.vetro_files_open(vm, port) !== 1) throw new Error('vetro_files_open: the machine has no virtio-vsock (DEV.VSOCK)');
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
    if (id === 0) return Promise.reject(new Error(`vetro_files_request(${op}, ${path}): refused`));
    return new Promise((ok, ko) => this.#pending.set(id, { ok, ko }));
  }

  /** Metadata: { kind, mode, uid, gid, size, mtime, mtimeNs, nlink, link, selinux }. */
  stat(path) {
    return this.#request(FILES_OP.STAT, path).then((r) => r.stat);
  }

  /** The entries of the folder: [{ name, stat }], in name order. */
  list(path) {
    return this.#request(FILES_OP.LIST, path).then((r) => r.entries);
  }

  /** { size, data: Uint8Array }: `length` bytes from `offset` (null = to the end). */
  read(path, offset = 0, length = null) {
    return this.#request(FILES_OP.READ, path, null, offset, length === null ? 0xffffffffffffffffn : length)
      .then((r) => ({ size: r.size, data: r.data }));
  }

  /** Replaces the file (atomic write; owner, mode and xattrs stay). Returns the new metadata. */
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

  /** Watches a folder with inotify: the id (wd) of the events. */
  watch(path) {
    return this.#request(FILES_OP.WATCH, path).then((r) => r.wd);
  }

  unwatch(wd) {
    return this.#request(FILES_OP.UNWATCH, '/', null, wd).then(() => undefined);
  }

  /**
   * SQL on the SQLite database `path`, in the guest with the real engine and as the
   * owner of the file (ADR 0021): statements in one transaction (except
   * `readonly`), `params` bound to ?1, ?2, ... (see encodeSqlArgs); with
   * `expect`, a different number of changed rows rolls everything back. Returns
   * { changes, lastRowid (BigInt), truncated, columns, rows }; a refusal
   * by SQLite is an error with code 'SQLITE' and sqlite (the code).
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

  /** Advances the client and delivers responses and events; returns how many messages. */
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

  /** Closes the connection; the requests in progress fail at the next pump (if there is one). */
  close() {
    this.#x.vetro_files_close(this.#vm);
    for (const p of this.#pending.values()) p.ko(Object.assign(new Error('file manager closed'), { code: 'CLOSED' }));
    this.#pending.clear();
  }
}
