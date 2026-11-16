"""DNS: queries checked, answers built from the system resolver's
addresses, failures, truncation, the resolver off the event loop."""

import asyncio
import socket
import threading
import unittest
from unittest import mock

from vetro_connect import dns
from vetro_connect.dns import MAX_UDP_ANSWER, Query, SystemResolver, answer_addrs, build_answer, build_query, fail_answer, fit_udp, lookup_ipv4, parse_query, resolvable


def table(hosts):
    async def resolve(name):
        if name not in hosts:
            raise OSError("lookup failed")
        return hosts[name]

    return resolve


class Queries(unittest.TestCase):
    def test_queries_checked(self):
        q = build_query("Example.COM", 0x1234)
        self.assertEqual(parse_query(q), Query(0x1234, "example.com", 1, 1, len(q)))
        self.assertEqual(parse_query(build_query("a.b", 7, 28)).qtype, 28)
        # With an EDNS record after the question.
        edns = bytearray(q + bytes([0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]))
        edns[11] = 1
        self.assertEqual(parse_query(bytes(edns)).end, len(q))
        r = bytearray(q)
        r[2] |= 0x80  # a response is not a query
        self.assertIsNone(parse_query(bytes(r)))
        o = bytearray(q)
        o[2] |= 0x08  # nor is another opcode
        self.assertIsNone(parse_query(bytes(o)))
        for counts in [(0, 0, 0), (2, 0, 0), (1, 1, 0), (1, 0, 1)]:
            c = bytearray(q)
            c[4:10] = bytes([0, counts[0], 0, counts[1], 0, counts[2]])
            self.assertIsNone(parse_query(bytes(c)), counts)
        self.assertIsNone(parse_query(q[:14]))  # cut inside the name
        self.assertIsNone(parse_query(q[:-1]))  # cut inside the question
        self.assertIsNone(parse_query(q[:11]))
        self.assertIsNone(parse_query(q[:12] + b"\x40" + bytes(70)))  # a label over 63 (or a pointer)
        self.assertIsNone(parse_query(q + bytes(1025 - len(q))))  # over 1024 bytes
        self.assertIsNotNone(parse_query(q + bytes(1024 - len(q))))
        self.assertIsNone(parse_query("not bytes"))

    def test_failures(self):
        q = build_query("example.com", 0x1234)
        f = fail_answer(q, 2)
        self.assertEqual(f[:2], q[:2])
        self.assertEqual(f[2] & 0x80, 0x80)  # a response
        self.assertEqual(f[2] & 0x01, 0x01)  # RD copied
        self.assertEqual(f[3], 0x82)  # RA, SERVFAIL
        self.assertEqual(f[4:12], bytes([0, 1, 0, 0, 0, 0, 0, 0]))
        self.assertEqual(f[12:], q[12:])
        self.assertEqual(fail_answer(q, 5)[3] & 0x0F, 5)
        # A message that is not a query: the header alone, no question.
        bad = bytearray(q)
        bad[2] |= 0x80
        g = fail_answer(bytes(bad), 1)
        self.assertEqual(len(g), 12)
        self.assertEqual(g[3] & 0x0F, 1)
        self.assertEqual(g[4:12], bytes(8))

    def test_answers(self):
        q = build_query("Example.com", 0xBEEF)
        p = parse_query(q)
        a = build_answer(q, p, ["93.184.215.14", "1.2.3.4"])
        self.assertEqual(a[:2], b"\xbe\xef")
        self.assertEqual(a[2:4], b"\x81\x80")  # response, RD, RA, NOERROR
        self.assertEqual(a[4:12], bytes([0, 1, 0, 2, 0, 0, 0, 0]))
        self.assertEqual(a[12 : p.end], q[12:], "the question as it was asked")
        record = bytes([0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4])
        self.assertEqual(a[p.end :], record + bytes([93, 184, 215, 14]) + record + bytes([1, 2, 3, 4]))
        self.assertEqual(answer_addrs(a), ["93.184.215.14", "1.2.3.4"])
        # No address: "no data", not an error.
        n = build_answer(q, p, [])
        self.assertEqual((n[3] & 0x0F, n[4:12], len(n)), (0, bytes([0, 1, 0, 0, 0, 0, 0, 0]), p.end))
        # RD is copied, not invented; EDNS is not echoed.
        norec = bytearray(q + bytes(11))
        norec[2] = 0
        norec[11] = 1
        b = build_answer(bytes(norec), parse_query(bytes(norec)), ["1.2.3.4"])
        self.assertEqual(b[2:4], b"\x80\x80")
        self.assertEqual(b[10:12], b"\x00\x00")
        self.assertEqual(len(b), p.end + 16)

    def test_answer_addrs_reads_real_answers(self):
        q = build_query("example.com", 0x1234)
        resp = q[:2] + bytes([0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0]) + q[12:]
        resp += bytes([0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 12])  # CNAME (rdata: a pointer)
        resp += bytes([0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14])
        self.assertEqual(answer_addrs(resp), ["93.184.215.14"])
        self.assertEqual(answer_addrs(resp[:-2]), [])
        self.assertEqual(answer_addrs(fail_answer(q, 3)), [])
        self.assertEqual(answer_addrs(b"short"), [])

    def test_truncation(self):
        q = build_query("example.com", 0x1234)
        small = build_answer(q, parse_query(q), ["1.2.3.4"])
        self.assertIs(fit_udp(q, small), small)
        huge = bytearray(MAX_UDP_ANSWER + 1)
        huge[:12] = small[:12]
        t = fit_udp(q, bytes(huge))
        self.assertLess(len(t), 512)
        self.assertEqual(t[2] & 0x02, 0x02, "TC")
        self.assertEqual(t[6:8], b"\x00\x00")
        self.assertEqual(len(fit_udp(q, bytes(MAX_UDP_ANSWER))), MAX_UDP_ANSWER)
        # Our own answers always fit the classic 512 bytes.
        long = ".".join(["a" * 63] * 3 + ["b" * 61])
        lq = build_query(long, 1)
        full = build_answer(lq, parse_query(lq), ["1.2.3.%d" % i for i in range(dns.MAX_ANSWERS)])
        self.assertLessEqual(len(full), 512)

    def test_names_for_the_system_resolver(self):
        for name in ["example.com", "a", "xn--caf-dma.example", "_dmarc.example.com", "a-b.c-d.e", "1e100.net", "3com.com", "x.0x7f.com"]:
            self.assertTrue(resolvable(name), name)
        for name in ["", ".", "a..b", "a b.com", "a/b.com", "a.com.", "é.com", "A.com", "a" * 64 + ".com", "x" * 254, "a.b\n", "a\x00b.com",
                     # What `getaddrinfo` would read as a number, not a name.
                     "127.0.0.1", "127.1", "2130706433", "0x7f000001", "0x7f.0.0.1", "10.0.0.0x1", "1.2.3", "host.123", "0x"]:
            self.assertFalse(resolvable(name), repr(name))


class Answers(unittest.IsolatedAsyncioTestCase):
    async def ask(self, name, hosts, qtype=1, qid=0x4242, qclass=None):
        q = bytearray(build_query(name, qid, qtype))
        if qclass is not None:
            q[-2:] = bytes([qclass >> 8, qclass & 255])
        q = bytes(q)
        return q, await dns.answer(q, parse_query(q), table(hosts))

    async def test_type_a_gets_the_names_addresses(self):
        q, a = await self.ask("Vetro.Example", {"vetro.example": ["93.184.215.14", "93.184.215.15"]})
        self.assertEqual(a[:2], b"\x42\x42")
        self.assertEqual(a[3] & 0x0F, 0)
        self.assertEqual(answer_addrs(a), ["93.184.215.14", "93.184.215.15"])
        self.assertEqual(a[12 : len(q)], q[12:])

    async def test_other_questions_get_no_records_and_no_lookup(self):
        for qtype in [28, 65, 15, 16, 255, 5, 33]:  # AAAA, HTTPS, MX, TXT, ANY, CNAME, SRV
            _, a = await self.ask("vetro.example", {}, qtype=qtype)
            self.assertEqual((a[3] & 0x0F, a[6:8], answer_addrs(a)), (0, b"\x00\x00", []), qtype)
        _, a = await self.ask("vetro.example", {}, qclass=3)  # class CHAOS
        self.assertEqual((a[3] & 0x0F, a[6:8]), (0, b"\x00\x00"))
        # Names the system resolver must not see: no lookup (the table would raise).
        for name in ["127.0.0.1", "2130706433", "bad name.example", "0x7f.1"]:
            _, a = await self.ask(name, {})
            self.assertEqual((a[3] & 0x0F, a[6:8]), (0, b"\x00\x00"), name)

    async def test_addresses_of_the_local_network_are_left_out(self):
        hosts = {
            "router.lan": ["192.168.1.1"],
            "localhost": ["127.0.0.1"],
            "mixed.example": ["10.0.0.5", "93.184.215.14", "169.254.169.254", "93.184.215.14", "100.64.0.1", "8.8.8.8"],
            "odd.example": ["::1", "not-an-address", "93.184.215.14"],
            "none.example": [],
        }
        for name in ["router.lan", "localhost", "none.example"]:
            _, a = await self.ask(name, hosts)
            self.assertEqual((a[3] & 0x0F, answer_addrs(a)), (0, []), name)
        _, a = await self.ask("mixed.example", hosts)
        self.assertEqual(answer_addrs(a), ["93.184.215.14", "8.8.8.8"], "public ones only, once each")
        _, a = await self.ask("odd.example", hosts)
        self.assertEqual(answer_addrs(a), ["93.184.215.14"])

    async def test_at_most_eight_records(self):
        _, a = await self.ask("many.example", {"many.example": ["93.184.215.%d" % i for i in range(1, 30)]})
        self.assertEqual(answer_addrs(a), ["93.184.215.%d" % i for i in range(1, 9)])

    async def test_a_failed_lookup_raises(self):
        with self.assertRaises(OSError):
            await self.ask("fail.example", {})


class Lookup(unittest.TestCase):
    def test_getaddrinfo_asked_for_ipv4_only(self):
        infos = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.215.14", 0)), (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.215.15", 0)), (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.215.14", 0))]
        with mock.patch("socket.getaddrinfo", return_value=infos) as gai:
            self.assertEqual(lookup_ipv4("vetro.example"), ["93.184.215.14", "93.184.215.15"])
        gai.assert_called_once_with(b"vetro.example", None, socket.AF_INET, socket.SOCK_STREAM)

    def test_no_such_name_is_no_address_other_errors_are_failures(self):
        for code in [socket.EAI_NONAME, getattr(socket, "EAI_NODATA", socket.EAI_NONAME), 11001, 11004]:
            with mock.patch("socket.getaddrinfo", side_effect=socket.gaierror(code, "not found")):
                self.assertEqual(lookup_ipv4("none.example"), [], code)
        for error in [socket.gaierror(socket.EAI_AGAIN, "try again"), socket.gaierror(socket.EAI_FAIL, "failed"), OSError("no network")]:
            with mock.patch("socket.getaddrinfo", side_effect=error):
                with self.assertRaises(OSError):
                    lookup_ipv4("fail.example")

    def test_the_real_resolver_knows_localhost(self):
        # The hosts file: no network needed.
        self.assertEqual(lookup_ipv4("localhost"), ["127.0.0.1"])


class Resolver(unittest.IsolatedAsyncioTestCase):
    async def test_lookups_run_off_the_event_loop(self):
        threads = []

        def lookup(name):
            threads.append(threading.current_thread())
            return ["93.184.215.14"] if name == "vetro.example" else []

        r = SystemResolver(lookup=lookup)
        self.assertEqual(await r("vetro.example"), ["93.184.215.14"])
        self.assertEqual(await r("none.example"), [])
        self.assertTrue(all(t is not threading.main_thread() and t.daemon for t in threads))
        self.assertEqual(len(threads), 2)

    async def test_names_that_are_numbers_never_reach_the_resolver(self):
        def lookup(name):
            raise AssertionError("looked up %r" % name)

        r = SystemResolver(lookup=lookup)
        for name in ["127.0.0.1", "2130706433", "0x7f.1", "bad name", ""]:
            self.assertEqual(await r(name), [], name)

    async def test_errors_and_the_time_limit(self):
        release = threading.Event()

        def lookup(name):
            if name == "slow.example":
                release.wait(5)
                return ["93.184.215.14"]
            raise socket.gaierror(socket.EAI_AGAIN, "try again")

        r = SystemResolver(workers=2, timeout=0.05, lookup=lookup)
        with self.assertRaises(socket.gaierror):
            await r("fail.example")
        # Both workers busy with slow lookups: the callers get their timeout,
        # and so does one queued behind them, which is then never looked up.
        results = await asyncio.gather(r("slow.example"), r("slow.example"), r("slow.example"), return_exceptions=True)
        self.assertTrue(all(isinstance(x, asyncio.TimeoutError) for x in results), results)
        release.set()
        # The workers are free again.
        fast = SystemResolver(workers=1, timeout=2, lookup=lambda name: ["1.2.3.4"])
        self.assertEqual(await asyncio.gather(*[fast("a%d.example" % i) for i in range(20)]), [["1.2.3.4"]] * 20)

    async def test_many_lookups_at_once_use_a_few_threads(self):
        seen = set()

        def lookup(name):
            seen.add(threading.current_thread().name + str(id(threading.current_thread())))
            return [name.split(".")[0].replace("-", ".")]

        r = SystemResolver(workers=4, lookup=lookup)
        got = await asyncio.gather(*[r("93-184-215-%d.example" % i) for i in range(100)])
        self.assertEqual(got, [["93.184.215.%d" % i] for i in range(100)])
        self.assertLessEqual(len(seen), 4)


if __name__ == "__main__":
    unittest.main()
