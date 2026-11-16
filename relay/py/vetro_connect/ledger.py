"""The session ledger (ADR 0048): the per-session limits, shared by all the
session's lanes. Pure state with an injected clock. A port of
relay/src/ledger.mjs with the same semantics and the same `LIMIT_*` names.

One difference, for a gateway that serves only its own user (ADR 0050): the
bandwidth rate and the total bytes have no cap by default (`None`); a
`LIMIT_RATE` or `LIMIT_BYTES` variable sets one.
"""

from __future__ import annotations

import math
import re
import time
from typing import Callable, Dict, List, Mapping, NamedTuple, Optional

Number = float  # ints or floats; None where a limit is "no cap"

DEFAULT_LIMITS: Dict[str, Optional[Number]] = {
    # Concurrent TCP connections in the session.
    "conns": 32,
    # Concurrent WebSocket lanes.
    "lanes": 8,
    # Bandwidth, both directions together (bytes/s), and the burst: no cap.
    "rate": None,
    "burst": None,
    # Total bytes of the session: no cap.
    "bytes": None,
    # DNS queries per second, and the burst.
    "dnsRate": 20,
    "dnsBurst": 100,
}

#: The Node gateway's defaults (relay/src/ledger.mjs), for reference and tests.
SERVER_LIMITS: Dict[str, Optional[Number]] = {
    **DEFAULT_LIMITS,
    "rate": 2 * 1024 * 1024,
    "burst": 4 * 1024 * 1024,
    "bytes": 1024 * 1024 * 1024,
}


def _variable(key: str) -> str:
    return "LIMIT_" + re.sub(r"[A-Z]", lambda m: "_" + m.group(0), key).upper()


def limits_from(variables: Optional[Mapping[str, str]] = None, defaults: Mapping[str, Optional[Number]] = DEFAULT_LIMITS) -> Dict[str, Optional[Number]]:
    """Limits from string variables (the environment), defaults otherwise. A
    value counts only if it is a finite number above zero."""
    variables = variables or {}
    out = dict(defaults)
    for key in defaults:
        raw = variables.get(_variable(key))
        if raw is None or raw == "":
            continue
        try:
            v = float(raw)
        except ValueError:
            continue
        if math.isfinite(v) and v > 0:
            out[key] = int(v) if v == int(v) else v
    # A rate without a burst: twice the rate, as the server's defaults.
    if out["rate"] is not None and out["burst"] is None:
        out["burst"] = 2 * out["rate"]
    return out


class Result(NamedTuple):
    """`ok` with `wait_ms` (wait that long first), or not `ok` with a `reason`."""

    ok: bool
    wait_ms: int = 0
    reason: Optional[str] = None


_OK = Result(True)


class Ledger:
    """`now()` in ms; `enabled()` is the kill switch, read on every request."""

    def __init__(self, limits: Optional[Mapping[str, Optional[Number]]] = None, *, now: Callable[[], float] = lambda: time.monotonic() * 1000, enabled: Callable[[], bool] = lambda: True):
        self.limits: Dict[str, Optional[Number]] = {**DEFAULT_LIMITS, **(limits or {})}
        if self.limits["rate"] is not None and self.limits["burst"] is None:
            self.limits["burst"] = 2 * self.limits["rate"]
        self.now = now
        self.enabled = enabled
        t = now()
        self.lanes: List[int] = []
        self.conns = 0
        self.bytes = 0
        self.tokens = self.limits["burst"] if self.limits["rate"] is not None else 0
        self.dns_tokens = self.limits["dnsBurst"]
        self.at = t
        self.dns_at = t
        self.ended: Optional[str] = None

    def _refill(self) -> None:
        t = self.now()
        lim = self.limits
        if lim["rate"] is not None:
            self.tokens = min(lim["burst"], self.tokens + ((t - self.at) / 1000) * lim["rate"])
        self.at = t
        self.dns_tokens = min(lim["dnsBurst"], self.dns_tokens + ((t - self.dns_at) / 1000) * lim["dnsRate"])
        self.dns_at = t

    def _refusal(self) -> Optional[str]:
        if not self.enabled():
            return "disabled"
        return self.ended

    def open_lane(self, k: int) -> Result:
        """A lane `k` opens, or is refused (`lanes`, `lane-in-use`, ...)."""
        r = self._refusal()
        if r:
            return Result(False, reason=r)
        if k in self.lanes:
            return Result(False, reason="lane-in-use")
        if len(self.lanes) >= self.limits["lanes"]:
            return Result(False, reason="lanes")
        self.lanes.append(k)
        return _OK

    def close_lane(self, k: int) -> None:
        self.lanes = [x for x in self.lanes if x != k]

    def open_conn(self) -> Result:
        """A connection opens, or is refused (`conns`, ...)."""
        r = self._refusal()
        if r:
            return Result(False, reason=r)
        if self.conns >= self.limits["conns"]:
            return Result(False, reason="conns")
        self.conns += 1
        return _OK

    def close_conn(self) -> None:
        self.conns = max(0, self.conns - 1)

    def grant(self, n: int) -> Result:
        """`n` bytes are about to cross the gateway (either way): ok with the
        wait before moving them (the bandwidth limit), or refused (`bytes`: the
        session is over its total and ends; `disabled`)."""
        r = self._refusal()
        if r:
            return Result(False, reason=r)
        lim = self.limits
        if lim["bytes"] is not None and self.bytes + n > lim["bytes"]:
            self.ended = "bytes"
            return Result(False, reason="bytes")
        self._refill()
        self.bytes += n
        if lim["rate"] is None:
            return _OK
        self.tokens -= n
        return Result(True, 0 if self.tokens >= 0 else math.ceil((-self.tokens / lim["rate"]) * 1000))

    def dns(self) -> Result:
        """A DNS query: ok with a wait, or refused."""
        r = self._refusal()
        if r:
            return Result(False, reason=r)
        self._refill()
        lim = self.limits
        # Far beyond the rate: refused rather than queued.
        if self.dns_tokens <= -lim["dnsBurst"]:
            return Result(False, reason="dns")
        self.dns_tokens -= 1
        return Result(True, 0 if self.dns_tokens >= 0 else math.ceil((-self.dns_tokens / lim["dnsRate"]) * 1000))

    def counters(self) -> Dict[str, object]:
        """Counters (no destinations, no contents)."""
        return {"lanes": len(self.lanes), "conns": self.conns, "bytes": self.bytes, "ended": self.ended}
