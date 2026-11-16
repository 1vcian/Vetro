// The page's half of the network gateway (ADR 0048, docs/specs/relay.md):
// carries the frames of the machine's uplink (Machine.relayTake /
// Machine.relayPush) over WebSocket lanes to the gateway (relay/), which
// opens the real connections.
//
// - A session token comes from POST /session (renewed before it expires,
//   same session); each lane is a WebSocket to /lane?token=…&lane=k.
// - A guest connection lives on one lane: the first lane with room
//   (laneConns, from the gateway), or a new lane up to the session's limit.
//   Ids on a lane are the machine's own (unique for the machine's life;
//   after a restore or a replay `reset()` closes every lane).
// - When a lane closes, its connections are reset for the guest (REFUSED
//   `down` while connecting, CLOSE with reset once open), frames the client
//   writes itself and hands to the machine like the gateway's. If the
//   gateway cannot be reached, new connections are refused at once and a
//   new attempt waits (1 s, doubling, at most 60 s).
//
// Everything that reaches the machine goes through `take()` and
// Machine.relayPush: a recorded input (ADR 0019). Platform: browser Workers
// and Node 22 (global fetch and WebSocket).

/**
 * The gateway the app uses by default (no user choice, ADR 0048), or null
 * while none is deployed: then the app stays offline (the sinkhole) unless
 * `?gateway=URL` is given.
 */
export const DEFAULT_GATEWAY = null;

export const VERSION = 1;
export const WINDOW = 256 * 1024;
const OP = { CONNECT: 0x01, CONNECT_NAME: 0x02, DATA: 0x03, SHUTDOWN: 0x04, CLOSE: 0x05, ACK: 0x06, DNS: 0x07, CONNECTED: 0x81, REFUSED: 0x82, CLOSE_IN: 0x85, ONLINE: 0xf0, DOWN: 0xf1 };
const REFUSED_LIMIT = 4;
const REFUSED_DOWN = 7;
/** Renew the token this long before it expires. */
const RENEW_MS = 60_000;
const PING_MS = 25_000;
/** A lane without connections closes after this long (lane 0 stays, for DNS). */
const LANE_IDLE_MS = 120_000;
const BACKOFF_MIN_MS = 1000;
const BACKOFF_MAX_MS = 60_000;

const frame = (op, id, payload = []) => {
  const b = new Uint8Array(5 + payload.length);
  b[0] = op;
  new DataView(b.buffer).setUint32(1, id >>> 0);
  b.set(payload, 5);
  return b;
};
const idOf = (f) => new DataView(f.buffer, f.byteOffset, f.byteLength).getUint32(1);

/** Host-local frames for the machine (never sent to the gateway). */
export const online = (on) => frame(OP.ONLINE, 0, [on ? 1 : 0]);
export const down = () => frame(OP.DOWN, 0);
const refused = (id, reason) => frame(OP.REFUSED, id, [reason]);
const resetFrame = (id) => frame(OP.CLOSE_IN, id, [1]);

export class GatewayClient {
  #url;
  #WS;
  #fetch;
  #now;
  #onData;
  #lanes = [];
  #conns = new Map();
  #toMachine = [];
  #session = null;
  #sessionPending = null;
  #retryAt = 0;
  #backoff = BACKOFF_MIN_MS;
  #timer = null;
  #maxLanes = 8;
  #laneConns = 5;
  #closed = false;
  stats = { lanesOpened: 0, laneFailures: 0, connects: 0, refusedLocally: 0, framesOut: 0, framesIn: 0, bytesOut: 0, bytesIn: 0, lastError: null, lastBye: null };

  #origin;

  /**
   * `url`: the gateway (https://… or http://… for a local one). `onData` is
   * called when frames for the machine arrive (to wake the Worker's loop).
   * `origin` only outside a browser (Node tests), where nothing sends the
   * Origin header by itself.
   */
  constructor(url, { WebSocketImpl = globalThis.WebSocket, fetchImpl = globalThis.fetch, now = () => Date.now(), onData = () => {}, origin = null } = {}) {
    this.#origin = origin;
    this.#url = String(url).replace(/\/+$/, '');
    this.#WS = WebSocketImpl;
    // Called as a plain function: a browser's `fetch` invoked as a method of
    // this object throws "Illegal invocation" (Node's does not mind).
    this.#fetch = (...args) => fetchImpl(...args);
    this.#now = now;
    this.#onData = onData;
    this.#timer = setInterval(() => this.#tick(), PING_MS);
  }

  /** online, connecting, unreachable or idle; lanes and connections. */
  status() {
    const open = this.#lanes.filter((l) => l.state === 'open').length;
    const state = open ? 'online' : this.#lanes.length ? 'connecting' : this.#now() < this.#retryAt ? 'unreachable' : 'idle';
    return { state, url: this.#url, lanes: this.#lanes.length, open, conns: this.#conns.size, ...this.stats };
  }

  /** Frames from the machine (Machine.relayTake), in order. */
  fromMachine(frames) {
    for (const f of frames) this.#route(f);
  }

  /** Frames for the machine (to Machine.relayPush), in arrival order. */
  take() {
    const out = this.#toMachine;
    this.#toMachine = [];
    return out;
  }

  /**
   * Forgets everything: lanes closed, no frames for the machine. After a
   * snapshot restore or a replay (the machine then gets `down()`).
   */
  reset() {
    for (const l of [...this.#lanes]) this.#closeLane(l, false);
    this.#conns.clear();
    this.#toMachine = [];
  }

  close() {
    this.#closed = true;
    this.reset();
    clearInterval(this.#timer);
  }

  #push(f) {
    this.#toMachine.push(f);
    this.#onData();
  }

  #route(f) {
    if (this.#closed || f.length < 5) return;
    const id = idOf(f);
    switch (f[0]) {
      case OP.CONNECT:
      case OP.CONNECT_NAME: {
        this.stats.connects++;
        const lane = this.#now() < this.#retryAt ? null : this.#laneWithRoom();
        if (!lane) {
          this.stats.refusedLocally++;
          this.#push(refused(id, this.#now() < this.#retryAt ? REFUSED_DOWN : REFUSED_LIMIT));
          return;
        }
        this.#conns.set(id, { lane, open: false });
        lane.conns.add(id);
        lane.idleSince = null;
        this.#send(lane, f);
        return;
      }
      case OP.DNS: {
        if (this.#now() < this.#retryAt) return; // the guest retries
        const lane = this.#lanes.find((l) => l.state === 'open') ?? this.#lanes[0] ?? this.#openLane();
        if (lane) this.#send(lane, f);
        return;
      }
      case OP.CLOSE: {
        const c = this.#conns.get(id);
        if (!c) return;
        this.#forget(id);
        this.#send(c.lane, f);
        return;
      }
      default: {
        const c = this.#conns.get(id);
        if (c) this.#send(c.lane, f);
      }
    }
  }

  #forget(id) {
    const c = this.#conns.get(id);
    if (!c) return;
    this.#conns.delete(id);
    c.lane.conns.delete(id);
    if (!c.lane.conns.size) c.lane.idleSince = this.#now();
  }

  #laneWithRoom() {
    const room = (l) => l.state !== 'closed' && l.conns.size < (l.laneConns ?? this.#laneConns);
    return this.#lanes.find(room) ?? (this.#lanes.length < this.#maxLanes ? this.#openLane() : null);
  }

  #send(lane, f) {
    if (lane.state === 'open') {
      this.stats.framesOut++;
      this.stats.bytesOut += f.length;
      lane.ws.send(f);
    } else if (lane.state === 'opening') {
      lane.queue.push(f);
    }
  }

  async #ensureSession() {
    const s = this.#session;
    if (s && s.expires - this.#now() > RENEW_MS) return s;
    if (!this.#sessionPending) {
      this.#sessionPending = (async () => {
        const r = await this.#fetch(`${this.#url}/session`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', ...(this.#origin ? { origin: this.#origin } : {}) },
          body: JSON.stringify(s ? { renew: s.token } : {}),
        });
        if (!r.ok) throw new Error(`gateway session: HTTP ${r.status}`);
        const j = await r.json();
        this.#session = j;
        if (j.lanes > 0) this.#maxLanes = j.lanes;
        if (j.laneConns > 0) this.#laneConns = j.laneConns;
        return j;
      })().finally(() => (this.#sessionPending = null));
    }
    return this.#sessionPending;
  }

  #openLane() {
    const used = new Set(this.#lanes.map((l) => l.k));
    let k = 0;
    while (used.has(k)) k++;
    const lane = { k, state: 'opening', ws: null, conns: new Set(), queue: [], laneConns: null, idleSince: null, hello: null };
    this.#lanes.push(lane);
    this.#connectLane(lane).catch((e) => this.#laneFailed(lane, e));
    return lane;
  }

  async #connectLane(lane) {
    const s = await this.#ensureSession();
    if (lane.state === 'closed') return;
    const url = `${this.#url.replace(/^http/, 'ws')}/lane?token=${encodeURIComponent(s.token)}&lane=${lane.k}`;
    // (Node's WebSocket takes headers in its second argument; browsers set Origin.)
    const ws = this.#origin ? new this.#WS(url, { headers: { origin: this.#origin } }) : new this.#WS(url);
    ws.binaryType = 'arraybuffer';
    lane.ws = ws;
    ws.onmessage = (e) => this.#onMessage(lane, e.data);
    // Browsers fire `error` then `close`; Node's WebSocket only `error` for a
    // refused upgrade. Either one ends the lane (the second is a no-op).
    const gone = () => (lane.hello ? this.#closeLane(lane, true) : this.#laneFailed(lane, new Error('lane refused')));
    ws.onclose = gone;
    ws.onerror = gone;
  }

  #onMessage(lane, data) {
    if (lane.state === 'closed') return;
    if (typeof data === 'string') {
      let m;
      try {
        m = JSON.parse(data);
      } catch {
        return;
      }
      if (m.type === 'hello') {
        if (m.v !== VERSION || m.window !== WINDOW) {
          this.stats.lastError = `gateway protocol ${m.v} window ${m.window}: not this one`;
          return this.#laneFailed(lane, new Error(this.stats.lastError));
        }
        lane.hello = m;
        lane.laneConns = m.laneConns;
        lane.state = 'open';
        this.stats.lanesOpened++;
        this.#backoff = BACKOFF_MIN_MS;
        for (const f of lane.queue.splice(0)) this.#send(lane, f);
      } else if (m.type === 'bye') {
        this.stats.lastBye = m.reason;
      }
      return;
    }
    const f = new Uint8Array(data);
    if (f.length < 5) return;
    this.stats.framesIn++;
    this.stats.bytesIn += f.length;
    const id = idOf(f);
    if (f[0] === OP.CONNECTED) {
      const c = this.#conns.get(id);
      if (c) c.open = true;
    } else if (f[0] === OP.REFUSED || f[0] === OP.CLOSE_IN) {
      this.#forget(id);
    }
    this.#push(f);
  }

  #laneFailed(lane, e) {
    if (lane.state === 'closed') return;
    this.stats.laneFailures++;
    this.stats.lastError = String(e?.message ?? e);
    // A refused lane may mean a token the gateway no longer accepts (a
    // gateway on the user's computer makes a new secret at every start, and
    // the token would otherwise be tried again until it expires, minutes
    // later): the next attempt renews the session. The same gateway keeps
    // the session and its limits in a renewal; another one gives a new one.
    if (this.#session) this.#session.expires = 0;
    this.#retryAt = this.#now() + this.#backoff;
    this.#backoff = Math.min(BACKOFF_MAX_MS, this.#backoff * 2);
    this.#closeLane(lane, true);
  }

  /** Closes a lane; `notify` = its connections are reset for the guest. */
  #closeLane(lane, notify) {
    if (lane.state === 'closed') return;
    lane.state = 'closed';
    this.#lanes = this.#lanes.filter((l) => l !== lane);
    try {
      lane.ws?.close();
    } catch {}
    for (const id of lane.conns) {
      const c = this.#conns.get(id);
      this.#conns.delete(id);
      if (notify && c) this.#push(c.open ? resetFrame(id) : refused(id, REFUSED_DOWN));
    }
    // (Connections still queued on a lane that never opened are among them.)
    lane.conns.clear();
    lane.queue = [];
  }

  #tick() {
    const t = this.#now();
    for (const l of [...this.#lanes]) {
      if (l.state !== 'open') continue;
      if (l.k !== 0 && !l.conns.size && l.idleSince !== null && t - l.idleSince > LANE_IDLE_MS) {
        this.#closeLane(l, false);
        continue;
      }
      try {
        l.ws.send(JSON.stringify({ type: 'ping' }));
      } catch {}
    }
  }
}
