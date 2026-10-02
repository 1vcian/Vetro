// Destination policy of the network gateway (ADR 0046): only TCP 80 and
// 443, only public IPv4 unicast addresses. Checked on the address the
// gateway is about to connect to, after any name resolution.

export const ALLOWED_PORTS = Object.freeze([80, 443]);

/** Rejected IPv4 ranges: [first octets, prefix length, why]. */
export const BLOCKED = Object.freeze([
  ['0.0.0.0', 8, 'this network'],
  ['10.0.0.0', 8, 'private'],
  ['100.64.0.0', 10, 'shared address space (CGNAT, some cloud metadata)'],
  ['127.0.0.0', 8, 'loopback'],
  ['169.254.0.0', 16, 'link-local (cloud metadata 169.254.169.254)'],
  ['172.16.0.0', 12, 'private'],
  ['192.0.0.0', 24, 'IETF protocol assignments'],
  ['192.0.2.0', 24, 'documentation'],
  ['192.88.99.0', 24, '6to4 relay anycast'],
  ['192.168.0.0', 16, 'private'],
  ['198.18.0.0', 15, 'benchmarking (the sinkhole\'s fake addresses)'],
  ['198.51.100.0', 24, 'documentation'],
  ['203.0.113.0', 24, 'documentation'],
  ['224.0.0.0', 4, 'multicast'],
  ['240.0.0.0', 4, 'reserved and broadcast'],
]);

/** Dotted IPv4 to u32, or null if it is not exactly four decimal octets. */
export function parseIPv4(s) {
  if (typeof s !== 'string') return null;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(s);
  if (!m) return null;
  let v = 0;
  for (let i = 1; i <= 4; i++) {
    // No leading zeros: "010" is octal for some parsers.
    if (m[i].length > 1 && m[i][0] === '0') return null;
    const o = Number(m[i]);
    if (o > 255) return null;
    v = v * 256 + o;
  }
  return v;
}

const RANGES = BLOCKED.map(([base, len, why]) => {
  const mask = len === 0 ? 0 : (0xffffffff << (32 - len)) >>> 0;
  return { net: (parseIPv4(base) & mask) >>> 0, mask, why };
});

/** Why the address is not allowed, or null if it is a public unicast address. */
export function addressBlocked(ip) {
  const v = parseIPv4(ip);
  if (v === null) return 'not an IPv4 address';
  for (const r of RANGES) if (((v & r.mask) >>> 0) === r.net) return r.why;
  return null;
}

export const portAllowed = (port) => ALLOWED_PORTS.includes(port);

/** The REFUSED reason for a destination, or null if it may be connected to. */
export function checkDestination(ip, port) {
  if (!portAllowed(port)) return 'port';
  if (addressBlocked(ip) !== null) return 'address';
  return null;
}
