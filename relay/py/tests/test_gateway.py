"""The HTTP side: sessions for the allowed origins only, renewal, and the
checks of a lane upgrade (the cases of relay/test/unit.mjs)."""

import json
import unittest

from vetro_connect.gateway import DEFAULT_ORIGIN, Config, Request, check_lane, handle_http
from vetro_connect.tokens import SESSION_MAX_S, TOKEN_TTL_S, issue, verify

SECRET = "x" * 40
ORIGIN = "https://vetro.example"
DEV = "http://127.0.0.1:8080"


def cfg(**over):
    return Config(**{"secret": SECRET, "origins": [ORIGIN, DEV], **over})


def post(body=b"{}", origin=ORIGIN, path="/session"):
    headers = {"content-type": "application/json"}
    if origin is not None:
        headers["origin"] = origin
    return Request("POST", path, headers, body if isinstance(body, bytes) else json.dumps(body).encode())


def session(response):
    return json.loads(response.body)


class Sessions(unittest.TestCase):
    def test_the_default_origin_is_the_live_site(self):
        self.assertEqual(DEFAULT_ORIGIN, "https://1vcian.me")
        self.assertEqual(Config(secret=SECRET).origins, ["https://1vcian.me"])
        self.assertEqual(handle_http(post(origin="https://1vcian.me"), Config(secret=SECRET)).status, 200)
        # Close is not the same: scheme, host, port and no path.
        for other in ["http://1vcian.me", "https://1vcian.me:443", "https://1vcian.me/", "https://www.1vcian.me", "https://1vcian.me.evil.example", "https://1vcian.github.io", "null", "HTTPS://1VCIAN.ME", ""]:
            self.assertEqual(handle_http(post(origin=other), Config(secret=SECRET)).status, 403, other)

    def test_sessions_only_for_allowed_origins(self):
        self.assertEqual(handle_http(post(origin="https://evil.example"), cfg()).status, 403)
        r = handle_http(post(origin="https://evil.example"), cfg())
        self.assertEqual((json.loads(r.body), r.headers.get("access-control-allow-origin")), ({"error": "origin"}, None))
        self.assertEqual(handle_http(post(origin=None), cfg()).status, 403)
        self.assertEqual(handle_http(post(), cfg(enabled=False)).status, 503)
        self.assertEqual(handle_http(Request("GET", "/session", {"origin": ORIGIN}), cfg()).status, 405)
        self.assertEqual(handle_http(Request("GET", "/other"), cfg()).status, 404)
        self.assertEqual(handle_http(Request("GET", "/lane", {"origin": ORIGIN}), cfg()).status, 404, "a lane without an upgrade")
        self.assertEqual(handle_http(Request("GET", "/session/x", {"origin": ORIGIN}), cfg()).status, 404)
        self.assertEqual(handle_http(post(origin=DEV), cfg()).status, 200)

    def test_preflight(self):
        pre = handle_http(Request("OPTIONS", "/session", {"origin": ORIGIN}), cfg())
        self.assertEqual(pre.status, 204)
        self.assertEqual(pre.body, b"")
        self.assertEqual(pre.headers, {"access-control-allow-origin": ORIGIN, "access-control-allow-methods": "POST", "access-control-allow-headers": "content-type", "access-control-max-age": "600", "vary": "Origin"})
        bad = handle_http(Request("OPTIONS", "/session", {"origin": "https://evil.example"}), cfg())
        self.assertEqual((bad.status, bad.headers, bad.body), (403, {}, b""))
        # The same answer as the Node gateway's, whatever else the browser asks.
        pna = {"access-control-request-private-network": "true"}
        self.assertEqual(handle_http(Request("OPTIONS", "/session", {"origin": ORIGIN, **pna}), cfg()).headers, pre.headers)
        self.assertEqual(handle_http(Request("OPTIONS", "/session", {"origin": "https://evil.example", **pna}), cfg()).headers, {})

    def test_health(self):
        r = handle_http(Request("GET", "/health"), cfg())
        self.assertEqual((r.status, json.loads(r.body)), (200, {"ok": True, "enabled": True}))
        self.assertNotIn("access-control-allow-origin", r.headers)
        self.assertEqual(handle_http(Request("GET", "/health", {"origin": ORIGIN}), cfg()).headers["access-control-allow-origin"], ORIGIN)
        self.assertEqual(json.loads(handle_http(Request("GET", "/health?x=1"), cfg(enabled=False)).body), {"ok": True, "enabled": False})

    def test_session_and_renewal(self):
        now = 5_000_000_000
        r = handle_http(post(b""), cfg(), now=now, lanes=8)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.headers, {"content-type": "application/json", "cache-control": "no-store", "access-control-allow-origin": ORIGIN, "vary": "Origin"})
        s = session(r)
        self.assertEqual(sorted(s), ["expires", "laneConns", "lanes", "token"])
        self.assertEqual((s["lanes"], s["laneConns"], s["expires"]), (8, 32, now + TOKEN_TTL_S * 1000))
        self.assertEqual(session(handle_http(post(), cfg(lane_conns=5), now=now, lanes=3))["laneConns"], 5)
        p = verify(SECRET, s["token"], origin=ORIGIN, now=now / 1000)
        self.assertEqual((p["iat"], p["exp"], p["first"]), (now // 1000, now // 1000 + TOKEN_TTL_S, now // 1000))
        # Every session has its own id.
        other = verify(SECRET, session(handle_http(post(), cfg(), now=now))["token"], origin=ORIGIN, now=now / 1000)
        self.assertNotEqual(other["sid"], p["sid"])
        # Renewal (even expired): the same session.
        later = now + (TOKEN_TTL_S + 60) * 1000
        r2 = session(handle_http(post({"renew": s["token"]}), cfg(), now=later))
        p2 = verify(SECRET, r2["token"], origin=ORIGIN, now=later / 1000)
        self.assertEqual((p2["sid"], p2["first"]), (p["sid"], p["first"]))
        self.assertEqual(p2["exp"], later // 1000 + TOKEN_TTL_S)
        # Too old, a bad token, or another origin's token: a new session.
        old = now + SESSION_MAX_S * 1000
        r3 = session(handle_http(post({"renew": s["token"]}), cfg(), now=old))
        self.assertNotEqual(verify(SECRET, r3["token"], origin=ORIGIN, now=old / 1000)["sid"], p["sid"])
        almost = old - 1000
        r3b = session(handle_http(post({"renew": s["token"]}), cfg(), now=almost))
        self.assertEqual(verify(SECRET, r3b["token"], origin=ORIGIN, now=almost / 1000)["sid"], p["sid"])
        for renew in ["garbage", 7, ["x"], {"a": 1}]:
            r4 = session(handle_http(post({"renew": renew}), cfg(), now=now))
            self.assertNotEqual(verify(SECRET, r4["token"], origin=ORIGIN, now=now / 1000)["sid"], p["sid"], renew)
        r5 = session(handle_http(post({"renew": s["token"]}, origin=DEV), cfg(), now=now))
        p5 = verify(SECRET, r5["token"], origin=DEV, now=now / 1000)
        self.assertNotEqual(p5["sid"], p["sid"])
        self.assertEqual(p5["origin"], DEV)

    def test_bodies(self):
        fits = b'{"a":"' + b"x" * 2040 + b'"}'
        self.assertEqual(len(fits), 2048)
        for body in [b"{", b"\xff", b"NaN", b"   ", b"[" * 2000, fits[:-2] + b'x"}']:
            r = handle_http(post(body), cfg())
            self.assertEqual((r.status, json.loads(r.body)), (400, {"error": "malformed"}), body[:10])
            self.assertEqual(r.headers["access-control-allow-origin"], ORIGIN)
        # JSON that is not an object is a request without a renewal.
        for body in [b"null", b"[]", b'"renew"', b"7", fits]:
            self.assertEqual(handle_http(post(body), cfg()).status, 200, body[:10])


class Lanes(unittest.TestCase):
    def lane(self, query, headers=None, method="GET"):
        return Request(method, "/lane?" + query, {"upgrade": "websocket", "origin": ORIGIN} if headers is None else headers)

    def test_lane_upgrades_need_a_valid_token_and_the_same_origin(self):
        now = 7_000_000_000
        token = issue(SECRET, sid="abc", origin=ORIGIN, now=now // 1000)
        ok = "token=%s&lane=" % token
        self.assertEqual(check_lane(self.lane(ok + "3"), cfg(), now=now), {"sid": "abc", "lane": 3})
        self.assertEqual(check_lane(self.lane(ok + "0"), cfg(), now=now), {"sid": "abc", "lane": 0})
        self.assertEqual(check_lane(self.lane(ok + "7"), cfg(), now=now, lanes=8), {"sid": "abc", "lane": 7})
        self.assertEqual(check_lane(self.lane(ok + "0", {"upgrade": "WebSocket", "origin": ORIGIN}), cfg(), now=now), {"sid": "abc", "lane": 0})
        for bad in ["8", "x", "-1", "1.5", "0x1", "1e0", " 1", "١", "99999999999999999999"]:
            self.assertEqual(check_lane(self.lane(ok + bad), cfg(), now=now), {"status": 400, "error": "lane"}, bad)
        self.assertEqual(check_lane(self.lane(ok + "3"), cfg(), now=now, lanes=3), {"status": 400, "error": "lane"})
        # As the Node gateway reads it: no `lane` at all is lane 0.
        self.assertEqual(check_lane(self.lane("token=" + token), cfg(), now=now), {"sid": "abc", "lane": 0})
        self.assertEqual(check_lane(self.lane(ok + "0", {"origin": ORIGIN}), cfg(), now=now), {"status": 426, "error": "upgrade"})
        self.assertEqual(check_lane(self.lane(ok + "0", {"upgrade": "h2c", "origin": ORIGIN}), cfg(), now=now)["status"], 426)
        self.assertEqual(check_lane(self.lane(ok + "0", {"upgrade": "websocket", "origin": DEV}), cfg(), now=now), {"status": 401, "error": "origin"}, "another allowed origin, not the token's")
        self.assertEqual(check_lane(self.lane(ok + "0", {"upgrade": "websocket"}), cfg(), now=now), {"status": 403, "error": "origin"})
        self.assertEqual(check_lane(self.lane(ok + "0", {"upgrade": "websocket", "origin": "https://evil.example"}), cfg(), now=now)["status"], 403)
        self.assertEqual(check_lane(self.lane(ok + "0"), cfg(), now=now + TOKEN_TTL_S * 1000), {"status": 401, "error": "expired"})
        self.assertNotIn("error", check_lane(self.lane(ok + "0"), cfg(), now=now + TOKEN_TTL_S * 1000 - 1))
        self.assertEqual(check_lane(self.lane("lane=0"), cfg(), now=now), {"status": 401, "error": "malformed"})
        self.assertEqual(check_lane(self.lane("token=&lane=0"), cfg(), now=now), {"status": 401, "error": "malformed"})
        self.assertEqual(check_lane(self.lane("token=%s&lane=0" % issue("y" * 40, sid="abc", origin=ORIGIN, now=now // 1000)), cfg(), now=now), {"status": 401, "error": "signature"})
        self.assertEqual(check_lane(self.lane(ok + "0"), cfg(enabled=False), now=now), {"status": 503, "error": "disabled"})
        self.assertEqual(check_lane(Request("GET", "/session?" + ok + "0", {"upgrade": "websocket", "origin": ORIGIN}), cfg(), now=now), {"status": 404, "error": "not-found"})

    def test_query_parameters(self):
        r = Request("GET", "/lane?a=1&token=x%2Ey&lane=2&lane=5&empty=")
        self.assertEqual((r.path, r.param("token"), r.param("lane"), r.param("empty"), r.param("none")), ("/lane", "x.y", "2", "", None))
        self.assertEqual(Request("GET", "/lane").param("lane"), None)


if __name__ == "__main__":
    unittest.main()
