"""The gateway as a server, over real loopback sockets: the HTTP surface,
the WebSocket upgrade and its refusals, a lane with real TCP behind it
(echo, half close, reset), violations of the WebSocket protocol, the port
fallback. The client here is made of the same codec, masked as a browser's.
"""

import asyncio
import json
import os
import socket
import time
import unittest
from unittest import mock

from vetro_connect import server, ws
from vetro_connect.dns import answer_addrs, build_query
from vetro_connect.ledger import limits_from
from vetro_connect.protocol import MAX_DATA, WINDOW, decode, encode
from vetro_connect.server import Gateway, mapped_connect, table_resolver, tcp_connect
from vetro_connect.tokens import SESSION_MAX_S, TOKEN_TTL_S, issue

SECRET = "k" * 40
ORIGIN = "http://127.0.0.1:8080"
PUBLIC = "93.184.215.14"
KEY = "dGhlIHNhbXBsZSBub25jZQ=="


def request(method, target, headers=None, body=b""):
    lines = ["%s %s HTTP/1.1" % (method, target), "Host: 127.0.0.1"]
    lines += ["%s: %s" % kv for kv in (headers or {}).items()]
    if body:
        lines.append("Content-Length: %d" % len(body))
    return ("\r\n".join(lines) + "\r\n\r\n").encode("latin-1") + body


def parse_head(head):
    lines = head.decode("latin-1").split("\r\n")
    return int(lines[0].split(" ")[1]), {k.lower(): v.strip() for k, v in (line.split(":", 1) for line in lines[1:] if line)}


async def http(port, raw):
    """Sends `raw`, reads until the server closes: (status, headers, body)."""
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    writer.write(raw)
    data = await asyncio.wait_for(reader.read(-1), 5)
    writer.close()
    if not data:
        return None, {}, b""
    head, _, body = data.partition(b"\r\n\r\n")
    status, headers = parse_head(head)
    return status, headers, body


class WsClient:
    def __init__(self, reader, writer):
        self.reader = reader
        self.writer = writer
        self.parser = ws.Parser(require_mask=False)
        self.pending = []

    @classmethod
    async def open(cls, port, target, headers=None):
        """(status, client): the client only if the upgrade was accepted."""
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        h = {"Upgrade": "websocket", "Connection": "Upgrade", "Sec-WebSocket-Key": KEY, "Sec-WebSocket-Version": "13", "Origin": ORIGIN}
        h.update(headers or {})
        writer.write(request("GET", target, {k: v for k, v in h.items() if v is not None}))
        head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
        status, got = parse_head(head[:-4])
        if status != 101:
            writer.close()
            return status, None
        assert got["sec-websocket-accept"] == ws.accept_key(KEY), got
        assert got["upgrade"].lower() == "websocket" and got["connection"].lower() == "upgrade", got
        return status, cls(reader, writer)

    def send(self, data, **how):
        opcode = ws.OP_TEXT if isinstance(data, str) else ws.OP_BINARY
        payload = data.encode() if isinstance(data, str) else data
        self.raw(ws.encode_frame(how.pop("opcode", opcode), payload, mask=os.urandom(4), **how))

    def raw(self, data):
        self.writer.write(data)

    async def recv(self, seconds=5.0):
        """The next message (opcode, payload); EOFError when the server closed."""
        while not self.pending:
            data = await asyncio.wait_for(self.reader.read(65536), seconds)
            if not data:
                raise EOFError
            self.pending += self.parser.feed(data)
        return self.pending.pop(0)

    async def frame(self, seconds=5.0):
        """The next protocol frame, decoded."""
        opcode, payload = await self.recv(seconds)
        assert opcode == ws.OP_BINARY, (opcode, payload)
        return decode(payload)

    async def text(self):
        opcode, payload = await self.recv()
        assert opcode == ws.OP_TEXT, (opcode, payload)
        return json.loads(payload)

    async def closed(self, seconds=5.0):
        """Reads to the end of the connection; True if it ended."""
        try:
            while True:
                await self.recv(seconds)
        except EOFError:
            return True
        except ConnectionError:
            return True

    def close(self):
        self.writer.close()


class Remote:
    """A local TCP server playing the remote site. Modes: echo (ends after
    the client's FIN), greet (says hi and closes its side, still reading),
    slow (closes its side at once, reads nothing until `gate` is set, then
    reads slowly; small socket buffers, so that what it does not read backs
    up into the gateway)."""

    def __init__(self, mode="echo"):
        self.mode = mode
        self.received = bytearray()
        self.accepted = 0
        self.ends = []  # how each connection ended: "fin" or "reset"
        self.server = None
        self.gate = asyncio.Event()

    async def start(self):
        listener = socket.socket()
        if self.mode == "slow":
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 16 * 1024)
        listener.bind(("127.0.0.1", 0))
        self.server = await asyncio.start_server(self._serve, sock=listener)
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    async def _serve(self, reader, writer):
        self.accepted += 1
        try:
            if self.mode == "greet":
                writer.write(b"hi")
                writer.write_eof()
            if self.mode == "slow":
                writer.write_eof()
                await self.gate.wait()
            while True:
                data = await reader.read(4096 if self.mode == "slow" else 65536)
                if not data:
                    break
                self.received += data
                if self.mode == "slow":
                    await asyncio.sleep(0.0005)
                if self.mode == "echo":
                    writer.write(data)
                    await writer.drain()
            self.ends.append("fin")
        except ConnectionError:
            self.ends.append("reset")
        except asyncio.CancelledError:
            pass  # the test is over
        finally:
            writer.close()

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()


async def until(cond, what, seconds=5.0):
    end = asyncio.get_running_loop().time() + seconds
    while not cond():
        if asyncio.get_running_loop().time() > end:
            raise AssertionError("timed out: " + what)
        await asyncio.sleep(0.005)


class ServerCase(unittest.IsolatedAsyncioTestCase):
    async def gateway(self, mapping=None, hosts=None, env=None, **options):
        gw = Gateway(
            secret=SECRET,
            origins=[ORIGIN],
            limits=limits_from(env or {}),
            connect=mapped_connect(mapping or {}),
            resolver=table_resolver(hosts or {}),
            **options,
        )
        await gw.start(0)
        self.addAsyncCleanup(gw.close)
        return gw

    async def remote(self, mode="echo"):
        r = await Remote(mode).start()
        self.addAsyncCleanup(r.stop)
        return r

    def token(self, sid="s", origin=ORIGIN, at=None):
        return issue(SECRET, sid=sid, origin=origin, now=int(time.time()) if at is None else at)

    async def lane(self, gw, k=0, sid="s"):
        status, c = await WsClient.open(gw.port, "/lane?token=%s&lane=%d" % (self.token(sid), k))
        self.assertEqual(status, 101)
        self.addCleanup(c.close)
        hello = await c.text()
        self.assertEqual(hello["type"], "hello")
        return c, hello


class Http(ServerCase):
    async def test_the_http_surface(self):
        gw = await self.gateway()
        status, headers, body = await http(gw.port, request("GET", "/health"))
        self.assertEqual((status, json.loads(body)), (200, {"ok": True, "enabled": True}))
        self.assertEqual((headers["connection"], headers["content-length"], headers["cache-control"]), ("close", str(len(body)), "no-store"))
        self.assertEqual(gw.stats.sessions, 0)
        status, headers, body = await http(gw.port, request("POST", "/session", {"Origin": ORIGIN, "Content-Type": "application/json"}, b"{}"))
        self.assertEqual(status, 200)
        self.assertEqual((headers["access-control-allow-origin"], headers["vary"], headers["content-type"]), (ORIGIN, "Origin", "application/json"))
        s = json.loads(body)
        self.assertEqual((s["lanes"], s["laneConns"]), (8, 32))
        self.assertAlmostEqual(s["expires"], (time.time() + TOKEN_TTL_S) * 1000, delta=5000)
        self.assertEqual(gw.stats.sessions, 1, "the page has reached the gateway")
        # The token it gave opens a lane; renewing keeps the session.
        status, c = await WsClient.open(gw.port, "/lane?token=%s&lane=0" % s["token"])
        self.assertEqual(status, 101)
        c.close()
        status, _, body = await http(gw.port, request("POST", "/session", {"Origin": ORIGIN}, json.dumps({"renew": s["token"]}).encode()))
        self.assertEqual(status, 200)
        self.assertEqual(len(gw.sessions), 1)
        # Refusals, each on its own connection.
        for raw, want in [
            (request("POST", "/session", {"Origin": "https://evil.example"}, b"{}"), 403),
            (request("POST", "/session", {}, b"{}"), 403),
            (request("GET", "/session", {"Origin": ORIGIN}), 405),
            (request("GET", "/"), 404),
            (request("GET", "/lane?token=x&lane=0", {"Origin": ORIGIN}), 404),
            (request("POST", "/session", {"Origin": ORIGIN}, b"{"), 400),
        ]:
            status, headers, _ = await http(gw.port, raw)
            self.assertEqual((status, headers["connection"]), (want, "close"), raw[:40])
        self.assertEqual(gw.stats.sessions, 2, "only the two that got a token")
        status, headers, body = await http(gw.port, request("OPTIONS", "/session", {"Origin": ORIGIN, "Access-Control-Request-Method": "POST"}))
        self.assertEqual((status, body, headers["access-control-allow-origin"], headers["access-control-allow-methods"]), (204, b"", ORIGIN, "POST"))
        self.assertNotIn("content-length", headers)
        gw.cfg.enabled = False
        self.assertEqual((await http(gw.port, request("POST", "/session", {"Origin": ORIGIN}, b"{}")))[0], 503)
        self.assertEqual(json.loads((await http(gw.port, request("GET", "/health")))[2]), {"ok": True, "enabled": False})
        self.assertEqual(gw.stats.errors, 0)

    async def test_requests_that_are_not_http_or_too_big(self):
        gw = await self.gateway()
        for raw, want in [
            (b"hello\r\n\r\n", 400),
            (b"GET /health\r\n\r\n", 400),
            (b"GET /health HTTP/2\r\n\r\n", 400),
            (b"GET health HTTP/1.1\r\n\r\n", 400),
            (b"GET /health HTTP/1.1\r\nno colon here\r\n\r\n", 400),
            (b"GET /health HTTP/1.1\r\n folded: x\r\n\r\n", 400),
            (b"POST /session HTTP/1.1\r\nContent-Length: abc\r\n\r\n", 400),
            (b"POST /session HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 400),
            (b"POST /session HTTP/1.1\r\nOrigin: " + ORIGIN.encode() + b"\r\nContent-Length: 4097\r\n\r\n" + bytes(4097), 413),
            (b"POST /session HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n", 411),
            (b"GET /health HTTP/1.1\r\nX-Big: " + b"a" * server.MAX_HEAD + b"\r\n\r\n", 431),
        ]:
            status, _, _ = await http(gw.port, raw)
            self.assertEqual(status, want, raw[:50])
        # A body up to the limit is read (and then judged as a session request).
        ok = b"POST /session HTTP/1.1\r\nOrigin: " + ORIGIN.encode() + b"\r\nContent-Length: 4096\r\n\r\n" + b" " * 4096
        self.assertEqual((await http(gw.port, ok))[0], 400)
        # Half a request, then silence or the end: no answer, and the server lives.
        with mock.patch.object(server, "REQUEST_TIMEOUT", 0.05):
            self.assertEqual(await http(gw.port, b"GET /health HTTP/1.1\r\n"), (None, {}, b""))
        reader, writer = await asyncio.open_connection("127.0.0.1", gw.port)
        writer.write(b"POST /session HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc")
        writer.write_eof()
        self.assertEqual(parse_head((await reader.read(-1)).partition(b"\r\n\r\n")[0])[0], 400)
        writer.close()
        self.assertEqual((await http(gw.port, request("GET", "/health")))[0], 200)
        self.assertEqual(gw.stats.errors, 0)


class Upgrade(ServerCase):
    async def test_upgrade_refusals(self):
        gw = await self.gateway(env={"LIMIT_LANES": "2"})
        good = "/lane?token=%s&lane=" % self.token()

        async def status(target, headers=None):
            return (await WsClient.open(gw.port, target, headers))[0]

        self.assertEqual(await status("/lane?lane=0"), 401)
        self.assertEqual(await status("/lane?token=garbage&lane=0"), 401)
        self.assertEqual(await status("/lane?token=%s&lane=0" % self.token(at=int(time.time()) - TOKEN_TTL_S - 1)), 401, "expired")
        self.assertEqual(await status("/lane?token=%s&lane=0" % issue("z" * 40, sid="s", origin=ORIGIN, now=int(time.time()))), 401, "another secret")
        self.assertEqual(await status(good + "0", {"Origin": "https://evil.example"}), 403)
        self.assertEqual(await status(good + "0", {"Origin": None}), 403)
        self.assertEqual(await status(good + "2"), 400, "beyond the session's lanes")
        self.assertEqual(await status(good + "x"), 400)
        self.assertEqual(await status("/other?token=x"), 404)
        self.assertEqual(await status(good + "0", {"Upgrade": "h2c"}), 426)
        # The WebSocket handshake itself.
        self.assertEqual(await status(good + "0", {"Sec-WebSocket-Key": None}), 400)
        self.assertEqual(await status(good + "0", {"Sec-WebSocket-Key": "short"}), 400)
        self.assertEqual(await status(good + "0", {"Sec-WebSocket-Version": "8"}), 400)
        self.assertEqual(gw.sessions, {}, "a refused upgrade takes no lane")
        # An upgrade without `Connection: upgrade` is a plain request: no such page.
        self.assertEqual(await status(good + "0", {"Connection": "keep-alive"}), 404)
        # One lane per index, and no more lanes than the session's limit.
        a, _ = await self.lane(gw, 0)
        self.assertEqual(await status(good + "0"), 409)
        b, _ = await self.lane(gw, 1)
        self.assertEqual(gw.sessions["s"].counters()["lanes"], 2)
        other, _ = await self.lane(gw, 0, sid="another-session")
        self.assertEqual(len(gw.sessions), 2)
        a.close()
        await until(lambda: gw.sessions["s"].counters()["lanes"] == 1, "lane 0 released")
        c, _ = await self.lane(gw, 0)
        gw.cfg.enabled = False
        self.assertEqual(await status(good + "0"), 503)
        self.assertEqual(gw.stats.errors, 0)

    async def test_the_lane_limit_of_the_session(self):
        gw = await self.gateway(env={"LIMIT_LANES": "3"})
        # A token for lane 2 of 3, with lanes 0 and 1 taken by... nobody: any
        # free index below the limit opens.
        c, hello = await self.lane(gw, 2)
        self.assertEqual((hello["lane"], hello["laneConns"], hello["window"], hello["maxData"], hello["v"]), (2, 32, WINDOW, MAX_DATA, 1))
        self.assertEqual(hello["limits"], {"conns": 32, "lanes": 3, "rate": None, "burst": None, "bytes": None, "dnsRate": 20, "dnsBurst": 100})
        # The ledger refuses a fourth lane even if the index were allowed.
        ledger = gw.sessions["s"]
        ledger.limits["lanes"] = 1
        status, _ = await WsClient.open(gw.port, "/lane?token=%s&lane=0" % self.token())
        self.assertEqual(status, 429)


class Lanes(ServerCase):
    async def test_a_lane_with_real_tcp_behind_it(self):
        echo = await self.remote()
        gw = await self.gateway(mapping={(PUBLIC, 443): ("127.0.0.1", echo.port)}, hosts={"vetro.example": [PUBLIC]})
        c, _ = await self.lane(gw)
        self.assertEqual((gw.stats.lanes, gw.stats.conns), (1, 0))
        # Control: the page's ping (text), and the WebSocket's own.
        c.send('{"type":"ping"}')
        self.assertEqual(await c.text(), {"type": "pong"})
        c.send(b"are you there", opcode=ws.OP_PING)
        self.assertEqual(await c.recv(), (ws.OP_PONG, b"are you there"))
        c.send(b"unsolicited", opcode=ws.OP_PONG)
        # A CONNECT in two fragments with a ping between them.
        frame = encode.connect(1, PUBLIC, 443)
        c.send(frame[:4], fin=False)
        c.send(b"", opcode=ws.OP_PING)
        c.send(frame[4:], opcode=ws.OP_CONT)
        self.assertEqual(await c.recv(), (ws.OP_PONG, b""))
        self.assertEqual((await c.frame()).type, "connected")
        self.assertEqual((gw.stats.conns, gw.stats.sockets, echo.accepted), (1, 1, 1))
        # 300 kB there and back: more than a window, so acks both ways.
        data = bytes((i * 31) & 255 ^ (i >> 9) & 255 for i in range(300_000))
        sent = 0
        credit = WINDOW
        back = bytearray()
        fin = False
        done = False
        while not done:
            while sent < len(data) and credit > 0:
                n = min(MAX_DATA, credit, len(data) - sent)
                c.send(encode.data(1, data[sent : sent + n], False))
                sent += n
                credit -= n
            if sent == len(data) and not fin:
                c.send(encode.shutdown(1, False))
                fin = True
            f = await c.frame()
            if f.type == "ack":
                credit += f.n
            elif f.type == "data":
                self.assertLessEqual(len(f.data), MAX_DATA)
                back += f.data
                c.send(encode.ack(1, len(f.data), False))
            elif f.type == "shutdown":
                done = True
            else:
                self.fail("unexpected %r" % f)
        self.assertEqual(bytes(back), data)
        self.assertEqual(bytes(echo.received), data)
        self.assertEqual((gw.stats.bytes_to_remote, gw.stats.bytes_to_guest), (len(data), len(data)))
        await until(lambda: gw.stats.conns == 0, "both directions finished: the socket goes")
        c.send(encode.close(1, False, False))
        # DNS on the same lane.
        c.send(encode.dns(9, build_query("vetro.example", 0x4242)))
        answer = await c.frame()
        self.assertEqual((answer.type, answer.id, answer.msg[:2], answer_addrs(answer.msg)), ("dnsAnswer", 9, b"\x42\x42", [PUBLIC]))
        # The client closes: the close frame comes back and the lane is released.
        c.send(ws.close_payload(1000, "done"), opcode=ws.OP_CLOSE)
        self.assertEqual(await c.recv(), (ws.OP_CLOSE, (1000, "")))
        self.assertTrue(await c.closed())
        await until(lambda: gw.stats.lanes == 0 and gw.sessions["s"].counters()["lanes"] == 0, "lane released")
        self.assertEqual(echo.ends, ["fin"])
        self.assertEqual(gw.stats.errors, 0)

    async def test_no_socket_for_a_blocked_destination_even_with_a_map(self):
        echo = await self.remote()
        to_echo = ("127.0.0.1", echo.port)
        # If the policy came after the map, these would reach the echo server.
        gw = await self.gateway(mapping={("127.0.0.1", 80): to_echo, ("192.168.1.1", 443): to_echo, ("10.0.0.1", 443): to_echo, (PUBLIC, 22): to_echo}, hosts={"lan.example": ["192.168.1.1"], "mixed.example": [PUBLIC, "10.0.0.1"]})
        c, _ = await self.lane(gw)
        c.send(encode.connect(1, "127.0.0.1", 80))
        c.send(encode.connect(2, "192.168.1.1", 443))
        c.send(encode.connect(3, PUBLIC, 22))
        c.send(encode.connect(4, "127.0.0.1", echo.port))
        c.send(encode.connect_name(5, "lan.example", 443))
        c.send(encode.connect_name(6, "mixed.example", 443))
        got = {}
        for _ in range(6):
            f = await c.frame()
            got[f.id] = f.reason
        self.assertEqual(got, {1: "address", 2: "address", 3: "port", 4: "port", 5: "address", 6: "address"})
        self.assertEqual((gw.stats.sockets, echo.accepted, gw.stats.refused), (0, 0, 6))

    async def test_half_close_from_the_remote_and_a_reset_from_the_guest(self):
        greet = await self.remote("greet")
        gw = await self.gateway(mapping={(PUBLIC, 80): ("127.0.0.1", greet.port)})
        c, _ = await self.lane(gw)
        c.send(encode.connect(1, PUBLIC, 80))
        self.assertEqual((await c.frame()).type, "connected")
        f = await c.frame()
        self.assertEqual((f.type, f.data), ("data", b"hi"))
        self.assertEqual((await c.frame()).type, "shutdown")
        # The remote has finished sending; the guest still can.
        c.send(encode.data(1, b"still here", False))
        ack = await c.frame()
        self.assertEqual((ack.type, ack.n), ("ack", 10))
        await until(lambda: bytes(greet.received) == b"still here", "received after the remote's FIN")
        c.send(encode.shutdown(1, False))
        await until(lambda: greet.ends == ["fin"], "the guest's FIN reached the remote")
        await until(lambda: gw.stats.conns == 0, "finished")
        # A connection the guest aborts ends with a TCP reset at the remote.
        c.send(encode.connect(2, PUBLIC, 80))
        self.assertEqual((await c.frame()).type, "connected")
        await until(lambda: greet.accepted == 2, "accepted")
        c.send(encode.close(2, True, False))
        await until(lambda: len(greet.ends) == 2, "ended")
        self.assertEqual(greet.ends, ["fin", "reset"])

    async def test_a_slow_remote_holds_the_guest_back_and_loses_nothing(self):
        slow = await self.remote("slow")

        async def dial(ip, port):
            s = await tcp_connect(ip, port)
            # A small kernel buffer, so that the test fills it.
            s._writer.get_extra_info("socket").setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 16 * 1024)
            return s

        gw = Gateway(secret=SECRET, origins=[ORIGIN], limits=limits_from({}), connect=mapped_connect({(PUBLIC, 80): ("127.0.0.1", slow.port)}, dial), resolver=table_resolver({}))
        await gw.start(0)
        self.addAsyncCleanup(gw.close)
        c, _ = await self.lane(gw)
        c.send(encode.connect(1, PUBLIC, 80))
        self.assertEqual((await c.frame()).type, "connected")
        self.assertEqual((await c.frame()).type, "shutdown", "the remote has nothing to say")
        data = bytes((i * 17) & 255 ^ (i >> 8) & 255 for i in range(1024 * 1024))
        sent = 0
        acked = 0

        async def exchange(quiet):
            """Sends within the window until no ACK comes for `quiet` seconds
            (or everything is acknowledged)."""
            nonlocal sent, acked
            while acked < len(data):
                while sent < len(data) and sent - acked < WINDOW:
                    n = min(MAX_DATA, WINDOW - (sent - acked), len(data) - sent)
                    c.send(encode.data(1, data[sent : sent + n], False))
                    sent += n
                try:
                    f = await c.frame(quiet)
                except asyncio.TimeoutError:
                    return
                self.assertEqual(f.type, "ack")
                acked += f.n

        # The remote reads nothing: the acknowledgements stop when the kernel
        # is full, with a whole window unacknowledged and no more.
        await exchange(0.3)
        self.assertEqual(sent - acked, WINDOW)
        self.assertLess(sent, len(data), "the remote's silence reached the guest")
        self.assertEqual(gw.stats.bytes_to_remote, acked, "acknowledged = written, not merely queued")
        # It starts reading, slowly: everything arrives, in order, then the FIN.
        slow.gate.set()
        await exchange(5)
        self.assertEqual((sent, acked), (len(data), len(data)))
        c.send(encode.shutdown(1, False))
        await until(lambda: slow.ends == ["fin"], "the guest's FIN after its last byte")
        self.assertEqual(len(slow.received), len(data))
        self.assertEqual(bytes(slow.received), data)
        await until(lambda: gw.stats.conns == 0, "finished")

    async def test_a_remote_that_refuses_and_one_that_vanishes(self):
        # A port nobody listens on.
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        dead = probe.getsockname()[1]
        probe.close()
        echo = await self.remote()
        gw = await self.gateway(mapping={(PUBLIC, 443): ("127.0.0.1", dead), (PUBLIC, 80): ("127.0.0.1", echo.port)})
        c, _ = await self.lane(gw)
        c.send(encode.connect(1, PUBLIC, 443))
        f = await c.frame()
        self.assertEqual((f.type, f.reason), ("refused", "unreachable"))
        self.assertEqual(gw.sessions["s"].counters()["conns"], 0)
        # The WebSocket goes away without a close frame: sockets closed, lane released.
        c.send(encode.connect(2, PUBLIC, 80))
        self.assertEqual((await c.frame()).type, "connected")
        c.writer.transport.abort()
        await until(lambda: gw.stats.lanes == 0 and gw.stats.conns == 0, "released")
        await until(lambda: len(echo.ends) == 1, "the remote saw the end")
        self.assertEqual(gw.sessions["s"].counters(), {"lanes": 0, "conns": 0, "bytes": 0, "ended": None})
        self.assertEqual(gw.stats.errors, 0)

    async def test_websocket_violations_close_with_their_code(self):
        gw = await self.gateway()
        cases = [
            (ws.encode_frame(ws.OP_BINARY, encode.connect(1, PUBLIC, 443)), 1002),  # not masked
            (ws.encode_frame(ws.OP_BINARY, bytes(ws.MAX_MESSAGE + 1), mask=b"abcd")[:14], 1009),  # too big, from the header
            (bytes([0xC2, 0x80]) + b"abcd", 1002),  # compressed, which nobody agreed
            (ws.encode_frame(ws.OP_TEXT, b"\xff", mask=b"abcd"), 1007),
            (ws.encode_frame(ws.OP_CONT, b"x", mask=b"abcd"), 1002),
        ]
        for k, (raw, code) in enumerate(cases):
            c, _ = await self.lane(gw, k)
            c.raw(raw)
            opcode, payload = await c.recv()
            self.assertEqual((opcode, payload[0]), (ws.OP_CLOSE, code), raw[:6])
            self.assertTrue(await c.closed())
        await until(lambda: gw.stats.lanes == 0, "lanes released")
        self.assertEqual(gw.sessions["s"].counters()["lanes"], 0)
        self.assertEqual(gw.stats.errors, 0)

    async def test_the_gateway_says_bye_then_closes(self):
        gw = await self.gateway()
        c, _ = await self.lane(gw)
        c.send(b"\x42\x00\x00\x00\x01")  # not a frame of the protocol
        self.assertEqual(await c.text(), {"type": "bye", "reason": "protocol"})
        self.assertEqual(await c.recv(), (ws.OP_CLOSE, (4002, "protocol")))
        # Frames sent after that are not acted on.
        c.send(encode.connect(1, PUBLIC, 443))
        c.send(ws.close_payload(4002), opcode=ws.OP_CLOSE)
        self.assertTrue(await c.closed())
        self.assertEqual((gw.stats.lanes, gw.stats.sockets), (0, 0))
        # A client that never answers the close frame is dropped all the same.
        with mock.patch.object(server, "CLOSE_WAIT", 0.1):
            d, _ = await self.lane(gw, 1)
            gw.cfg.enabled = False
            d.send(encode.dns(1, build_query("vetro.example", 1)))
            self.assertEqual(await d.text(), {"type": "bye", "reason": "disabled"})
            self.assertEqual(await d.recv(), (ws.OP_CLOSE, (4000, "disabled")))
            started = asyncio.get_running_loop().time()
            self.assertTrue(await d.closed())
            self.assertLess(asyncio.get_running_loop().time() - started, 2)

    async def test_closing_the_gateway_ends_its_lanes(self):
        echo = await self.remote()
        gw = await self.gateway(mapping={(PUBLIC, 443): ("127.0.0.1", echo.port)})
        c, _ = await self.lane(gw)
        c.send(encode.connect(1, PUBLIC, 443))
        self.assertEqual((await c.frame()).type, "connected")
        await gw.close()
        self.assertTrue(await c.closed())
        self.assertEqual((gw.stats.lanes, gw.stats.conns), (0, 0))
        await until(lambda: len(echo.ends) == 1, "the remote saw the end")
        with self.assertRaises(OSError):
            await asyncio.open_connection("127.0.0.1", gw.port)
        await gw.close()  # twice is fine


class Listening(unittest.IsolatedAsyncioTestCase):
    def make(self, **options):
        return Gateway(secret=SECRET, origins=[ORIGIN], limits=limits_from({}), **options)

    async def test_loopback_only_and_the_port_fallback(self):
        first = self.make()
        port = await first.start(0)
        self.addAsyncCleanup(first.close)
        self.assertEqual(first.url, "http://127.0.0.1:%d" % port)
        self.assertEqual([s.getsockname()[0] for s in first._server.sockets], ["127.0.0.1"], "never on the network's addresses")
        # The port is taken: an error, or with the fallback another free one.
        second = self.make()
        with self.assertRaises(OSError):
            await second.start(port)
        got = await second.start(port, fallback=True)
        self.addAsyncCleanup(second.close)
        self.assertNotEqual(got, port)
        self.assertEqual((await http(got, request("GET", "/health")))[0], 200)
        self.assertEqual(server.DEFAULT_PORT, 8787)

    async def test_old_sessions_are_forgotten(self):
        clock = [1_000_000_000.0]
        gw = self.make(now=lambda: clock[0])
        a = gw._ledger_of("a")
        self.assertIs(gw._ledger_of("a"), a)
        b = gw._ledger_of("b")
        self.assertTrue(b.open_lane(0).ok)
        clock[0] += (SESSION_MAX_S + 2 * TOKEN_TTL_S) * 1000 + 1
        gw._ledger_of("c")
        self.assertEqual(sorted(gw.sessions), ["b", "c"], "the one with a lane open stays")

    async def test_the_real_socket_factory(self):
        remote = await Remote().start()
        self.addAsyncCleanup(remote.stop)
        s = await tcp_connect("127.0.0.1", remote.port)
        await s.write(b"ping")
        self.assertEqual(await s.read(100), b"ping")
        await s.shutdown()
        self.assertEqual(await s.read(100), b"", "the echo server ends after our FIN")
        s.close()
        with self.assertRaises(OSError):
            await tcp_connect("127.0.0.1", 1)
        # The test map only replaces what it lists.
        seen = []

        async def dial(ip, port):
            seen.append((ip, port))

        connect = mapped_connect({(PUBLIC, 443): ("127.0.0.1", 9)}, dial)
        await connect(PUBLIC, 443)
        await connect(PUBLIC, 80)
        await connect("1.1.1.1", 443)
        self.assertEqual(seen, [("127.0.0.1", 9), (PUBLIC, 80), ("1.1.1.1", 443)])
        resolve = table_resolver({"a.example": ["1.2.3.4"], "*": ["5.6.7.8"]})
        self.assertEqual((await resolve("a.example"), await resolve("b.example")), (["1.2.3.4"], ["5.6.7.8"]))
        self.assertEqual(await table_resolver({})("a.example"), [])


if __name__ == "__main__":
    unittest.main()
