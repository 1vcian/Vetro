"""Destination policy of the network gateway (ADR 0048, ADR 0050).

Only TCP 80 and 443, only public IPv4 unicast addresses, checked on the
address the gateway is about to connect to, after any name resolution. A
port of relay/src/policy.mjs with the same ranges. On the user's own
computer this is what keeps a page from reaching the router and the local
network through the gateway.
"""

from __future__ import annotations

import re
from typing import Optional

ALLOWED_PORTS = (80, 443)

#: Rejected IPv4 ranges: (first address, prefix length, why).
BLOCKED = (
    ("0.0.0.0", 8, "this network"),
    ("10.0.0.0", 8, "private"),
    ("100.64.0.0", 10, "shared address space (CGNAT, some cloud metadata)"),
    ("127.0.0.0", 8, "loopback"),
    ("169.254.0.0", 16, "link-local (cloud metadata 169.254.169.254)"),
    ("172.16.0.0", 12, "private"),
    ("192.0.0.0", 24, "IETF protocol assignments"),
    ("192.0.2.0", 24, "documentation"),
    ("192.88.99.0", 24, "6to4 relay anycast"),
    ("192.168.0.0", 16, "private"),
    ("198.18.0.0", 15, "benchmarking (the sinkhole's fake addresses)"),
    ("198.51.100.0", 24, "documentation"),
    ("203.0.113.0", 24, "documentation"),
    ("224.0.0.0", 4, "multicast"),
    ("240.0.0.0", 4, "reserved and broadcast"),
)

# ASCII digits only (`\d` would also take other scripts' digits).
_IPV4_RE = re.compile(r"([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})")


def parse_ipv4(s: object) -> Optional[int]:
    """Dotted IPv4 to u32, or None if it is not exactly four decimal octets."""
    if not isinstance(s, str):
        return None
    m = _IPV4_RE.fullmatch(s)
    if not m:
        return None
    v = 0
    for part in m.groups():
        # No leading zeros: "010" is octal for some parsers.
        if len(part) > 1 and part[0] == "0":
            return None
        o = int(part)
        if o > 255:
            return None
        v = v * 256 + o
    return v


def _ranges():
    out = []
    for base, length, why in BLOCKED:
        mask = 0 if length == 0 else (0xFFFFFFFF << (32 - length)) & 0xFFFFFFFF
        out.append((parse_ipv4(base) & mask, mask, why))
    return tuple(out)


_RANGES = _ranges()


def address_blocked(ip: object) -> Optional[str]:
    """Why the address is not allowed, or None if it is a public unicast address."""
    v = parse_ipv4(ip)
    if v is None:
        return "not an IPv4 address"
    for net, mask, why in _RANGES:
        if v & mask == net:
            return why
    return None


def port_allowed(port: object) -> bool:
    return port in ALLOWED_PORTS


def check_destination(ip: object, port: object) -> Optional[str]:
    """The REFUSED reason for a destination, or None if it may be connected to."""
    if not port_allowed(port):
        return "port"
    if address_blocked(ip) is not None:
        return "address"
    return None
