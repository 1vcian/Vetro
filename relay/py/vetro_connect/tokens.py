"""Session tokens of the network gateway (ADR 0048).

Short-lived, signed, bound to the Vetro site's origin: `payload.signature`,
both base64url; the signature is HMAC-SHA-256 with the gateway's secret over
the payload text. A port of relay/src/token.mjs: tokens made by one
implementation verify in the other.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import hmac
import json
import re
import secrets
from typing import Any, Dict, Optional

#: Token lifetime (s): lanes can be opened while it is valid.
TOKEN_TTL_S = 10 * 60
#: A session can be renewed with the same id while younger than this (s).
SESSION_MAX_S = 2 * 60 * 60

_B64URL_RE = re.compile(r"[A-Za-z0-9_-]*")


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def unb64url(s: str) -> Optional[bytes]:
    if not _B64URL_RE.fullmatch(s):
        return None
    try:
        return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))
    except (binascii.Error, ValueError):
        return None


def new_secret() -> str:
    """A secret for one run of the gateway (never stored)."""
    return secrets.token_urlsafe(32)


def new_session_id() -> str:
    """A random session id (128 bits, base64url)."""
    return b64url(secrets.token_bytes(16))


def _sign(secret: str, text: str) -> bytes:
    if not isinstance(secret, str) or len(secret) < 32:
        raise ValueError("the gateway secret must be at least 32 characters")
    return hmac.new(secret.encode("utf-8"), text.encode("utf-8"), hashlib.sha256).digest()


def issue(secret: str, *, sid: str, origin: str, now: int, first: Optional[int] = None, ttl: int = TOKEN_TTL_S) -> str:
    """Signs a token for `origin`. `first` = when the session started (s),
    kept across renewals; `now` in seconds."""
    if first is None:
        first = now
    # The same text as JSON.stringify gives (key order, no spaces).
    body = json.dumps(
        {"v": 1, "sid": sid, "origin": origin, "iat": now, "exp": now + ttl, "first": first},
        separators=(",", ":"),
        ensure_ascii=False,
    )
    payload = b64url(body.encode("utf-8"))
    return payload + "." + b64url(_sign(secret, payload))


def _is_number(v: Any) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def verify(secret: str, token: Any, *, origin: Optional[str], now: float, allow_expired: bool = False) -> Dict[str, Any]:
    """Verifies a token: its payload, or {"error": ...} (`malformed`,
    `signature`, `expired`, `origin`). `origin` = the request's Origin, which
    must be the token's. With `allow_expired` (renewal) an expired token is
    accepted."""
    if not isinstance(token, str) or len(token) > 1024:
        return {"error": "malformed"}
    parts = token.split(".")
    if len(parts) != 2:
        return {"error": "malformed"}
    sig = unb64url(parts[1])
    body = unb64url(parts[0])
    if sig is None or body is None:
        return {"error": "malformed"}
    if not hmac.compare_digest(sig, _sign(secret, parts[0])):
        return {"error": "signature"}
    try:
        p = json.loads(body.decode("utf-8", "replace"))
    except ValueError:
        return {"error": "malformed"}
    if not isinstance(p, dict):
        return {"error": "malformed"}
    v = p.get("v")
    if not (_is_number(v) and v == 1) or not isinstance(p.get("sid"), str) or not _is_number(p.get("exp")) or not _is_number(p.get("first")):
        return {"error": "malformed"}
    if p.get("origin") != origin:
        return {"error": "origin"}
    if not allow_expired and now >= p["exp"]:
        return {"error": "expired"}
    return p
