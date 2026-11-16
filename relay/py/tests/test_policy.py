"""The destination policy: every blocked range and the ports."""

import os
import re
import unittest

from vetro_connect.policy import ALLOWED_PORTS, BLOCKED, address_blocked, check_destination, parse_ipv4, port_allowed

POLICY_MJS = os.path.join(os.path.dirname(__file__), "..", "..", "src", "policy.mjs")


def ip(v: int) -> str:
    return ".".join(str((v >> s) & 255) for s in (24, 16, 8, 0))


class Ranges(unittest.TestCase):
    def test_the_same_ranges_as_the_node_gateway(self):
        with open(POLICY_MJS, encoding="utf-8") as fh:
            src = fh.read()
        start = src.index("export const BLOCKED")
        table = src[start : src.index("]);", start)]
        theirs = [(base, int(length)) for base, length in re.findall(r"\['([0-9.]+)', (\d+),", table)]
        self.assertEqual(len(theirs), 15)
        self.assertEqual([(base, length) for base, length, _ in BLOCKED], theirs)
        self.assertEqual(list(ALLOWED_PORTS), [int(p) for p in re.search(r"ALLOWED_PORTS = Object\.freeze\(\[([0-9, ]+)\]\)", src).group(1).split(",")])

    def test_every_blocked_range_first_and_last_address_neighbours_allowed(self):
        blocked = [(parse_ipv4(base), parse_ipv4(base) + 2 ** (32 - length) - 1) for base, length, _ in BLOCKED]
        for (base, length, why), (first, last) in zip(BLOCKED, blocked):
            with self.subTest("%s/%d" % (base, length)):
                self.assertEqual(address_blocked(ip(first)), why)
                self.assertEqual(address_blocked(ip(last)), why)
                self.assertIsNotNone(address_blocked(ip((first + last) // 2)))
                # The addresses just outside, unless another range starts there.
                for neighbour in (first - 1, last + 1):
                    if 0 <= neighbour <= 0xFFFFFFFF and not any(a <= neighbour <= b for a, b in blocked):
                        self.assertIsNone(address_blocked(ip(neighbour)), ip(neighbour))

    def test_addresses(self):
        for a in ["169.254.169.254", "127.0.0.1", "0.0.0.0", "255.255.255.255", "10.0.2.2", "100.100.100.200", "192.168.1.1", "172.31.255.255", "198.18.0.1", "224.0.0.251", "192.168.0.1", "10.255.255.255", "127.255.255.254"]:
            self.assertIsNotNone(address_blocked(a), a)
        for a in ["9.255.255.255", "11.0.0.0", "100.63.255.255", "100.128.0.0", "172.15.255.255", "172.32.0.0", "198.17.255.255", "198.20.0.0", "223.255.255.255", "1.1.1.1", "93.184.215.14", "8.8.8.8", "126.255.255.255", "128.0.0.0", "169.253.255.255", "169.255.0.0", "192.167.255.255", "192.169.0.0"]:
            self.assertIsNone(address_blocked(a), a)

    def test_only_plain_dotted_ipv4(self):
        for bad in ["1.2.3", "01.2.3.4", "256.1.1.1", "::1", "1.2.3.4.5", " 1.2.3.4", "1.2.3.4 ", "1.2.3.4\n", "localhost", "", "0x7f.0.0.1", "2130706433", "1.2.3.-4", "１.2.3.4", "1.2.3.٤", None, 16843009, b"1.1.1.1"]:
            self.assertIsNone(parse_ipv4(bad), repr(bad))
            self.assertEqual(address_blocked(bad), "not an IPv4 address", repr(bad))
            self.assertEqual(check_destination(bad, 443), "address", repr(bad))
        self.assertEqual(parse_ipv4("1.2.3.4"), 0x01020304)
        self.assertEqual(parse_ipv4("255.255.255.255"), 0xFFFFFFFF)
        self.assertEqual(parse_ipv4("0.0.0.0"), 0)


class Ports(unittest.TestCase):
    def test_only_ports_80_and_443(self):
        self.assertIsNone(check_destination("93.184.215.14", 443))
        self.assertIsNone(check_destination("93.184.215.14", 80))
        for p in [0, 22, 25, 53, 81, 442, 444, 853, 5228, 8080, 8443, 65535, None, "443", 443.5]:
            self.assertEqual(check_destination("93.184.215.14", p), "port", repr(p))
            self.assertFalse(port_allowed(p))
        # The port is looked at first, then the address.
        self.assertEqual(check_destination("10.0.0.1", 443), "address")
        self.assertEqual(check_destination("10.0.0.1", 22), "port")


if __name__ == "__main__":
    unittest.main()
