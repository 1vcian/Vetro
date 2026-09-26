// Client ADB in JavaScript (M5/M6, ADR 0027, docs/specs/net.md "Come adb
// userà l'inoltro"): parla il protocollo di adb con adbd del guest su una
// connessione TCP verso la porta 5555, senza adb dell'host. Non usa API di
// Node: gira nel Worker dell'app e nei test.
//
// Il trasporto è qualunque oggetto con `send(bytes) -> byte presi`,
// `recv() -> Uint8Array` e `state() -> { state, guestEof }`: il GuestSocket di
// web/node/vetro.mjs (i byte si muovono mentre la macchina esegue) o un finto
// adbd nei test. Il client è a interrogazione, come il resto dell'API:
// `pump()` fra un quanto e l'altro manda i byte in coda, legge quelli
// arrivati e fa avanzare le Promise.
//
// Protocollo (system/core/adb/protocol.txt di AOSP): messaggi di 24 byte
// little endian (comando, arg0, arg1, lunghezza dei dati, somma dei byte dei
// dati, comando ^ 0xffffffff) seguiti dai dati. CNXN per il saluto (con
// `ro.adb.secure=1` il dispositivo manda prima AUTH: si firma il gettone con
// una chiave RSA, o si offre la chiave pubblica), OPEN/OKAY/WRTE/CLSE per i
// flussi, uno per servizio (`shell,v2,raw:`, `exec:`, `sync:`). Su un flusso
// un solo WRTE alla volta: il prossimo parte dopo l'OKAY del dispositivo.

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
/** Pezzo dei dati di push (il limite di sync è 64 KiB). */
const SYNC_CHUNK = 64 * 1024;

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Un messaggio ADB codificato (intestazione + dati). */
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

/** Legge i messaggi completi da un accumulatore: { messages, rest }. */
export function decodeMessages(buf) {
  const messages = [];
  let at = 0;
  while (buf.length - at >= 24) {
    const v = new DataView(buf.buffer, buf.byteOffset + at, 24);
    const command = v.getUint32(0, true);
    const len = v.getUint32(12, true);
    if (v.getUint32(20, true) !== (command ^ 0xffffffff) >>> 0) throw new Error(`adb: messaggio non valido (magia) a ${at}`);
    if (len > 16 << 20) throw new Error(`adb: messaggio di ${len} byte`);
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

// ---- Chiave RSA di adb (AUTH) ---------------------------------------------

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
/** Prefisso DigestInfo di SHA-1 (PKCS#1 v1.5): adb firma il gettone come se fosse un digest SHA-1. */
const SHA1_PREFIX = [0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];

/** Il blocco PKCS#1 v1.5 del gettone per una chiave di `len` byte. */
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
 * Chiave RSA a 2048 bit per l'autenticazione di adb: `n`, `e`, `d` (BigInt).
 * `AdbKey.generate()` usa WebCrypto; `toJSON`/`fromJSON` per conservarla
 * (IndexedDB/OPFS nel browser).
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

  /** Firma del gettone di AUTH (256 byte). */
  sign(token) {
    return fromBig(modPow(toBig(adbPadding(token, 256)), this.d, this.n), 256);
  }

  /** La chiave pubblica nel formato di adb (`adb_keys`): base64 + nome. */
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

/** Verifica una firma di AdbKey.sign con la chiave pubblica (per i finti adbd dei test). */
export function adbVerify(n, e, token, signature) {
  return modPow(toBig(signature), BigInt(e), BigInt(n)) === toBig(adbPadding(token, 256));
}

// ---- Flussi ------------------------------------------------------------------

/** Un flusso verso un servizio di adbd (OPEN). */
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
    /** Chiamato con i dati arrivati (se impostato, i dati non si accumulano). */
    this.onData = null;
  }

  /** Mette in coda byte per il servizio (a pezzi di MAX_DATA, uno alla volta). */
  write(bytes) {
    if (this.closed) throw new Error(`adb: flusso ${this.service} chiuso`);
    const max = this.#client.maxData;
    for (let at = 0; at < bytes.length; at += max) this.#queue.push(bytes.subarray(at, at + max));
    this.#flush();
  }

  /** Vero quando tutto quello che si è scritto è stato preso dal dispositivo. */
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
        if (!this.remote) this.onRefused(new Error(`adb: servizio ${this.service} rifiutato`));
        this.#end();
        break;
    }
  }

  #drainWaiters = [];
  #wakeDrain() {
    if (!this.drained) return;
    for (const w of this.#drainWaiters.splice(0)) w();
  }

  /** Promise risolta quando i byte scritti sono tutti arrivati al dispositivo. */
  drain() {
    return this.drained ? Promise.resolve() : new Promise((ok) => this.#drainWaiters.push(ok));
  }

  #wakeReaders() {
    for (const r of this.#readers.splice(0)) r();
  }

  /** Aspetta almeno `n` byte (o la chiusura) e li toglie dal buffer. */
  async read(n) {
    for (;;) {
      const have = this.#chunks.reduce((s, c) => s + c.length, 0);
      if (have >= n || this.closed) {
        const all = this.#chunks.reduce(concat, new Uint8Array());
        const take = Math.min(n, all.length);
        this.#chunks = take < all.length ? [all.slice(take)] : [];
        if (take < n) throw new Error(`adb: flusso ${this.service} chiuso dopo ${take} di ${n} byte`);
        return all.slice(0, take);
      }
      await new Promise((ok) => this.#readers.push(ok));
    }
  }

  /** Tutto fino alla chiusura del servizio. */
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

/** Il client: un dispositivo su un trasporto. */
export class AdbClient {
  #t;
  #in = new Uint8Array();
  #out = [];
  #nextId = 1;
  #connected = null;
  #authTries = 0;
  streams = new Map();
  /** Dal saluto del dispositivo: { kind, serial, props, features }. */
  banner = null;
  maxData = 4096;
  /** Byte scambiati (per la barra di stato). */
  stats = { sent: 0, received: 0 };

  /**
   * `transport`: vedi l'intestazione. `key`: AdbKey per AUTH (senza, un
   * dispositivo che chiede AUTH fa fallire `connect`).
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

  /** Manda i byte in coda e legge quelli arrivati; restituisce se ha fatto qualcosa. */
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
      this.lost = st.reason ?? 'chiusa';
      this.#connected?.ko(new Error(`adb: connessione chiusa (${this.lost})`));
      for (const s of [...this.streams.values()]) {
        s.onRefused(new Error('adb: connessione chiusa'));
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
          this.#connected?.ko(new Error('adb: il dispositivo chiede AUTH e non c\'è una chiave'));
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

  /** Saluto (CNXN, ed eventualmente AUTH): Promise del banner del dispositivo. */
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

  /** Apre un flusso verso `service` (es. `shell:ls`): Promise dell'AdbStream. */
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
   * Esegue un comando: { stdout, stderr, exitCode } (con shell_v2 stdout e
   * stderr separati e il codice d'uscita; senza, tutto in stdout ed
   * exitCode null). `stdin` facoltativo (solo con shell_v2).
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

  /** Copia `bytes` nel file `path` del dispositivo (servizio sync, SEND/DATA/DONE). */
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
      const msg = id === 'FAIL' ? dec.decode(await s.read(len)) : `risposta ${id}`;
      s.close();
      throw new Error(`adb push ${path}: ${msg}`);
    }
    s.write(req('QUIT', new Uint8Array()));
    await s.drain();
    s.close();
  }

  /**
   * Installa un APK: push in /data/local/tmp e `pm install -r`, poi toglie il
   * file. Restituisce l'uscita di pm; lancia se non dice Success.
   */
  async install(apk, { name = 'vetro-install.apk', args = '-r' } = {}) {
    const tmp = `/data/local/tmp/${name.replace(/[^A-Za-z0-9._-]/g, '_')}`;
    await this.push(tmp, apk);
    const r = await this.shell(`pm install ${args} ${tmp}; e=$?; rm -f ${tmp}; exit $e`);
    const text = `${r.stdout}${r.stderr}`.trim();
    if (!/\bSuccess\b/.test(text)) throw new Error(`adb install: ${text || `codice ${r.exitCode}`}`);
    return text;
  }

  /** Come `adb devices -l`: il dispositivo di questo client. */
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

/** Il banner del CNXN: `device::ro.product.name=x;...;features=a,b`. */
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
