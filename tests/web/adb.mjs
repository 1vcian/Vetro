// The ADB client in web/node/adb.mjs (M5/M6, ADR 0028) against an in-memory
// fake adbd speaking AOSP's protocol (24-byte messages, CNXN, AUTH,
// OPEN/OKAY/WRTE/CLSE, shell v2, sync). The fake checks what a real adbd would
// reject: one WRTE in flight per stream, data sums, magic, maximum data size,
// SEND/DATA/DONE format. The test against a real adbd (Android in the guest)
// is in tests/web/android.mjs (VETRO_ANDROID=1) and in the long Chrome test.

import { ADB, AdbClient, AdbKey, adbVerify, decodeMessages, encodeMessage, parseBanner } from '../../web/node/adb.mjs';
import { Fail } from './lib.mjs';

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A fake adbd: receives the client's bytes, answers like AOSP's adbd. */
class FakeAdbd {
  files = new Map();
  commands = [];
  opened = [];
  #in = new Uint8Array();
  #streams = new Map();
  #next = 100;
  out = [];

  constructor({ secure = false, acceptSignature = true, maxData = 4096, features = 'shell_v2,cmd,stat_v2' } = {}) {
    this.secure = secure;
    this.acceptSignature = acceptSignature;
    this.maxData = maxData;
    this.features = features;
    this.authorized = !secure;
    this.token = new Uint8Array(20).map((_, i) => (i * 37 + 11) & 0xff);
  }

  /** `delivered`: called when the client has received the whole message. */
  send(type, a0, a1, data, delivered = null) {
    this.out.push({ bytes: encodeMessage(type, a0, a1, data), delivered });
  }

  feed(bytes) {
    const all = new Uint8Array(this.#in.length + bytes.length);
    all.set(this.#in);
    all.set(bytes, this.#in.length);
    const { messages, rest } = decodeMessages(all);
    this.#in = rest.slice();
    for (const m of messages) this.#handle(m);
  }

  #banner() {
    this.send(ADB.A_CNXN, 0x01000001, this.maxData, enc.encode(`device::ro.product.name=vetro_arm64;ro.product.model=Vetro arm64;ro.product.device=vsoc_arm64_only;features=${this.features}\0`));
  }

  #handle(m) {
    let sum = 0;
    for (const b of m.data) sum = (sum + b) >>> 0;
    if (m.data.length > this.maxData && m.command !== ADB.A_CNXN) throw new Fail(`${m.data.length} bytes of data beyond the maximum ${this.maxData}`);
    switch (m.command) {
      case ADB.A_CNXN:
        if (this.secure && !this.authorized) this.send(ADB.A_AUTH, 1, 0, this.token);
        else this.#banner();
        return;
      case ADB.A_AUTH:
        if (m.arg0 === 2) {
          this.signature = m.data;
          if (this.acceptSignature && this.key && adbVerify(this.key.n, this.key.e, this.token, m.data)) {
            this.authorized = true;
            this.#banner();
          } else this.send(ADB.A_AUTH, 1, 0, this.token);
        } else if (m.arg0 === 3) {
          this.publicKey = dec.decode(m.data).replace(/\0$/, '');
          this.authorized = true;
          this.#banner();
        }
        return;
      case ADB.A_OPEN: {
        if (!this.authorized) throw new Fail('OPEN before authorization');
        const service = dec.decode(m.data).replace(/\0$/, '');
        this.opened.push(service);
        const id = this.#next++;
        const s = { id, peer: m.arg0, service, buf: new Uint8Array(), inflight: false };
        this.#streams.set(id, s);
        this.send(ADB.A_OKAY, id, m.arg0);
        if (service.startsWith('shell,v2,raw:') || service.startsWith('shell:')) this.#shell(s);
        return;
      }
      case ADB.A_WRTE: {
        const s = this.#streams.get(m.arg1);
        if (!s) throw new Fail(`WRTE on an unknown stream ${m.arg1}`);
        if (s.inflight) throw new Fail('second WRTE before the OKAY');
        s.inflight = true;
        const all = new Uint8Array(s.buf.length + m.data.length);
        all.set(s.buf);
        all.set(m.data, s.buf.length);
        s.buf = all;
        // The next WRTE is legal only after the client has seen the OKAY.
        this.send(ADB.A_OKAY, s.id, s.peer, undefined, () => (s.inflight = false));
        if (s.service === 'sync:') this.#sync(s);
        return;
      }
      case ADB.A_OKAY:
        return;
      case ADB.A_CLSE: {
        const s = this.#streams.get(m.arg1);
        if (s) {
          this.#streams.delete(s.id);
          this.send(ADB.A_CLSE, s.id, s.peer);
        }
        return;
      }
      default:
        throw new Fail(`unexpected command ${m.command.toString(16)} (sum ${sum})`);
    }
  }

  #write(s, data) {
    for (let at = 0; at < data.length; at += this.maxData) this.send(ADB.A_WRTE, s.id, s.peer, data.subarray(at, at + this.maxData));
  }

  #shell(s) {
    const v2 = s.service.startsWith('shell,v2,raw:');
    const cmd = s.service.slice(v2 ? 13 : 6);
    this.commands.push(cmd);
    let out = '';
    let err = '';
    let code = 0;
    if (cmd === 'getprop ro.serialno') out = 'VETRO00001\n';
    else if (cmd.startsWith('echo ')) out = `${cmd.slice(5)}\n`;
    else if (cmd.startsWith('pm install')) {
      const path = /\/data\/local\/tmp\/\S+/.exec(cmd)[0].replace(/;$/, '');
      const apk = this.files.get(path);
      if (apk && dec.decode(apk.bytes.subarray(0, 2)) === 'PK') {
        out = 'Success\n';
        this.installed = apk.bytes;
        this.files.delete(path);
      } else {
        err = 'Failure [INSTALL_FAILED_INVALID_APK]\n';
        code = 1;
      }
    } else if (cmd === 'false') code = 1;
    else {
      err = `/system/bin/sh: ${cmd}: inaccessible or not found\n`;
      code = 127;
    }
    const pkt = (id, text) => {
      const d = typeof text === 'string' ? enc.encode(text) : text;
      const p = new Uint8Array(5 + d.length);
      p[0] = id;
      new DataView(p.buffer).setUint32(1, d.length, true);
      p.set(d, 5);
      return p;
    };
    if (v2) {
      if (out) this.#write(s, pkt(1, out));
      if (err) this.#write(s, pkt(2, err));
      this.#write(s, pkt(3, new Uint8Array([code])));
    } else this.#write(s, enc.encode(out + err));
    this.send(ADB.A_CLSE, s.id, s.peer);
    this.#streams.delete(s.id);
  }

  #sync(s) {
    for (;;) {
      if (s.buf.length < 8) return;
      const id = dec.decode(s.buf.subarray(0, 4));
      const len = new DataView(s.buf.buffer, s.buf.byteOffset + 4, 4).getUint32(0, true);
      if (id === 'DONE') {
        s.buf = s.buf.slice(8);
        this.files.set(s.path, { bytes: s.data, mode: s.mode, mtime: len });
        this.#write(s, enc.encode('OKAY\0\0\0\0'));
        continue;
      }
      if (id === 'QUIT') {
        s.buf = s.buf.slice(8);
        this.quit = true;
        continue;
      }
      if (s.buf.length < 8 + len) return;
      const payload = s.buf.slice(8, 8 + len);
      s.buf = s.buf.slice(8 + len);
      if (id === 'SEND') {
        const [path, mode] = dec.decode(payload).split(',');
        s.path = path;
        s.mode = Number(mode);
        s.data = new Uint8Array();
        if (path.startsWith('/proc/')) {
          const msg = enc.encode('read-only file system');
          const f = new Uint8Array(8 + msg.length);
          f.set(enc.encode('FAIL'));
          new DataView(f.buffer).setUint32(4, msg.length, true);
          f.set(msg, 8);
          this.#write(s, f);
          s.failed = true;
        }
      } else if (id === 'DATA') {
        if (len > 64 * 1024) throw new Fail(`DATA of ${len} bytes beyond 64 KiB`);
        const d = new Uint8Array(s.data.length + payload.length);
        d.set(s.data);
        d.set(payload, s.data.length);
        s.data = d;
      } else throw new Fail(`sync request ${id}`);
    }
  }
}

/**
 * In-memory transport between the client and the fake adbd, with bytes
 * passing in small pieces and a queue limit (like the 256 KiB GuestSocket).
 */
class Pipe {
  constructor(adbd, { chunk = 1000, queue = 64 * 1024 } = {}) {
    this.adbd = adbd;
    this.chunk = chunk;
    this.queue = queue;
    this.pending = 0;
  }
  send(bytes) {
    const n = Math.min(bytes.length, this.queue - this.pending);
    if (n > 0) {
      this.pending += n;
      this.adbd.feed(bytes.subarray(0, n));
      this.pending = 0;
    }
    return n;
  }
  recv() {
    if (!this.adbd.out.length) return new Uint8Array();
    const m = this.adbd.out[0];
    const n = Math.min(this.chunk, m.bytes.length);
    const part = m.bytes.slice(0, n);
    if (n < m.bytes.length) m.bytes = m.bytes.subarray(n);
    else {
      this.adbd.out.shift();
      m.delivered?.();
    }
    return part;
  }
  state() {
    return { state: 'Open', guestEof: false };
  }
}

/** Runs the client until the Promise settles (like the machine loop). */
async function drive(client, p, limit = 100000) {
  let done = false;
  let value;
  let error;
  p.then((v) => {
    done = true;
    value = v;
  }, (e) => {
    done = true;
    error = e;
  });
  for (let i = 0; i < limit && !done; i++) {
    client.pump();
    await new Promise((ok) => setImmediate(ok));
  }
  if (!done) throw new Fail('adb operation not finished');
  if (error) throw error;
  return value;
}

function check(cond, what) {
  if (!cond) throw new Fail(what);
  console.log(`ok: ${what}`);
}

async function main() {
  // Banner.
  const b = parseBanner('device::ro.product.name=x;ro.product.model=Vetro arm64;features=shell_v2,cmd\0');
  check(b.kind === 'device' && b.props['ro.product.model'] === 'Vetro arm64' && b.features.join() === 'shell_v2,cmd', 'device banner');

  // Device without authentication (ro.adb.secure=0, userdebug).
  const adbd = new FakeAdbd();
  const c = new AdbClient(new Pipe(adbd));
  const banner = await drive(c, c.connect());
  check(banner.props['ro.product.name'] === 'vetro_arm64' && c.maxData === 4096, 'CNXN: banner and maximum data size');
  const devs = await drive(c, c.devices());
  check(devs.length === 1 && devs[0].serial === 'VETRO00001' && devs[0].model === 'Vetro arm64' && devs[0].state === 'device', 'devices: serial, model, state');
  const r = await drive(c, c.shell('echo ciao vetro'));
  check(r.stdout === 'ciao vetro\n' && r.exitCode === 0, 'shell v2: output and exit code');
  const e = await drive(c, c.shell('nonesiste'));
  check(e.exitCode === 127 && e.stderr.includes('not found') && e.stdout === '', 'shell v2: separate stderr, exit code 127');
  // A fake 300 KB APK: many WRTEs, one at a time, 64 KiB DATA pieces.
  const apk = new Uint8Array(300_000).map((_, i) => (i * 7) & 0xff);
  apk.set(enc.encode('PK\x03\x04'));
  const out = await drive(c, c.install(apk, { name: 'prova.apk' }));
  check(out === 'Success' && adbd.installed?.length === apk.length && adbd.installed.every((x, i) => x === apk[i]), 'install: 300 KB push and pm install');
  check(adbd.opened.includes('sync:') && adbd.quit && adbd.files.size === 0, 'install: sync with QUIT, temporary file removed');
  check(adbd.commands.some((x) => x.startsWith('pm install -r /data/local/tmp/prova.apk')), 'install: pm install -r');
  const bad = await drive(c, c.install(enc.encode('non un apk')).then(() => null, (err) => err));
  check(bad instanceof Error && bad.message.includes('INSTALL_FAILED_INVALID_APK'), 'install: pm error reported');
  const fail = await drive(c, c.push('/proc/x', enc.encode('x')).then(() => null, (err) => err));
  check(fail instanceof Error && fail.message.includes('read-only file system'), 'push: device FAIL reported');
  await drive(c, c.push('/sdcard/a.txt', enc.encode('ciao'), { mode: 0o600, mtime: 1234 }));
  const f = adbd.files.get('/sdcard/a.txt');
  check(f && dec.decode(f.bytes) === 'ciao' && f.mode === 0o600 && f.mtime === 1234, 'push: content, mode and mtime');

  // Device without shell_v2: raw shell:.
  const old = new FakeAdbd({ features: 'cmd' });
  const c2 = new AdbClient(new Pipe(old, { chunk: 7 }));
  await drive(c2, c2.connect());
  const r2 = await drive(c2, c2.shell('echo vecchio'));
  check(r2.stdout === 'vecchio\n' && r2.exitCode === null && old.opened[0] === 'shell:echo vecchio', 'shell without v2 (bytes in pieces of 7)');

  // AUTH: signature accepted.
  const key = await AdbKey.generate();
  const sec = new FakeAdbd({ secure: true });
  sec.key = key;
  const c3 = new AdbClient(new Pipe(sec), { key });
  await drive(c3, c3.connect());
  check(sec.authorized && adbVerify(key.n, key.e, sec.token, sec.signature), 'AUTH: RSA signature of the token verified');
  // AUTH: signature rejected, then the public key (adb_keys format).
  const sec2 = new FakeAdbd({ secure: true, acceptSignature: false });
  const c4 = new AdbClient(new Pipe(sec2), { key: AdbKey.fromJSON(JSON.parse(JSON.stringify(key.toJSON()))) });
  await drive(c4, c4.connect());
  const [b64key, name] = sec2.publicKey.split(' ');
  const raw = Uint8Array.from(atob(b64key), (ch) => ch.charCodeAt(0));
  const v = new DataView(raw.buffer);
  let n = 0n;
  for (let i = 63; i >= 0; i--) n = (n << 32n) | BigInt(v.getUint32(8 + 4 * i, true));
  const n0inv = BigInt(v.getUint32(4, true));
  check(name === 'vetro@browser' && raw.length === 524 && v.getUint32(0, true) === 64 && n === key.n && v.getUint32(520, true) === 65537 &&
    ((n % (1n << 32n)) * n0inv + 1n) % (1n << 32n) === 0n, 'AUTH: public key in adb format (n, n0inv, e)');
  // Without a key, AUTH is a clear error.
  const c5 = new AdbClient(new Pipe(new FakeAdbd({ secure: true })));
  const noKey = await drive(c5, c5.connect().then(() => null, (err) => err));
  check(noKey instanceof Error && noKey.message.includes('AUTH'), 'AUTH without a key: error');
  // Wrong magic: error.
  const broken = encodeMessage(ADB.A_OKAY, 1, 2);
  broken[20] ^= 1;
  let threw = false;
  try {
    decodeMessages(broken);
  } catch {
    threw = true;
  }
  check(threw, 'message with the wrong magic rejected');
  console.log('adb: all good');
}

main().catch((e) => {
  console.error(e instanceof Fail ? `FAILED: ${e.message}` : e);
  process.exit(1);
});
