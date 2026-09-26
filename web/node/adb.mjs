// ADB client in JavaScript (M5/M6, ADR 0028, docs/specs/net.md "Come adb
// userà l'inoltro"): speaks the adb protocol with the guest's adbd over a TCP
// connection to port 5555, without a host adb. Uses no Node API: it runs in
// the app's Worker and in tests.
//
// The transport is any object with `send(bytes) -> bytes taken`,
// `recv() -> Uint8Array` and `state() -> { state, guestEof }`: the GuestSocket
// of web/node/vetro.mjs (bytes move while the machine runs) or a fake adbd in
// tests. The client is polled, like the rest of the API: `pump()` between
// quanta sends the queued bytes, reads the ones that arrived and advances the
// Promises.
//
// Protocol (AOSP's system/core/adb/protocol.txt): 24-byte little-endian
// messages (command, arg0, arg1, data length, sum of the data bytes,
// command ^ 0xffffffff) followed by the data. CNXN for the handshake (with
// `ro.adb.secure=1` the device sends AUTH first: the token is signed with an
// RSA key, or the public key is offered), OPEN/OKAY/WRTE/CLSE for streams, one
// per service (`shell,v2,raw:`, `exec:`, `sync:`). One WRTE at a time per
// stream: the next one goes after the device's OKAY.

const A_SYNC = 0x434e5953;
const A_CNXN = 0x4e584e43;
const A_OPEN = 0x4e45504f;
const A_OKAY = 0x59414b4f;
const A_CLSE = 0x45534c43;
const A_WRTE = 0x45545257;
const A_AUTH = 0x48545541;
export const ADB = { A_SYNC, A_CNXN, A_OPEN, A_OKAY, A_CLSE, A_WRTE, A_AUTH };
const VERSION = 0x01000001;
const MAX_DATA = 256 * 1024;
const AUTH_TOKEN = 1;
const AUTH_SIGNATURE = 2;
const AUTH_RSAPUBLICKEY = 3;
/** Push data piece (sync's limit is 64 KiB). */
const SYNC_CHUNK = 64 * 1024;

const enc = new TextEncoder();
const dec = new TextDecoder();

/** An encoded ADB message (header + data). */
export function encodeMessage(command, arg0, arg1, data = new Uint8Array()) {
  const out = new Uint8Array(24 + data.length);
  const v = new DataView(out.buffer);
  let sum = 0;
  for (const b of data) sum = (sum + b) >>> 0;
  v.setUint32(0, command, true);
  v.setUint32(4, arg0 >>> 0, true);
  v.setUint32(8, arg1 >>> 0, true);
  v.setUint32(12, data.length, true);
  v.setUint32(16, sum, true);
  v.setUint32(20, (command ^ 0xffffffff) >>> 0, true);
  out.set(data, 24);
  return out;
}

/** Reads the complete messages from an accumulator: { messages, rest }. */
export function decodeMessages(buf) {
  const messages = [];
  let at = 0;
  while (buf.length - at >= 24) {
    const v = new DataView(buf.buffer, buf.byteOffset + at, 24);
    const command = v.getUint32(0, true);
    const len = v.getUint32(12, true);
    if (v.getUint32(20, true) !== (command ^ 0xffffffff) >>> 0) throw new Error(`adb: invalid message (magic) at ${at}`);
    if (len > 16 << 20) throw new Error(`adb: message of ${len} bytes`);
    if (buf.length - at < 24 + len) break;
    messages.push({ command, arg0: v.getUint32(4, true), arg1: v.getUint32(8, true), data: buf.slice(at + 24, at + 24 + len) });
    at += 24 + len;
  }
  return { messages, rest: buf.subarray(at) };
}

const concat = (a, b) => {
  if (!a.length) return b;
  if (!b.length) return a;
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
};

// ---- adb RSA key (AUTH) -------------------------------------------------------

const b64 = (bytes) => btoa(String.fromCharCode(...bytes));
const unb64url = (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4)), (c) => c.charCodeAt(0));
const toBig = (bytes) => bytes.reduce((n, b) => (n << 8n) | BigInt(b), 0n);
function fromBig(n, len) {
  const out = new Uint8Array(len);
  for (let i = len - 1; i >= 0; i--) {
    out[i] = Number(n & 0xffn);
    n >>= 8n;
  }
  return out;
}
function modPow(b, e, m) {
  let r = 1n;
  b %= m;
  while (e > 0n) {
    if (e & 1n) r = (r * b) % m;
    b = (b * b) % m;
    e >>= 1n;
  }
  return r;
}
/** SHA-1 DigestInfo prefix (PKCS#1 v1.5): adb signs the token as if it were a SHA-1 digest. */
const SHA1_PREFIX = [0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];

/** The PKCS#1 v1.5 block of the token for a key of `len` bytes. */
export function adbPadding(token, len) {
  const em = new Uint8Array(len).fill(0xff);
  em[0] = 0;
  em[1] = 1;
  const t = [...SHA1_PREFIX, ...token];
  em[len - t.length - 1] = 0;
  em.set(t, len - t.length);
  return em;
}

/**
 * 2048-bit RSA key for adb authentication: `n`, `e`, `d` (BigInt).
 * `AdbKey.generate()` uses WebCrypto; `toJSON`/`fromJSON` to keep it
 * (IndexedDB/OPFS in the browser).
 */
export class AdbKey {
  constructor({ n, e, d }) {
    this.n = typeof n === 'bigint' ? n : BigInt(n);
    this.e = typeof e === 'bigint' ? e : BigInt(e);
    this.d = typeof d === 'bigint' ? d : BigInt(d);
  }

  static async generate() {
    const k = await crypto.subtle.generateKey({ name: 'RSASSA-PKCS1-v1_5', modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-1' }, true, ['sign']);
    const j = await crypto.subtle.exportKey('jwk', k.privateKey);
    return new AdbKey({ n: toBig(unb64url(j.n)), e: toBig(unb64url(j.e)), d: toBig(unb64url(j.d)) });
  }

  toJSON() {
    return { n: this.n.toString(), e: this.e.toString(), d: this.d.toString() };
  }

  static fromJSON(j) {
    return new AdbKey(j);
  }

  /** Signature of the AUTH token (256 bytes). */
  sign(token) {
    return fromBig(modPow(toBig(adbPadding(token, 256)), this.d, this.n), 256);
  }

  /** The public key in adb's format (`adb_keys`): base64 + name. */
  publicKey(name = 'vetro@browser') {
    const words = 64;
    const buf = new Uint8Array(4 + 4 + 4 * words * 2 + 4);
    const v = new DataView(buf.buffer);
    v.setUint32(0, words, true);
    const r32 = 1n << 32n;
    // n0inv = -1 / n mod 2^32.
    const n0 = this.n % r32;
    let inv = 1n;
    for (let i = 0; i < 5; i++) inv = (inv * (2n - n0 * inv)) % r32;
    v.setUint32(4, Number((r32 - inv) % r32), true);
    const rr = modPow(2n, 4096n, this.n);
    for (let i = 0; i < words; i++) {
      v.setUint32(8 + 4 * i, Number((this.n >> BigInt(32 * i)) & 0xffffffffn), true);
      v.setUint32(8 + 4 * words + 4 * i, Number((rr >> BigInt(32 * i)) & 0xffffffffn), true);
    }
    v.setUint32(8 + 8 * words, Number(this.e), true);
    return `${b64(buf)} ${name}`;
  }
}

/** Verifies an AdbKey.sign signature with the public key (for the tests' fake adbd). */
export function adbVerify(n, e, token, signature) {
  return modPow(toBig(signature), BigInt(e), BigInt(n)) === toBig(adbPadding(token, 256));
}

// ---- Streams -----------------------------------------------------------------

/** A stream to an adbd service (OPEN). */
export class AdbStream {
  #client;
  #queue = [];
  #waitingOkay = false;
  #chunks = [];
  #readers = [];
  closed = false;
  remote = 0;

  constructor(client, local, service) {
    this.#client = client;
    this.local = local;
    this.service = service;
    this.opened = new Promise((ok, ko) => {
      this.onOpen = ok;
      this.onRefused = ko;
    });
    this.done = new Promise((ok) => (this.onClose = ok));
    /** Called with the incoming data (when set, data is not accumulated). */
    this.onData = null;
  }

  /** Queues bytes for the service (in MAX_DATA pieces, one at a time). */
  write(bytes) {
    if (this.closed) throw new Error(`adb: stream ${this.service} closed`);
    const max = this.#client.maxData;
    for (let at = 0; at < bytes.length; at += max) this.#queue.push(bytes.subarray(at, at + max));
    this.#flush();
  }

  /** True when everything written has been taken by the device. */
  get drained() {
    return !this.#queue.length && !this.#waitingOkay;
  }

  #flush() {
    if (this.#waitingOkay || !this.remote || !this.#queue.length || this.closed) return;
    this.#waitingOkay = true;
    this.#client.sendMessage(A_WRTE, this.local, this.remote, this.#queue.shift());
  }

  /** @internal */
  handle(msg) {
    switch (msg.command) {
      case A_OKAY:
        if (!this.remote) {
          this.remote = msg.arg0;
          this.onOpen(this);
        } else {
          this.#waitingOkay = false;
        }
        this.#flush();
        this.#wakeDrain();
        break;
      case A_WRTE:
        this.#client.sendMessage(A_OKAY, this.local, this.remote);
        if (this.onData) this.onData(msg.data);
        else this.#chunks.push(msg.data);
        this.#wakeReaders();
        break;
      case A_CLSE:
        if (!this.remote) this.onRefused(new Error(`adb: service ${this.service} refused`));
        this.#end();
        break;
    }
  }

  #drainWaiters = [];
  #wakeDrain() {
    if (!this.drained) return;
    for (const w of this.#drainWaiters.splice(0)) w();
  }

  /** A Promise resolved when all the bytes written have reached the device. */
  drain() {
    return this.drained ? Promise.resolve() : new Promise((ok) => this.#drainWaiters.push(ok));
  }

  #wakeReaders() {
    for (const r of this.#readers.splice(0)) r();
  }

  /** Waits for at least `n` bytes (or the close) and removes them from the buffer. */
  async read(n) {
    for (;;) {
      const have = this.#chunks.reduce((s, c) => s + c.length, 0);
      if (have >= n || this.closed) {
        const all = this.#chunks.reduce(concat, new Uint8Array());
        const take = Math.min(n, all.length);
        this.#chunks = take < all.length ? [all.slice(take)] : [];
        if (take < n) throw new Error(`adb: stream ${this.service} closed after ${take} of ${n} bytes`);
        return all.slice(0, take);
      }
      await new Promise((ok) => this.#readers.push(ok));
    }
  }

  /** Everything until the service closes. */
  async readAll() {
    await this.done;
    const all = this.#chunks.reduce(concat, new Uint8Array());
    this.#chunks = [];
    return all;
  }

  close() {
    if (this.closed) return;
    this.#client.sendMessage(A_CLSE, this.local, this.remote);
    this.#end();
  }

  #end() {
    if (this.closed) return;
    this.closed = true;
    this.#client.streams.delete(this.local);
    this.onClose();
    this.#wakeReaders();
    for (const w of this.#drainWaiters.splice(0)) w();
  }
}

// ---- Client ------------------------------------------------------------------

/** The client: one device over one transport. */
export class AdbClient {
  #t;
  #in = new Uint8Array();
  #out = [];
  #nextId = 1;
  #connected = null;
  #authTries = 0;
  streams = new Map();
  /** From the device's handshake: { kind, serial, props, features }. */
  banner = null;
  maxData = 4096;
  /** Bytes exchanged (for the status bar). */
  stats = { sent: 0, received: 0 };

  /**
   * `transport`: see the header. `key`: AdbKey for AUTH (without it, a
   * device asking for AUTH makes `connect` fail).
   */
  constructor(transport, { key = null, banner = 'host::features=shell_v2,cmd,stat_v2' } = {}) {
    this.#t = transport;
    this.key = key;
    this.hostBanner = banner;
  }

  sendMessage(command, arg0, arg1, data) {
    const m = encodeMessage(command, arg0, arg1, data);
    this.#out.push(m);
    this.stats.sent += m.length;
  }

  /** Sends the queued bytes and reads the incoming ones; returns whether it did anything. */
  pump() {
    let moved = false;
    while (this.#out.length) {
      const m = this.#out[0];
      const n = this.#t.send(m);
      if (n) moved = true;
      if (n < m.length) {
        this.#out[0] = m.subarray(n);
        break;
      }
      this.#out.shift();
    }
    const got = this.#t.recv();
    if (got.length) {
      moved = true;
      this.stats.received += got.length;
      this.#in = concat(this.#in, got);
      const { messages, rest } = decodeMessages(this.#in);
      this.#in = rest.length ? rest.slice() : new Uint8Array();
      for (const msg of messages) this.#handle(msg);
    }
    const st = this.#t.state?.();
    if (st && (st.state === 'Closed' || st.guestEof) && !this.lost) {
      this.lost = st.reason ?? 'closed';
      this.#connected?.ko(new Error(`adb: connection closed (${this.lost})`));
      for (const s of [...this.streams.values()]) {
        s.onRefused(new Error('adb: connection closed'));
        s.handle({ command: A_CLSE, arg0: 0, arg1: s.local, data: new Uint8Array() });
      }
    }
    return moved;
  }

  #handle(msg) {
    switch (msg.command) {
      case A_CNXN: {
        this.maxData = Math.min(msg.arg1 || 4096, MAX_DATA);
        this.banner = parseBanner(dec.decode(msg.data));
        this.#connected?.ok(this.banner);
        break;
      }
      case A_AUTH: {
        if (msg.arg0 !== AUTH_TOKEN) break;
        if (!this.key) {
          this.#connected?.ko(new Error('adb: the device asks for AUTH and there is no key'));
          break;
        }
        this.#authTries++;
        if (this.#authTries === 1) this.sendMessage(A_AUTH, AUTH_SIGNATURE, 0, this.key.sign(msg.data));
        else if (this.#authTries === 2) this.sendMessage(A_AUTH, AUTH_RSAPUBLICKEY, 0, enc.encode(`${this.key.publicKey()}\0`));
        break;
      }
      default: {
        const s = this.streams.get(msg.arg1);
        if (s) s.handle(msg);
        else if (msg.command === A_WRTE || msg.command === A_OKAY) this.sendMessage(A_CLSE, 0, msg.arg0);
      }
    }
  }

  /** Handshake (CNXN, and AUTH if needed): a Promise of the device banner. */
  connect() {
    if (!this.#connected) {
      let ok;
      let ko;
      const p = new Promise((a, b) => {
        ok = a;
        ko = b;
      });
      this.#connected = { ok, ko, p };
      this.sendMessage(A_CNXN, VERSION, MAX_DATA, enc.encode(`${this.hostBanner}\0`));
    }
    return this.#connected.p;
  }

  /** Opens a stream to `service` (e.g. `shell:ls`): a Promise of the AdbStream. */
  open(service) {
    const s = new AdbStream(this, this.#nextId++, service);
    this.streams.set(s.local, s);
    this.sendMessage(A_OPEN, s.local, 0, enc.encode(`${service}\0`));
    return s.opened;
  }

  get features() {
    return this.banner?.features ?? [];
  }

  /**
   * Runs a command: { stdout, stderr, exitCode } (with shell_v2, separate
   * stdout and stderr and the exit code; without it, everything in stdout and
   * exitCode null). Optional `stdin` (shell_v2 only).
   */
  async shell(cmd, { stdin = null } = {}) {
    if (!this.features.includes('shell_v2')) {
      const s = await this.open(`shell:${cmd}`);
      const out = await s.readAll();
      return { stdout: dec.decode(out), stderr: '', exitCode: null };
    }
    const s = await this.open(`shell,v2,raw:${cmd}`);
    const out = [];
    const err = [];
    let exitCode = null;
    let buf = new Uint8Array();
    s.onData = (d) => {
      buf = concat(buf, d);
      while (buf.length >= 5) {
        const id = buf[0];
        const len = new DataView(buf.buffer, buf.byteOffset + 1, 4).getUint32(0, true);
        if (buf.length < 5 + len) break;
        const payload = buf.slice(5, 5 + len);
        if (id === 1) out.push(payload);
        else if (id === 2) err.push(payload);
        else if (id === 3) exitCode = payload[0];
        buf = buf.slice(5 + len);
      }
    };
    if (stdin) {
      const pkt = (id, data) => {
        const p = new Uint8Array(5 + data.length);
        p[0] = id;
        new DataView(p.buffer).setUint32(1, data.length, true);
        p.set(data, 5);
        return p;
      };
      s.write(pkt(0, stdin));
      s.write(pkt(3, new Uint8Array()));
    }
    await s.done;
    const join = (parts) => dec.decode(parts.reduce(concat, new Uint8Array()));
    return { stdout: join(out), stderr: join(err), exitCode };
  }

  /** Copies `bytes` to the device file `path` (sync service, SEND/DATA/DONE). */
  async push(path, bytes, { mode = 0o644, mtime = 0 } = {}) {
    const s = await this.open('sync:');
    const req = (id, payload) => {
      const p = new Uint8Array(8 + payload.length);
      p.set(enc.encode(id));
      new DataView(p.buffer).setUint32(4, payload.length, true);
      p.set(payload, 8);
      return p;
    };
    s.write(req('SEND', enc.encode(`${path},${mode}`)));
    for (let at = 0; at < bytes.length; at += SYNC_CHUNK) s.write(req('DATA', bytes.subarray(at, at + SYNC_CHUNK)));
    const done = new Uint8Array(8);
    done.set(enc.encode('DONE'));
    new DataView(done.buffer).setUint32(4, mtime >>> 0, true);
    s.write(done);
    const head = await s.read(8);
    const id = dec.decode(head.subarray(0, 4));
    const len = new DataView(head.buffer, head.byteOffset + 4, 4).getUint32(0, true);
    if (id !== 'OKAY') {
      const msg = id === 'FAIL' ? dec.decode(await s.read(len)) : `reply ${id}`;
      s.close();
      throw new Error(`adb push ${path}: ${msg}`);
    }
    s.write(req('QUIT', new Uint8Array()));
    await s.drain();
    s.close();
  }

  /**
   * Installs an APK: push to /data/local/tmp and `pm install -r`, then removes
   * the file. Returns pm's output; throws if it does not say Success.
   */
  async install(apk, { name = 'vetro-install.apk', args = '-r' } = {}) {
    const tmp = `/data/local/tmp/${name.replace(/[^A-Za-z0-9._-]/g, '_')}`;
    await this.push(tmp, apk);
    const r = await this.shell(`pm install ${args} ${tmp}; e=$?; rm -f ${tmp}; exit $e`);
    const text = `${r.stdout}${r.stderr}`.trim();
    if (!/\bSuccess\b/.test(text)) throw new Error(`adb install: ${text || `exit code ${r.exitCode}`}`);
    return text;
  }

  /** Like `adb devices -l`: this client's device. */
  async devices() {
    const b = this.banner ?? (await this.connect());
    let serial = b.serial;
    if (!serial) {
      const r = await this.shell('getprop ro.serialno');
      serial = r.stdout.trim();
      b.serial = serial;
    }
    return [{ serial, state: b.kind, product: b.props['ro.product.name'] ?? '', model: b.props['ro.product.model'] ?? '', device: b.props['ro.product.device'] ?? '' }];
  }
}

/** The CNXN banner: `device::ro.product.name=x;...;features=a,b`. */
export function parseBanner(s) {
  s = s.replace(/\0+$/, '');
  const [kind, serial, rest = ''] = s.split(':');
  const props = {};
  let features = [];
  for (const kv of rest.split(';')) {
    const i = kv.indexOf('=');
    if (i < 0) continue;
    const k = kv.slice(0, i);
    const v = kv.slice(i + 1);
    if (k === 'features') features = v.split(',').filter(Boolean);
    else props[k] = v;
  }
  return { kind, serial, props, features };
}
