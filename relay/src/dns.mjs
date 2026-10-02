// DNS for the network gateway (ADR 0046): the guest's raw queries are
// checked and answered with DNS over HTTPS (RFC 8484); names in CONNECT_NAME
// are resolved to A records the same way. Platform-free (fetch only).

export const DEFAULT_DOH = 'https://cloudflare-dns.com/dns-query';
const TYPE_A = 1;
const CLASS_IN = 1;
/** Largest answer the guest can take over UDP without fragments (MTU 1500). */
export const MAX_UDP_ANSWER = 1472;

/**
 * Checks a guest query: a standard query (QR 0, opcode 0) with exactly one
 * question, at most 512 bytes plus EDNS. Returns { id, name, qtype, end }
 * (end = offset after the question) or null.
 */
export function parseQuery(q) {
  if (!(q instanceof Uint8Array) || q.length < 12 || q.length > 1024) return null;
  const flags = (q[2] << 8) | q[3];
  if (flags & 0x8000 || (flags >> 11) & 0xf) return null;
  const qd = (q[4] << 8) | q[5];
  const an = (q[6] << 8) | q[7];
  const ns = (q[8] << 8) | q[9];
  if (qd !== 1 || an !== 0 || ns !== 0) return null;
  const labels = [];
  let at = 12;
  for (;;) {
    if (at >= q.length) return null;
    const len = q[at++];
    if (len === 0) break;
    if (len > 63 || at + len > q.length) return null;
    labels.push(String.fromCharCode(...q.subarray(at, at + len)));
    at += len;
  }
  if (at + 4 > q.length) return null;
  const qtype = (q[at] << 8) | q[at + 1];
  return { id: (q[0] << 8) | q[1], name: labels.join('.').toLowerCase(), qtype, end: at + 4 };
}

/** A query for `name` (type A, recursion desired). */
export function buildQuery(name, id = 0, qtype = TYPE_A) {
  const parts = [];
  for (const l of name.split('.')) {
    if (!l) continue;
    parts.push(l.length, ...[...l].map((c) => c.charCodeAt(0)));
  }
  return Uint8Array.from([id >> 8, id & 0xff, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, ...parts, 0, qtype >> 8, qtype & 0xff, 0, CLASS_IN]);
}

/** A failure answer (SERVFAIL by default) to the query `q` (checked). */
export function failAnswer(q, rcode = 2) {
  const p = parseQuery(q);
  const out = new Uint8Array(q.subarray(0, p ? p.end : 12));
  out[2] = 0x80 | (q[2] & 0x01); // QR, RD copied
  out[3] = 0x80 | rcode; // RA
  out.fill(0, 6, 12); // no answers, no authority, no additional
  if (!p) out[5] = 0;
  return out;
}

/** Truncated answer (TC) if it would not fit the guest's UDP. */
export function fitUdp(q, answer) {
  if (answer.length <= MAX_UDP_ANSWER) return answer;
  const t = failAnswer(q, answer[3] & 0x0f);
  t[2] |= 0x02; // TC
  return t;
}

function skipName(m, at) {
  for (let guard = 0; guard < 128; guard++) {
    if (at >= m.length) return -1;
    const len = m[at];
    if (len === 0) return at + 1;
    if ((len & 0xc0) === 0xc0) return at + 2;
    at += 1 + len;
  }
  return -1;
}

/** The A records of an answer (dotted strings), [] if none or malformed. */
export function answerAddrs(m) {
  if (!(m instanceof Uint8Array) || m.length < 12) return [];
  if ((m[3] & 0x0f) !== 0) return [];
  const qd = (m[4] << 8) | m[5];
  const an = (m[6] << 8) | m[7];
  let at = 12;
  for (let i = 0; i < qd; i++) {
    at = skipName(m, at);
    if (at < 0) return [];
    at += 4;
  }
  const out = [];
  for (let i = 0; i < an; i++) {
    at = skipName(m, at);
    if (at < 0 || at + 10 > m.length) return out;
    const type = (m[at] << 8) | m[at + 1];
    const cls = (m[at + 2] << 8) | m[at + 3];
    const rdlen = (m[at + 8] << 8) | m[at + 9];
    at += 10;
    if (at + rdlen > m.length) return out;
    if (type === TYPE_A && cls === CLASS_IN && rdlen === 4) out.push(`${m[at]}.${m[at + 1]}.${m[at + 2]}.${m[at + 3]}`);
    at += rdlen;
  }
  return out;
}

/** A DoH resolver: async (query bytes) => answer bytes. Throws on failure. */
export function dohResolver(url = DEFAULT_DOH, fetchImpl = globalThis.fetch, timeoutMs = 5000) {
  return async (query) => {
    const r = await fetchImpl(url, {
      method: 'POST',
      headers: { 'content-type': 'application/dns-message', accept: 'application/dns-message' },
      body: query,
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!r.ok) throw new Error(`DoH ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  };
}

/** Resolves `name` to A records with a raw resolver (the DoH one). */
export async function resolve4(resolver, name) {
  return answerAddrs(await resolver(buildQuery(name)));
}
