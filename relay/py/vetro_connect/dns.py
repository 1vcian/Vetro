"""DNS for the local network gateway (ADR 0050).

The guest's raw queries are checked as in relay/src/dns.mjs, but answered
from the computer's own resolver (`getaddrinfo`) instead of DNS over HTTPS:
no TLS trust store to bundle, and the user's own DNS (VPN, filters, hosts
file) is respected. Only type A questions get records; everything else gets
an empty NOERROR answer (the guest has no IPv6, and the machine answers AAAA
itself).
"""

from __future__ import annotations

import asyncio
import queue
import re
import socket
import struct
import threading
import time
from typing import Awaitable, Callable, List, NamedTuple, Optional

from .policy import address_blocked

TYPE_A = 1
CLASS_IN = 1
#: Largest answer the guest can take over UDP without fragments (MTU 1500).
MAX_UDP_ANSWER = 1472
#: Time to live of the answers (s): the system resolver does not tell its own.
TTL = 60
#: At most this many A records in an answer (it then fits 512 bytes).
MAX_ANSWERS = 8

RCODE_FORMERR = 1
RCODE_SERVFAIL = 2
RCODE_REFUSED = 5

#: `resolve(name) -> [dotted IPv4, ...]`: [] if the name has no address,
#: an exception if the lookup failed.
Resolve = Callable[[str], Awaitable[List[str]]]

# Names handed to the system resolver: LDH labels (and `_`), as host names
# are. Anything else is answered as "no address" without a lookup.
_HOST_RE = re.compile(r"(?=.{1,253}\Z)[a-z0-9_-]{1,63}(\.[a-z0-9_-]{1,63})*\Z")
# A last label that `getaddrinfo` could read as a number ("127.1", "0x7f.1",
# "2130706433"): no top-level domain looks like that.
_NUMERIC_RE = re.compile(r"([0-9]+|0x[0-9a-f]*)\Z")


class Query(NamedTuple):
    id: int
    name: str
    qtype: int
    qclass: int
    #: Offset after the question.
    end: int


def parse_query(q: bytes) -> Optional[Query]:
    """Checks a guest query: a standard query (QR 0, opcode 0) with exactly
    one question, at most 1024 bytes. None if it is not one."""
    if not isinstance(q, (bytes, bytearray)) or len(q) < 12 or len(q) > 1024:
        return None
    flags = (q[2] << 8) | q[3]
    if flags & 0x8000 or (flags >> 11) & 0xF:
        return None
    qd, an, ns = struct.unpack_from(">HHH", q, 4)
    if qd != 1 or an != 0 or ns != 0:
        return None
    labels = []
    at = 12
    while True:
        if at >= len(q):
            return None
        length = q[at]
        at += 1
        if length == 0:
            break
        if length > 63 or at + length > len(q):
            return None
        labels.append(bytes(q[at : at + length]).decode("latin-1"))
        at += length
    if at + 4 > len(q):
        return None
    qtype, qclass = struct.unpack_from(">HH", q, at)
    return Query((q[0] << 8) | q[1], ".".join(labels).lower(), qtype, qclass, at + 4)


def build_query(name: str, id: int = 0, qtype: int = TYPE_A) -> bytes:
    """A query for `name` (type A by default, recursion desired)."""
    out = bytearray(struct.pack(">HHHHHH", id & 0xFFFF, 0x0100, 1, 0, 0, 0))
    for label in name.split("."):
        if label:
            out.append(len(label))
            out += label.encode("latin-1")
    out += b"\x00" + struct.pack(">HH", qtype, CLASS_IN)
    return bytes(out)


def fail_answer(q: bytes, rcode: int = RCODE_SERVFAIL) -> bytes:
    """A failure answer (SERVFAIL by default) to the query `q`: its header
    and, if it parses, its question."""
    p = parse_query(q)
    out = bytearray(q[: p.end if p else 12])
    out[2] = 0x80 | (q[2] & 0x01)  # QR, RD copied
    out[3] = 0x80 | rcode  # RA
    out[6:12] = bytes(6)  # no answers, no authority, no additional
    if not p:
        out[4:6] = bytes(2)  # no question either
    return bytes(out)


def build_answer(q: bytes, p: Query, addrs: List[str], ttl: int = TTL) -> bytes:
    """A NOERROR answer to the checked query `q` with `addrs` as A records
    (none: "no data"). It carries the query's id and its question."""
    out = bytearray(q[: p.end])
    out[2] = 0x80 | (q[2] & 0x01)  # QR, RD copied
    out[3] = 0x80  # RA, NOERROR
    out[4:12] = struct.pack(">HHHH", 1, len(addrs), 0, 0)
    for a in addrs:
        # The name is a pointer to the question's (offset 12).
        out += b"\xc0\x0c" + struct.pack(">HHIH", TYPE_A, CLASS_IN, ttl, 4) + socket.inet_aton(a)
    return bytes(out)


def fit_udp(q: bytes, answer: bytes) -> bytes:
    """Truncated answer (TC, question only) if it would not fit the guest's UDP."""
    if len(answer) <= MAX_UDP_ANSWER:
        return answer
    t = bytearray(fail_answer(q, answer[3] & 0x0F))
    t[2] |= 0x02  # TC
    return bytes(t)


def _skip_name(m: bytes, at: int) -> int:
    for _ in range(128):
        if at >= len(m):
            return -1
        length = m[at]
        if length == 0:
            return at + 1
        if length & 0xC0 == 0xC0:
            return at + 2
        at += 1 + length
    return -1


def answer_addrs(m: bytes) -> List[str]:
    """The A records of an answer (dotted strings), [] if none or malformed."""
    if not isinstance(m, (bytes, bytearray)) or len(m) < 12:
        return []
    if m[3] & 0x0F:
        return []
    qd, an = struct.unpack_from(">HH", m, 4)
    at = 12
    for _ in range(qd):
        at = _skip_name(m, at)
        if at < 0:
            return []
        at += 4
    out: List[str] = []
    for _ in range(an):
        at = _skip_name(m, at)
        if at < 0 or at + 10 > len(m):
            return out
        rtype, rclass, _ttl, rdlen = struct.unpack_from(">HHIH", m, at)
        at += 10
        if at + rdlen > len(m):
            return out
        if rtype == TYPE_A and rclass == CLASS_IN and rdlen == 4:
            out.append("%d.%d.%d.%d" % tuple(m[at : at + 4]))
        at += rdlen
    return out


def resolvable(name: str) -> bool:
    """Whether `name` may be handed to the system resolver: a host name, and
    not something it would read as a number."""
    if not _HOST_RE.match(name):
        return False
    return not _NUMERIC_RE.match(name.rsplit(".", 1)[-1])


async def answer(q: bytes, p: Query, resolve: Resolve) -> bytes:
    """The answer to the checked query `q`. Type A, class IN: the name's
    addresses from `resolve`, without those the destination policy refuses
    (they could not be connected to, and local names would tell the guest's
    apps the addresses of the user's network). Other questions: no records.
    Raises if the lookup failed (the caller answers SERVFAIL)."""
    addrs: List[str] = []
    if p.qtype == TYPE_A and p.qclass == CLASS_IN and resolvable(p.name):
        seen = set()
        for a in await resolve(p.name):
            if a not in seen and address_blocked(a) is None:
                seen.add(a)
                addrs.append(a)
    return build_answer(q, p, addrs[:MAX_ANSWERS])


def _not_found(e: BaseException) -> bool:
    """`getaddrinfo` said "no such name" or "no address of this kind"."""
    if not isinstance(e, socket.gaierror):
        return False
    codes = {getattr(socket, n) for n in ("EAI_NONAME", "EAI_NODATA", "EAI_ADDRFAMILY") if hasattr(socket, n)}
    # Windows: WSAHOST_NOT_FOUND, WSANO_DATA.
    codes |= {11001, 11004}
    return e.errno in codes


def lookup_ipv4(name: str) -> List[str]:
    """The system resolver, blocking: the name's IPv4 addresses, [] if it has
    none. Raises `socket.gaierror` for a failed lookup."""
    try:
        infos = socket.getaddrinfo(name.encode("ascii"), None, socket.AF_INET, socket.SOCK_STREAM)
    except socket.gaierror as e:
        if _not_found(e):
            return []
        raise
    out: List[str] = []
    for info in infos:
        a = info[4][0]
        if a not in out:
            out.append(a)
    return out


class SystemResolver:
    """`getaddrinfo` off the event loop: a few daemon threads (they never
    keep the program from quitting) and a time limit per lookup."""

    def __init__(self, workers: int = 16, timeout: float = 5.0, lookup: Callable[[str], List[str]] = lookup_ipv4):
        self._workers = workers
        self._timeout = timeout
        self._lookup = lookup
        self._queue: "queue.SimpleQueue" = queue.SimpleQueue()
        self._threads: List[threading.Thread] = []
        self._waiting = 0

    async def __call__(self, name: str) -> List[str]:
        if not resolvable(name):
            return []
        loop = asyncio.get_running_loop()
        fut: "asyncio.Future[List[str]]" = loop.create_future()
        self._waiting += 1
        if len(self._threads) < min(self._workers, self._waiting):
            t = threading.Thread(target=self._work, name="vetro-dns", daemon=True)
            self._threads.append(t)
            t.start()
        self._queue.put((name, fut, loop, time.monotonic() + self._timeout))
        try:
            return await asyncio.wait_for(fut, self._timeout)
        finally:
            self._waiting -= 1

    def _work(self) -> None:
        while True:
            name, fut, loop, deadline = self._queue.get()
            # Nobody waits for a lookup that is already late.
            if time.monotonic() >= deadline:
                continue
            try:
                result, error = self._lookup(name), None
            except Exception as e:  # noqa: BLE001 - handed to the waiter
                result, error = None, e
            try:
                loop.call_soon_threadsafe(_deliver, fut, result, error)
            except RuntimeError:
                pass  # the loop is closed: the program is quitting


def _deliver(fut: "asyncio.Future", result: Optional[List[str]], error: Optional[BaseException]) -> None:
    if fut.done():
        return
    if error is not None:
        fut.set_exception(error)
    else:
        fut.set_result(result)
