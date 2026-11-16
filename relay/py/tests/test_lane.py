"""Lanes against fake sockets and a fake resolver: the cases of
relay/test/unit.mjs (refusals, connect by name with every address checked,
windows both ways, half close, the client's window, idle, lane life, kill
switch, total bytes, counters-only log, DNS)."""

import asyncio
import json
import unittest

from vetro_connect.dns import answer_addrs, build_query
from vetro_connect.lane import GRANT, Lane, Stats
from vetro_connect.ledger import SERVER_LIMITS, Ledger
from vetro_connect.protocol import MAX_DATA, WINDOW, decode, encode

PUBLIC = "93.184.215.14"


async def settle(rounds=30):
    """Lets the lane's tasks run."""
    for _ in range(rounds):
        await asyncio.sleep(0)


async def until(cond, what, seconds=3.0):
    end = asyncio.get_running_loop().time() + seconds
    while not cond():
        if asyncio.get_running_loop().time() > end:
            raise AssertionError("timed out: " + what)
        await asyncio.sleep(0.001)


class FakeSocket:
    """A remote end the test plays: `push` and `end` what it sends, `written`
    what it received."""

    def __init__(self, stuck=False):
        self.written = bytearray()
        self.write_closed = False
        self.closed = False
        self.was_reset = False
        self.stuck = stuck
        self._incoming = asyncio.Queue()
        self._pending = b""

    def push(self, data):
        self._incoming.put_nowait(bytes(data))

    def end(self):
        self._incoming.put_nowait(b"")

    async def read(self, n):
        if not self._pending:
            self._pending = await self._incoming.get()
        out, self._pending = self._pending[:n], self._pending[n:]
        return out

    async def write(self, data):
        if self.stuck:
            await asyncio.Event().wait()
        self.written += data

    async def shutdown(self):
        self.write_closed = True

    def close(self):
        self.closed = True

    def reset(self):
        self.closed = True
        self.was_reset = True


class Harness:
    def __init__(self, *, sockets=None, resolver=None, ledger=None, lane_conns=4, **rest):
        self.sent = []
        self.logs = []
        self.opened = []
        self.ws_closed = None
        self.sockets = sockets or {}
        self.stats = Stats()

        async def no_dns(name):
            raise OSError("no dns")

        self.lane = Lane(
            send=self.sent.append,
            close=self._close,
            connect=self._connect,
            resolver=resolver or no_dns,
            ledger=ledger or Ledger(),
            lane_conns=lane_conns,
            log=self.logs.append,
            stats=self.stats,
            **{"tick_ms": 0, **rest},
        )
        self.lane.start()

    def _close(self, code, reason):
        self.ws_closed = (code, reason)

    async def _connect(self, ip, port):
        made = self.sockets.get("%s:%d" % (ip, port), FakeSocket)()
        self.opened.append((ip, port, made))
        if isinstance(made, BaseException):
            raise made
        if asyncio.iscoroutine(made):
            return await made
        return made

    def frames(self):
        return [decode(d) for d in self.sent if not isinstance(d, str)]

    def texts(self):
        return [json.loads(d) for d in self.sent if isinstance(d, str)]

    def answers(self):
        """id -> 'connected' or the refusal's reason."""
        return {f.id: f.reason if f.type == "refused" else f.type for f in self.frames() if f.type in ("connected", "refused")}

    async def msg(self, data):
        self.lane.on_message(data)
        await settle()


def table(hosts):
    async def resolve(name):
        if name not in hosts:
            raise OSError("lookup failed")
        return hosts[name]

    return resolve


class LaneCases(unittest.IsolatedAsyncioTestCase):
    async def test_hello_refusals_by_policy_and_limits(self):
        t = Harness(lane_conns=2, limits={"conns": 32}, sockets={"93.184.215.15:443": lambda: ConnectionRefusedError("refused"), "93.184.215.16:443": lambda: OSError("unreachable")})
        hello = t.texts()[0]
        self.assertEqual(hello, {"type": "hello", "v": 1, "lane": 0, "window": WINDOW, "maxData": MAX_DATA, "laneConns": 2, "limits": {"conns": 32}})
        await t.msg(encode.connect(1, "10.0.0.1", 443))
        await t.msg(encode.connect(2, PUBLIC, 22))
        await t.msg(encode.connect(3, "169.254.169.254", 80))
        await t.msg(encode.connect(4, "93.184.215.15", 443))
        await t.msg(encode.connect(5, "93.184.215.16", 443))
        await t.msg(encode.connect(6, PUBLIC, 443))
        await t.msg(encode.connect(7, PUBLIC, 80))
        await t.msg(encode.connect(8, PUBLIC, 80))
        self.assertEqual(t.answers(), {1: "address", 2: "port", 3: "address", 4: "unreachable", 5: "unreachable", 6: "connected", 7: "connected", 8: "limit"})
        self.assertEqual([(ip, port) for ip, port, _ in t.opened], [("93.184.215.15", 443), ("93.184.215.16", 443), (PUBLIC, 443), (PUBLIC, 80)], "no socket for refused destinations")
        self.assertEqual(t.lane.counters["refused"], {"address": 2, "port": 1, "unreachable": 2, "limit": 1})
        self.assertEqual((t.stats.lanes, t.stats.conns, t.stats.conns_total, t.stats.refused), (1, 2, 2, 6))
        self.assertEqual(t.lane.ledger.counters()["conns"], 2, "refused connections are not counted")
        # A second CONNECT on a live id is a protocol violation.
        await t.msg(encode.connect(6, PUBLIC, 443))
        self.assertEqual(t.texts()[-1], {"type": "bye", "reason": "protocol"})
        self.assertEqual(t.ws_closed, (4002, "protocol"))
        self.assertTrue(all(s.closed for _, _, s in t.opened[2:]), "every socket closed with the lane")
        self.assertEqual((t.stats.lanes, t.stats.conns), (0, 0))
        self.assertEqual(t.lane.ledger.counters(), {"lanes": 0, "conns": 0, "bytes": 0, "ended": None})
        # And nothing more is said or done on a lane that ended.
        sent = len(t.sent)
        await t.msg(encode.connect(9, PUBLIC, 443))
        self.assertEqual((len(t.sent), len(t.opened)), (sent, 4))

    async def test_every_blocked_address_is_refused_before_any_socket(self):
        from vetro_connect.policy import BLOCKED

        t = Harness(lane_conns=100)
        for i, (base, _, _) in enumerate(BLOCKED):
            for port in (80, 443):
                await t.msg(encode.connect(i * 2 + (port == 443), base, port))
        self.assertEqual(set(t.answers().values()), {"address"})
        self.assertEqual(len(t.answers()), 2 * len(BLOCKED))
        self.assertEqual(t.opened, [])
        self.assertEqual(len(t.lane.conns), 0)

    async def test_connect_by_name_resolved_every_address_checked(self):
        hosts = {
            "good.example": [PUBLIC],
            "rebind.example": [PUBLIC, "127.0.0.1"],
            "lan-first.example": ["192.168.1.1", PUBLIC],
            "odd.example": [PUBLIC, "::1"],
            "none.example": [],
        }
        t = Harness(resolver=table(hosts), lane_conns=10)
        await t.msg(encode.connect_name(1, "good.example", 443))
        await t.msg(encode.connect_name(2, "rebind.example", 443))
        await t.msg(encode.connect_name(3, "none.example", 443))
        await t.msg(encode.connect_name(4, "good.example", 8080))
        await t.msg(encode.connect_name(5, "fail.example", 443))
        await t.msg(encode.connect_name(6, "lan-first.example", 80))
        await t.msg(encode.connect_name(7, "odd.example", 80))
        self.assertEqual(t.answers(), {1: "connected", 2: "address", 3: "resolve", 4: "port", 5: "resolve", 6: "address", 7: "address"})
        self.assertEqual([(ip, port) for ip, port, _ in t.opened], [(PUBLIC, 443)], "the checked address literal, and nothing else")
        self.assertEqual(t.lane.counters["dns"], 6, "a name looked up counts as a DNS query (not the refused port)")

    async def test_connect_by_name_is_rate_limited_like_dns(self):
        t = Harness(resolver=table({"good.example": [PUBLIC]}), ledger=Ledger({"dnsRate": 1, "dnsBurst": 1}, now=lambda: 0), lane_conns=10)
        for i in range(1, 4):
            await t.msg(encode.connect_name(i, "good.example", 443))
        self.assertEqual(t.answers(), {1: "connected", 2: "connected", 3: "limit"})

    async def test_data_both_ways_within_the_window_acks_half_close(self):
        t = Harness()
        await t.msg(encode.connect(1, PUBLIC, 443))
        s = t.opened[0][2]
        # Guest -> remote: written, then acknowledged.
        out = bytes((i * 13) & 255 for i in range(40_000))
        for at in range(0, len(out), MAX_DATA):
            await t.msg(encode.data(1, out[at : at + MAX_DATA], False))
        await until(lambda: len(s.written) == len(out), "written")
        await until(lambda: sum(f.n for f in t.frames() if f.type == "ack") == len(out), "acks")
        self.assertEqual(bytes(s.written), out)
        self.assertTrue(all(f.from_gateway for f in t.frames()))
        # Remote -> guest: no more than WINDOW unacknowledged.
        big = bytes((i * 7) & 255 for i in range(3 * WINDOW + 1000))
        for at in range(0, len(big), 50_000):
            s.push(big[at : at + 50_000])
        s.end()
        got = bytearray()
        acked = 0
        seen = 0
        stalled = False
        for _ in range(10_000):
            if len(got) >= len(big):
                break
            await settle(5)
            data = [f for f in t.frames() if f.type == "data"]
            for f in data[seen:]:
                got += f.data
            seen = len(data)
            self.assertLessEqual(len(got) - acked, WINDOW, "in flight")
            if len(got) - acked == WINDOW and not stalled:
                # A full window: the gateway waits for the guest, however long.
                stalled = True
                await settle(200)
                self.assertEqual(sum(len(f.data) for f in t.frames() if f.type == "data"), len(got))
            if len(got) > acked:
                await t.msg(encode.ack(1, len(got) - acked, False))
                acked = len(got)
        self.assertTrue(stalled, "the window was reached")
        self.assertEqual(bytes(got), big)
        self.assertTrue(all(len(f.data) <= MAX_DATA for f in t.frames() if f.type == "data"))
        await until(lambda: any(f.type == "shutdown" for f in t.frames()), "remote FIN")
        self.assertEqual(len(t.lane.conns), 1, "half closed: still there")
        self.assertFalse(s.closed)
        # Guest FIN: the socket's write side closes; both done, the socket goes.
        await t.msg(encode.data(1, b"last", False))
        await t.msg(encode.shutdown(1, False))
        await until(lambda: s.write_closed, "write side closed")
        await until(lambda: len(t.lane.conns) == 0, "connection finished")
        self.assertEqual(bytes(s.written), out + b"last")
        self.assertTrue(s.closed and not s.was_reset)
        self.assertFalse(any(f.type == "close" for f in t.frames()), "no CLOSE from the gateway: the client's follows")
        self.assertEqual((t.lane.counters["bytesToRemote"], t.lane.counters["bytesToGuest"]), (len(out) + 4, len(big)))
        self.assertEqual((t.stats.bytes_to_remote, t.stats.bytes_to_guest, t.stats.conns), (len(out) + 4, len(big), 0))
        # The client's CLOSE for a connection already gone is ignored.
        await t.msg(encode.close(1, False, False))
        self.assertIsNone(t.ws_closed)
        # An ACK beyond what was sent is a violation (on a live connection).
        await t.msg(encode.connect(2, PUBLIC, 443))
        await t.msg(encode.ack(2, 1, False))
        self.assertEqual(t.texts()[-1]["reason"], "protocol")

    async def test_data_after_the_guests_fin_is_ignored_and_order_is_kept(self):
        t = Harness()
        await t.msg(encode.connect(1, PUBLIC, 443))
        s = t.opened[0][2]
        # All at once, before the lane's tasks run.
        for chunk in [b"one ", b"two ", b"three"]:
            t.lane.on_message(encode.data(1, chunk, False))
        t.lane.on_message(encode.shutdown(1, False))
        t.lane.on_message(encode.data(1, b" late", False))
        await until(lambda: s.write_closed, "write side closed")
        self.assertEqual(bytes(s.written), b"one two three")
        self.assertIsNone(t.ws_closed)

    async def test_the_client_must_respect_the_window(self):
        # The socket never finishes writing: nothing is acknowledged.
        t = Harness(sockets={PUBLIC + ":443": lambda: FakeSocket(stuck=True)})
        await t.msg(encode.connect(1, PUBLIC, 443))
        chunk = bytes(MAX_DATA)
        for _ in range(WINDOW // MAX_DATA):
            await t.msg(encode.data(1, chunk, False))
        self.assertIsNone(t.ws_closed, "a full window is allowed")
        self.assertFalse(any(f.type == "ack" for f in t.frames()))
        await t.msg(encode.data(1, b"x", False))
        self.assertEqual(t.texts()[-1]["reason"], "protocol")
        self.assertEqual(t.ws_closed, (4002, "protocol"))

    async def test_close_from_the_client_and_failures_of_the_socket(self):
        class Failing(FakeSocket):
            async def write(self, data):
                raise ConnectionResetError("reset by peer")

        class Dropped(FakeSocket):
            async def read(self, n):
                raise ConnectionResetError("reset by peer")

        t = Harness(sockets={PUBLIC + ":80": Failing, "93.184.215.15:80": Dropped}, lane_conns=10)
        await t.msg(encode.connect(1, PUBLIC, 443))
        await t.msg(encode.connect(2, PUBLIC, 443))
        await t.msg(encode.close(1, False, False))
        await t.msg(encode.close(2, True, False))
        self.assertTrue(t.opened[0][2].closed and not t.opened[0][2].was_reset)
        self.assertTrue(t.opened[1][2].was_reset, "the guest's abort is a reset")
        self.assertEqual(len(t.lane.conns), 0)
        self.assertFalse(any(f.type == "close" for f in t.frames()), "nothing to tell the client")
        # A socket that fails: CLOSE(reset) to the client, once.
        await t.msg(encode.connect(3, PUBLIC, 80))
        await t.msg(encode.data(3, b"x", False))
        await t.msg(encode.connect(4, "93.184.215.15", 80))
        closes = [(f.id, f.reset, f.from_gateway) for f in t.frames() if f.type == "close"]
        self.assertEqual(closes, [(3, True, True), (4, True, True)])
        self.assertEqual((len(t.lane.conns), t.stats.conns, t.lane.ledger.counters()["conns"]), (0, 0, 0))
        self.assertIsNone(t.ws_closed)
        # Frames for ids the lane does not know are ignored (races are legitimate).
        for f in [encode.data(77, b"x", False), encode.shutdown(77, False), encode.ack(77, 5, False), encode.close(77, True, False)]:
            await t.msg(f)
        self.assertIsNone(t.ws_closed)

    async def test_connect_timeout_and_close_while_connecting(self):
        gate = asyncio.Event()
        late = FakeSocket()

        async def slow():
            await gate.wait()
            return late

        t = Harness(sockets={PUBLIC + ":443": slow, "93.184.215.15:443": lambda: asyncio.Event().wait()}, connect_timeout_ms=30, lane_conns=10)
        await t.msg(encode.connect(1, "93.184.215.15", 443))
        self.assertEqual(t.answers(), {})
        await until(lambda: 1 in t.answers(), "timeout")
        self.assertEqual(t.answers(), {1: "unreachable"})
        self.assertEqual(t.lane.ledger.counters()["conns"], 0)
        # The client gives up while the gateway is still connecting.
        await t.msg(encode.connect(2, PUBLIC, 443))
        self.assertEqual(t.lane.ledger.counters()["conns"], 1)
        await t.msg(encode.close(2, True, False))
        self.assertEqual((len(t.lane.conns), t.lane.ledger.counters()["conns"]), (0, 0))
        gate.set()
        await settle()
        self.assertEqual(t.answers(), {1: "unreachable"}, "no answer for a connection the client closed")
        self.assertEqual(t.stats.conns, 0)

    async def test_control_messages_and_protocol_violations(self):
        t = Harness()
        await t.msg('{"type":"ping"}')
        self.assertEqual(t.texts()[-1], {"type": "pong"})
        for ignored in ['{"type":"other"}', "7", "null", "[]", '"ping"']:
            await t.msg(ignored)
        self.assertEqual(len(t.texts()), 2)
        self.assertIsNone(t.ws_closed)
        for bad in ["{", "[" * 5000, b"", b"\x42\x00\x00\x00\x01", encode.connected(1), encode.refused(1, "down"), encode.data(1, b"x", True), encode.dns_answer(1, bytes(12)), encode.online(True), encode.down(), encode.data(1, b"", False)]:
            v = Harness()
            await v.msg(bad)
            self.assertEqual(v.texts()[-1], {"type": "bye", "reason": "protocol"}, repr(bad)[:30])
            self.assertEqual(v.ws_closed, (4002, "protocol"))
            self.assertEqual(v.lane.ledger.counters()["lanes"], 0)

    async def test_idle_connections_lane_life_and_the_log(self):
        clock = [0]
        t = Harness(now=lambda: clock[0], idle_ms=1000, max_life_ms=5000)
        await t.msg(encode.connect(1, PUBLIC, 443))
        clock[0] = 999
        t.lane.tick()
        self.assertEqual(len(t.lane.conns), 1)
        clock[0] = 1000
        t.lane.tick()
        last = t.frames()[-1]
        self.assertEqual((last.type, last.id, last.from_gateway, last.reset), ("close", 1, True, True))
        self.assertTrue(t.opened[0][2].closed)
        # Traffic keeps a connection alive.
        await t.msg(encode.connect(2, PUBLIC, 443))
        clock[0] = 1900
        await t.msg(encode.data(2, b"x", False))
        clock[0] = 2800
        t.lane.tick()
        self.assertEqual(len(t.lane.conns), 1)
        clock[0] = 4999
        t.lane.tick()
        self.assertIsNone(t.ws_closed)
        clock[0] = 5000
        t.lane.tick()
        self.assertEqual(t.texts()[-1]["reason"], "expired")
        self.assertEqual(t.ws_closed, (4000, "expired"))
        self.assertEqual(len(t.logs), 1)
        log = json.dumps(t.logs[0])
        self.assertNotRegex(log, r"93\.184|example", "no destinations in the log")
        self.assertEqual(sorted(t.logs[0]), ["bytesToGuest", "bytesToRemote", "conns", "dns", "event", "platform", "reason", "refused", "secs"])
        self.assertEqual(t.logs[0], {"event": "lane-end", "platform": "python", "reason": "expired", "secs": 5, "conns": 2, "refused": {}, "bytesToRemote": 1, "bytesToGuest": 0, "dns": 0})

    async def test_the_ticker_runs_by_itself(self):
        t = Harness(tick_ms=5, idle_ms=20, max_life_ms=80)
        await t.msg(encode.connect(1, PUBLIC, 443))
        await until(lambda: any(f.type == "close" for f in t.frames()), "idle connection dropped")
        await until(lambda: t.ws_closed is not None, "lane expired")
        self.assertEqual(t.ws_closed, (4000, "expired"))

    async def test_kill_switch_and_total_bytes(self):
        on = [True]
        k = Harness(ledger=Ledger(SERVER_LIMITS, enabled=lambda: on[0]))
        await k.msg(encode.connect(1, PUBLIC, 443))
        on[0] = False
        await k.msg(encode.data(1, b"\x01", False))
        await until(lambda: k.ws_closed is not None, "kill switch")
        self.assertEqual(k.texts()[-1]["reason"], "disabled")
        self.assertEqual(k.ws_closed, (4000, "disabled"))
        self.assertEqual(bytes(k.opened[0][2].written), b"", "not one byte after the switch")
        self.assertTrue(k.opened[0][2].closed)
        # While it is off a connection is refused, not opened.
        off = Harness(ledger=Ledger(SERVER_LIMITS, enabled=lambda: False))
        await off.msg(encode.connect(1, PUBLIC, 443))
        self.assertEqual((off.answers(), off.opened), ({1: "limit"}, []))

        b = Harness(ledger=Ledger({**SERVER_LIMITS, "bytes": GRANT}))
        await b.msg(encode.connect(1, PUBLIC, 443))
        await b.msg(encode.data(1, bytes(1000), False))
        await until(lambda: any(f.type == "ack" for f in b.frames()), "first grant")
        b.opened[0][2].push(bytes(GRANT))
        await until(lambda: b.ws_closed is not None, "total bytes")
        self.assertEqual(b.texts()[-1]["reason"], "limit-bytes")
        self.assertLess(sum(len(f.data) for f in b.frames() if f.type == "data"), GRANT)

    async def test_bandwidth_limit_waits(self):
        clock = [0.0]
        t = Harness(ledger=Ledger({"rate": 1_000_000, "burst": GRANT}, now=lambda: clock[0]))
        await t.msg(encode.connect(1, PUBLIC, 443))
        s = t.opened[0][2]
        # The first grant is the burst; the second must wait 66 ms (64 KiB at 1 MB/s).
        s.push(bytes(GRANT + 1))
        started = asyncio.get_running_loop().time()
        await until(lambda: sum(len(f.data) for f in t.frames() if f.type == "data") == GRANT + 1, "all the bytes")
        self.assertGreaterEqual(asyncio.get_running_loop().time() - started, 0.06)

    async def test_the_session_connection_limit(self):
        ledger = Ledger({"conns": 3})
        a = Harness(ledger=ledger, lane_conns=10)
        b = Harness(ledger=ledger, lane_conns=10, lane=1)
        await a.msg(encode.connect(1, PUBLIC, 443))
        await a.msg(encode.connect(2, PUBLIC, 443))
        await b.msg(encode.connect(3, PUBLIC, 443))
        await b.msg(encode.connect(4, PUBLIC, 443))
        self.assertEqual({**a.answers(), **b.answers()}, {1: "connected", 2: "connected", 3: "connected", 4: "limit"})
        await a.msg(encode.close(1, False, False))
        await b.msg(encode.connect(5, PUBLIC, 443))
        self.assertEqual(b.answers()[5], "connected")
        self.assertEqual(b.texts()[0]["lane"], 1)

    async def test_dns_through_the_lane(self):
        hosts = {"ok.example": ["1.2.3.4"], "lan.example": ["192.168.1.7"], "none.example": []}
        t = Harness(resolver=table(hosts))
        await t.msg(encode.dns(1, build_query("ok.example", 0xABCD)))
        await t.msg(encode.dns(2, build_query("fail.example", 7)))
        await t.msg(encode.dns(3, build_query("lan.example", 8)))
        bad = bytearray(build_query("x.example", 9))
        bad[2] |= 0x80
        await t.msg(encode.dns(4, bytes(bad)))
        await t.msg(encode.dns(5, build_query("ok.example", 10, 28)))
        await t.msg(encode.dns(6, build_query("none.example", 11)))
        await until(lambda: len(t.frames()) == 6, "answers")
        self.assertTrue(all(f.type == "dnsAnswer" and f.from_gateway for f in t.frames()))
        a = {f.id: f.msg for f in t.frames()}
        self.assertEqual((a[1][0] << 8) | a[1][1], 0xABCD, "the guest's id")
        self.assertEqual(answer_addrs(a[1]), ["1.2.3.4"])
        self.assertEqual(a[2][3] & 0x0F, 2, "SERVFAIL")
        self.assertEqual((a[2][0] << 8) | a[2][1], 7)
        self.assertEqual((a[3][3] & 0x0F, answer_addrs(a[3])), (0, []), "a local address is not told")
        self.assertEqual(a[4][3] & 0x0F, 1, "FORMERR")
        self.assertEqual((a[5][3] & 0x0F, a[5][6:8]), (0, b"\x00\x00"), "AAAA: no records")
        self.assertEqual((a[6][3] & 0x0F, a[6][6:8]), (0, b"\x00\x00"), "no such name: no records")
        self.assertEqual(t.lane.counters["dns"], 5, "the malformed one is not a query")
        self.assertGreater(t.lane.ledger.counters()["bytes"], 0, "answers cross the ledger")

    async def test_dns_beyond_the_rate_is_refused_and_the_kill_switch_ends_the_lane(self):
        clock = [0.0]
        t = Harness(resolver=table({"ok.example": ["1.2.3.4"]}), ledger=Ledger({"dnsRate": 1000, "dnsBurst": 2}, now=lambda: clock[0]))
        for i in range(1, 6):
            t.lane.on_message(encode.dns(i, build_query("ok.example", i)))
        await until(lambda: len(t.frames()) == 5, "answers")
        rcodes = {f.id: f.msg[3] & 0x0F for f in t.frames()}
        self.assertEqual(rcodes, {1: 0, 2: 0, 3: 0, 4: 0, 5: 5}, "the fifth is REFUSED")
        off = Harness(ledger=Ledger(enabled=lambda: False))
        await off.msg(encode.dns(1, build_query("ok.example", 1)))
        self.assertEqual(off.ws_closed, (4000, "disabled"))

    async def test_a_lane_that_cannot_send_ends_quietly(self):
        sent = []

        def send(data):
            if len(sent) >= 1:
                raise ConnectionError("the WebSocket is gone")
            sent.append(data)

        closed = []
        lane = Lane(send=send, close=lambda code, reason: closed.append((code, reason)), connect=None, resolver=None, ledger=Ledger(), tick_ms=0)
        lane.start()
        self.assertTrue(lane.ledger.open_lane(0).ok)
        lane.on_message('{"type":"ping"}')
        self.assertTrue(lane.closed)
        self.assertEqual(closed, [(4000, "send-failed")])
        self.assertEqual(lane.ledger.counters()["lanes"], 0)
        lane.on_close()
        self.assertEqual(len(closed), 1, "ended once")

    async def test_an_internal_error_ends_the_lane_and_is_only_counted(self):
        async def broken(name):
            return 5  # not a list: a bug, not a failed lookup

        t = Harness(resolver=broken)
        await t.msg(encode.connect_name(1, "good.example", 443))
        self.assertEqual(t.stats.errors, 1)
        self.assertEqual(t.ws_closed, (4000, "error"))
        self.assertEqual((len(t.lane.conns), t.opened), (0, []))

    async def test_the_websocket_closing_frees_everything(self):
        ledger = Ledger()
        self.assertTrue(ledger.open_lane(0).ok)
        t = Harness(ledger=ledger, resolver=table({"ok.example": ["1.2.3.4"]}))
        await t.msg(encode.connect(1, PUBLIC, 443))
        await t.msg(encode.connect(2, PUBLIC, 80))
        t.opened[0][2].push(b"hello")
        await settle()
        t.lane.on_close()
        await settle()
        self.assertTrue(all(s.was_reset for _, _, s in t.opened))
        self.assertEqual(ledger.counters(), {"lanes": 0, "conns": 0, "bytes": GRANT, "ended": None})
        self.assertEqual((t.stats.lanes, t.stats.conns, t.stats.conns_total), (0, 0, 2))
        self.assertEqual(t.ws_closed, (4000, "closed"))
        self.assertFalse(any(m.get("type") == "bye" for m in t.texts()), "nobody to say bye to")
        self.assertEqual(t.logs[0]["reason"], "closed")
        self.assertEqual(len(t.lane._tasks), 0, "no task left behind")


if __name__ == "__main__":
    unittest.main()
