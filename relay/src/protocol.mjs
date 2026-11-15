// The network gateway's protocol (ADR 0048, docs/specs/relay.md): binary
// frames over WebSocket, one frame per message: op (1 byte), id (u32 big
// endian), payload. The same encoding as `vetro_net::relay` in Rust, checked
// against the same vectors (relay/test/vectors.json). Text messages carry
// JSON control messages (hello, bye, ping, pong).
//
// Platform-free: used by the Cloudflare Worker, the Node gateway and the
// page's Worker (web/node/relay.mjs).

export const VERSION = 1;
/** Per-connection, per-direction flow control window (bytes). */
export const WINDOW = 256 * 1024;
/** Largest DATA payload. */
export const MAX_DATA = 16 * 1024;
/** Largest DNS message in a DNS frame. */
export const MAX_DNS = 4096;
export const MAX_NAME = 253;
export const HEADER = 5;

export const OP = Object.freeze({
  CONNECT: 0x01,
  CONNECT_NAME: 0x02,
  DATA: 0x03,
  SHUTDOWN: 0x04,
  CLOSE: 0x05,
  ACK: 0x06,
  DNS: 0x07,
  CONNECTED: 0x81,
  REFUSED: 0x82,
  DATA_IN: 0x83,
  SHUTDOWN_IN: 0x84,
  CLOSE_IN: 0x85,
  ACK_IN: 0x86,
  DNS_ANSWER: 0x87,
  // Host-local (the page's Worker to the machine), never on a lane.
  ONLINE: 0xf0,
  DOWN: 0xf1,
});

/** REFUSED reasons. */
export const REFUSAL = Object.freeze({
  unreachable: 1,
  port: 2,
  address: 3,
  limit: 4,
  platform: 5,
  resolve: 6,
  down: 7,
});
export const REFUSAL_NAME = Object.freeze(Object.fromEntries(Object.entries(REFUSAL).map(([k, v]) => [v, k])));

const NAME_RE = /^(?=.{1,253}$)([a-z0-9-]{1,63})(\.[a-z0-9-]{1,63})*$/;

function frame(op, id, payload = new Uint8Array()) {
  const out = new Uint8Array(HEADER + payload.length);
  out[0] = op;
  new DataView(out.buffer).setUint32(1, id >>> 0);
  out.set(payload, HEADER);
  return out;
}

const u32 = (n) => {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n >>> 0);
  return b;
};

/** Encoders, by frame kind. `id` is a u32. */
export const encode = {
  connect: (id, ip, port) => {
    const p = new Uint8Array(6);
    p.set(ip.split('.').map(Number));
    new DataView(p.buffer).setUint16(4, port);
    return frame(OP.CONNECT, id, p);
  },
  connectName: (id, name, port) => {
    const n = new TextEncoder().encode(name);
    const p = new Uint8Array(2 + n.length);
    new DataView(p.buffer).setUint16(0, port);
    p.set(n, 2);
    return frame(OP.CONNECT_NAME, id, p);
  },
  /** `fromGateway`: DATA, SHUTDOWN, CLOSE and ACK have one code per direction. */
  data: (id, bytes, fromGateway) => frame(fromGateway ? OP.DATA_IN : OP.DATA, id, bytes),
  shutdown: (id, fromGateway) => frame(fromGateway ? OP.SHUTDOWN_IN : OP.SHUTDOWN, id),
  close: (id, reset, fromGateway) => frame(fromGateway ? OP.CLOSE_IN : OP.CLOSE, id, Uint8Array.of(reset ? 1 : 0)),
  ack: (id, n, fromGateway) => frame(fromGateway ? OP.ACK_IN : OP.ACK, id, u32(n)),
  dns: (id, msg) => frame(OP.DNS, id, msg),
  dnsAnswer: (id, msg) => frame(OP.DNS_ANSWER, id, msg),
  connected: (id) => frame(OP.CONNECTED, id),
  refused: (id, reason) => frame(OP.REFUSED, id, Uint8Array.of(typeof reason === 'string' ? REFUSAL[reason] : reason)),
  online: (on) => frame(OP.ONLINE, 0, Uint8Array.of(on ? 1 : 0)),
  down: () => frame(OP.DOWN, 0),
};

/**
 * Decodes a frame: { type, id, fromGateway, ... } or null if malformed (the
 * same rules as the Rust decoder). Types: connect {ip, port}, connectName
 * {name, port}, data {data}, shutdown, close {reset}, ack {n}, dns {msg},
 * dnsAnswer {msg}, connected, refused {reason (name)}, online {on}, down.
 */
export function decode(bytes) {
  const b = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (b.length < HEADER) return null;
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const op = b[0];
  const id = dv.getUint32(1);
  const p = b.subarray(HEADER);
  const fromGateway = (op & 0x80) !== 0;
  const flag = () => (p.length === 1 && p[0] <= 1 ? p[0] === 1 : null);
  const base = { id, fromGateway };
  switch (op) {
    case OP.CONNECT:
      if (p.length !== 6) return null;
      return { type: 'connect', ...base, ip: `${p[0]}.${p[1]}.${p[2]}.${p[3]}`, port: dv.getUint16(HEADER + 4) };
    case OP.CONNECT_NAME: {
      if (p.length < 3) return null;
      const name = String.fromCharCode(...p.subarray(2));
      if (!NAME_RE.test(name)) return null;
      return { type: 'connectName', ...base, name, port: dv.getUint16(HEADER) };
    }
    case OP.DATA:
    case OP.DATA_IN:
      if (p.length === 0 || p.length > MAX_DATA) return null;
      return { type: 'data', ...base, data: p };
    case OP.SHUTDOWN:
    case OP.SHUTDOWN_IN:
      return p.length === 0 ? { type: 'shutdown', ...base } : null;
    case OP.CLOSE:
    case OP.CLOSE_IN: {
      const reset = flag();
      return reset === null ? null : { type: 'close', ...base, reset };
    }
    case OP.ACK:
    case OP.ACK_IN:
      return p.length === 4 ? { type: 'ack', ...base, n: dv.getUint32(HEADER) } : null;
    case OP.DNS:
    case OP.DNS_ANSWER:
      if (p.length < 12 || p.length > MAX_DNS) return null;
      return { type: op === OP.DNS ? 'dns' : 'dnsAnswer', ...base, msg: p };
    case OP.CONNECTED:
      return p.length === 0 ? { type: 'connected', ...base } : null;
    case OP.REFUSED:
      return p.length === 1 && REFUSAL_NAME[p[0]] ? { type: 'refused', ...base, reason: REFUSAL_NAME[p[0]] } : null;
    case OP.ONLINE: {
      const on = flag();
      return id === 0 && on !== null ? { type: 'online', ...base, on } : null;
    }
    case OP.DOWN:
      return id === 0 && p.length === 0 ? { type: 'down', ...base } : null;
    default:
      return null;
  }
}

/** The frame's id (bytes 1..4), without decoding the rest. */
export const frameId = (b) => new DataView(b.buffer, b.byteOffset, b.byteLength).getUint32(1);

/** A copy of the frame with another id (the page's Worker maps machine ids to lane ids). */
export function withId(b, id) {
  const out = new Uint8Array(b);
  new DataView(out.buffer).setUint32(1, id >>> 0);
  return out;
}
