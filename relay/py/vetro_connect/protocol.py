"""The network gateway's protocol (ADR 0048, docs/specs/relay.md).

Binary frames over WebSocket, one frame per message: op (1 byte), id (u32
big endian), payload. A port of relay/src/protocol.mjs, the same encoding as
`vetro_net::relay` in Rust, checked against the same vectors
(relay/test/vectors.txt). Text messages carry JSON control messages (hello,
bye, ping, pong).
"""

from __future__ import annotations

import re
import struct
from dataclasses import dataclass
from typing import Optional, Union

VERSION = 1
#: Per-connection, per-direction flow control window (bytes).
WINDOW = 256 * 1024
#: Largest DATA payload.
MAX_DATA = 16 * 1024
#: Largest DNS message in a DNS frame.
MAX_DNS = 4096
MAX_NAME = 253
HEADER = 5


class OP:
    CONNECT = 0x01
    CONNECT_NAME = 0x02
    DATA = 0x03
    SHUTDOWN = 0x04
    CLOSE = 0x05
    ACK = 0x06
    DNS = 0x07
    CONNECTED = 0x81
    REFUSED = 0x82
    DATA_IN = 0x83
    SHUTDOWN_IN = 0x84
    CLOSE_IN = 0x85
    ACK_IN = 0x86
    DNS_ANSWER = 0x87
    # Host-local (the page's Worker to the machine), never on a lane.
    ONLINE = 0xF0
    DOWN = 0xF1


#: REFUSED reasons.
REFUSAL = {
    "unreachable": 1,
    "port": 2,
    "address": 3,
    "limit": 4,
    "platform": 5,
    "resolve": 6,
    "down": 7,
}
REFUSAL_NAME = {v: k for k, v in REFUSAL.items()}

# Labels of 1-63 `[a-z0-9-]`, dots between them, 1-253 bytes in all.
_NAME_RE = re.compile(r"(?=.{1,253}\Z)([a-z0-9-]{1,63})(\.[a-z0-9-]{1,63})*\Z")


@dataclass
class Frame:
    """A decoded frame. `type` is one of connect, connectName, data, shutdown,
    close, ack, dns, dnsAnswer, connected, refused, online, down (the names of
    the JS decoder); the other fields are set for the types that carry them."""

    type: str
    id: int
    from_gateway: bool
    ip: Optional[str] = None
    port: Optional[int] = None
    name: Optional[str] = None
    data: Optional[bytes] = None
    reset: Optional[bool] = None
    n: Optional[int] = None
    msg: Optional[bytes] = None
    reason: Optional[str] = None
    on: Optional[bool] = None


def _frame(op: int, id: int, payload: bytes = b"") -> bytes:
    return struct.pack(">BI", op, id & 0xFFFFFFFF) + bytes(payload)


class encode:
    """Encoders, by frame kind. `id` is a u32. `from_gateway`: DATA, SHUTDOWN,
    CLOSE and ACK have one code per direction."""

    @staticmethod
    def connect(id: int, ip: str, port: int) -> bytes:
        return _frame(OP.CONNECT, id, bytes(int(o) for o in ip.split(".")) + struct.pack(">H", port))

    @staticmethod
    def connect_name(id: int, name: str, port: int) -> bytes:
        return _frame(OP.CONNECT_NAME, id, struct.pack(">H", port) + name.encode("utf-8"))

    @staticmethod
    def data(id: int, data: bytes, from_gateway: bool) -> bytes:
        return _frame(OP.DATA_IN if from_gateway else OP.DATA, id, data)

    @staticmethod
    def shutdown(id: int, from_gateway: bool) -> bytes:
        return _frame(OP.SHUTDOWN_IN if from_gateway else OP.SHUTDOWN, id)

    @staticmethod
    def close(id: int, reset: bool, from_gateway: bool) -> bytes:
        return _frame(OP.CLOSE_IN if from_gateway else OP.CLOSE, id, b"\x01" if reset else b"\x00")

    @staticmethod
    def ack(id: int, n: int, from_gateway: bool) -> bytes:
        return _frame(OP.ACK_IN if from_gateway else OP.ACK, id, struct.pack(">I", n & 0xFFFFFFFF))

    @staticmethod
    def dns(id: int, msg: bytes) -> bytes:
        return _frame(OP.DNS, id, msg)

    @staticmethod
    def dns_answer(id: int, msg: bytes) -> bytes:
        return _frame(OP.DNS_ANSWER, id, msg)

    @staticmethod
    def connected(id: int) -> bytes:
        return _frame(OP.CONNECTED, id)

    @staticmethod
    def refused(id: int, reason: Union[str, int]) -> bytes:
        return _frame(OP.REFUSED, id, bytes([REFUSAL[reason] if isinstance(reason, str) else reason]))

    @staticmethod
    def online(on: bool) -> bytes:
        return _frame(OP.ONLINE, 0, b"\x01" if on else b"\x00")

    @staticmethod
    def down() -> bytes:
        return _frame(OP.DOWN, 0)


def decode(data: bytes) -> Optional[Frame]:
    """Decodes a frame, or None if it is malformed (the same rules as the Rust
    and the JS decoders)."""
    b = bytes(data)
    if len(b) < HEADER:
        return None
    op = b[0]
    (id,) = struct.unpack_from(">I", b, 1)
    p = b[HEADER:]
    from_gateway = (op & 0x80) != 0

    def flag() -> Optional[bool]:
        return p[0] == 1 if len(p) == 1 and p[0] <= 1 else None

    if op == OP.CONNECT:
        if len(p) != 6:
            return None
        return Frame("connect", id, from_gateway, ip="%d.%d.%d.%d" % tuple(p[:4]), port=(p[4] << 8) | p[5])
    if op == OP.CONNECT_NAME:
        if len(p) < 3:
            return None
        name = p[2:].decode("latin-1")
        if not _NAME_RE.match(name):
            return None
        return Frame("connectName", id, from_gateway, name=name, port=(p[0] << 8) | p[1])
    if op in (OP.DATA, OP.DATA_IN):
        if len(p) == 0 or len(p) > MAX_DATA:
            return None
        return Frame("data", id, from_gateway, data=p)
    if op in (OP.SHUTDOWN, OP.SHUTDOWN_IN):
        return Frame("shutdown", id, from_gateway) if len(p) == 0 else None
    if op in (OP.CLOSE, OP.CLOSE_IN):
        reset = flag()
        return None if reset is None else Frame("close", id, from_gateway, reset=reset)
    if op in (OP.ACK, OP.ACK_IN):
        return Frame("ack", id, from_gateway, n=struct.unpack(">I", p)[0]) if len(p) == 4 else None
    if op in (OP.DNS, OP.DNS_ANSWER):
        if len(p) < 12 or len(p) > MAX_DNS:
            return None
        return Frame("dns" if op == OP.DNS else "dnsAnswer", id, from_gateway, msg=p)
    if op == OP.CONNECTED:
        return Frame("connected", id, from_gateway) if len(p) == 0 else None
    if op == OP.REFUSED:
        if len(p) == 1 and p[0] in REFUSAL_NAME:
            return Frame("refused", id, from_gateway, reason=REFUSAL_NAME[p[0]])
        return None
    if op == OP.ONLINE:
        on = flag()
        return Frame("online", id, from_gateway, on=on) if id == 0 and on is not None else None
    if op == OP.DOWN:
        return Frame("down", id, from_gateway) if id == 0 and len(p) == 0 else None
    return None


def frame_id(b: bytes) -> int:
    """The frame's id (bytes 1..4), without decoding the rest."""
    return struct.unpack_from(">I", b, 1)[0]


def with_id(b: bytes, id: int) -> bytes:
    """A copy of the frame with another id."""
    return b[:1] + struct.pack(">I", id & 0xFFFFFFFF) + b[HEADER:]
