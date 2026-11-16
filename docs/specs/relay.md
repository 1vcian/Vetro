# Spec — network gateway protocol (`relay/`, ADR 0048)

## Scope
The guest's TCP connections and DNS queries leave the browser through a
gateway, because a page cannot open raw sockets: one we operate (ADR 0048),
or the Python one on the user's own computer (ADR 0050). The guest's
TCP/IP is still terminated in `vetro-net` (`docs/specs/net.md`): the gateway
carries byte streams per connection, never packets. Decisions and
safeguards in ADR 0048.

| Piece | Where |
|---|---|
| Frames (Rust) | `vetro_net::relay` (`Frame`, `Side`, `Refusal`, constants) |
| Machine state | `vetro_net::Uplink` (the stack's upstream: sinkhole offline, gateway online) |
| Machine API | `Machine::relay_take` (output), `Machine::input(Input::Relay(frame))` (input) |
| WASM | `vetro_relay_take`, `vetro_relay_push`, `vetro_relay_state` (ABI 17, `docs/specs/wasm.md`) |
| Page client | `web/node/relay.mjs` (`GatewayClient`, `DEFAULT_GATEWAY`, `online`, `down`) |
| Gateway core | `relay/src/` (`protocol`, `policy`, `token`, `ledger`, `dns`, `lane`, `gateway`) |
| Cloudflare | `relay/src/worker.mjs`, `relay/wrangler.toml`, `relay/deploy.sh`, `relay/kill.sh` |
| Node | `relay/src/node.mjs` (`startGateway`, `nodeSocket`) |
| Python (the user's own computer, ADR 0050) | `relay/py/` (`connect.py`, package `vetro_connect`), section "Python gateway" below |

## Frames
One frame per WebSocket binary message: `op` (u8), `id` (u32 big endian),
payload. Lengths are the message's; nothing else is framed. All integers big
endian. Text messages carry JSON control (below).

| op | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | `CONNECT` | machine → gateway | IPv4 (4 bytes), port (u16) |
| `0x02` | `CONNECT_NAME` | machine → gateway | port (u16), host name (1–253 bytes: `[a-z0-9-]` labels of 1–63, dots) |
| `0x03` / `0x83` | `DATA` | both (machine / gateway) | 1–16384 bytes |
| `0x04` / `0x84` | `SHUTDOWN` | both | none: the sender closed its direction (FIN), after its data |
| `0x05` / `0x85` | `CLOSE` | both | u8 reset (0/1): the connection is over for the sender |
| `0x06` / `0x86` | `ACK` | both | u32: bytes of the other side's `DATA` consumed |
| `0x07` | `DNS` | machine → gateway | a raw DNS query (12–4096 bytes) |
| `0x87` | `DNS` answer | gateway → machine | the raw answer, with the query's id |
| `0x81` | `CONNECTED` | gateway → machine | none |
| `0x82` | `REFUSED` | gateway → machine | u8 reason: 1 unreachable, 2 port, 3 address, 4 limit, 5 platform, 6 resolve, 7 down |
| `0xf0` | `ONLINE` | host → machine (never on a lane) | u8 on; id 0 |
| `0xf1` | `DOWN` | host → machine (never on a lane) | none; id 0 |

A frame that breaks these rules is malformed: the machine rejects it
(`Reply::Rejected`), the gateway ends the lane (`bye` `protocol`). The
vectors in `relay/test/vectors.txt` are produced by the Rust encoder and
checked by both implementations.

### Connections
- Ids are the machine's connection ids (`ConnId`, unique for the machine's
  life; a DNS frame carries its UDP flow's id). A lane never reuses an id
  while it is open.
- `CONNECT`/`CONNECT_NAME` is answered by exactly one `CONNECTED` or
  `REFUSED`. `CONNECT_NAME` is sent only for the sinkhole's fake addresses
  (198.18.0.0/15) left in the guest's DNS cache by an offline snapshot.
- After `CONNECTED`, `DATA` and `SHUTDOWN` flow both ways. The machine sends
  `CLOSE` once (when the stack's connection ends: `reset` = aborted); the
  gateway forgets the id then. The gateway sends `CLOSE` (reset) only on an
  error, an idle timeout (10 min) or a dropped socket; it frees a
  connection whose both directions have finished without a frame (the
  machine's `CLOSE` follows). Frames for an unknown id are ignored (races
  are legitimate).
- A `REFUSED` connection is gone on both sides: no `CLOSE`.

### Flow control
Per connection and direction, a window of `WINDOW` = 256 KiB: the sender's
`DATA` bytes not yet acknowledged with `ACK` never exceed it. The gateway
acknowledges the machine's bytes when they are written to the socket; the
machine acknowledges the gateway's when the stack has taken them (at least
every 32 KiB, and when its queue empties). The gateway reads its socket only
within the window, so a slow guest slows the remote server (TCP back
pressure). A machine that receives `DATA` beyond the window resets the
connection; a gateway that receives `DATA` beyond the window, or an `ACK`
for more than it sent, ends the lane.

### DNS
The gateway checks the query (one question, standard query, ≤ 1024 bytes),
resolves it with DNS over HTTPS (RFC 8484, `DOH_URL`, default Cloudflare's
resolver), puts back the query's id, and truncates (TC, question only) an
answer over 1472 bytes. Failures answer SERVFAIL, malformed queries
FORMERR, queries beyond the rate REFUSED. The machine answers AAAA queries
itself with no records (the guest has no IPv6) and drops UDP that is not
DNS. (The Python gateway checks the query the same way and answers from the
system resolver instead of DNS over HTTPS: section "Python gateway".)

## Machine side (`Uplink`)
- Offline (the default, and every snapshot made before ADR 0048): the
  sinkhole. `ONLINE(true)`: new TCP connections and UDP flows go to the
  gateway; those opened before stay where they are.
- `DOWN`: connections waiting for an answer are refused (reason `down`),
  open ones reset at the next read. The page sends `DOWN` then `ONLINE` at
  start, after a snapshot restore and after a replay.
- State (in snapshots, `UPST` section of the stack, after the sinkhole: an
  optional `GWAY` section): mode, connections (status, credit, received
  bytes not yet taken, unacknowledged count, end, reset), UDP flow owners,
  pending DNS answers, counters. The outbox (frames not yet taken) is an
  output and is not saved.
- Determinism: the same inputs at the same instruction numbers give the same
  frames out (checked by `tests/boot/tests/relay.rs`: the replay without a
  gateway sends the same frames).

## Gateway HTTP
| Request | Answer |
|---|---|
| `POST /session`, `Origin` allowed, body `{}` or `{"renew": token}` | `200 {token, expires (ms), lanes, laneConns}`; `403` other origin; `503` disabled; `400` bad body. A renewal keeps the session id while the session is younger than 2 h |
| `OPTIONS /session` | CORS preflight for allowed origins |
| `GET /lane?token=…&lane=k` (WebSocket upgrade) | `101`; `401` bad/expired token; `403` origin not the token's; `400` lane out of range; `409` lane in use; `429` lane limit; `503` disabled |
| `GET /health` | `{ok, enabled}` |

Token: `base64url(JSON {v:1, sid, origin, iat, exp, first}) "." base64url(HMAC-SHA-256(TOKEN_SECRET, first part))`,
valid 10 minutes.

Control messages (text, JSON): gateway → page `{"type":"hello","v":1,"lane":k,"window":262144,"maxData":16384,"laneConns":n,"limits":{…}}`
first; `{"type":"bye","reason":…}` before closing (`expired`, `limit-bytes`,
`disabled`, `protocol`, `dns`); `{"type":"pong"}`. Page → gateway
`{"type":"ping"}` every 25 s. The page refuses a gateway whose `v` or
`window` differ.

## Lanes (page side)
A guest connection lives on the first lane with room (`laneConns`: 5 on
Cloudflare, where an invocation holds at most six connections; 32 in Node
and in Python),
else on a new lane up to the session's `lanes` (8). DNS goes on any open
lane. Lane `k` > 0 closes after 2 min without connections. When a lane
closes, the page resets its connections for the guest itself (`REFUSED`
`down` while connecting, `CLOSE` reset once open). If the gateway cannot be
reached, `CONNECT`s are refused at once (`down`) and DNS queries dropped,
and a new attempt waits 1 s, doubling to 60 s.

## Safeguards (gateway)
Ports 80 and 443 only; destinations checked after resolution against
`relay/src/policy.mjs` (`BLOCKED`: this network, private, shared, loopback,
link-local, IETF, documentation, 6to4 relay, benchmarking, multicast,
reserved); per-session ledger (`relay/src/ledger.mjs`, defaults:
32 connections, 8 lanes, 2 MiB/s with a 4 MiB burst, 1 GiB, 20 DNS/s with a
burst of 100; `LIMIT_*` variables) consulted before every byte (grants of
64 KiB); idle connections 10 min; lane life 1 h; only counters logged
(`lane-end`: duration, connections, refusals by reason, bytes each way, DNS
queries); kill switch `GATEWAY_ENABLED=0` (`relay/kill.sh`).

## Configuration
| Variable | Meaning |
|---|---|
| `TOKEN_SECRET` | secret (≥ 32 characters), Cloudflare secret / environment |
| `ALLOWED_ORIGINS` | comma separated origins of the Vetro site |
| `GATEWAY_ENABLED` | `0` = everything refused |
| `LANE_CONNS` | connections per lane (Node; the Worker uses 5) |
| `LIMIT_CONNS`, `LIMIT_LANES`, `LIMIT_RATE`, `LIMIT_BURST`, `LIMIT_BYTES`, `LIMIT_DNS_RATE`, `LIMIT_DNS_BURST` | ledger limits |
| `DOH_URL` | the DNS-over-HTTPS resolver |

Web app: `DEFAULT_GATEWAY` in `web/node/relay.mjs` (null until deployed),
`?gateway=URL`, `?net=offline`.

## Python gateway (`relay/py/`, ADR 0050)
The same protocol, HTTP surface, tokens, control messages, lanes and
safeguards as above, as a program on the user's own computer: the page's
`GatewayClient` works with it unchanged. Python 3.10+, standard library
only (asyncio; the server side of RFC 6455 is in `vetro_connect/ws.py`).

| Module (`relay/py/vetro_connect/`) | Port of |
|---|---|
| `protocol.py`, `policy.py`, `tokens.py`, `ledger.py`, `lane.py`, `gateway.py` | `relay/src/{protocol,policy,token,ledger,lane,gateway}.mjs` |
| `dns.py` | `relay/src/dns.mjs` for the checks; answers from `getaddrinfo` |
| `ws.py` | (the `ws` package) WebSocket frames, 64 KiB per message, no extensions |
| `server.py` | `relay/src/node.mjs`: HTTP, upgrade, lanes, TCP sockets, ledgers in memory |
| `app.py`, `../connect.py` | the launcher: options, browser, status window or console |

`python3 relay/py/connect.py [--port N] [--origin URL]… [--url PAGE] [--no-browser] [--no-ui] [--verbose]`
starts the gateway and opens `https://1vcian.me/Vetro/app/?gateway=http://127.0.0.1:PORT`
(Chrome or Edge when found, else the default browser).

What differs from the Node gateway:
- **Listening**: `127.0.0.1` only; port 8787, or a free one if it is taken
  (`--port N` taken is an error; `--port 0` any free port).
- **Configuration**: `TOKEN_SECRET` is random at every start (no variable);
  origins are `https://1vcian.me` plus `--origin` (no `ALLOWED_ORIGINS`);
  no `GATEWAY_ENABLED` (quitting is the kill switch; the ledger's switch
  remains and is driven by the tests) and no `DOH_URL`. `LANE_CONNS` and the
  `LIMIT_*` variables are read as in Node.
- **Default limits**: `LIMIT_RATE`, `LIMIT_BURST` and `LIMIT_BYTES` have no
  cap unless set (a rate without a burst gets a burst of twice the rate);
  the others are the defaults above. `hello`'s `limits` carries `null` for
  a limit without a cap.
- **DNS**: a type A, class IN question is answered with the IPv4 addresses
  `getaddrinfo` gives for the name (at most 8, TTL 60, the query's id and
  question, RA set, RD copied), without the addresses the destination
  policy blocks. Other question types and classes, a name with no address
  (or none left), and names that are not host names or that `getaddrinfo`
  would read as numbers: NOERROR with no records. A failed or late (5 s)
  lookup: SERVFAIL. Malformed: FORMERR; far beyond the rate: REFUSED.
  `CONNECT_NAME` uses the same resolver, and every address of the name must
  pass the policy (none is left out first).
- **Refusals**: `platform` (reason 5) is never sent; a failed or late (15 s)
  connection is `unreachable`.
- **HTTP**: every response closes its connection (`connection: close`); a
  request with `Transfer-Encoding` is answered 411, a head over 16 KiB 431,
  a body over 4096 bytes 413 (as Node), a request that does not arrive
  within 10 s gets no answer. The `lane` parameter must be decimal digits
  (or absent, which is lane 0 as in Node). The WebSocket handshake
  (`Sec-WebSocket-Version: 13`, a 16-byte key) is checked before the lane is
  taken in the ledger; a bad one is answered 400.
- **Control**: one more `bye` reason, `error` (an internal error ended the
  lane; it is counted, never printed).
- **Log**: nothing by default; `--verbose` prints each lane's `lane-end`
  counters. The status shows counters only: sessions given, connections
  open and so far, bytes each way, name lookups, refusals.
- **Test-only options** (marked `TEST ONLY` in `--help`, announced on
  stderr): `--test-connect-map IP:PORT=IP:PORT,…` (destinations sent to
  local servers by the socket factory, so after the destination policy),
  `--test-hosts NAME=IP[+IP…],…` (`*` = any other name; replaces the system
  resolver), `--test-control` (JSON lines on stdout: `ready`; commands on
  stdin: `counters`, `disable`, `enable`, `quit`; the program quits when
  stdin ends).

Chrome asks the user once before a public page may talk to the loopback
address (the `loopback-network` permission, ADR 0050); nothing in the HTTP
surface is needed for it.

Packaging: `.github/workflows/connect.yml` (pinned PyInstaller,
`relay/py/build-requirements.txt`): `vetro-connect-windows-x64.exe` and
`vetro-connect-macos-arm64.zip`, not signed.

## Tests
- `node relay/test/unit.mjs`: vectors, malformed frames, every blocked
  range (first and last address, neighbours allowed), ports, tokens, ledger,
  HTTP and lane checks, lanes against fake sockets (refusals, connect by name
  with every address checked, windows both ways, half close, the client's
  window, idle, lane life, kill switch, total bytes, counters-only log),
  DNS.
- `node relay/test/node.mjs`: the Node gateway over real WebSockets and TCP
  with the page's `GatewayClient` (700 KB echo with half close, refusals
  before any socket, DNS, lanes on demand and the session's lane limit,
  kill switch, a foreign origin).
- `python3 -m unittest discover -s relay/py -t relay/py`: the Python
  gateway's unit tests (standard library only): the vectors decoded and
  encoded back, malformed frames, every blocked range and the table against
  `policy.mjs`, ports, tokens, the ledger, the WebSocket codec, DNS answers
  and the resolver, the HTTP side, lanes against fake sockets, the server
  over loopback sockets, the launcher.
- `node relay/test/python.mjs`: the scenarios of `relay/test/node.mjs`
  with the page's `GatewayClient` against `python3 relay/py/connect.py` as
  a child process, and tokens equal byte for byte to `token.mjs`'s.
- `node relay/test/python-online.mjs`: a BusyBox guest under vetro-wasm in
  Node online through the Python gateway (as `tests/web/online.mjs`).
- `node relay/test/python-browser.mjs`: the app in headless Chrome online
  through the Python gateway (the first part of
  `tests/web/browser-online.mjs`).
- `node relay/test/workerd.mjs` (internet): the Cloudflare Worker under
  `wrangler dev` (workerd): session, DNS, refusals, a real page by name.
- `cargo test -p vetro-net --test relay`: the `Uplink` and the vectors.
- `cargo test --release -p vetro-boot-tests --test relay`: BusyBox online
  through the protocol, recorded and replayed identically.
- `VETRO_ANDROID=1 node tests/web/android-online.mjs`: Android in Chrome
  with the local Node gateway (a real HTTPS page in the browser app, a
  catalog app's request).
