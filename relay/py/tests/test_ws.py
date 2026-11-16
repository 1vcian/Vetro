"""The WebSocket codec (RFC 6455, server side): the handshake key, masking,
the three length forms, fragments, control frames, sizes, close."""

import struct
import unittest

from vetro_connect import ws
from vetro_connect.ws import OP_BINARY, OP_CLOSE, OP_CONT, OP_PING, OP_PONG, OP_TEXT, Parser, WsError, apply_mask, encode_frame

MASK = b"\x37\xfa\x21\x3d"


def client(opcode, payload=b"", fin=True, mask=MASK):
    return encode_frame(opcode, payload, fin=fin, mask=mask)


class Handshake(unittest.TestCase):
    def test_accept_key_of_the_rfc(self):
        # RFC 6455, section 1.3.
        self.assertEqual(ws.accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")

    def test_key_must_be_16_bytes_of_base64(self):
        self.assertTrue(ws.valid_key("dGhlIHNhbXBsZSBub25jZQ=="))
        for bad in [None, "", "abc", "dGhlIHNhbXBsZSBub25jZQ", "dGhlIHNhbXBsZSBub25jZQ==\n", "AAAA", "é" * 24, "dGhlIHNhbXBsZSBub25jZSE=", 5]:
            self.assertFalse(ws.valid_key(bad), repr(bad))


class Encoding(unittest.TestCase):
    def test_masking_of_the_rfc(self):
        # RFC 6455, section 5.7: a masked "Hello".
        self.assertEqual(client(OP_TEXT, b"Hello"), bytes.fromhex("818537fa213d7f9f4d5158"))
        self.assertEqual(encode_frame(OP_TEXT, b"Hello"), bytes.fromhex("810548656c6c6f"))
        self.assertEqual(apply_mask(apply_mask(b"Hello, world", MASK), MASK), b"Hello, world")
        self.assertEqual(apply_mask(b"", MASK), b"")
        for n in range(1, 10):
            data = bytes(range(n))
            self.assertEqual(apply_mask(data, MASK), bytes(b ^ MASK[i % 4] for i, b in enumerate(data)))

    def test_the_three_length_forms(self):
        for n, head in [(0, b"\x82\x00"), (125, b"\x82\x7d"), (126, b"\x82\x7e\x00\x7e"), (65535, b"\x82\x7e\xff\xff"), (65536, b"\x82\x7f" + struct.pack(">Q", 65536))]:
            frame = encode_frame(OP_BINARY, bytes(n))
            self.assertEqual(frame[: len(head)], head, n)
            self.assertEqual(len(frame), len(head) + n)
            masked = client(OP_BINARY, bytes(n))
            self.assertEqual(masked[0], 0x82)
            self.assertEqual(masked[1], head[1] | 0x80)
            self.assertEqual(masked[2 : len(head)], head[2:])
            self.assertEqual(len(masked), len(head) + 4 + n)

    def test_close_payload(self):
        self.assertEqual(ws.close_payload(), b"")
        self.assertEqual(ws.close_payload(4002, "protocol"), b"\x0f\xa2protocol")
        self.assertEqual(len(ws.close_payload(1000, "x" * 500)), 125)


class Parsing(unittest.TestCase):
    def test_text_binary_and_the_length_forms(self):
        p = Parser(max_message=70_000)
        self.assertEqual(p.feed(client(OP_TEXT, b"Hello")), [(OP_TEXT, "Hello")])
        self.assertEqual(p.feed(client(OP_TEXT, "caffè ☕".encode())), [(OP_TEXT, "caffè ☕")])
        for n in [0, 1, 125, 126, 127, 65535, 65536, 70_000]:
            data = bytes((i * 7) & 255 for i in range(n))
            self.assertEqual(p.feed(client(OP_BINARY, data)), [(OP_BINARY, data)], n)

    def test_bytes_arriving_in_any_pieces(self):
        frames = client(OP_TEXT, b"one") + client(OP_BINARY, bytes(300)) + client(OP_PING, b"p") + client(OP_BINARY, bytes(70_000))
        want = [(OP_TEXT, "one"), (OP_BINARY, bytes(300)), (OP_PING, b"p"), (OP_BINARY, bytes(70_000))]
        self.assertEqual(Parser(max_message=70_000).feed(frames), want)
        for step in [1, 2, 3, 7, 1000]:
            p = Parser(max_message=70_000)
            got = []
            for at in range(0, len(frames), step):
                got += p.feed(frames[at : at + step])
            self.assertEqual(got, want, step)

    def test_fragments_with_control_frames_between(self):
        p = Parser()
        self.assertEqual(p.feed(client(OP_BINARY, b"ab", fin=False)), [])
        self.assertEqual(p.feed(client(OP_PING, b"x")), [(OP_PING, b"x")])
        self.assertEqual(p.feed(client(OP_CONT, b"cd", fin=False)), [])
        self.assertEqual(p.feed(client(OP_CONT, b"", fin=False) + client(OP_PONG, b"y") + client(OP_CONT, b"ef")), [(OP_PONG, b"y"), (OP_BINARY, b"abcdef")])
        # Text split inside a character.
        euro = "€".encode()
        self.assertEqual(p.feed(client(OP_TEXT, euro[:1], fin=False) + client(OP_CONT, euro[1:])), [(OP_TEXT, "€")])
        # And the next message starts clean.
        self.assertEqual(p.feed(client(OP_BINARY, b"z")), [(OP_BINARY, b"z")])

    def expect(self, code, data, **options):
        with self.assertRaises(WsError) as raised:
            Parser(**options).feed(data)
        self.assertEqual(raised.exception.code, code)

    def test_violations(self):
        self.expect(1002, encode_frame(OP_TEXT, b"Hello"))  # a client frame without a mask
        self.expect(1002, bytes([0x80 | 0x40 | OP_TEXT, 0x80]) + MASK)  # RSV1: compression nobody agreed
        self.expect(1002, bytes([0x80 | 0x10 | OP_TEXT, 0x80]) + MASK)
        for opcode in [3, 4, 5, 6, 7, 0xB, 0xC, 0xF]:
            self.expect(1002, client(opcode, b""))
        self.expect(1002, client(OP_CONT, b"x"))  # nothing to continue
        self.expect(1002, client(OP_TEXT, b"a", fin=False) + client(OP_TEXT, b"b"))  # a message inside a message
        self.expect(1002, client(OP_PING, b"x", fin=False))  # a fragmented control frame
        self.expect(1002, client(OP_PING, bytes(126)))  # a control frame over 125 bytes
        self.expect(1002, client(OP_CLOSE, bytes(126)))
        self.expect(1002, client(OP_CLOSE, b"\x03"))  # half a close code
        self.expect(1002, bytes([0x82, 0xFF]) + struct.pack(">Q", 1 << 63) + MASK)  # a length with the top bit
        self.expect(1007, client(OP_TEXT, b"\xff\xfe"))  # text that is not UTF-8
        self.expect(1007, client(OP_TEXT, "€".encode()[:2]))

    def test_message_size_is_bounded(self):
        self.assertEqual(ws.MAX_MESSAGE, 64 * 1024)
        self.assertEqual(len(Parser().feed(client(OP_BINARY, bytes(ws.MAX_MESSAGE)))[0][1]), ws.MAX_MESSAGE)
        self.expect(1009, client(OP_BINARY, bytes(ws.MAX_MESSAGE + 1)))
        # Refused from the header alone, before any of the payload arrives.
        self.expect(1009, client(OP_BINARY, bytes(ws.MAX_MESSAGE + 1))[:10])
        self.expect(1009, bytes([0x82, 0xFF]) + struct.pack(">Q", 1 << 40) + MASK)
        # Fragments add up.
        self.expect(1009, client(OP_BINARY, bytes(600), fin=False) + client(OP_CONT, bytes(500)), max_message=1000)
        self.assertEqual(Parser(max_message=1000).feed(client(OP_BINARY, bytes(600), fin=False) + client(OP_CONT, bytes(400))), [(OP_BINARY, bytes(1000))])
        # Control frames do not count.
        self.assertEqual(Parser(max_message=10).feed(client(OP_PING, bytes(125))), [(OP_PING, bytes(125))])

    def test_close(self):
        p = Parser()
        self.assertEqual(p.feed(client(OP_CLOSE, ws.close_payload(1000, "bye")) + client(OP_TEXT, b"late")), [(OP_CLOSE, (1000, "bye"))])
        self.assertTrue(p.closed)
        self.assertEqual(p.feed(client(OP_TEXT, b"later")), [], "nothing after a close")
        self.assertEqual(Parser().feed(client(OP_CLOSE)), [(OP_CLOSE, (None, ""))])
        self.assertEqual(Parser().feed(client(OP_TEXT, b"a") + client(OP_CLOSE, b"\x03\xe9")), [(OP_TEXT, "a"), (OP_CLOSE, (1001, ""))])

    def test_server_frames_parse_without_a_mask(self):
        p = Parser(require_mask=False)
        self.assertEqual(p.feed(encode_frame(OP_TEXT, b"hello") + encode_frame(OP_BINARY, bytes(200))), [(OP_TEXT, "hello"), (OP_BINARY, bytes(200))])


if __name__ == "__main__":
    unittest.main()
