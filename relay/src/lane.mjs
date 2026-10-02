// One WebSocket lane of the network gateway (ADR 0046, docs/specs/relay.md):
// the guest's TCP connections and DNS queries carried by one WebSocket, and
// the real sockets behind them. Platform-free: the socket factory, the DNS
// resolver, the session ledger and the WebSocket are injected, so the same
// code runs in the Cloudflare Worker, in the Node gateway and in the unit
// tests.
//
// Safeguards enforced here: the destination policy (ports 80/443, public
// addresses only, checked after resolution), per-lane connections, the
// session ledger before every byte (bandwidth, total, connections, kill
// switch), idle connections and the lane's maximum life. Nothing about the
// traffic is logged except counters at the end.

import { decode, encode, MAX_DATA, VERSION, WINDOW } from './protocol.mjs';
import { addressBlocked, checkDestination, portAllowed } from './policy.mjs';
import { failAnswer, fitUdp, parseQuery, resolve4 } from './dns.mjs';

/** Bytes asked to the ledger at a time. */
export const GRANT = 64 * 1024;
export const IDLE_MS = 10 * 60 * 1000;
export const LANE_MAX_MS = 60 * 60 * 1000;
export const CONNECT_TIMEOUT_MS = 15_000;
/** How often the lane checks idle connections and its own age. */
export const TICK_MS = 15_000;

const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

function withTimeout(p, ms) {
  let t;
  return Promise.race([p, new Promise((_, no) => (t = setTimeout(() => no(Object.assign(new Error('timeout'), { code: 'TIMEOUT' })), ms)))]).finally(() => clearTimeout(t));
}

/** Cloudflare refuses sockets to its own ranges with this kind of message. */
export const isPlatformError = (e) => e?.code === 'PLATFORM' || /proxy request failed|cannot connect to the specified address|TCP Loop detected/i.test(String(e?.message ?? ''));

export class Lane {
  /**
   * - `send(data)`: sends a WebSocket message (Uint8Array or string);
   * - `close(code, reason)`: closes the WebSocket;
   * - `connect({ hostname, port })`: a socket { opened, readable, writable,
   *   close(), reset?() } (Cloudflare's `connect` shape);
   * - `resolver(query) -> answer`: raw DNS (DoH);
   * - `ledger`: the session's (methods may be async);
   * - `lane`: index; `laneConns`: connections per lane; `limits`: shown in hello.
   */
  constructor({ send, close, connect, resolver, ledger, lane = 0, laneConns = 32, limits = {}, platform = 'node', now = Date.now, log = (o) => console.log(JSON.stringify(o)), idleMs = IDLE_MS, maxLifeMs = LANE_MAX_MS, connectTimeoutMs = CONNECT_TIMEOUT_MS, tickMs = TICK_MS }) {
    Object.assign(this, { sendRaw: send, closeRaw: close, connect, resolver, ledger, lane, laneConns, limits, platform, now, log, idleMs, maxLifeMs, connectTimeoutMs, tickMs });
    this.conns = new Map();
    this.budget = 0;
    this.spendChain = Promise.resolve(true);
    this.closed = false;
    this.started = now();
    this.timer = null;
    this.counters = { conns: 0, refused: {}, bytesToRemote: 0, bytesToGuest: 0, dns: 0 };
  }

  start() {
    this.send(JSON.stringify({ type: 'hello', v: VERSION, lane: this.lane, window: WINDOW, maxData: MAX_DATA, laneConns: this.laneConns, limits: this.limits }));
    if (this.tickMs > 0) this.timer = setInterval(() => this.tick(), this.tickMs);
  }

  send(data) {
    if (this.closed) return;
    try {
      this.sendRaw(data);
    } catch {
      this.end('send-failed', false);
    }
  }

  /** Ends the lane: `bye` with the reason, the WebSocket closed, every socket closed. */
  end(reason, notify = true) {
    if (this.closed) return;
    if (notify) this.send(JSON.stringify({ type: 'bye', reason }));
    this.closed = true;
    if (this.timer) clearInterval(this.timer);
    for (const c of [...this.conns.values()]) this.drop(c, false, true);
    try {
      this.closeRaw(reason === 'protocol' ? 4002 : 4000, reason);
    } catch {}
    Promise.resolve(this.ledger.closeLane(this.lane)).catch(() => {});
    this.log({ event: 'lane-end', platform: this.platform, reason, secs: Math.round((this.now() - this.started) / 1000), ...this.counters });
  }

  /** The WebSocket closed from the other side. */
  onClose() {
    this.end('closed', false);
  }

  /** Idle connections and the lane's age (also called by the tests). */
  tick() {
    const t = this.now();
    if (t - this.started >= this.maxLifeMs) return this.end('expired');
    for (const c of [...this.conns.values()]) if (t - c.last >= this.idleMs) this.drop(c, true, true);
  }

  refuse(id, reason) {
    this.counters.refused[reason] = (this.counters.refused[reason] ?? 0) + 1;
    this.send(encode.refused(id, reason));
  }

  /** Forgets connection `c`, closes its socket; `notify` = CLOSE(reset) to the client. */
  drop(c, notify, reset) {
    if (c.closed) return;
    c.closed = true;
    this.conns.delete(c.id);
    if (c.wake) c.wake();
    if (c.socket) {
      try {
        if (reset && c.socket.reset) c.socket.reset();
        else c.socket.close();
      } catch {}
    }
    if (notify && !this.closed) this.send(encode.close(c.id, true, true));
    if (c.counted) Promise.resolve(this.ledger.closeConn()).catch(() => {});
  }

  /** Waits for the ledger's permission to move `n` bytes; false = the lane ended. */
  spend(n) {
    const run = async () => {
      while (this.budget < n) {
        if (this.closed) return false;
        const want = Math.max(GRANT, n);
        const g = await this.ledger.grant(want);
        if (!g.ok) {
          this.end(g.reason === 'bytes' ? 'limit-bytes' : g.reason);
          return false;
        }
        if (g.waitMs > 0) await sleep(g.waitMs);
        this.budget += want;
      }
      this.budget -= n;
      return !this.closed;
    };
    const p = this.spendChain.then(run);
    this.spendChain = p.catch(() => false);
    return p;
  }

  /** A WebSocket message from the client. */
  async onMessage(data) {
    if (this.closed) return;
    if (typeof data === 'string') {
      let m;
      try {
        m = JSON.parse(data);
      } catch {
        return this.end('protocol');
      }
      if (m?.type === 'ping') this.send(JSON.stringify({ type: 'pong' }));
      return;
    }
    const f = decode(data instanceof Uint8Array ? data : new Uint8Array(data));
    if (!f || f.fromGateway) return this.end('protocol');
    const c = this.conns.get(f.id);
    switch (f.type) {
      case 'connect':
      case 'connectName':
        if (c) return this.end('protocol');
        return this.open(f);
      case 'data':
        if (!c || c.state !== 'open' || c.shut) return;
        if (c.clientInFlight + f.data.length > WINDOW) return this.end('protocol');
        return this.write(c, f.data.slice());
      case 'shutdown':
        if (!c || c.state !== 'open' || c.shut) return;
        c.shut = true;
        c.writes = c.writes.then(async () => {
          if (c.closed) return;
          await c.writer.close();
          c.writeDone = true;
          this.maybeFinish(c);
        }).catch(() => this.drop(c, true, true));
        return;
      case 'close':
        if (c) this.drop(c, false, f.reset);
        return;
      case 'ack':
        if (!c) return;
        if (f.n > c.outInFlight) return this.end('protocol');
        c.outInFlight -= f.n;
        c.last = this.now();
        if (c.wake) c.wake();
        return;
      case 'dns':
        return this.dns(f.id, f.msg.slice());
      default:
        return this.end('protocol');
    }
  }

  async open(f) {
    const { id, port } = f;
    if (this.conns.size >= this.laneConns) return this.refuse(id, 'limit');
    if (!portAllowed(port)) return this.refuse(id, 'port');
    // Reserve the id at once: frames for it may arrive while we resolve.
    const c = { id, state: 'connecting', closed: false, counted: false, socket: null, clientInFlight: 0, outInFlight: 0, writes: Promise.resolve(), shut: false, writeDone: false, readDone: false, last: this.now(), wake: null };
    this.conns.set(id, c);
    const fail = (reason) => {
      if (c.closed) return;
      this.drop(c, false, false);
      this.refuse(id, reason);
    };
    let ip = f.ip;
    if (f.type === 'connectName') {
      let addrs;
      try {
        const d = await this.ledger.dns();
        if (!d.ok) return fail('limit');
        this.counters.dns++;
        addrs = await resolve4(this.resolver, f.name);
      } catch {
        return fail('resolve');
      }
      if (c.closed) return;
      if (!addrs.length) return fail('resolve');
      // Every address the name resolves to must be allowed.
      if (addrs.some((a) => addressBlocked(a) !== null)) return fail('address');
      ip = addrs[0];
    }
    const why = checkDestination(ip, port);
    if (why) return fail(why);
    const l = await this.ledger.openConn();
    if (c.closed) {
      if (l.ok) Promise.resolve(this.ledger.closeConn()).catch(() => {});
      return;
    }
    if (!l.ok) return fail('limit');
    c.counted = true;
    try {
      c.socket = this.connect({ hostname: ip, port });
      await withTimeout(c.socket.opened, this.connectTimeoutMs);
    } catch (e) {
      return fail(isPlatformError(e) ? 'platform' : 'unreachable');
    }
    if (c.closed) {
      try {
        c.socket.close();
      } catch {}
      return;
    }
    c.writer = c.socket.writable.getWriter();
    c.reader = c.socket.readable.getReader();
    c.state = 'open';
    c.last = this.now();
    this.counters.conns++;
    this.send(encode.connected(id));
    this.pump(c);
  }

  write(c, data) {
    c.clientInFlight += data.length;
    c.last = this.now();
    c.writes = c.writes.then(async () => {
      if (c.closed) return;
      if (!(await this.spend(data.length))) return;
      await c.writer.write(data);
      if (c.closed) return;
      c.clientInFlight -= data.length;
      this.counters.bytesToRemote += data.length;
      this.send(encode.ack(c.id, data.length, true));
    }).catch(() => this.drop(c, true, true));
  }

  /** Remote → guest, within the window the client acknowledges. */
  async pump(c) {
    const room = async () => {
      while (c.outInFlight >= WINDOW && !c.closed) await new Promise((ok) => (c.wake = ok));
      c.wake = null;
      return !c.closed;
    };
    try {
      for (;;) {
        if (!(await room())) return;
        const { value, done } = await c.reader.read();
        if (done) break;
        let chunk = value instanceof Uint8Array ? value : new Uint8Array(value);
        while (chunk.length) {
          if (!(await room())) return;
          const n = Math.min(chunk.length, MAX_DATA, WINDOW - c.outInFlight);
          if (!(await this.spend(n)) || c.closed) return;
          c.outInFlight += n;
          c.last = this.now();
          this.counters.bytesToGuest += n;
          this.send(encode.data(c.id, chunk.subarray(0, n), true));
          chunk = chunk.subarray(n);
        }
      }
      if (c.closed) return;
      c.readDone = true;
      this.send(encode.shutdown(c.id, true));
      this.maybeFinish(c);
    } catch {
      this.drop(c, true, true);
    }
  }

  /** Both directions finished: the socket can go (the client's CLOSE comes later). */
  maybeFinish(c) {
    if (c.readDone && c.writeDone) this.drop(c, false, false);
  }

  async dns(id, msg) {
    const fail = (rcode) => this.send(encode.dnsAnswer(id, failAnswer(msg, rcode)));
    const q = parseQuery(msg);
    if (!q) return fail(1); // FORMERR
    const d = await this.ledger.dns();
    if (!d.ok) return d.reason === 'dns' ? fail(5) : this.end(d.reason);
    if (d.waitMs > 0) await sleep(d.waitMs);
    if (this.closed) return;
    this.counters.dns++;
    let answer;
    try {
      answer = await this.resolver(msg);
    } catch {
      return fail(2); // SERVFAIL
    }
    if (!(answer instanceof Uint8Array) || answer.length < 12) return fail(2);
    // DoH may answer with id 0 (RFC 8484 4.1): the guest needs its own.
    answer = answer.slice();
    answer[0] = msg[0];
    answer[1] = msg[1];
    answer = fitUdp(msg, answer);
    if (!(await this.spend(answer.length))) return;
    this.send(encode.dnsAnswer(id, answer));
  }
}
