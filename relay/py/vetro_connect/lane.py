"""One WebSocket lane of the network gateway (ADR 0048, docs/specs/relay.md):
the guest's TCP connections and DNS queries carried by one WebSocket, and
the real sockets behind them. A port of relay/src/lane.mjs to asyncio: the
socket factory, the name resolver, the session ledger and the WebSocket are
injected, so the same code runs in the gateway and in the unit tests.

Safeguards enforced here: the destination policy (ports 80/443, public
addresses only, checked after resolution), per-lane connections, the
session ledger before every byte (bandwidth, total, connections, kill
switch), idle connections and the lane's maximum life. Nothing about the
traffic is logged or kept except counters.
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import Any, Awaitable, Callable, Dict, List, Optional, Set

from . import dns
from .ledger import Ledger
from .policy import address_blocked, check_destination, port_allowed
from .protocol import MAX_DATA, VERSION, WINDOW, Frame, decode, encode

#: Bytes asked to the ledger at a time.
GRANT = 64 * 1024
IDLE_MS = 10 * 60 * 1000
LANE_MAX_MS = 60 * 60 * 1000
CONNECT_TIMEOUT_MS = 15_000
#: How often the lane checks idle connections and its own age.
TICK_MS = 15_000

_PONG = json.dumps({"type": "pong"}, separators=(",", ":"))


class Stats:
    """Counters of the whole gateway for the status display: numbers only,
    no destinations and no contents (ADR 0048, safeguard 5)."""

    FIELDS = ("sessions", "lanes", "conns", "conns_total", "refused", "bytes_to_remote", "bytes_to_guest", "dns", "sockets", "errors")

    def __init__(self) -> None:
        #: Session tokens given to the page (none yet: the page has not
        #: reached the gateway).
        self.sessions = 0
        #: Lanes and connections open now.
        self.lanes = 0
        self.conns = 0
        #: Since the start: connections opened, refused, bytes each way, DNS
        #: questions, outbound sockets tried, internal errors.
        self.conns_total = 0
        self.refused = 0
        self.bytes_to_remote = 0
        self.bytes_to_guest = 0
        self.dns = 0
        self.sockets = 0
        self.errors = 0

    def snapshot(self) -> Dict[str, int]:
        return {k: getattr(self, k) for k in self.FIELDS}


class _Conn:
    __slots__ = ("id", "state", "closed", "counted", "socket", "client_in_flight", "out_in_flight", "queue", "shut", "write_done", "read_done", "last", "wake", "tasks")

    def __init__(self, id: int, now: float):
        self.id = id
        self.state = "connecting"
        self.closed = False
        #: Counted in the ledger.
        self.counted = False
        self.socket: Any = None
        #: The client's DATA bytes not yet written to the socket.
        self.client_in_flight = 0
        #: Our DATA bytes the client has not acknowledged.
        self.out_in_flight = 0
        #: Writes in order: bytes, then None for the client's SHUTDOWN.
        self.queue: "asyncio.Queue[Optional[bytes]]" = asyncio.Queue()
        self.shut = False
        self.write_done = False
        self.read_done = False
        self.last = now
        self.wake = asyncio.Event()
        self.tasks: List["asyncio.Future[Any]"] = []


def _current_task() -> Optional["asyncio.Task[Any]"]:
    try:
        return asyncio.current_task()
    except RuntimeError:
        return None


class Lane:
    """- `send(data)`: sends a WebSocket message (bytes or str);
    - `close(code, reason)`: closes the WebSocket;
    - `connect(ip, port)`: a coroutine giving a socket with `read(n)` (b"" at
      the end), `write(data)` (done when the bytes are written), `shutdown()`
      (FIN after the data), `close()` and `reset()`; it raises if the
      connection fails;
    - `resolver(name)`: a coroutine giving the name's IPv4 addresses ([] if it
      has none), raising if the lookup fails;
    - `ledger`: the session's;
    - `lane`: index; `lane_conns`: connections per lane; `limits`: shown in hello.
    """

    def __init__(
        self,
        *,
        send: Callable[[Any], None],
        close: Callable[[int, str], None],
        connect: Callable[[str, int], Awaitable[Any]],
        resolver: dns.Resolve,
        ledger: Ledger,
        lane: int = 0,
        lane_conns: int = 32,
        limits: Optional[Dict[str, Any]] = None,
        platform: str = "python",
        now: Callable[[], float] = lambda: time.monotonic() * 1000,
        log: Callable[[Dict[str, Any]], None] = lambda record: None,
        idle_ms: float = IDLE_MS,
        max_life_ms: float = LANE_MAX_MS,
        connect_timeout_ms: float = CONNECT_TIMEOUT_MS,
        tick_ms: float = TICK_MS,
        stats: Optional[Stats] = None,
    ):
        self.send_raw = send
        self.close_raw = close
        self.connect = connect
        self.resolver = resolver
        self.ledger = ledger
        self.lane = lane
        self.lane_conns = lane_conns
        self.limits = limits or {}
        self.platform = platform
        self.now = now
        self.log = log
        self.idle_ms = idle_ms
        self.max_life_ms = max_life_ms
        self.connect_timeout_ms = connect_timeout_ms
        self.tick_ms = tick_ms
        self.stats = stats or Stats()
        self.conns: Dict[int, _Conn] = {}
        self.budget = 0
        self.closed = False
        self.started = now()
        self.counters: Dict[str, Any] = {"conns": 0, "refused": {}, "bytesToRemote": 0, "bytesToGuest": 0, "dns": 0}
        self._spend_lock = asyncio.Lock()
        self._tasks: Set["asyncio.Future[Any]"] = set()

    def start(self) -> None:
        self.stats.lanes += 1
        hello = {"type": "hello", "v": VERSION, "lane": self.lane, "window": WINDOW, "maxData": MAX_DATA, "laneConns": self.lane_conns, "limits": self.limits}
        self.send(json.dumps(hello, separators=(",", ":"), allow_nan=False))
        if self.tick_ms > 0:
            self._spawn(self._ticker())

    def send(self, data: Any) -> None:
        if self.closed:
            return
        try:
            self.send_raw(data)
        except Exception:  # noqa: BLE001 - whatever the WebSocket says, the lane is over
            self.end("send-failed", False)

    def end(self, reason: str, notify: bool = True) -> None:
        """Ends the lane: `bye` with the reason, the WebSocket closed, every
        socket closed."""
        if self.closed:
            return
        if notify:
            self.send(json.dumps({"type": "bye", "reason": reason}, separators=(",", ":")))
            if self.closed:  # the send failed and ended the lane
                return
        self.closed = True
        for c in list(self.conns.values()):
            self.drop(c, False, True)
        try:
            self.close_raw(4002 if reason == "protocol" else 4000, reason)
        except Exception:  # noqa: BLE001
            pass
        self.ledger.close_lane(self.lane)
        self.stats.lanes -= 1
        current = _current_task()
        for t in list(self._tasks):
            if t is not current:
                t.cancel()
        self.log({"event": "lane-end", "platform": self.platform, "reason": reason, "secs": round((self.now() - self.started) / 1000), **self.counters})

    def on_close(self) -> None:
        """The WebSocket closed from the other side."""
        self.end("closed", False)

    def tick(self) -> None:
        """Idle connections and the lane's age (also called by the tests)."""
        t = self.now()
        if t - self.started >= self.max_life_ms:
            return self.end("expired")
        for c in list(self.conns.values()):
            if t - c.last >= self.idle_ms:
                self.drop(c, True, True)

    async def _ticker(self) -> None:
        while not self.closed:
            await asyncio.sleep(self.tick_ms / 1000)
            self.tick()

    def refuse(self, id: int, reason: str) -> None:
        self.counters["refused"][reason] = self.counters["refused"].get(reason, 0) + 1
        self.stats.refused += 1
        self.send(encode.refused(id, reason))

    def drop(self, c: _Conn, notify: bool, reset: bool) -> None:
        """Forgets connection `c`, closes its socket; `notify` = CLOSE(reset)
        to the client."""
        if c.closed:
            return
        c.closed = True
        if self.conns.get(c.id) is c:
            del self.conns[c.id]
        c.wake.set()
        if c.socket is not None:
            try:
                if reset:
                    c.socket.reset()
                else:
                    c.socket.close()
            except Exception:  # noqa: BLE001
                pass
        current = _current_task()
        for t in c.tasks:
            if t is not current:
                t.cancel()
        if notify and not self.closed:
            self.send(encode.close(c.id, True, True))
        if c.counted:
            self.ledger.close_conn()
        if c.state == "open":
            self.stats.conns -= 1

    async def spend(self, n: int) -> bool:
        """Waits for the ledger's permission to move `n` bytes; False = the
        lane ended."""
        async with self._spend_lock:
            while self.budget < n:
                if self.closed:
                    return False
                want = max(GRANT, n)
                g = self.ledger.grant(want)
                if not g.ok:
                    self.end("limit-bytes" if g.reason == "bytes" else str(g.reason))
                    return False
                if g.wait_ms > 0:
                    await asyncio.sleep(g.wait_ms / 1000)
                self.budget += want
            self.budget -= n
            return not self.closed

    def on_message(self, data: Any) -> None:
        """A WebSocket message from the client: str (control) or bytes (a frame)."""
        if self.closed:
            return
        if isinstance(data, str):
            try:
                m = json.loads(data)
            except (ValueError, RecursionError):
                return self.end("protocol")
            if isinstance(m, dict) and m.get("type") == "ping":
                self.send(_PONG)
            return
        f = decode(data)
        if f is None or f.from_gateway:
            return self.end("protocol")
        c = self.conns.get(f.id)
        kind = f.type
        if kind in ("connect", "connectName"):
            if c is not None:
                return self.end("protocol")
            return self._open(f)
        if kind == "data":
            if c is None or c.state != "open" or c.shut:
                return
            if c.client_in_flight + len(f.data) > WINDOW:
                return self.end("protocol")
            c.client_in_flight += len(f.data)
            c.last = self.now()
            c.queue.put_nowait(f.data)
            return
        if kind == "shutdown":
            if c is None or c.state != "open" or c.shut:
                return
            c.shut = True
            c.queue.put_nowait(None)
            return
        if kind == "close":
            if c is not None:
                self.drop(c, False, bool(f.reset))
            return
        if kind == "ack":
            if c is None:
                return
            if f.n > c.out_in_flight:
                return self.end("protocol")
            c.out_in_flight -= f.n
            c.last = self.now()
            c.wake.set()
            return
        if kind == "dns":
            return self._dns(f.id, f.msg)
        return self.end("protocol")

    def _fail(self, c: _Conn, reason: str) -> None:
        """A connection that never opened: forgotten, and REFUSED to the client."""
        if c.closed:
            return
        self.drop(c, False, False)
        self.refuse(c.id, reason)

    def _open(self, f: Frame) -> None:
        if len(self.conns) >= self.lane_conns:
            return self.refuse(f.id, "limit")
        if not port_allowed(f.port):
            return self.refuse(f.id, "port")
        # Reserve the id at once: frames for it may arrive while we resolve.
        c = _Conn(f.id, self.now())
        self.conns[f.id] = c
        if f.type != "connectName":
            return self._dial(c, f.ip, f.port)
        if not self.ledger.dns().ok:
            return self._fail(c, "limit")
        self.counters["dns"] += 1
        self.stats.dns += 1
        c.tasks.append(self._spawn(self._resolve(c, f.name, f.port)))

    async def _resolve(self, c: _Conn, name: str, port: int) -> None:
        try:
            addrs = await self.resolver(name)
        except Exception:  # noqa: BLE001 - any failed lookup
            return self._fail(c, "resolve")
        if c.closed:
            return
        if not addrs:
            return self._fail(c, "resolve")
        # Every address the name resolves to must be allowed.
        if any(address_blocked(a) is not None for a in addrs):
            return self._fail(c, "address")
        self._dial(c, addrs[0], port)

    def _dial(self, c: _Conn, ip: Any, port: Any) -> None:
        """The destination policy, on the address literal about to be
        connected to; no socket exists before it has passed."""
        why = check_destination(ip, port)
        if why:
            return self._fail(c, why)
        if not self.ledger.open_conn().ok:
            return self._fail(c, "limit")
        c.counted = True
        c.tasks.append(self._spawn(self._establish(c, ip, port)))

    async def _establish(self, c: _Conn, ip: str, port: int) -> None:
        try:
            sock = await asyncio.wait_for(self.connect(ip, port), self.connect_timeout_ms / 1000)
        except Exception:  # noqa: BLE001 - refused, unreachable, timed out
            return self._fail(c, "unreachable")
        if c.closed:
            try:
                sock.close()
            except Exception:  # noqa: BLE001
                pass
            return
        c.socket = sock
        c.state = "open"
        c.last = self.now()
        self.counters["conns"] += 1
        self.stats.conns += 1
        self.stats.conns_total += 1
        self.send(encode.connected(c.id))
        c.tasks.append(self._spawn(self._writer(c)))
        c.tasks.append(self._spawn(self._pump(c)))

    async def _writer(self, c: _Conn) -> None:
        """Guest → remote, in order; each chunk acknowledged once written."""
        try:
            while True:
                data = await c.queue.get()
                if c.closed:
                    return
                if data is None:
                    await c.socket.shutdown()
                    c.write_done = True
                    self._maybe_finish(c)
                    return
                if not await self.spend(len(data)):
                    return
                await c.socket.write(data)
                if c.closed:
                    return
                c.client_in_flight -= len(data)
                self.counters["bytesToRemote"] += len(data)
                self.stats.bytes_to_remote += len(data)
                self.send(encode.ack(c.id, len(data), True))
        except Exception:  # noqa: BLE001 - the socket failed
            self.drop(c, True, True)

    async def _room(self, c: _Conn) -> bool:
        while c.out_in_flight >= WINDOW and not c.closed:
            c.wake.clear()
            await c.wake.wait()
        return not c.closed

    async def _pump(self, c: _Conn) -> None:
        """Remote → guest, within the window the client acknowledges."""
        try:
            while True:
                if not await self._room(c):
                    return
                chunk = await c.socket.read(min(MAX_DATA, WINDOW - c.out_in_flight))
                if not chunk:
                    break
                if not await self.spend(len(chunk)) or c.closed:
                    return
                c.out_in_flight += len(chunk)
                c.last = self.now()
                self.counters["bytesToGuest"] += len(chunk)
                self.stats.bytes_to_guest += len(chunk)
                self.send(encode.data(c.id, chunk, True))
            if c.closed:
                return
            c.read_done = True
            self.send(encode.shutdown(c.id, True))
            self._maybe_finish(c)
        except Exception:  # noqa: BLE001 - the socket failed
            self.drop(c, True, True)

    def _maybe_finish(self, c: _Conn) -> None:
        """Both directions finished: the socket can go (the client's CLOSE
        comes later)."""
        if c.read_done and c.write_done:
            self.drop(c, False, False)

    def _dns_fail(self, id: int, msg: bytes, rcode: int) -> None:
        self.send(encode.dns_answer(id, dns.fail_answer(msg, rcode)))

    def _dns(self, id: int, msg: bytes) -> None:
        q = dns.parse_query(msg)
        if q is None:
            return self._dns_fail(id, msg, dns.RCODE_FORMERR)
        d = self.ledger.dns()
        if not d.ok:
            if d.reason == "dns":
                return self._dns_fail(id, msg, dns.RCODE_REFUSED)
            return self.end(str(d.reason))
        self._spawn(self._dns_answer(id, msg, q, d.wait_ms))

    async def _dns_answer(self, id: int, msg: bytes, q: dns.Query, wait_ms: int) -> None:
        if wait_ms > 0:
            await asyncio.sleep(wait_ms / 1000)
        if self.closed:
            return
        self.counters["dns"] += 1
        self.stats.dns += 1
        try:
            answer = await dns.answer(msg, q, self.resolver)
        except Exception:  # noqa: BLE001 - any failed lookup
            return self._dns_fail(id, msg, dns.RCODE_SERVFAIL)
        answer = dns.fit_udp(msg, answer)
        if not await self.spend(len(answer)):
            return
        self.send(encode.dns_answer(id, answer))

    def _spawn(self, coro: Awaitable[Any]) -> "asyncio.Future[Any]":
        t = asyncio.ensure_future(coro)
        self._tasks.add(t)
        t.add_done_callback(self._done)
        return t

    def _done(self, t: "asyncio.Future[Any]") -> None:
        self._tasks.discard(t)
        if t.cancelled() or t.exception() is None:
            return
        # A bug of ours. Counted, never printed with its message (it could
        # name a destination); the lane ends so that the page starts again.
        self.stats.errors += 1
        self.end("error")
