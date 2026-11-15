// The network gateway on Cloudflare Workers (ADR 0048). Outbound TCP with the
// platform's sockets API (`cloudflare:sockets`); the session ledger in a
// Durable Object per session, so the limits hold across the session's lanes
// (each lane is its own invocation, with its own six-connection limit).
//
// Variables (wrangler.toml / secrets): GATEWAY_ENABLED ("0" = kill switch),
// ALLOWED_ORIGINS (comma separated), TOKEN_SECRET (secret), LANE_CONNS,
// LIMIT_* (relay/src/ledger.mjs), DOH_URL.

import { connect } from 'cloudflare:sockets';
import { DurableObject } from 'cloudflare:workers';
import { checkLane, configFrom, handleHttp } from './gateway.mjs';
import { Lane } from './lane.mjs';
import { Ledger, limitsFrom } from './ledger.mjs';
import { DEFAULT_DOH, dohResolver } from './dns.mjs';

/** Cloudflare: at most six simultaneous connections per invocation, one kept for DNS. */
const CF_LANE_CONNS = 5;
/** Ledger state is dropped this long after the last activity. */
const LEDGER_KEEP_MS = 3 * 60 * 60 * 1000;

export class SessionLedger extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.env = env;
    const state = ctx.storage.kv.get('state') ?? null;
    this.ledger = new Ledger(limitsFrom(env), { enabled: () => configFrom(this.env).enabled, state });
  }

  #save(r) {
    this.ctx.storage.kv.put('state', this.ledger.state);
    this.ctx.storage.setAlarm(Date.now() + LEDGER_KEEP_MS);
    return r;
  }

  openLane(k) {
    return this.#save(this.ledger.openLane(k));
  }
  closeLane(k) {
    return this.#save(this.ledger.closeLane(k));
  }
  openConn() {
    return this.#save(this.ledger.openConn());
  }
  closeConn() {
    return this.#save(this.ledger.closeConn());
  }
  grant(n) {
    return this.#save(this.ledger.grant(n));
  }
  dns() {
    return this.#save(this.ledger.dns());
  }

  async alarm() {
    await this.ctx.storage.deleteAll();
  }
}

export default {
  async fetch(request, env) {
    const cfg = { ...configFrom(env), laneConns: CF_LANE_CONNS };
    const lanes = limitsFrom(env).lanes;
    const url = new URL(request.url);
    if (url.pathname !== '/lane') return handleHttp(request, cfg, { lanes });
    const lane = await checkLane(request, cfg, { lanes });
    if (lane.error) return new Response(lane.error, { status: lane.status });
    const ledger = env.SESSIONS.get(env.SESSIONS.idFromName(lane.sid));
    const open = await ledger.openLane(lane.lane);
    if (!open.ok) return new Response(open.reason, { status: open.reason === 'disabled' ? 503 : open.reason === 'lane-in-use' ? 409 : 429 });

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    server.accept();
    const l = new Lane({
      send: (d) => server.send(d),
      close: (code, reason) => server.close(code, reason),
      connect: ({ hostname, port }) => connect({ hostname, port }, { allowHalfOpen: true, secureTransport: 'off' }),
      resolver: dohResolver(env.DOH_URL || DEFAULT_DOH),
      ledger,
      lane: lane.lane,
      laneConns: CF_LANE_CONNS,
      limits: limitsFrom(env),
      platform: 'cloudflare',
    });
    server.addEventListener('message', (e) => l.onMessage(e.data));
    server.addEventListener('close', () => l.onClose());
    server.addEventListener('error', () => l.onClose());
    l.start();
    return new Response(null, { status: 101, webSocket: client });
  },
};
