"""The launcher: the page's address, the browser to open, the options, the
status texts, the gateway on its thread, and the program itself on the
console (as a child process)."""

import json
import os
import socket
import subprocess
import sys
import unittest
import urllib.request
from unittest import mock

from vetro_connect import app
from vetro_connect.app import Runner, browser_command, format_bytes, headline, page_url, parse_args, parse_connect_map, parse_hosts, status_lines
from vetro_connect.ledger import limits_from
from vetro_connect.server import Gateway

CONNECT = os.path.join(os.path.dirname(__file__), "..", "connect.py")
PAGE = "https://1vcian.me/Vetro/app/?gateway=http://127.0.0.1:8787"

# Straight to the loopback address, whatever proxy the system has.
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def fetch(url, origin=None, data=None):
    """GET (or POST with `data`) as JSON."""
    request = urllib.request.Request(url, data=data, headers={"Origin": origin} if origin else {}, method="POST" if data is not None else "GET")
    with _opener.open(request, timeout=5) as r:
        return json.load(r)


class Page(unittest.TestCase):
    def test_the_page_address(self):
        self.assertEqual(app.DEFAULT_PAGE, "https://1vcian.me/Vetro/app/")
        self.assertEqual(page_url(app.DEFAULT_PAGE, 8787), PAGE)
        self.assertEqual(page_url("http://127.0.0.1:8080/app/?os=linux", 5001), "http://127.0.0.1:8080/app/?os=linux&gateway=http://127.0.0.1:5001")

    def test_chrome_or_edge_when_found(self):
        def found(*paths):
            return lambda p: p in paths

        mac = dict(platform="darwin", env={"HOME": "/Users/u"})
        chrome = "/Applications/Google Chrome.app"
        edge = "/Applications/Microsoft Edge.app"
        self.assertEqual(browser_command(PAGE, exists=found(chrome, edge), **mac), ["open", "-a", chrome, PAGE])
        self.assertEqual(browser_command(PAGE, exists=found(edge), **mac), ["open", "-a", edge, PAGE])
        self.assertEqual(browser_command(PAGE, exists=found("/Users/u/Applications/Google Chrome.app", edge), **mac), ["open", "-a", "/Users/u/Applications/Google Chrome.app", PAGE])
        self.assertEqual(browser_command(PAGE, exists=found("/Applications/Chromium.app"), **mac), ["open", "-a", "/Applications/Chromium.app", PAGE])
        self.assertIsNone(browser_command(PAGE, exists=found("/Applications/Safari.app", "/Applications/Firefox.app"), **mac))

        win = dict(platform="win32", env={"PROGRAMFILES": "C:\\Program Files", "PROGRAMFILES(X86)": "C:\\Program Files (x86)", "LOCALAPPDATA": "C:\\Users\\u\\AppData\\Local"})
        chrome = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"
        user_chrome = "C:\\Users\\u\\AppData\\Local\\Google\\Chrome\\Application\\chrome.exe"
        edge = "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"
        self.assertEqual(browser_command(PAGE, exists=found(chrome, edge), **win), [chrome, PAGE])
        self.assertEqual(browser_command(PAGE, exists=found(user_chrome, edge), **win), [user_chrome, PAGE])
        self.assertEqual(browser_command(PAGE, exists=found(edge), **win), [edge, PAGE])
        self.assertIsNone(browser_command(PAGE, exists=found(), **win))
        self.assertIsNone(browser_command(PAGE, exists=found(), platform="win32", env={}))

        which = {"chromium": "/usr/bin/chromium", "microsoft-edge": "/usr/bin/microsoft-edge"}
        self.assertEqual(browser_command(PAGE, platform="linux", env={}, which=which.get), ["/usr/bin/microsoft-edge", PAGE])
        self.assertEqual(browser_command(PAGE, platform="linux", env={}, which={"google-chrome": "/opt/chrome"}.get), ["/opt/chrome", PAGE])
        self.assertIsNone(browser_command(PAGE, platform="linux", env={}, which={}.get))


class Options(unittest.TestCase):
    def test_defaults_nothing_to_choose(self):
        a = parse_args([])
        self.assertEqual((a.port, a.origin, a.url, a.no_browser, a.no_ui, a.verbose), (None, [], app.DEFAULT_PAGE, False, False, False))
        self.assertEqual((a.test_connect_map, a.test_hosts, a.test_control), ({}, None, False))

    def test_options(self):
        a = parse_args(["--port", "9000", "--origin", "http://127.0.0.1:8080", "--origin", "http://localhost:8080", "--url", "http://127.0.0.1:8080/app/", "--no-browser", "--no-ui"])
        self.assertEqual((a.port, a.origin, a.url, a.no_browser, a.no_ui), (9000, ["http://127.0.0.1:8080", "http://localhost:8080"], "http://127.0.0.1:8080/app/", True, True))
        # What the Finder of older macOS versions adds is not an option of ours.
        self.assertEqual(parse_args(["-psn_0_123456"]).port, None)
        with mock.patch.object(sys, "argv", ["Vetro Connect", "-psn_0_1", "--port", "7"]):
            self.assertEqual(parse_args().port, 7)

    def test_the_test_only_options(self):
        self.assertEqual(parse_connect_map("93.184.215.14:443=127.0.0.1:5001,93.184.215.15:80=127.0.0.1:5002"), {("93.184.215.14", 443): ("127.0.0.1", 5001), ("93.184.215.15", 80): ("127.0.0.1", 5002)})
        self.assertEqual(parse_connect_map(""), {})
        for bad in ["x", "1.2.3.4:80", "1.2.3.4:80=localhost:5", "1.2.3.4=127.0.0.1:5", "1.2.3.4:x=127.0.0.1:5"]:
            with self.assertRaises(ValueError):
                parse_connect_map(bad)
        self.assertEqual(parse_hosts("a.example=1.2.3.4,b.example=1.2.3.4+10.0.0.1,none.example=,*=5.6.7.8"), {"a.example": ["1.2.3.4"], "b.example": ["1.2.3.4", "10.0.0.1"], "none.example": [], "*": ["5.6.7.8"]})
        for bad in ["=1.2.3.4", "a.example=localhost", "a.example=1.2.3"]:
            with self.assertRaises(ValueError):
                parse_hosts(bad)
        # They say what they are.
        with open(app.__file__, encoding="utf-8") as fh:
            source = fh.read()
        for option in ["--test-connect-map", "--test-hosts", "--test-control"]:
            line = next(x for x in source.split("\n") if 'add_argument("%s"' % option in x)
            self.assertIn("TEST ONLY", line)


class Status(unittest.TestCase):
    def test_counters_as_text(self):
        s = {"sessions": 1, "lanes": 1, "conns": 3, "conns_total": 120, "refused": 4, "bytes_to_remote": 1_234_567, "bytes_to_guest": 34_500_000_000, "dns": 52, "sockets": 124, "errors": 0}
        self.assertEqual(status_lines(s), ["Connections: 3 open, 120 so far", "Data: 1.2 MB sent, 34.5 GB received", "Name lookups: 52   Refused: 4"])
        self.assertEqual([format_bytes(n) for n in [0, 999, 1000, 1500, 999_949, 999_999, 1_000_000, 999_999_999, 2_500_000_000_000]], ["0 bytes", "999 bytes", "1.0 kB", "1.5 kB", "999.9 kB", "1.0 MB", "1.0 MB", "1.0 GB", "2500.0 GB"])

    def test_the_headline_waits_for_the_page(self):
        # Chrome asks the user before the site may talk to this computer:
        # until a session arrives, the status says what to do.
        self.assertEqual(headline({"sessions": 0}), app.WAITING)
        self.assertIn("allow the connection in Chrome", app.WAITING)
        self.assertEqual(headline({"sessions": 1}), app.ONLINE)
        self.assertEqual(headline({"sessions": 7}), app.ONLINE)


class Running(unittest.TestCase):
    def make(self):
        return Gateway(secret="k" * 40, origins=["https://1vcian.me"], limits=limits_from({}))

    def test_no_window_without_tkinter_or_a_display(self):
        # Then the console is used (the Program tests below run that way).
        try:
            import tkinter
        except ImportError:
            self.assertFalse(app.run_window(None, PAGE))
            return
        with mock.patch.object(tkinter, "Tk", side_effect=tkinter.TclError("no display")):
            self.assertFalse(app.run_window(None, PAGE))

    def test_signals_interrupt_the_console_and_only_ask_the_window(self):
        signals = app._Signals()
        with self.assertRaises(KeyboardInterrupt):
            signals(15, None)
        self.assertTrue(signals.asked.is_set())
        signals = app._Signals()
        signals.interrupt = False
        signals(15, None)
        self.assertTrue(signals.asked.is_set())

    def test_the_gateway_on_its_thread(self):
        runner = Runner(self.make, 0, fallback=False)
        gateway = runner.start()
        try:
            self.assertGreater(gateway.port, 0)
            self.assertEqual(fetch(gateway.url + "/health"), {"ok": True, "enabled": True})
            self.assertEqual(runner.call(lambda: 6 * 7), 42)
            with self.assertRaises(ZeroDivisionError):
                runner.call(lambda: 1 / 0)
            self.assertEqual(runner.call(gateway.stats.snapshot)["sessions"], 0)
            # The port is taken now: an error, or another port with the fallback.
            with self.assertRaises(OSError):
                Runner(self.make, gateway.port, fallback=False).start()
            other = Runner(self.make, gateway.port, fallback=True)
            try:
                self.assertNotIn(other.start().port, (0, gateway.port))
            finally:
                other.stop()
        finally:
            runner.stop()
        with self.assertRaises(OSError):
            socket.create_connection(("127.0.0.1", gateway.port), timeout=2).close()
        runner.stop()  # twice is fine


class Program(unittest.TestCase):
    """`python3 relay/py/connect.py` itself, on the console."""

    def start(self, *args):
        env = {**os.environ, "PYTHONUNBUFFERED": "1"}
        p = subprocess.Popen([sys.executable, CONNECT, "--no-browser", "--no-ui", *args], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        self.addCleanup(p.kill)
        self.addCleanup(p.stdout.close)
        self.addCleanup(p.stderr.close)
        return p

    def test_it_runs_says_where_and_waits_for_the_page(self):
        p = self.start("--port", "0", "--origin", "http://127.0.0.1:8080/")
        banner = p.stdout.readline()
        self.assertRegex(banner, r"^Vetro Connect [0-9.]+: the gateway is running at http://127\.0\.0\.1:\d+ \(this computer only\)\.$")
        url = banner.split(" at ")[1].split(" ")[0]
        self.assertEqual(p.stdout.readline(), "The phone: https://1vcian.me/Vetro/app/?gateway=%s\n" % url)
        self.assertEqual(p.stdout.readline(), "Allowed origins: https://1vcian.me, http://127.0.0.1:8080\n")
        self.assertEqual(p.stdout.readline(), "Press Ctrl+C to quit.\n")
        self.assertEqual(p.stdout.readline(), app.WAITING + "\n")
        # The live site's origin and the development one get a session; then
        # the status says the phone is online.
        for origin in ["https://1vcian.me", "http://127.0.0.1:8080"]:
            self.assertEqual(sorted(fetch(url + "/session", origin, b"{}")), ["expires", "laneConns", "lanes", "token"])
        self.assertEqual(p.stdout.readline(), app.ONLINE + "\n")
        p.terminate()
        code = p.wait(timeout=10)
        if sys.platform != "win32":
            self.assertEqual(code, 0, "a clean end on SIGTERM")
        self.assertEqual(p.stderr.read(), "")
        with self.assertRaises(OSError):
            fetch(url + "/health")

    def test_a_port_asked_for_and_taken_is_an_error(self):
        busy = socket.socket()
        self.addCleanup(busy.close)
        busy.bind(("127.0.0.1", 0))
        busy.listen()
        p = self.start("--port", str(busy.getsockname()[1]))
        self.assertEqual(p.wait(timeout=10), 1)
        self.assertIn("cannot listen on 127.0.0.1", p.stderr.read())

    def test_the_limits_come_from_the_environment_and_test_mode_is_announced(self):
        os.environ["LIMIT_LANES"] = "3"
        os.environ["LANE_CONNS"] = "2"
        self.addCleanup(os.environ.pop, "LIMIT_LANES")
        self.addCleanup(os.environ.pop, "LANE_CONNS")
        p = self.start("--port", "0", "--test-hosts", "a.example=93.184.215.14")
        url = p.stdout.readline().split(" at ")[1].split(" ")[0]
        s = fetch(url + "/session", "https://1vcian.me", b"{}")
        self.assertEqual((s["lanes"], s["laneConns"]), (3, 2))
        p.terminate()
        p.wait(timeout=10)
        self.assertIn("TEST MODE", p.stderr.read())

    def test_version(self):
        out = subprocess.run([sys.executable, CONNECT, "--version"], capture_output=True, text=True, timeout=20)
        self.assertEqual((out.returncode, out.stdout.strip()), (0, "vetro-connect " + app.__version__))


if __name__ == "__main__":
    unittest.main()
