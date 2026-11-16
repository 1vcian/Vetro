# ADR 0050 — The network gateway on the user's own computer, in Python

- Status: accepted (M7, 2026-11-15). Builds on ADR 0048 (network gateway:
  protocol, safeguards), whose protocol and page client it uses unchanged.
  Spec: `docs/specs/relay.md` (section "Python gateway").

## Context
ADR 0048 gave the guest real internet through a gateway that opens the TCP
connections a page cannot open, and planned to run it on Cloudflare Workers.
The owner decided against Cloudflare: the free plan's CPU limit does not fit
a data pump (ADR 0048 already said the paid plan was needed), the account's
quotas are shared with his other projects, and Workers cannot reach sites
that are themselves behind Cloudflare. A server of ours is a cost and a
liability (an open TCP proxy, however well fenced).

The gateway can instead run where the user is: on their own computer, next
to the browser. The page talks to it on the loopback address; the
connections leave from the user's own network with the user's own address;
we operate nothing. The owner asked for it in Python, as something that can
be downloaded and run on Windows and macOS.

## Decision

1. **Where and with what.** `relay/py/`: the package `vetro_connect` and the
   entry script `connect.py`. **Python standard library only** at run time
   (Python 3.10+, asyncio): no `websockets`, no `aiohttp`, no `certifi`. The
   server side of WebSocket (RFC 6455) is written here (`ws.py`: the
   handshake, client-masked frames, the 7, 16 and 64 bit lengths, text,
   binary, ping, pong and close, continuation frames, a 64 KiB bound on a
   message, the close handshake; no extensions), and so is the small HTTP/1.1
   side (one request per connection, a 16 KiB head, a 4 KiB body).
2. **The same protocol, a port of the same code.** `protocol`, `policy`,
   `tokens`, `ledger`, `lane` and `gateway` are ports of
   `relay/src/{protocol,policy,token,ledger,lane,gateway}.mjs`, and
   `server.py` is the counterpart of `relay/src/node.mjs`. The page's
   `GatewayClient` (`web/node/relay.mjs`) works with it unchanged. The frame
   vectors (`relay/test/vectors.txt`) are checked by a third implementation.
3. **Loopback only.** It listens on `127.0.0.1`, never on the network's
   addresses. Port 8787, or a free one if that is taken (a port asked for
   with `--port` and taken is an error). `TOKEN_SECRET` is random at every
   start and never stored. Allowed origins: `https://1vcian.me` (the live
   site; `https://1vcian.github.io` redirects there), plus `--origin`
   (repeatable) for development. Origin check and tokens as in ADR 0048.
4. **The same destination safeguards**, which matter more here than on a
   server: they are what keeps a page from reaching the user's router, the
   devices of the local network and the services of the computer itself
   through the gateway. Only TCP 80 and 443; every address checked after
   resolution against the same ranges as `relay/src/policy.mjs` (a unit test
   compares the two tables); a `CONNECT` by name is refused if any of the
   name's addresses is blocked, and the connection goes to the checked
   address literal. No socket exists before the check has passed.
5. **Session ledger: same semantics, same `LIMIT_*` names; no cap on
   bandwidth and total bytes by default.** A gateway that serves only its
   own user on their own connection has nothing to ration: `LIMIT_RATE`,
   `LIMIT_BURST` and `LIMIT_BYTES` are unlimited unless set. Connections
   (32), lanes (8) and the DNS rate (20/s, burst 100) keep the spec's
   defaults; a lane carries 32 connections as in Node (`LANE_CONNS`). Idle
   connections (10 min) and the lane life (1 h) are unchanged.
6. **DNS through the system resolver, not DNS over HTTPS.** A type A
   question is answered with the addresses `getaddrinfo` gives (IPv4 only,
   on a few daemon threads off the event loop, 5 s at most), TTL 60, as a
   well-formed answer with the query's id and question. Other question
   types get NOERROR with no records (the machine answers AAAA itself);
   failed lookups SERVFAIL; malformed queries FORMERR; queries far beyond
   the rate REFUSED, as in the JS. Reasons: no TLS trust store to bundle
   (the standard library has no portable one, and `certifi` is a
   dependency), and the user's own DNS (VPN, filters, hosts file) is
   respected. Details that follow from it:
   - "No such name" and "no address of this kind" are one error for
     `getaddrinfo`: both are answered NOERROR with no records, never
     NXDOMAIN (which would be wrong for a name that has only AAAA records).
   - Addresses the destination policy refuses are left out of the answers.
     They could not be connected to anyway, and the system resolver, unlike
     a public one, knows the names of the local network (`router.lan`,
     hosts file entries, mDNS): the guest's apps do not get to map it.
   - Names `getaddrinfo` would read as numbers (`127.1`, `0x7f.1`,
     `2130706433`) and names that are not host names never reach it.
7. **Nothing to choose at start.** Running it starts the gateway and opens
   `https://1vcian.me/Vetro/app/?gateway=http://127.0.0.1:PORT`, which
   starts the phone by itself with that gateway: in Chrome or Edge when
   found (the emulator is made for desktop Chromium), else in the default
   browser. `--no-browser` and `--url` are for tests and development. A
   small tkinter window (the console without tkinter or a display, or with
   `--no-ui`) shows the status, counters of connections, bytes, name
   lookups and refusals, and the buttons "Open Vetro" and "Quit". Until the
   page has asked for its first session the status reads "Waiting for the
   page: allow the connection in Chrome if it asks" (see below). Nothing
   about the traffic is shown, logged or kept except counters (ADR 0048,
   safeguard 5): no destinations, no names, no contents; even internal
   errors are only counted, because asyncio's own reports can name a peer.
8. **Packaging.** `.github/workflows/connect.yml` (`workflow_dispatch` and
   tags `connect-v*`) builds with a **pinned** PyInstaller
   (`relay/py/build-requirements.txt`, the whole tool chain pinned) on
   `windows-latest` (one `.exe`) and `macos-latest` (an `.app` in a `.zip`:
   PyInstaller deprecates a single-file `.app`, and a bare executable loses
   its executable bit in a browser download), runs the unit tests on both,
   checks that the built program starts and answers `/health`, uploads the
   two files as artifacts and, on a tag, attaches them to a release.
   PyInstaller is a build-time tool only; from source it is
   `python3 relay/py/connect.py`. The executables are **not signed**:
   Gatekeeper and SmartScreen warn, and the user guide says how to open
   them (`docs/user/internet.md`).
9. **Test-only options**, marked as such in `--help` and announced on
   stderr when used: `--test-connect-map` (destinations sent to local
   servers, behind the lane, so after the destination policy has passed on
   the address the guest asked for), `--test-hosts` (names from a table
   instead of the system resolver) and `--test-control` (counters and the
   kill switch over stdin/stdout). They are the child-process equivalent of
   the `connect` and `resolver` the JS tests inject.

## Chrome asks the user once (verified)
A public page that talks to the loopback address needs the user's
permission in Chrome. Verified on the live site on 2026-11-15 with Chrome
157, `https://1vcian.me/Vetro/app/?os=linux&gateway=http://127.0.0.1:PORT`
against the Node gateway allowing that origin, and on 2026-11-16 against
the Python gateway (`python3 relay/py/connect.py --no-browser --no-ui`,
the permission granted through the DevTools protocol: the guest resolved
`example.com` through the system resolver and fetched `http://example.com/`
from the internet; the program counted 1 connection, 87 bytes out, 932 in):
- Without the permission every request to the gateway fails with "Failed to
  fetch"; DevTools reports the CORS error `LocalNetworkAccessPermissionDenied`
  ("Permission was denied for this request to access the `loopback` address
  space"). There is no mixed-content problem: `http://127.0.0.1` and
  `ws://127.0.0.1` are accepted from the https page.
- The permission is `loopback-network` (the name in `navigator.permissions`;
  `loopbackNetwork` in the DevTools protocol's `Browser.grantPermissions`).
  `local-network` is a different one and is not needed. A normal Chrome
  window asks the user once for the site; headless Chrome cannot show the
  prompt, so it denies.
- With it granted: `fetch` of `/health` and `POST /session` answer 200 from
  the page and from a Worker, the WebSocket lane opens (hello received), and
  the BusyBox guest on the live site resolved `example.com` and fetched
  `http://example.com/` through the gateway on the Mac.
- Nothing is needed in the gateway's HTTP surface for it (no
  `Access-Control-Allow-Private-Network` header was required), so the
  surface stays exactly ADR 0048's.

The user guide tells the user to click Allow, and the status says so while
no session has arrived.

## Rejected alternatives
- **Cloudflare Workers** (ADR 0048's plan): out, for the reasons above.
- **A server of ours** (the Node gateway on a VM): works, and stays possible
  with the same protocol, but it is a cost and an open proxy to police.
- **Shipping the Node gateway to users**: needs Node and `ws` on the user's
  machine or a bundled runtime of tens of megabytes; the owner asked for
  Python.
- **`websockets` / `aiohttp`**: a dependency to pin, audit and bundle for a
  few hundred lines of protocol we can test against the browser's own
  client and the RFC's examples.
- **DNS over HTTPS as in Node**: needs a trust store, and ignores the user's
  resolver (decision 6).
- **A browser extension** (which could open sockets through native
  messaging): still needs a native program, plus a store review.
- **Listening on every interface** so that a phone or another computer
  could use the gateway: it would make the user's machine a proxy for the
  local network.

## Maintenance burden
3/5. A second implementation of the gateway to keep in step with
`relay/src/` (every protocol change now lands in Rust, JS and Python: the
shared vectors and the same client scenarios are the net), a hand-written
HTTP and WebSocket server, and two platforms packaged but not signed, with
a build that only runs on GitHub's Windows and macOS runners.

## Consequences
- The live site's `DEFAULT_GATEWAY` stays null: without the program the
  phone is offline (the sinkhole), with it the page is opened with
  `?gateway=…`. Making the page look for a local gateway by itself is a
  change to `web/` and a later decision.
- The sites see the user's own address, not a gateway's: the privacy page's
  sentence about the gateway's address is true only for a gateway we run.
- The secret changes at every start, so a page left open across a restart
  of the program keeps a token the new gateway refuses until the page
  renews it (up to 10 minutes) or is reloaded. The program opens the page
  again when it starts, which is the normal path.
- Firefox and Safari are not targets (the emulator is made for desktop
  Chromium); the permission above is Chrome's.
- The Node gateway and its tests stay as the reference implementation and
  for the Android runs in CI.

## Verification
- `python3 -m unittest discover -s relay/py -t relay/py` (standard library
  only): every vector of `relay/test/vectors.txt` decoded and encoded back;
  malformed frames; every blocked range (first and last address, neighbours
  allowed, the table compared with `policy.mjs`) and the ports; tokens;
  the ledger with the server's and the local defaults; the WebSocket codec
  (the RFC's examples, masking, the three length forms, fragments, control
  frames, sizes, close); DNS answers and the resolver; the HTTP side; lanes
  against fake sockets (the cases of `relay/test/unit.mjs`); the server over
  real loopback sockets (upgrade refusals, echo beyond the window, half
  close, a TCP reset, WebSocket violations, the port fallback, loopback
  only); the launcher (browser choice, options, status texts, the program
  as a child process).
- `node relay/test/python.mjs`: the scenarios of `relay/test/node.mjs` with
  the page's `GatewayClient` against `python3 relay/py/connect.py` as a
  child process (echo beyond the window with half close, policy refusals
  before any socket, names, DNS, lanes on demand and the lane limit, the
  kill switch, a foreign origin), and tokens byte for byte equal to
  `relay/src/token.mjs`'s, each verified by the other.
- `node relay/test/python-online.mjs`: a BusyBox guest under vetro-wasm in
  Node online through the Python gateway (the run of `tests/web/online.mjs`).
- `node relay/test/python-browser.mjs`: the app in headless Chrome online
  through the Python gateway (the first part of
  `tests/web/browser-online.mjs`): the browser's own `fetch`, with its CORS
  preflight, and `WebSocket` against the hand-written server.
- All four are in `tools/web-test.sh`.
- A mutation check while writing it (39 single changes that each break one
  safeguard or limit, in a copy): every one is caught by the unit tests, the
  end-to-end test, or both.
- Measured on the Mac (Apple silicon, loopback, 64 MiB each way with the
  page's client in Node acknowledging at once): 675–761 MiB/s down and
  275–283 MiB/s up, against 413–418 and 235–237 MiB/s for the Node gateway
  in the same harness. The gateway is not the slow part.
- By hand on the Mac: the status window (waiting, then online after the
  first session; Open Vetro; Quit; SIGTERM), and a trial build with the
  pinned PyInstaller (`Vetro Connect.app`, 29 MB, 11 MB zipped): it starts
  and answers `/health` and `/session`.
- Not verified: the workflow itself and the Windows executable (the
  workflow has not run), the program on Windows at all, Python 3.10 (3.11
  to 3.15 were run), the Gatekeeper and SmartScreen steps of the user guide,
  and Android through the Python gateway (Android online is verified with
  the Node gateway on a hosted runner; the Linux guest on the live site with
  the Python one).
