// The session ledger (ADR 0048): the per-session limits, shared by all the
// session's lanes. Pure state with an injected clock, so it can live in a
// Cloudflare Durable Object (relay/src/worker.mjs, persisted after every
// change) or in memory (relay/src/node.mjs), and be unit-tested.

export const DEFAULT_LIMITS = Object.freeze({
  /** Concurrent TCP connections in the session. */
  conns: 32,
  /** Concurrent WebSocket lanes. */
  lanes: 8,
  /** Bandwidth, both directions together (bytes/s), and the burst. */
  rate: 2 * 1024 * 1024,
  burst: 4 * 1024 * 1024,
  /** Total bytes of the session. */
  bytes: 1024 * 1024 * 1024,
  /** DNS queries per second, and the burst. */
  dnsRate: 20,
  dnsBurst: 100,
});

/** Limits from string variables (Cloudflare `env`, Node options), defaults otherwise. */
export function limitsFrom(vars = {}) {
  const out = { ...DEFAULT_LIMITS };
  for (const k of Object.keys(DEFAULT_LIMITS)) {
    const v = vars[`LIMIT_${k.replace(/[A-Z]/g, (c) => `_${c}`).toUpperCase()}`];
    if (v !== undefined && v !== '' && Number.isFinite(Number(v)) && Number(v) > 0) out[k] = Number(v);
  }
  return out;
}

export class Ledger {
  /**
   * `now()` in ms; `enabled()` is the kill switch, read on every request;
   * `state` a previous `state` to resume from (Durable Object storage).
   */
  constructor(limits = DEFAULT_LIMITS, { now = Date.now, enabled = () => true, state = null } = {}) {
    this.limits = { ...DEFAULT_LIMITS, ...limits };
    this.now = now;
    this.enabled = enabled;
    const t = now();
    this.state = state ?? {
      lanes: [],
      conns: 0,
      bytes: 0,
      tokens: this.limits.burst,
      dnsTokens: this.limits.dnsBurst,
      at: t,
      dnsAt: t,
      ended: null,
    };
  }

  #refill() {
    const s = this.state;
    const t = this.now();
    s.tokens = Math.min(this.limits.burst, s.tokens + ((t - s.at) / 1000) * this.limits.rate);
    s.at = t;
    s.dnsTokens = Math.min(this.limits.dnsBurst, s.dnsTokens + ((t - s.dnsAt) / 1000) * this.limits.dnsRate);
    s.dnsAt = t;
  }

  #refusal() {
    if (!this.enabled()) return 'disabled';
    return this.state.ended;
  }

  /** A lane `k` opens: { ok } or { ok: false, reason } (`lanes`, `lane-in-use`, ...). */
  openLane(k) {
    const r = this.#refusal();
    if (r) return { ok: false, reason: r };
    const s = this.state;
    if (s.lanes.includes(k)) return { ok: false, reason: 'lane-in-use' };
    if (s.lanes.length >= this.limits.lanes) return { ok: false, reason: 'lanes' };
    s.lanes.push(k);
    return { ok: true };
  }

  closeLane(k) {
    this.state.lanes = this.state.lanes.filter((x) => x !== k);
  }

  /** A connection opens: { ok } or { ok: false, reason }. */
  openConn() {
    const r = this.#refusal();
    if (r) return { ok: false, reason: r };
    if (this.state.conns >= this.limits.conns) return { ok: false, reason: 'conns' };
    this.state.conns++;
    return { ok: true };
  }

  closeConn() {
    this.state.conns = Math.max(0, this.state.conns - 1);
  }

  /**
   * `n` bytes are about to cross the gateway (either way): { ok, waitMs }
   * (wait that long before moving them: the bandwidth limit), or
   * { ok: false, reason } (`bytes`: the session is over its total and
   * ends; `disabled`).
   */
  grant(n) {
    const r = this.#refusal();
    if (r) return { ok: false, reason: r };
    const s = this.state;
    if (s.bytes + n > this.limits.bytes) {
      s.ended = 'bytes';
      return { ok: false, reason: 'bytes' };
    }
    this.#refill();
    s.bytes += n;
    s.tokens -= n;
    return { ok: true, waitMs: s.tokens >= 0 ? 0 : Math.ceil((-s.tokens / this.limits.rate) * 1000) };
  }

  /** A DNS query: { ok, waitMs } or { ok: false, reason }. */
  dns() {
    const r = this.#refusal();
    if (r) return { ok: false, reason: r };
    this.#refill();
    const s = this.state;
    // Far beyond the rate: refused rather than queued.
    if (s.dnsTokens <= -this.limits.dnsBurst) return { ok: false, reason: 'dns' };
    s.dnsTokens -= 1;
    return { ok: true, waitMs: s.dnsTokens >= 0 ? 0 : Math.ceil((-s.dnsTokens / this.limits.dnsRate) * 1000) };
  }

  /** Counters (no destinations, no contents). */
  counters() {
    const s = this.state;
    return { lanes: s.lanes.length, conns: s.conns, bytes: s.bytes, ended: s.ended };
  }
}
