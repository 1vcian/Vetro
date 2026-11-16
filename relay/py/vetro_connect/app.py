"""Vetro Connect (ADR 0050): starts the network gateway on this computer and
opens the phone in the browser, with nothing to choose.

    python3 relay/py/connect.py [--port N] [--origin URL]... [--no-browser] [--url PAGE] [--no-ui]

The gateway listens on 127.0.0.1 only and serves the Vetro site's origin
(plus `--origin`, for development). A small window shows that it is running
and counters of connections and bytes; without tkinter the same goes to the
console. Nothing about the traffic is shown or kept except counters.

Variables, as the Node gateway's: LANE_CONNS, LIMIT_CONNS, LIMIT_LANES,
LIMIT_RATE, LIMIT_BURST, LIMIT_BYTES, LIMIT_DNS_RATE, LIMIT_DNS_BURST.
"""

from __future__ import annotations

import argparse
import asyncio
import concurrent.futures
import json
import ntpath
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import webbrowser
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, TextIO, Tuple

from . import __version__
from .gateway import DEFAULT_ORIGIN
from .ledger import limits_from
from .policy import parse_ipv4
from .server import DEFAULT_PORT, Gateway, mapped_connect, table_resolver
from .tokens import new_secret

#: The page that starts the phone by itself with the gateway it is given.
DEFAULT_PAGE = "https://1vcian.me/Vetro/app/"


def page_url(page: str, port: int) -> str:
    """The page's address with this gateway as its `gateway` parameter."""
    return "%s%sgateway=http://127.0.0.1:%d" % (page, "&" if "?" in page else "?", port)


def browser_command(
    url: str,
    *,
    platform: str = sys.platform,
    env: Optional[Mapping[str, str]] = None,
    exists: Callable[[str], bool] = os.path.exists,
    which: Callable[[str], Optional[str]] = shutil.which,
) -> Optional[List[str]]:
    """The command that opens `url` in Chrome or Edge (the emulator is made
    for desktop Chromium), or None if neither is found."""
    env = os.environ if env is None else env
    if platform == "darwin":
        for app in ("Google Chrome", "Microsoft Edge", "Chromium"):
            for base in ("/Applications", env.get("HOME", "") + "/Applications"):
                path = "%s/%s.app" % (base, app)
                if exists(path):
                    return ["open", "-a", path, url]
        return None
    if platform == "win32":
        roots = [env.get(k) for k in ("PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA")]
        for exe in ("Google\\Chrome\\Application\\chrome.exe", "Microsoft\\Edge\\Application\\msedge.exe"):
            for root in roots:
                if root and exists(ntpath.join(root, exe)):
                    return [ntpath.join(root, exe), url]
        return None
    for name in ("google-chrome", "google-chrome-stable", "microsoft-edge", "microsoft-edge-stable", "chromium", "chromium-browser"):
        path = which(name)
        if path:
            return [path, url]
    return None


def open_browser(url: str) -> bool:
    """Opens `url` in Chrome or Edge when found, else in the default browser."""
    command = browser_command(url)
    if command:
        try:
            subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return True
        except OSError:
            pass
    try:
        return bool(webbrowser.open(url))
    except Exception:  # noqa: BLE001 - no browser at all: the address is on screen
        return False


def format_bytes(n: int) -> str:
    if n < 1000:
        return "%d bytes" % n
    value = float(n)
    for unit in ("kB", "MB"):
        value /= 1000
        if value < 999.95:  # what "%.1f" still shows below 1000
            return "%.1f %s" % (value, unit)
    return "%.1f GB" % (value / 1000)


#: Until the page has asked for its first session. Chrome asks the user once
#: whether the site may connect to apps on this device (ADR 0050).
WAITING = "Waiting for the page: allow the connection in Chrome if it asks."
ONLINE = "The phone's internet is on. Keep this window open while you use Vetro."


def headline(s: Mapping[str, int]) -> str:
    """The status in one line: whether the page has reached the gateway."""
    return ONLINE if s["sessions"] else WAITING


def status_lines(s: Mapping[str, int]) -> List[str]:
    """The counters as text (numbers only: no sites, no contents)."""
    return [
        "Connections: %d open, %d so far" % (s["conns"], s["conns_total"]),
        "Data: %s sent, %s received" % (format_bytes(s["bytes_to_remote"]), format_bytes(s["bytes_to_guest"])),
        "Name lookups: %d   Refused: %d" % (s["dns"], s["refused"]),
    ]


def parse_connect_map(text: str) -> Dict[Tuple[str, int], Tuple[str, int]]:
    """`ip:port=ip:port,…` (TEST ONLY, see --test-connect-map)."""
    out = {}
    for item in filter(None, text.split(",")):
        src, _, dst = item.partition("=")

        def endpoint(s: str) -> Tuple[str, int]:
            ip, _, port = s.partition(":")
            if parse_ipv4(ip) is None or not port.isdigit():
                raise ValueError("not ip:port: %r" % s)
            return ip, int(port)

        out[endpoint(src)] = endpoint(dst)
    return out


def parse_hosts(text: str) -> Dict[str, List[str]]:
    """`name=ip[+ip…],…`, `*` for any other name (TEST ONLY, see --test-hosts)."""
    out = {}
    for item in filter(None, text.split(",")):
        name, _, addrs = item.partition("=")
        out[name] = [a for a in addrs.split("+") if a]
        if not name or any(parse_ipv4(a) is None for a in out[name]):
            raise ValueError("not name=ip: %r" % item)
    return out


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(prog="vetro-connect", description="Internet for the Vetro phone, through this computer.")
    p.add_argument("--port", type=int, default=None, help="port on 127.0.0.1 (default %d, or a free one if it is taken; 0 = any free port)" % DEFAULT_PORT)
    p.add_argument("--origin", action="append", default=[], metavar="URL", help="another origin allowed to use the gateway, for development (repeatable); %s always is" % DEFAULT_ORIGIN)
    p.add_argument("--url", default=DEFAULT_PAGE, metavar="PAGE", help="the page to open (default %(default)s)")
    p.add_argument("--no-browser", action="store_true", help="do not open the browser")
    p.add_argument("--no-ui", action="store_true", help="no window: status on the console")
    p.add_argument("--verbose", action="store_true", help="print each lane's counters when it ends")
    p.add_argument("--version", action="version", version="vetro-connect " + __version__)
    t = p.add_argument_group("test only (never needed to use Vetro)")
    t.add_argument("--test-connect-map", type=parse_connect_map, default={}, metavar="IP:PORT=IP:PORT,...", help="TEST ONLY: connections to these destinations go to these local servers instead (after the destination policy has passed on the address asked for)")
    t.add_argument("--test-hosts", type=parse_hosts, default=None, metavar="NAME=IP,...", help="TEST ONLY: resolve names from this table (* = any other name) instead of the system resolver")
    t.add_argument("--test-control", action="store_true", help="TEST ONLY: JSON lines on stdout, and commands on stdin (counters, disable, enable, quit); quits when stdin ends")
    # (Older macOS versions hand an app started from the Finder a "-psn_…"
    # argument: not ours, and no reason to refuse to start.)
    argv = sys.argv[1:] if argv is None else argv
    return p.parse_args([a for a in argv if not a.startswith("-psn_")])


class Runner:
    """The gateway and its event loop on their own thread: the window, when
    there is one, needs the main thread."""

    def __init__(self, make: Callable[[], Gateway], port: int, fallback: bool, verbose: bool = False):
        self._make = make
        self._port = port
        self._fallback = fallback
        self._verbose = verbose
        self._ready = threading.Event()
        self._error: Optional[BaseException] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._stop: Optional[asyncio.Event] = None
        self._thread = threading.Thread(target=self._run, name="vetro-gateway", daemon=True)
        self.gateway: Optional[Gateway] = None

    def start(self) -> Gateway:
        """Starts listening; raises OSError if the port cannot be had."""
        self._thread.start()
        self._ready.wait()
        if self._error is not None:
            raise self._error
        assert self.gateway is not None
        return self.gateway

    def _run(self) -> None:
        try:
            asyncio.run(self._main())
        except BaseException as e:  # noqa: BLE001 - reported by start()
            if not self._ready.is_set():
                self._error = e
                self._ready.set()

    async def _main(self) -> None:
        self._loop = asyncio.get_running_loop()
        self._loop.set_exception_handler(self._on_error)
        self._stop = asyncio.Event()
        gateway = self._make()
        await gateway.start(self._port, fallback=self._fallback)
        self.gateway = gateway
        self._ready.set()
        await self._stop.wait()
        await gateway.close()

    def _on_error(self, loop: asyncio.AbstractEventLoop, context: Dict[str, Any]) -> None:
        # asyncio's own report could name a peer or a destination: only the
        # kind of error is ever shown.
        if self.gateway is not None:
            self.gateway.stats.errors += 1
        if self._verbose:
            print("vetro-connect: internal error (%s)" % type(context.get("exception")).__name__, file=sys.stderr)

    def call(self, fn: Callable[[], Any]) -> Any:
        """Runs `fn` on the gateway's thread and returns its result."""
        assert self._loop is not None
        result: "concurrent.futures.Future[Any]" = concurrent.futures.Future()

        def run() -> None:
            try:
                result.set_result(fn())
            except BaseException as e:  # noqa: BLE001 - handed to the caller
                result.set_exception(e)

        self._loop.call_soon_threadsafe(run)
        return result.result(timeout=10)

    def stop(self) -> None:
        if self._loop is not None and self._stop is not None:
            try:
                self._loop.call_soon_threadsafe(self._stop.set)
            except RuntimeError:
                pass  # the loop is already closed
        self._thread.join(timeout=5)


def run_window(runner: Runner, url: str, note: str = "", signals: Optional["_Signals"] = None) -> bool:
    """The status window, until Quit (or until a signal asks: `signals`).
    False if there is no usable tkinter (no module, no display): the caller
    uses the console."""
    try:
        import tkinter as tk
        from tkinter import ttk

        root = tk.Tk()
    except Exception:  # noqa: BLE001 - ImportError, TclError, a broken install
        return False
    if signals is not None:
        # Tk on macOS takes SIGTERM and SIGINT for itself when it starts (and
        # exits at once with status 1): they are ours again, for a clean end.
        signals.install()
    gateway = runner.gateway
    assert gateway is not None
    root.title("Vetro Connect")
    root.resizable(False, False)
    box = ttk.Frame(root, padding=16)
    box.grid()
    ttk.Label(box, text="Vetro Connect", font=("", 15, "bold")).grid(sticky="w")
    # As wide as the longer of its two texts, so the window does not jump.
    status = ttk.Label(box, text="", width=max(len(WAITING), len(ONLINE)))
    status.grid(sticky="w", pady=(4, 0))
    ttk.Label(box, text="Gateway: %s (this computer only)%s" % (gateway.url, note)).grid(sticky="w", pady=(2, 10))
    counters = [ttk.Label(box, text="") for _ in range(3)]
    for label in counters:
        label.grid(sticky="w")
    buttons = ttk.Frame(box)
    buttons.grid(sticky="e", pady=(14, 0))
    ttk.Button(buttons, text="Open Vetro", command=lambda: open_browser(url)).grid(row=0, column=0, padx=(0, 8))
    ttk.Button(buttons, text="Quit", command=root.destroy).grid(row=0, column=1)

    def refresh() -> None:
        if signals is not None and signals.asked.is_set():
            return root.destroy()
        snapshot = gateway.stats.snapshot()
        status.configure(text=headline(snapshot))
        for label, text in zip(counters, status_lines(snapshot)):
            label.configure(text=text)
        root.after(500, refresh)

    refresh()
    root.mainloop()
    return True


def _control(runner: Runner, out: TextIO) -> None:
    """TEST ONLY (--test-control): commands on stdin, JSON answers on stdout."""
    gateway = runner.gateway
    assert gateway is not None

    def say(record: Dict[str, Any]) -> None:
        print(json.dumps(record, separators=(",", ":")), file=out, flush=True)

    def enabled(on: bool) -> bool:
        gateway.cfg.enabled = on
        return on

    for line in sys.stdin:
        command = line.strip()
        if command == "counters":
            say({"test": "counters", "stats": runner.call(gateway.stats.snapshot), "sessions": runner.call(lambda: [ledger.counters() for ledger in gateway.sessions.values()])})
        elif command in ("disable", "enable"):
            say({"test": "enabled", "enabled": runner.call(lambda: enabled(command == "enable"))})
        elif command == "quit":
            return


def run_console(runner: Runner, test_control: bool, out: TextIO) -> None:
    """The status on the console, until Ctrl+C."""
    gateway = runner.gateway
    assert gateway is not None
    try:
        if test_control:
            return _control(runner, out)
        print("Press Ctrl+C to quit.", file=out)
        last = gateway.stats.snapshot()
        said = headline(last)
        print(said, file=out, flush=True)
        printed = time.monotonic()
        while True:
            time.sleep(1)
            now = gateway.stats.snapshot()
            if headline(now) != said:
                said = headline(now)
                print(said, file=out, flush=True)
            # The counters, when they change, at most every five seconds.
            if now != last and time.monotonic() - printed >= 5:
                print(" | ".join(status_lines(now)), file=out, flush=True)
                last, printed = now, time.monotonic()
    except KeyboardInterrupt:
        pass


class _Signals:
    """SIGTERM and Ctrl+C: on the console they interrupt the wait; with the
    window open they only set `asked` (tkinter would swallow the exception),
    and the window closes at its next refresh."""

    def __init__(self) -> None:
        self.asked = threading.Event()
        self.interrupt = True

    def __call__(self, signum: int, frame: Any) -> None:
        self.asked.set()
        if self.interrupt:
            raise KeyboardInterrupt

    def install(self) -> None:
        for name in ("SIGTERM", "SIGINT"):
            try:
                signal.signal(getattr(signal, name), self)
            except (ValueError, OSError, AttributeError):
                pass  # not the main thread, or no such signal here


def main(argv: Optional[Sequence[str]] = None) -> int:
    # An executable without a console (the packaged app) has no streams.
    for name in ("stdout", "stderr"):
        if getattr(sys, name) is None:
            setattr(sys, name, open(os.devnull, "w"))
    args = parse_args(argv)
    out = sys.stdout
    origins = [DEFAULT_ORIGIN]
    for origin in args.origin:
        origin = origin.rstrip("/")
        if origin and origin not in origins:
            origins.append(origin)
    try:
        lane_conns = int(os.environ.get("LANE_CONNS", ""))
    except ValueError:
        lane_conns = 0
    testing = bool(args.test_connect_map) or args.test_hosts is not None

    def log(record: Dict[str, Any]) -> None:
        if args.verbose:
            print(json.dumps(record, separators=(",", ":")), file=out, flush=True)

    def make() -> Gateway:
        return Gateway(
            secret=new_secret(),
            origins=origins,
            limits=limits_from(os.environ),
            lane_conns=lane_conns if lane_conns > 0 else 32,
            connect=mapped_connect(args.test_connect_map) if args.test_connect_map else None,
            resolver=table_resolver(args.test_hosts) if args.test_hosts is not None else None,
            log=log,
        )

    runner = Runner(make, DEFAULT_PORT if args.port is None else args.port, fallback=args.port is None, verbose=args.verbose)
    try:
        gateway = runner.start()
    except OSError as e:
        print("vetro-connect: cannot listen on 127.0.0.1: %s" % e, file=sys.stderr)
        return 1
    url = page_url(args.url, gateway.port)
    if args.test_control:
        print(json.dumps({"test": "ready", "url": gateway.url, "port": gateway.port, "page": url}, separators=(",", ":")), file=out, flush=True)
    else:
        print("Vetro Connect %s: the gateway is running at %s (this computer only)." % (__version__, gateway.url), file=out)
        print("The phone: %s" % url, file=out)
        print("Allowed origins: %s" % ", ".join(origins), file=out, flush=True)
    if testing:
        print("TEST MODE: destinations or names are replaced by --test-* options.", file=sys.stderr, flush=True)
    signals = _Signals()
    signals.install()
    try:
        if not args.no_browser:
            open_browser(url)
        shown = False
        if not (args.no_ui or args.test_control):
            signals.interrupt = False
            shown = run_window(runner, url, "   TEST MODE" if testing else "", signals)
            signals.interrupt = True
        if not shown and not signals.asked.is_set():
            run_console(runner, args.test_control, out)
    except KeyboardInterrupt:
        pass
    runner.stop()
    return 0
