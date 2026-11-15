// Session tokens of the network gateway (ADR 0048): short-lived, signed,
// bound to the Vetro site's origin. `payload.signature`, both base64url;
// the signature is HMAC-SHA-256 with the gateway's secret over the payload
// text. WebCrypto only (the same in Cloudflare Workers and Node).

/** Token lifetime (s): lanes can be opened while it is valid. */
export const TOKEN_TTL_S = 10 * 60;
/** A session can be renewed with the same id while younger than this (s). */
export const SESSION_MAX_S = 2 * 60 * 60;

const enc = new TextEncoder();
const dec = new TextDecoder();

export function b64url(bytes) {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function unb64url(s) {
  if (!/^[A-Za-z0-9_-]*$/.test(s)) return null;
  const pad = s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4);
  try {
    return Uint8Array.from(atob(pad), (c) => c.charCodeAt(0));
  } catch {
    return null;
  }
}

const keys = new Map();
async function key(secret) {
  if (typeof secret !== 'string' || secret.length < 32) throw new Error('the gateway secret must be at least 32 characters');
  let k = keys.get(secret);
  if (!k) {
    k = await crypto.subtle.importKey('raw', enc.encode(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
    keys.set(secret, k);
  }
  return k;
}

/** A random session id (128 bits, base64url). */
export function newSessionId() {
  return b64url(crypto.getRandomValues(new Uint8Array(16)));
}

/**
 * Signs a token for `origin`. `first` = when the session started (s), kept
 * across renewals; `now` in seconds.
 */
export async function issue(secret, { sid, origin, now, first = now, ttl = TOKEN_TTL_S }) {
  const payload = b64url(enc.encode(JSON.stringify({ v: 1, sid, origin, iat: now, exp: now + ttl, first })));
  const sig = new Uint8Array(await crypto.subtle.sign('HMAC', await key(secret), enc.encode(payload)));
  return `${payload}.${b64url(sig)}`;
}

/**
 * Verifies a token: its payload, or { error } (`malformed`, `signature`,
 * `expired`, `origin`). `origin` = the request's Origin, which must be the
 * token's. With `allowExpired` (renewal) an expired token is accepted.
 */
export async function verify(secret, token, { origin, now, allowExpired = false }) {
  if (typeof token !== 'string' || token.length > 1024) return { error: 'malformed' };
  const parts = token.split('.');
  if (parts.length !== 2) return { error: 'malformed' };
  const sig = unb64url(parts[1]);
  const body = unb64url(parts[0]);
  if (!sig || !body) return { error: 'malformed' };
  const ok = await crypto.subtle.verify('HMAC', await key(secret), sig, enc.encode(parts[0]));
  if (!ok) return { error: 'signature' };
  let p;
  try {
    p = JSON.parse(dec.decode(body));
  } catch {
    return { error: 'malformed' };
  }
  if (p.v !== 1 || typeof p.sid !== 'string' || typeof p.exp !== 'number' || typeof p.first !== 'number') return { error: 'malformed' };
  if (p.origin !== origin) return { error: 'origin' };
  if (!allowExpired && now >= p.exp) return { error: 'expired' };
  return p;
}
