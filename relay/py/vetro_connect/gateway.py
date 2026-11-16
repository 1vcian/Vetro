"""The network gateway's HTTP side (ADR 0048, docs/specs/relay.md), a port
of relay/src/gateway.mjs:

    POST /session              a token for an allowed Origin (renewal with {"renew": token})
    GET  /lane?token=…&lane=k  WebSocket upgrade (checked here, accepted by the server)
    GET  /health               {"ok": true, "enabled": …}
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional
from urllib.parse import parse_qsl

from .tokens import SESSION_MAX_S, TOKEN_TTL_S, issue, new_session_id, verify

#: The live site's origin (https://1vcian.github.io redirects there).
DEFAULT_ORIGIN = "https://1vcian.me"


@dataclass
class Config:
    secret: str
    origins: List[str] = field(default_factory=lambda: [DEFAULT_ORIGIN])
    #: The kill switch: False = everything refused.
    enabled: bool = True
    #: Connections per lane.
    lane_conns: int = 32


@dataclass
class Request:
    method: str
    #: The request target as sent: path and query.
    target: str
    #: Names in lower case.
    headers: Dict[str, str] = field(default_factory=dict)
    body: bytes = b""

    @property
    def path(self) -> str:
        return self.target.partition("?")[0]

    def param(self, name: str) -> Optional[str]:
        """The first value of a query parameter, or None."""
        for k, v in parse_qsl(self.target.partition("?")[2], keep_blank_values=True):
            if k == name:
                return v
        return None


@dataclass
class Response:
    status: int
    headers: Dict[str, str] = field(default_factory=dict)
    body: bytes = b""


def _json(body: Any, status: int, origin: Optional[str]) -> Response:
    headers = {"content-type": "application/json", "cache-control": "no-store"}
    if origin:
        headers.update({"access-control-allow-origin": origin, "vary": "Origin"})
    return Response(status, headers, json.dumps(body, separators=(",", ":")).encode("utf-8"))


def _now_ms() -> float:
    return time.time() * 1000


def _reject_constant(name: str) -> Any:
    raise ValueError(name)  # NaN and Infinity are not JSON


def handle_http(request: Request, cfg: Config, *, now: Optional[float] = None, lanes: int = 8) -> Response:
    """Handles a request that is not a lane. `now` in ms; `lanes` = the
    session's lane limit (told to the client)."""
    now = _now_ms() if now is None else now
    origin = request.headers.get("origin")
    allowed = origin is not None and origin in cfg.origins
    if request.path == "/health":
        return _json({"ok": True, "enabled": cfg.enabled}, 200, origin if allowed else None)
    if request.path != "/session":
        return _json({"error": "not-found"}, 404, None)
    if request.method == "OPTIONS":
        if not allowed:
            return Response(403)
        # (Chrome asks the user before a public page may talk to this
        # computer, the `loopback-network` permission; nothing is needed here
        # for it, ADR 0050.)
        return Response(
            204,
            {
                "access-control-allow-origin": origin,
                "access-control-allow-methods": "POST",
                "access-control-allow-headers": "content-type",
                "access-control-max-age": "600",
                "vary": "Origin",
            },
        )
    if request.method != "POST":
        return _json({"error": "method"}, 405, None)
    if not allowed:
        return _json({"error": "origin"}, 403, None)
    if not cfg.enabled:
        return _json({"error": "disabled"}, 503, origin)
    body: Any = {}
    text = request.body.decode("utf-8", "replace")
    if len(text) > 2048:
        return _json({"error": "malformed"}, 400, origin)
    if text:
        try:
            body = json.loads(text, parse_constant=_reject_constant)
        except (ValueError, RecursionError):
            return _json({"error": "malformed"}, 400, origin)
    t = int(now // 1000)
    sid = new_session_id()
    first = t
    renew = body.get("renew") if isinstance(body, dict) else None
    if renew:
        p = verify(cfg.secret, renew, origin=origin, now=t, allow_expired=True)
        # A renewal keeps the session (and its limits) while it is young enough.
        if "error" not in p and t - p["first"] < SESSION_MAX_S:
            sid = p["sid"]
            first = p["first"]
    token = issue(cfg.secret, sid=sid, origin=origin, now=t, first=first)
    return _json({"token": token, "expires": (t + TOKEN_TTL_S) * 1000, "lanes": lanes, "laneConns": cfg.lane_conns}, 200, origin)


def check_lane(request: Request, cfg: Config, *, now: Optional[float] = None, lanes: int = 8) -> Dict[str, Any]:
    """Checks a lane upgrade: {"sid", "lane"}, or {"status", "error"} for the
    refusal. The server accepts the WebSocket only after the ledger's
    `open_lane`."""
    now = _now_ms() if now is None else now
    if request.path != "/lane":
        return {"status": 404, "error": "not-found"}
    if request.headers.get("upgrade", "").lower() != "websocket":
        return {"status": 426, "error": "upgrade"}
    origin = request.headers.get("origin")
    if origin is None or origin not in cfg.origins:
        return {"status": 403, "error": "origin"}
    if not cfg.enabled:
        return {"status": 503, "error": "disabled"}
    # As the Node gateway reads it: no `lane` is lane 0.
    raw = request.param("lane") or "0"
    if not (raw.isascii() and raw.isdigit()) or len(raw) > 9 or int(raw) >= lanes:
        return {"status": 400, "error": "lane"}
    p = verify(cfg.secret, request.param("token"), origin=origin, now=int(now // 1000))
    if "error" in p:
        return {"status": 401, "error": p["error"]}
    return {"sid": p["sid"], "lane": int(raw)}
