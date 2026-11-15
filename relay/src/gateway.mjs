// The network gateway's HTTP side (ADR 0048, docs/specs/relay.md), shared by
// the Cloudflare Worker and the Node gateway: Fetch API Request/Response.
//
//   POST /session      a token for an allowed Origin (renewal with { renew })
//   GET  /lane?token=…&lane=k   WebSocket upgrade (checked here, accepted by
//                      the platform)
//   GET  /health       { ok, enabled }

import { issue, newSessionId, SESSION_MAX_S, TOKEN_TTL_S, verify } from './token.mjs';

/** Configuration from string variables (Cloudflare `env`, Node options). */
export function configFrom(vars) {
  return {
    enabled: String(vars.GATEWAY_ENABLED ?? '1') !== '0',
    secret: vars.TOKEN_SECRET,
    origins: String(vars.ALLOWED_ORIGINS ?? '').split(',').map((s) => s.trim()).filter(Boolean),
    laneConns: Number(vars.LANE_CONNS) > 0 ? Number(vars.LANE_CONNS) : 32,
  };
}

const json = (body, status, origin) =>
  new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
      'cache-control': 'no-store',
      ...(origin ? { 'access-control-allow-origin': origin, vary: 'Origin' } : {}),
    },
  });

const seconds = (now) => Math.floor(now / 1000);

/**
 * Handles a request that is not a lane: a Response. `now` in ms; `lanes` =
 * the session's lane limit (told to the client).
 */
export async function handleHttp(request, cfg, { now = Date.now(), lanes = 8 } = {}) {
  const url = new URL(request.url);
  const origin = request.headers.get('origin');
  const allowed = origin !== null && cfg.origins.includes(origin);
  if (url.pathname === '/health') return json({ ok: true, enabled: cfg.enabled }, 200, allowed ? origin : null);
  if (url.pathname !== '/session') return json({ error: 'not-found' }, 404, null);
  if (request.method === 'OPTIONS') {
    if (!allowed) return new Response(null, { status: 403 });
    return new Response(null, {
      status: 204,
      headers: {
        'access-control-allow-origin': origin,
        'access-control-allow-methods': 'POST',
        'access-control-allow-headers': 'content-type',
        'access-control-max-age': '600',
        vary: 'Origin',
      },
    });
  }
  if (request.method !== 'POST') return json({ error: 'method' }, 405, null);
  if (!allowed) return json({ error: 'origin' }, 403, null);
  if (!cfg.enabled) return json({ error: 'disabled' }, 503, origin);
  let body = {};
  try {
    const text = await request.text();
    if (text.length > 2048) return json({ error: 'malformed' }, 400, origin);
    if (text) body = JSON.parse(text);
  } catch {
    return json({ error: 'malformed' }, 400, origin);
  }
  const t = seconds(now);
  let sid = newSessionId();
  let first = t;
  if (body?.renew) {
    const p = await verify(cfg.secret, body.renew, { origin, now: t, allowExpired: true });
    // A renewal keeps the session (and its limits) while it is young enough.
    if (!p.error && t - p.first < SESSION_MAX_S) {
      sid = p.sid;
      first = p.first;
    }
  }
  const token = await issue(cfg.secret, { sid, origin, now: t, first });
  return json({ token, expires: (t + TOKEN_TTL_S) * 1000, lanes, laneConns: cfg.laneConns }, 200, origin);
}

/**
 * Checks a lane upgrade: { sid, lane } or { status, error } for the refusal.
 * The platform accepts the WebSocket only after the ledger's `openLane`.
 */
export async function checkLane(request, cfg, { now = Date.now(), lanes = 8 } = {}) {
  const url = new URL(request.url);
  if (url.pathname !== '/lane') return { status: 404, error: 'not-found' };
  if ((request.headers.get('upgrade') ?? '').toLowerCase() !== 'websocket') return { status: 426, error: 'upgrade' };
  const origin = request.headers.get('origin');
  if (origin === null || !cfg.origins.includes(origin)) return { status: 403, error: 'origin' };
  if (!cfg.enabled) return { status: 503, error: 'disabled' };
  const lane = Number(url.searchParams.get('lane'));
  if (!Number.isInteger(lane) || lane < 0 || lane >= lanes) return { status: 400, error: 'lane' };
  const p = await verify(cfg.secret, url.searchParams.get('token'), { origin, now: seconds(now) });
  if (p.error) return { status: 401, error: p.error };
  return { sid: p.sid, lane };
}
