"""The session ledger: the same cases as relay/test/unit.mjs, and the local
gateway's defaults (no cap on bandwidth and total bytes)."""

import unittest

from vetro_connect.ledger import DEFAULT_LIMITS, SERVER_LIMITS, Ledger, Result, limits_from


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


class LedgerCases(unittest.TestCase):
    def test_connections_lanes_bandwidth_total(self):
        clock = Clock()
        on = [True]
        led = Ledger({**SERVER_LIMITS, "conns": 2, "lanes": 2, "rate": 1000, "burst": 2000, "bytes": 10_000, "dnsRate": 1, "dnsBurst": 2}, now=clock, enabled=lambda: on[0])
        self.assertEqual(led.open_lane(0), Result(True))
        self.assertEqual(led.open_lane(0).reason, "lane-in-use")
        self.assertEqual(led.open_lane(1), Result(True))
        self.assertEqual(led.open_lane(2).reason, "lanes")
        led.close_lane(1)
        self.assertEqual(led.open_lane(2), Result(True))
        self.assertTrue(led.open_conn().ok and led.open_conn().ok)
        self.assertEqual(led.open_conn().reason, "conns")
        led.close_conn()
        self.assertTrue(led.open_conn().ok)
        # Bandwidth: the burst at once, then a wait proportional to the debt.
        self.assertEqual(led.grant(2000), Result(True, 0))
        self.assertEqual(led.grant(500), Result(True, 500))
        clock.t += 500  # refilled 500: back to zero
        self.assertEqual(led.grant(1000), Result(True, 1000))
        self.assertEqual(led.counters(), {"lanes": 2, "conns": 2, "bytes": 3500, "ended": None})
        # Total: 3500 so far; beyond 10 000 the session ends for good.
        self.assertEqual(led.grant(7000).reason, "bytes")
        self.assertEqual(led.open_conn().reason, "bytes")
        self.assertEqual(led.open_lane(5).reason, "bytes")
        self.assertEqual(led.dns().reason, "bytes")
        self.assertEqual(led.counters()["ended"], "bytes")
        # Closing never goes below zero.
        for _ in range(5):
            led.close_conn()
        self.assertEqual(led.counters()["conns"], 0)

    def test_dns_rate(self):
        clock = Clock()
        d = Ledger({**SERVER_LIMITS, "dnsRate": 1, "dnsBurst": 2}, now=clock)
        # The burst, then waits, then refusals far beyond the rate.
        self.assertEqual([d.dns().wait_ms for _ in range(4)], [0, 0, 1000, 2000])
        self.assertEqual(d.dns(), Result(False, reason="dns"))
        clock.t += 3000
        self.assertEqual(d.dns(), Result(True, 0))

    def test_kill_switch(self):
        on = [True]
        k = Ledger(SERVER_LIMITS, now=Clock(), enabled=lambda: on[0])
        self.assertTrue(k.open_lane(0).ok)
        on[0] = False
        for r in [k.open_lane(1), k.open_conn(), k.grant(1), k.dns()]:
            self.assertEqual(r, Result(False, reason="disabled"))
        on[0] = True
        self.assertTrue(k.grant(1).ok)

    def test_local_defaults_no_cap_on_bandwidth_and_total(self):
        self.assertEqual(DEFAULT_LIMITS, {"conns": 32, "lanes": 8, "rate": None, "burst": None, "bytes": None, "dnsRate": 20, "dnsBurst": 100})
        self.assertEqual({k: SERVER_LIMITS[k] for k in ("rate", "burst", "bytes")}, {"rate": 2 * 1024 * 1024, "burst": 4 * 1024 * 1024, "bytes": 1024**3})
        clock = Clock()
        on = [True]
        led = Ledger(now=clock, enabled=lambda: on[0])
        # Far more than the server's 1 GiB and 2 MiB/s, at one instant: no wait, no end.
        for _ in range(100):
            self.assertEqual(led.grant(64 * 1024 * 1024), Result(True, 0))
        self.assertEqual(led.counters()["bytes"], 100 * 64 * 1024 * 1024)
        self.assertIsNone(led.counters()["ended"])
        # The other limits are the spec's.
        for _ in range(32):
            self.assertTrue(led.open_conn().ok)
        self.assertEqual(led.open_conn().reason, "conns")
        for k in range(8):
            self.assertTrue(led.open_lane(k).ok)
        self.assertEqual(led.open_lane(8).reason, "lanes")
        self.assertEqual([led.dns().wait_ms for _ in range(101)][-2:], [0, 50])
        # And the kill switch still stops every byte.
        on[0] = False
        self.assertEqual(led.grant(1), Result(False, reason="disabled"))

    def test_limits_from_variables(self):
        self.assertEqual(limits_from({}), DEFAULT_LIMITS)
        self.assertEqual(limits_from(None), DEFAULT_LIMITS)
        got = limits_from({"LIMIT_CONNS": "4", "LIMIT_DNS_RATE": "3", "LIMIT_BYTES": "x", "LIMIT_LANES": "3", "LIMIT_DNS_BURST": "7"})
        self.assertEqual(got, {**DEFAULT_LIMITS, "conns": 4, "lanes": 3, "dnsRate": 3, "dnsBurst": 7})
        self.assertIs(type(got["conns"]), int)
        for bad in ["", "0", "-1", "nan", "inf", "x", "1,5"]:
            self.assertEqual(limits_from({"LIMIT_CONNS": bad})["conns"], 32, bad)
            self.assertIsNone(limits_from({"LIMIT_BYTES": bad})["bytes"], bad)
        # A cap on the bandwidth: the burst is twice the rate unless given.
        self.assertEqual(limits_from({"LIMIT_RATE": "1000"})["burst"], 2000)
        self.assertEqual(limits_from({"LIMIT_RATE": "1000", "LIMIT_BURST": "5000"})["burst"], 5000)
        self.assertEqual(limits_from({"LIMIT_BYTES": "1e6"})["bytes"], 1_000_000)
        self.assertEqual(limits_from({"LIMIT_RATE": "0.5"})["rate"], 0.5)
        # With the variables, the server's behaviour.
        clock = Clock()
        led = Ledger(limits_from({"LIMIT_RATE": "1000", "LIMIT_BYTES": "5000"}), now=clock)
        self.assertEqual(led.grant(2000), Result(True, 0))
        self.assertEqual(led.grant(1000), Result(True, 1000))
        self.assertEqual(led.grant(2001).reason, "bytes")
        self.assertEqual(limits_from({}, SERVER_LIMITS), SERVER_LIMITS)


if __name__ == "__main__":
    unittest.main()
