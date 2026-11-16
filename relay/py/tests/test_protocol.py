"""The protocol: the vectors shared with `vetro_net::relay` and
relay/src/protocol.mjs (relay/test/vectors.txt), and malformed frames."""

import os
import unittest

from vetro_connect.protocol import HEADER, MAX_DATA, MAX_DNS, OP, WINDOW, Frame, decode, encode, frame_id, with_id

VECTORS = os.path.join(os.path.dirname(__file__), "..", "..", "test", "vectors.txt")

DNS_Q = bytes([0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0])
DNS_A = bytes([0x12, 0x34, 0x81, 0x80, 0, 0, 0, 0, 0, 0, 0, 0])

# name -> (the frame built here, the fields the decoder must give).
BUILD = {
    "connect": (encode.connect(7, "93.184.215.14", 443), dict(type="connect", id=7, ip="93.184.215.14", port=443)),
    "connect_name": (encode.connect_name(8, "example.com", 80), dict(type="connectName", id=8, name="example.com", port=80)),
    "data_machine": (encode.data(7, b"hello", False), dict(type="data", id=7, data=b"hello")),
    "data_gateway": (encode.data(7, b"world", True), dict(type="data", id=7, data=b"world", from_gateway=True)),
    "shutdown_machine": (encode.shutdown(7, False), dict(type="shutdown", id=7)),
    "shutdown_gateway": (encode.shutdown(7, True), dict(type="shutdown", id=7)),
    "close_machine": (encode.close(7, False, False), dict(type="close", id=7, reset=False)),
    "close_gateway": (encode.close(9, True, True), dict(type="close", id=9, reset=True)),
    "ack_machine": (encode.ack(7, 1000, False), dict(type="ack", id=7, n=1000)),
    "ack_gateway": (encode.ack(7, WINDOW, True), dict(type="ack", id=7, n=WINDOW)),
    "dns": (encode.dns(3, DNS_Q), dict(type="dns", id=3, msg=DNS_Q)),
    "dns_answer": (encode.dns_answer(3, DNS_A), dict(type="dnsAnswer", id=3, msg=DNS_A)),
    "connected": (encode.connected(7), dict(type="connected", id=7)),
    "refused": (encode.refused(9, "platform"), dict(type="refused", id=9, reason="platform")),
    "online": (encode.online(True), dict(type="online", id=0, on=True)),
    "down": (encode.down(), dict(type="down", id=0)),
    "big_id": (encode.data(0xFFFFFFFE, b"\xff", False), dict(type="data", id=0xFFFFFFFE, data=b"\xff")),
}


def reencode(f: Frame) -> bytes:
    """The frame again, from nothing but what the decoder gave."""
    g = f.from_gateway
    return {
        "connect": lambda: encode.connect(f.id, f.ip, f.port),
        "connectName": lambda: encode.connect_name(f.id, f.name, f.port),
        "data": lambda: encode.data(f.id, f.data, g),
        "shutdown": lambda: encode.shutdown(f.id, g),
        "close": lambda: encode.close(f.id, f.reset, g),
        "ack": lambda: encode.ack(f.id, f.n, g),
        "dns": lambda: encode.dns(f.id, f.msg),
        "dnsAnswer": lambda: encode.dns_answer(f.id, f.msg),
        "connected": lambda: encode.connected(f.id),
        "refused": lambda: encode.refused(f.id, f.reason),
        "online": lambda: encode.online(f.on),
        "down": lambda: encode.down(),
    }[f.type]()


def vectors():
    with open(VECTORS, encoding="utf-8") as fh:
        return [line.split(" ") for line in fh.read().split("\n") if line and not line.startswith("#")]


class Vectors(unittest.TestCase):
    def test_every_vector_decodes_and_encodes_back(self):
        lines = vectors()
        self.assertEqual(len(lines), len(BUILD))
        self.assertEqual(sorted(name for name, _, _ in lines), sorted(BUILD))
        for name, side, hexa in lines:
            with self.subTest(name):
                raw = bytes.fromhex(hexa)
                built, want = BUILD[name]
                self.assertEqual(built.hex(), hexa)
                f = decode(raw)
                self.assertIsNotNone(f)
                self.assertEqual(f.from_gateway, side == "gateway")
                for k, v in want.items():
                    self.assertEqual(getattr(f, k), v, k)
                self.assertEqual(reencode(f).hex(), hexa)

    def test_refusal_reasons_by_name_and_number(self):
        for name, number in {"unreachable": 1, "port": 2, "address": 3, "limit": 4, "platform": 5, "resolve": 6, "down": 7}.items():
            self.assertEqual(encode.refused(1, name), encode.refused(1, number))
            self.assertEqual(decode(encode.refused(1, number)).reason, name)


class Malformed(unittest.TestCase):
    def test_malformed_frames_are_rejected(self):
        bad = [
            [],
            [OP.DATA, 0, 0, 0, 1],  # DATA without bytes
            [OP.CLOSE, 0, 0, 0, 1, 2],  # a flag that is not 0 or 1
            [OP.CLOSE, 0, 0, 0, 1],
            [OP.CLOSE, 0, 0, 0, 1, 0, 0],
            [OP.REFUSED, 0, 0, 0, 1, 9],  # no such reason
            [OP.REFUSED, 0, 0, 0, 1, 0],
            [OP.REFUSED, 0, 0, 0, 1],
            [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80, 0x41],  # upper case
            [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80, 0x61, 0x2E, 0x2E, 0x62],  # empty label
            [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80, 0x61, 0x0A],  # a line feed at the end
            [OP.CONNECT_NAME, 0, 0, 0, 1, 0, 80],  # no name
            [OP.ONLINE, 0, 0, 0, 1, 1],  # ONLINE with an id
            [OP.ONLINE, 0, 0, 0, 0, 2],
            [OP.DOWN, 0, 0, 0, 1],
            [OP.DOWN, 0, 0, 0, 0, 0],
            [0x42, 0, 0, 0, 1],  # no such op
            [0x00, 0, 0, 0, 1],
            [OP.CONNECT, 0, 0, 0, 1, 1, 2, 3, 4, 0],  # 5 bytes, not 6
            [OP.CONNECT, 0, 0, 0, 1, 1, 2, 3, 4, 0, 80, 0],
            [OP.DNS, 0, 0, 0, 1, 1, 2, 3],  # shorter than a DNS header
            [OP.ACK, 0, 0, 0, 1, 0, 0, 1],
            [OP.ACK, 0, 0, 0, 1, 0, 0, 0, 1, 0],
            [OP.SHUTDOWN, 0, 0, 0, 1, 0],
            [OP.CONNECTED, 0, 0, 0, 1, 0],
            [OP.DATA, 0, 0, 0],  # shorter than the header
        ]
        for b in bad:
            self.assertIsNone(decode(bytes(b)), b)
        big = bytearray(HEADER + MAX_DATA + 1)
        big[0] = OP.DATA
        self.assertIsNone(decode(bytes(big)))
        self.assertIsNotNone(decode(bytes(big[:-1])))
        self.assertIsNone(decode(encode.dns(1, bytes(MAX_DNS + 1))))
        self.assertIsNotNone(decode(encode.dns(1, bytes(MAX_DNS))))
        # Names: 63-byte labels and 253 bytes in all, no more.
        self.assertIsNotNone(decode(encode.connect_name(1, "a" * 63 + ".b", 80)))
        self.assertIsNone(decode(encode.connect_name(1, "a" * 64 + ".b", 80)))
        long = ".".join(["a" * 63] * 3 + ["b" * 61])
        self.assertEqual(len(long), 253)
        self.assertIsNotNone(decode(encode.connect_name(1, long, 80)))
        self.assertIsNone(decode(encode.connect_name(1, long + "c", 80)))

    def test_ids(self):
        f = with_id(encode.connected(5), 77)
        self.assertEqual(frame_id(f), 77)
        self.assertEqual(decode(f).type, "connected")
        self.assertEqual(frame_id(encode.connected(5)), 5)
        self.assertEqual(frame_id(encode.connected(0x1_0000_0002)), 2, "ids are u32")


if __name__ == "__main__":
    unittest.main()
