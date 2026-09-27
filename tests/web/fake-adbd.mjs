// A fake adbd for the tests of the ADB client (tests/web/adb.mjs) and of the
// app catalog (tests/web/catalog.mjs): an in-memory adbd speaking AOSP's
// protocol (24-byte messages, CNXN, AUTH, OPEN/OKAY/WRTE/CLSE, shell v2,
// sync). It checks what a real adbd would reject: one WRTE in flight per
// stream, data sums, magic, maximum data size, SEND/DATA/DONE format.
// `pm install` accepts files starting with PK and records the package (named
// by `identify(bytes)`, if given) for `pm list packages`.

import { ADB, adbVerify, decodeMessages, encodeMessage } from '../../web/node/adb.mjs';
import { Fail } from './lib.mjs';

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A fake adbd: receives the client's bytes, answers like AOSP's adbd. */
export class FakeAdbd {
  files = new Map();
  commands = [];
  opened = [];
  #in = new Uint8Array();
  #streams = new Map();
  #next = 100;
  out = [];

  /** Installed packages: name -> versionCode (for `pm list packages`). */
  packages = new Map();

  constructor({ secure = false, acceptSignature = true, maxData = 4096, features = 'shell_v2,cmd,stat_v2', identify = null } = {}) {
    this.identify = identify;
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
        const id = this.identify?.(apk.bytes);
        if (id) this.packages.set(id.package, id.versionCode);
        this.files.delete(path);
      } else {
        err = 'Failure [INSTALL_FAILED_INVALID_APK]\n';
        code = 1;
      }
    } else if (cmd === 'pm list packages --show-versioncode') {
      out = ['package:com.android.settings versionCode:35', ...[...this.packages].map(([p, v]) => `package:${p} versionCode:${v}`)].join('\n') + '\n';
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
export class Pipe {
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
export async function drive(client, p, limit = 100000) {
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
