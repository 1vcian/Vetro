"""The network gateway as a local server (ADR 0050): asyncio streams for the
HTTP side, the WebSocket lanes and the outbound TCP connections; the
session ledgers in memory. The Python counterpart of relay/src/node.mjs.

It listens on the loopback address only. What the page may reach through it
is decided by the destination policy (policy.py), applied in the lane before
any socket is opened.
"""

from __future__ import annotations

import asyncio
import http
import socket
import struct
import sys
import time
from typing import Any, Awaitable, Callable, Dict, List, Mapping, Optional, Set, Tuple

from . import ws
from .dns import Resolve, SystemResolver
from .gateway import Config, Request, Response, check_lane, handle_http
from .lane import Lane, Stats
from .ledger import Ledger
from .tokens import SESSION_MAX_S, TOKEN_TTL_S

LOOPBACK = "127.0.0.1"
DEFAULT_PORT = 8787
#: Largest request head (request line and headers).
MAX_HEAD = 16 * 1024
#: Largest request body (a session request is a few hundred bytes).
MAX_BODY = 4096
#: A request must arrive within this long (s).
REQUEST_TIMEOUT = 10.0
#: After our close frame, the client's is awaited this long (s).
CLOSE_WAIT = 2.0

Connect = Callable[[str, int], Awaitable[Any]]


class TcpSocket:
    """An outbound TCP connection in the shape the lane uses."""

    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self._reader = reader
        self._writer = writer
        # `write` is over when the bytes have left for the kernel: nothing is
        # acknowledged to the guest while it still waits here.
        writer.transport.set_write_buffer_limits(high=0)

    async def read(self, n: int) -> bytes:
        return await self._reader.read(n)

    async def write(self, data: bytes) -> None:
        self._writer.write(data)
        await self._writer.drain()

    async def shutdown(self) -> None:
        """The guest closed its direction: FIN after the data."""
        await self._writer.drain()
        if self._writer.can_write_eof():
            self._writer.write_eof()

    def close(self) -> None:
        self._writer.transport.abort()

    def reset(self) -> None:
        """Closes with a TCP reset (linger 0), as the guest's abort asks."""
        sock = self._writer.get_extra_info("socket")
        if sock is not None:
            try:
                linger = struct.pack("HH", 1, 0) if sys.platform == "win32" else struct.pack("ii", 1, 0)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
            except (OSError, ValueError):
                pass
        self._writer.transport.abort()


async def tcp_connect(ip: str, port: int) -> TcpSocket:
    """Connects to an IPv4 address literal (already checked by the lane)."""
    reader, writer = await asyncio.open_connection(ip, port, family=socket.AF_INET)
    return TcpSocket(reader, writer)


class BadRequest(Exception):
    def __init__(self, status: int):
        super().__init__(str(status))
        self.status = status


async def read_request(reader: asyncio.StreamReader) -> Request:
    """One HTTP/1.1 request: head and (with Content-Length) body. Raises
    `BadRequest` with the status to answer."""
    try:
        head = await reader.readuntil(b"\r\n\r\n")
    except asyncio.LimitOverrunError:
        raise BadRequest(431) from None
    if len(head) > MAX_HEAD:
        raise BadRequest(431)
    lines = head[:-4].decode("latin-1").split("\r\n")
    parts = lines[0].split(" ")
    if len(parts) != 3 or not parts[2].startswith("HTTP/1.") or not parts[1].startswith("/"):
        raise BadRequest(400)
    headers: Dict[str, str] = {}
    for line in lines[1:]:
        name, sep, value = line.partition(":")
        if not sep or not name or name != name.strip():
            raise BadRequest(400)
        key = name.lower()
        value = value.strip()
        headers[key] = headers[key] + ", " + value if key in headers else value
    body = b""
    if "transfer-encoding" in headers:
        raise BadRequest(411)
    if "content-length" in headers:
        raw = headers["content-length"]
        if not (raw.isascii() and raw.isdigit()) or len(raw) > 9:
            raise BadRequest(400)
        if int(raw) > MAX_BODY:
            raise BadRequest(413)
        try:
            body = await reader.readexactly(int(raw))
        except asyncio.IncompleteReadError:
            raise BadRequest(400) from None
    return Request(parts[0], parts[1], headers, body)


def http_bytes(response: Response) -> bytes:
    """The response on the wire. Every connection carries one request."""
    try:
        phrase = http.HTTPStatus(response.status).phrase
    except ValueError:
        phrase = "Status"
    lines = ["HTTP/1.1 %d %s" % (response.status, phrase)]
    lines += ["%s: %s" % kv for kv in response.headers.items()]
    if response.status != 204:  # "No Content" carries no length either
        lines.append("content-length: %d" % len(response.body))
    lines += ["connection: close", "", ""]
    return "\r\n".join(lines).encode("latin-1") + response.body


class Gateway:
    """The gateway: `await start()`, then `await close()`.

    - `origins`: the origins that may ask for a session;
    - `limits`: the session ledger's (ledger.limits_from);
    - `connect`, `resolver`: the socket factory and the name resolver (the
      real ones by default; the tests give their own);
    - `log`: called with each lane's final counters;
    - `lane_options`: more arguments for every Lane (tests).
    """

    def __init__(
        self,
        *,
        secret: str,
        origins: List[str],
        limits: Mapping[str, Any],
        lane_conns: int = 32,
        connect: Optional[Connect] = None,
        resolver: Optional[Resolve] = None,
        log: Callable[[Dict[str, Any]], None] = lambda record: None,
        lane_options: Optional[Dict[str, Any]] = None,
        now: Callable[[], float] = lambda: time.time() * 1000,
    ):
        self.cfg = Config(secret=secret, origins=list(origins), lane_conns=lane_conns)
        self.limits = dict(limits)
        self.stats = Stats()
        self.sessions: Dict[str, Ledger] = {}
        self.port = 0
        self._born: Dict[str, float] = {}
        self._dial = connect or tcp_connect
        self._resolver = resolver or SystemResolver()
        self._log = log
        self._lane_options = lane_options or {}
        self._now = now
        self._server: Optional[asyncio.AbstractServer] = None
        self._handlers: Set["asyncio.Task[Any]"] = set()

    @property
    def url(self) -> str:
        return "http://%s:%d" % (LOOPBACK, self.port)

    async def start(self, port: int = DEFAULT_PORT, *, fallback: bool = False) -> int:
        """Listens on the loopback address. `fallback`: any free port if
        `port` is taken. Returns the port."""
        try:
            self._server = await asyncio.start_server(self._serve, LOOPBACK, port, limit=MAX_HEAD)
        except OSError:
            if not fallback or port == 0:
                raise
            self._server = await asyncio.start_server(self._serve, LOOPBACK, 0, limit=MAX_HEAD)
        self.port = self._server.sockets[0].getsockname()[1]
        return self.port

    async def close(self) -> None:
        if self._server is None:
            return
        self._server.close()
        for task in list(self._handlers):
            task.cancel()
        if self._handlers:
            await asyncio.gather(*self._handlers, return_exceptions=True)
        try:
            await asyncio.wait_for(self._server.wait_closed(), CLOSE_WAIT)
        except asyncio.TimeoutError:
            pass
        self._server = None

    async def _connect(self, ip: str, port: int) -> Any:
        self.stats.sockets += 1
        return await self._dial(ip, port)

    def _ledger_of(self, sid: str) -> Ledger:
        ledger = self.sessions.get(sid)
        if ledger is None:
            # Sessions whose tokens can no longer be valid, with nothing open.
            limit = self._now() - (SESSION_MAX_S + 2 * TOKEN_TTL_S) * 1000
            for old in [s for s, born in self._born.items() if born < limit]:
                c = self.sessions[old].counters()
                if not c["lanes"] and not c["conns"]:
                    del self.sessions[old], self._born[old]
            ledger = Ledger(self.limits, enabled=lambda: self.cfg.enabled)
            self.sessions[sid] = ledger
            self._born[sid] = self._now()
        return ledger

    async def _serve(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.current_task()
        assert task is not None
        self._handlers.add(task)
        try:
            try:
                request = await asyncio.wait_for(read_request(reader), REQUEST_TIMEOUT)
            except BadRequest as e:
                writer.write(http_bytes(Response(e.status)))
                return
            except (asyncio.TimeoutError, asyncio.IncompleteReadError, ConnectionError):
                return
            h = request.headers
            if "upgrade" in h and "upgrade" in [t.strip().lower() for t in h.get("connection", "").split(",")]:
                await self._upgrade(request, reader, writer)
            else:
                response = handle_http(request, self.cfg, now=self._now(), lanes=self.limits["lanes"])
                if response.status == 200 and request.path == "/session":
                    self.stats.sessions += 1  # the page has reached the gateway
                writer.write(http_bytes(response))
        except (ConnectionError, OSError):
            pass
        except asyncio.CancelledError:
            # The gateway is closing (`close`): this connection ends here. The
            # task finishes instead of staying cancelled, which some asyncio
            # versions report as an error of the connection callback.
            pass
        except Exception:  # noqa: BLE001 - a bug of ours: counted, its message never printed
            self.stats.errors += 1
        finally:
            self._handlers.discard(task)
            await _finish(writer)

    async def _upgrade(self, request: Request, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        def refuse(status: int, why: str) -> None:
            writer.write(("HTTP/1.1 %d %s\r\nconnection: close\r\ncontent-length: 0\r\n\r\n" % (status, why)).encode("latin-1"))

        checked = check_lane(request, self.cfg, now=self._now(), lanes=self.limits["lanes"])
        if "error" in checked:
            return refuse(checked["status"], checked["error"])
        # The WebSocket handshake itself, before the lane is taken in the ledger.
        h = request.headers
        if request.method != "GET" or h.get("sec-websocket-version") != "13" or not ws.valid_key(h.get("sec-websocket-key")):
            return refuse(400, "handshake")
        ledger = self._ledger_of(checked["sid"])
        opened = ledger.open_lane(checked["lane"])
        if not opened.ok:
            return refuse(503 if opened.reason == "disabled" else 409 if opened.reason == "lane-in-use" else 429, str(opened.reason))
        writer.write(
            (
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n"
                % ws.accept_key(h["sec-websocket-key"])
            ).encode("latin-1")
        )
        await self._lane(reader, writer, ledger, checked["lane"])

    async def _lane(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter, ledger: Ledger, k: int) -> None:
        loop = asyncio.get_running_loop()
        closing = False

        def send(data: Any) -> None:
            if closing or writer.transport.is_closing():
                raise ConnectionError("the WebSocket is closed")
            if isinstance(data, str):
                writer.write(ws.encode_frame(ws.OP_TEXT, data.encode("utf-8")))
            else:
                writer.write(ws.encode_frame(ws.OP_BINARY, data))

        def close(code: int, reason: str) -> None:
            nonlocal closing
            if closing or writer.transport.is_closing():
                return
            closing = True
            writer.write(ws.encode_frame(ws.OP_CLOSE, ws.close_payload(code, reason)))
            # The client answers with its own close frame; if it does not, the
            # connection goes anyway.
            loop.call_later(CLOSE_WAIT, writer.close)

        lane = Lane(
            send=send,
            close=close,
            connect=self._connect,
            resolver=self._resolver,
            ledger=ledger,
            lane=k,
            lane_conns=self.cfg.lane_conns,
            limits=self.limits,
            log=self._log,
            stats=self.stats,
            **self._lane_options,
        )
        parser = ws.Parser()
        try:
            lane.start()
            done = False
            while not done:
                data = await reader.read(64 * 1024)
                if not data:
                    break
                try:
                    messages = parser.feed(data)
                except ws.WsError as e:
                    if not closing:
                        writer.write(ws.encode_frame(ws.OP_CLOSE, ws.close_payload(e.code, e.reason)))
                    break
                for opcode, payload in messages:
                    if opcode in (ws.OP_TEXT, ws.OP_BINARY):
                        if not closing:
                            lane.on_message(payload)
                    elif opcode == ws.OP_PING:
                        if not closing:
                            writer.write(ws.encode_frame(ws.OP_PONG, payload))
                    elif opcode == ws.OP_CLOSE:
                        if not closing:
                            writer.write(ws.encode_frame(ws.OP_CLOSE, ws.close_payload(payload[0])))
                        done = True
                        break
        finally:
            lane.on_close()


async def _finish(writer: asyncio.StreamWriter) -> None:
    """Flushes what is left and closes the connection."""
    try:
        writer.close()
        await asyncio.wait_for(writer.wait_closed(), CLOSE_WAIT)
    except (asyncio.TimeoutError, asyncio.CancelledError, ConnectionError, OSError):
        writer.transport.abort()


def mapped_connect(mapping: Mapping[Tuple[str, int], Tuple[str, int]], dial: Connect = tcp_connect) -> Connect:
    """TEST ONLY. A socket factory that sends some destinations to local
    servers. It sits behind the lane, so the destination policy has already
    passed on the address the guest asked for."""

    async def connect(ip: str, port: int) -> Any:
        return await dial(*mapping.get((ip, port), (ip, port)))

    return connect


def table_resolver(hosts: Mapping[str, List[str]]) -> Resolve:
    """TEST ONLY. A resolver from a table (`*` = any other name) instead of
    the system's: a name that is not in it has no address."""

    async def resolve(name: str) -> List[str]:
        return list(hosts.get(name, hosts.get("*", [])))

    return resolve
