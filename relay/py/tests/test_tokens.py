"""Session tokens: signature, origin, expiry, renewal. (That they are the
same tokens as relay/src/token.mjs makes is checked by relay/test/python.mjs,
where Node is at hand.)"""

import base64
import hashlib
import hmac
import json
import unittest

from vetro_connect.tokens import SESSION_MAX_S, TOKEN_TTL_S, b64url, issue, new_secret, new_session_id, unb64url, verify

SECRET = "x" * 40
ORIGIN = "https://vetro.example"


class Tokens(unittest.TestCase):
    def test_signature_origin_expiry_renewal(self):
        now = 1_000_000
        t = issue(SECRET, sid="s1", origin=ORIGIN, now=now)
        ok = verify(SECRET, t, origin=ORIGIN, now=now + 1)
        self.assertEqual(ok, {"v": 1, "sid": "s1", "origin": ORIGIN, "iat": now, "exp": now + TOKEN_TTL_S, "first": now})
        self.assertEqual(verify(SECRET, t, origin="https://evil.example", now=now), {"error": "origin"})
        self.assertEqual(verify(SECRET, t, origin=None, now=now), {"error": "origin"})
        self.assertNotIn("error", verify(SECRET, t, origin=ORIGIN, now=now + TOKEN_TTL_S - 1))
        self.assertEqual(verify(SECRET, t, origin=ORIGIN, now=now + TOKEN_TTL_S), {"error": "expired"})
        self.assertEqual(verify(SECRET, t, origin=ORIGIN, now=now + TOKEN_TTL_S, allow_expired=True)["sid"], "s1")
        # An expired token of another origin is still not that origin's.
        self.assertEqual(verify(SECRET, t, origin="https://evil.example", now=now + TOKEN_TTL_S, allow_expired=True), {"error": "origin"})
        self.assertEqual(verify("y" * 40, t, origin=ORIGIN, now=now), {"error": "signature"})
        # A renewal keeps when the session started.
        r = issue(SECRET, sid="s1", origin=ORIGIN, now=now + 500, first=now)
        self.assertEqual(verify(SECRET, r, origin=ORIGIN, now=now + 500)["first"], now)

    def test_forged_and_malformed(self):
        now = 1_000_000
        t = issue(SECRET, sid="s1", origin=ORIGIN, now=now)
        p, s = t.split(".")
        forged = b64url(json.dumps({"v": 1, "sid": "s2", "origin": ORIGIN, "iat": now, "exp": now + 10**6, "first": now}).encode())
        self.assertEqual(verify(SECRET, forged + "." + s, origin=ORIGIN, now=now), {"error": "signature"})
        # One bit of the signature, or of the payload.
        flipped = s[:-1] + ("A" if s[-1] != "A" else "B")
        self.assertEqual(verify(SECRET, p + "." + flipped, origin=ORIGIN, now=now), {"error": "signature"})
        self.assertEqual(verify(SECRET, p[:-1] + "." + s, origin=ORIGIN, now=now), {"error": "signature"})
        self.assertEqual(verify(SECRET, ".", origin=ORIGIN, now=now), {"error": "signature"})
        for bad in ["", "abc", p + "." + s + ".x", p + ".!!", p + ".a", None, 5, b"x.y", "a" * 2000, p + "." + s + "=", p + " ." + s]:
            self.assertEqual(verify(SECRET, bad, origin=ORIGIN, now=now), {"error": "malformed"}, repr(bad)[:30])

    def test_a_signed_payload_must_still_be_a_token(self):
        def signed(body: bytes) -> str:
            payload = b64url(body)
            return payload + "." + b64url(hmac.new(SECRET.encode(), payload.encode(), hashlib.sha256).digest())

        good = {"v": 1, "sid": "s", "origin": ORIGIN, "iat": 1, "exp": 100, "first": 1}
        self.assertNotIn("error", verify(SECRET, signed(json.dumps(good).encode()), origin=ORIGIN, now=5))
        for change in [{"v": 2}, {"v": True}, {"sid": 7}, {"exp": "100"}, {"exp": True}, {"first": None}]:
            body = json.dumps({**good, **change}).encode()
            self.assertEqual(verify(SECRET, signed(body), origin=ORIGIN, now=5), {"error": "malformed"}, change)
        for body in [b"null", b"[]", b"7", b"{", b"\xff\xfe"]:
            self.assertEqual(verify(SECRET, signed(body), origin=ORIGIN, now=5), {"error": "malformed"}, body)

    def test_the_secret_and_the_ids(self):
        with self.assertRaises(ValueError):
            issue("short", sid="x", origin=ORIGIN, now=1)
        with self.assertRaises(ValueError):
            verify("short", "YQ.YQ", origin=ORIGIN, now=1)
        a, b = new_secret(), new_secret()
        self.assertNotEqual(a, b)
        self.assertGreaterEqual(len(a), 32)
        sid = new_session_id()
        self.assertEqual(len(unb64url(sid)), 16)
        self.assertNotEqual(sid, new_session_id())

    def test_the_wire_form(self):
        # Compact JSON in the order of the JS (token.mjs), base64url without padding.
        t = issue(SECRET, sid="s1", origin=ORIGIN, now=1000, first=900)
        p, s = t.split(".")
        self.assertEqual(unb64url(p), b'{"v":1,"sid":"s1","origin":"https://vetro.example","iat":1000,"exp":1600,"first":900}')
        self.assertEqual(unb64url(s), hmac.new(SECRET.encode(), p.encode(), hashlib.sha256).digest())
        self.assertNotIn("=", t)
        self.assertEqual(b64url(b"\xfb\xff\xfe"), "-__-")
        self.assertEqual(unb64url("-__-"), b"\xfb\xff\xfe")
        self.assertEqual(unb64url(base64.urlsafe_b64encode(b"ab").decode().rstrip("=")), b"ab")
        for bad in ["a", "a+b/", "ab==", "a b"]:
            self.assertIsNone(unb64url(bad), bad)
        self.assertEqual(SESSION_MAX_S, 7200)
        self.assertEqual(TOKEN_TTL_S, 600)


if __name__ == "__main__":
    unittest.main()
