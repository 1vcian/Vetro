"""The server side of WebSocket (RFC 6455), with the standard library only.

What the gateway needs and nothing more: the handshake's accept key, frames
with the three length forms, the client's masking (required, and removed),
text, binary, ping, pong and close, continuation frames, and a bound on the
size of a message. No extensions (no compression): the reserved bits must
be zero.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import struct
from typing import Any, List, Optional, Tuple

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

OP_CONT = 0x0
OP_TEXT = 0x1
OP_BINARY = 0x2
OP_CLOSE = 0x8
OP_PING = 0x9
OP_PONG = 0xA

CLOSE_NORMAL = 1000
CLOSE_PROTOCOL = 1002
CLOSE_BAD_DATA = 1007
CLOSE_TOO_BIG = 1009

#: Largest message accepted (the largest frame of the protocol is 16 389 bytes).
MAX_MESSAGE = 64 * 1024


class WsError(Exception):
    """A violation by the peer: the connection is closed with `code`."""

    def __init__(self, code: int, reason: str):
        super().__init__(reason)
        self.code = code
        self.reason = reason


def accept_key(key: str) -> str:
    """The Sec-WebSocket-Accept value for a Sec-WebSocket-Key."""
    return base64.b64encode(hashlib.sha1((key + GUID).encode("ascii")).digest()).decode("ascii")


def valid_key(key: Any) -> bool:
    """A Sec-WebSocket-Key: 16 bytes in base64."""
    if not isinstance(key, str):
        return False
    try:
        return len(base64.b64decode(key.encode("ascii"), validate=True)) == 16
    except (binascii.Error, ValueError):
        return False


def apply_mask(data: bytes, mask: bytes) -> bytes:
    """`data` XOR the 4-byte `mask`, repeated (masking is its own inverse)."""
    n = len(data)
    if n == 0:
        return b""
    pad = (mask * (n // 4 + 1))[:n]
    return (int.from_bytes(data, "big") ^ int.from_bytes(pad, "big")).to_bytes(n, "big")


def encode_frame(opcode: int, payload: bytes = b"", *, fin: bool = True, mask: Optional[bytes] = None) -> bytes:
    """One frame. A server sends them unmasked; `mask` (4 bytes) is for the
    client side (tests)."""
    n = len(payload)
    head = bytearray([(0x80 if fin else 0) | opcode])
    m = 0x80 if mask is not None else 0
    if n < 126:
        head.append(m | n)
    elif n < 0x10000:
        head.append(m | 126)
        head += struct.pack(">H", n)
    else:
        head.append(m | 127)
        head += struct.pack(">Q", n)
    if mask is not None:
        return bytes(head) + mask + apply_mask(payload, mask)
    return bytes(head) + bytes(payload)


def close_payload(code: Optional[int] = None, reason: str = "") -> bytes:
    """The payload of a close frame (the reason cut to fit 125 bytes)."""
    if code is None:
        return b""
    return struct.pack(">H", code) + reason.encode("utf-8")[:123]


Message = Tuple[int, Any]


class Parser:
    """Frames from a client, fed as they arrive: `feed` returns the complete
    messages, as (opcode, payload): text (str), binary (bytes), ping and pong
    (bytes), close ((code or None, reason)). Raises `WsError` on a violation.
    """

    def __init__(self, max_message: int = MAX_MESSAGE, require_mask: bool = True):
        self._max = max_message
        self._require_mask = require_mask
        self._buf = bytearray()
        # A fragmented message in progress: its opcode and its parts.
        self._op: Optional[int] = None
        self._parts: List[bytes] = []
        self._size = 0
        self.closed = False

    def feed(self, data: bytes) -> List[Message]:
        out: List[Message] = []
        if self.closed:
            return out
        self._buf += data
        while not self.closed:
            frame = self._frame()
            if frame is None:
                break
            fin, opcode, payload = frame
            if opcode >= 0x8:
                out.append(self._control(opcode, payload))
                continue
            if opcode == OP_CONT:
                if self._op is None:
                    raise WsError(CLOSE_PROTOCOL, "continuation without a message")
            elif self._op is not None:
                raise WsError(CLOSE_PROTOCOL, "a new message inside a fragmented one")
            else:
                self._op = opcode
            self._parts.append(payload)
            if fin:
                whole = b"".join(self._parts)
                op = self._op
                self._op, self._parts, self._size = None, [], 0
                if op == OP_TEXT:
                    try:
                        out.append((OP_TEXT, whole.decode("utf-8")))
                    except UnicodeDecodeError:
                        raise WsError(CLOSE_BAD_DATA, "text that is not UTF-8") from None
                else:
                    out.append((OP_BINARY, whole))
        return out

    def _frame(self) -> Optional[Tuple[bool, int, bytes]]:
        """The next whole frame of the buffer, checked, or None if incomplete."""
        buf = self._buf
        if len(buf) < 2:
            return None
        b0, b1 = buf[0], buf[1]
        fin = bool(b0 & 0x80)
        opcode = b0 & 0x0F
        masked = bool(b1 & 0x80)
        n = b1 & 0x7F
        if b0 & 0x70:
            raise WsError(CLOSE_PROTOCOL, "reserved bits set (no extension was agreed)")
        if opcode not in (OP_CONT, OP_TEXT, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG):
            raise WsError(CLOSE_PROTOCOL, "unknown opcode")
        if self._require_mask and not masked:
            raise WsError(CLOSE_PROTOCOL, "a client frame must be masked")
        at = 2
        if opcode >= 0x8:
            if not fin or n > 125:
                raise WsError(CLOSE_PROTOCOL, "a control frame is whole and at most 125 bytes")
        elif n == 126:
            if len(buf) < 4:
                return None
            (n,) = struct.unpack_from(">H", buf, 2)
            at = 4
        elif n == 127:
            if len(buf) < 10:
                return None
            (n,) = struct.unpack_from(">Q", buf, 2)
            if n >> 63:
                raise WsError(CLOSE_PROTOCOL, "a length with the top bit set")
            at = 10
        if opcode < 0x8 and self._size + n > self._max:
            raise WsError(CLOSE_TOO_BIG, "message too big")
        if masked:
            at += 4
        if len(buf) < at + n:
            return None
        payload = bytes(buf[at : at + n])
        if masked:
            payload = apply_mask(payload, bytes(buf[at - 4 : at]))
        del buf[: at + n]
        if opcode < 0x8:
            self._size += n
        return fin, opcode, payload

    def _control(self, opcode: int, payload: bytes) -> Message:
        if opcode != OP_CLOSE:
            return (opcode, payload)
        self.closed = True
        if len(payload) == 1:
            raise WsError(CLOSE_PROTOCOL, "a close frame with half a code")
        if not payload:
            return (OP_CLOSE, (None, ""))
        (code,) = struct.unpack_from(">H", payload)
        return (OP_CLOSE, (code, payload[2:].decode("utf-8", "replace")))
