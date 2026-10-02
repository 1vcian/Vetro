# ADR 0046 — Network gateway: real internet for the guest, with safeguards

- Status: accepted (M7, 2026-10-02). Builds on ADR 0007 (network stack),
  ADR 0016 (network analysis), ADR 0019 (record & replay: every host input
  goes through `Machine::input`) and ADR 0029 (TLS hooks).
- Number: renumber on rebase if 0046 is already taken on main.

## Context
Until now the guest's traffic ended in the sinkhole (`vetro-net`): DNS
answers with fake addresses, TCP connections terminated on the host side
with canned replies. Apps that call the network do not work, and the
browser app cannot load a page. The owner's decision: the emulated phone
must have real internet access.

A browser page cannot open raw TCP connections (only `fetch` and
WebSocket), so the guest's connections need a small server we operate that
opens them for it: a network gateway. Operating an open TCP proxy is a
liability (abuse, scanning, access to internal networks, cost), so the
gateway is designed around mandatory safeguards.

## Decision

### Shape
```
guest TCP/IP ─ virtio-net ─ vetro-net Stack ─ Uplink (in the machine)
                                                 │ frames (outbox / Machine::input)
                                    page Worker: GatewayClient (web/node/relay.mjs)
                                                 │ WebSocket "lanes" (wss)
                                    gateway (relay/): Cloudflare Worker, or the
                                    Node implementation of the same protocol
                                                 │ outbound TCP (platform sockets API)
                                              the internet (ports 80, 443)
```
- The guest's TCP/IP is still terminated in `vetro-net` (ADR 0007): the
  gateway never sees packets, only byte streams per connection. The
  network inspector (frame capture at the virtio-net boundary, ADR 0016)
  keeps working unchanged, and the TLS hooks (ADR 0029) give plaintext.
- **TLS stays end to end.** The guest's TLS runs from the app to the real
  server; the gateway forwards the encrypted bytes (it never terminates
  TLS, never uses `secureTransport`).
- The protocol (`docs/specs/relay.md`) is binary frames over WebSocket:
  `CONNECT` (IPv4 address and port, or a host name), `DATA`, `SHUTDOWN`
  (half close), `CLOSE`, `ACK` (flow control) and `DNS` (a raw DNS query,
  answered by the gateway). One encoding, implemented twice (Rust in
  `vetro_net::relay`, JS in `relay/src/protocol.mjs`) and checked against
  one shared set of vectors (`relay/test/vectors.json`).
- **Two implementations of the gateway, one core.** `relay/src/` holds the
  platform-free core (protocol, destination policy, session ledger, lane
  logic), used by the Cloudflare Worker (`relay/src/worker.mjs`, sockets
  from `cloudflare:sockets`, the session ledger in a Durable Object) and by
  a Node server (`relay/src/node.mjs`, `node:net` and the `ws` package,
  ledger in memory) for tests, the build VM and self-hosting.

### Determinism (ADR 0019)
The network is part of the machine. The upstream of the machine's stack is
now an `Uplink`: the sinkhole (offline) plus the client half of the gateway
protocol (online). It is pure state, with no I/O:
- what the guest sends towards the gateway becomes frames in an **outbox**
  that the host drains (`Machine::relay_take`): an output, like the
  console, recomputed in replay and kept out of snapshots and digests;
- everything that comes back from the gateway (connected, data, acks, DNS
  answers, closes) enters through **`Machine::input(Input::Relay(frame))`**:
  a recorded input with its instruction number. In replay the gateway is
  not contacted at all: the frames come from the log, and the outbox is
  discarded;
- flow control is in the machine's state, not in timing: the guest's bytes
  are accepted only within the credit the gateway has acknowledged (`ACK`
  frames, inputs), and the stack reads the gateway's bytes at its own pace,
  sending `ACK`s for what it has consumed. A fixed per-connection window
  (256 KiB each way) bounds the memory on both sides;
- the mode (online or offline) is state too, set by an input
  (`ONLINE`), and so is the loss of the gateway (`DOWN`: every connection
  through the gateway is refused or reset). The Worker sends `DOWN` after
  a snapshot restore and after a replay, because the connections recorded
  in the machine no longer exist at the gateway.

Connections and DNS flows opened in offline mode stay with the sinkhole
until they close; new ones follow the current mode. A snapshot made
offline (the prebuilt Android snapshot, ADR 0031) still loads: the
Uplink's state is an optional trailing section after the sinkhole's
(`GWAY`), absent in older snapshots, so `FORMAT_VERSION` does not change
(an older build reading a newer snapshot fails on the trailing bytes,
which is the behaviour a version bump would give). Android may still have
the sinkhole's fake addresses (198.18.0.0/15) in its DNS cache after such
a restore (TTL 300 s): a connection to a fake address the sinkhole handed
out goes to the gateway as `CONNECT` by name, and the gateway resolves it.

### Safeguards (all mandatory, enforced at the gateway)
1. **Ports**: only TCP 80 and 443. No UDP except DNS, which the gateway
   answers itself (DNS over HTTPS to a public resolver); QUIC and other UDP
   are dropped in the machine, and apps fall back to TCP.
2. **Destinations**: every address the gateway is about to connect to is
   checked **after resolution** (a `CONNECT` by name is resolved by the
   gateway first; every resolved address must pass, and the connection
   goes to a checked address literal). Rejected: `0.0.0.0/8`, private
   (`10/8`, `172.16/12`, `192.168/16`), shared (`100.64/10`, which also
   covers some cloud metadata endpoints), loopback (`127/8`), link-local
   (`169.254/16`, including the `169.254.169.254` metadata endpoint), IETF
   and documentation (`192.0.0/24`, `192.0.2/24`, `198.51.100/24`,
   `203.0.113/24`), 6to4 relay (`192.88.99/24`), benchmarking
   (`198.18/15`), multicast (`224/4`), reserved and broadcast (`240/4`).
   IPv6 is not offered (the guest has none, ADR 0007), so no IPv6 checks
   are needed: names resolve to A records only.
3. **Per-session limits**, kept by a session ledger shared by all the
   session's lanes (a Durable Object on Cloudflare): concurrent
   connections (32), WebSocket lanes (8), bandwidth (token bucket, 2 MiB/s
   with a 4 MiB burst, both directions together), total bytes (1 GiB),
   DNS queries (20/s, burst 100). Every byte crosses the ledger before it
   is read from or written to a socket; a session over its total is ended.
   Plus per-connection idle timeout (10 min) and per-lane maximum life (1 h).
4. **Short-lived signed tokens, for the Vetro site only.** `POST /session`
   answers only to an allowed `Origin` (the Vetro site; development origins
   only in the local Node gateway) with a token: HMAC-SHA-256 over
   `{sid, origin, iat, exp, first}`, valid 10 minutes, renewable with the
   same session id while the session is younger than 2 hours. Lanes
   (`GET /lane?token=…&lane=k`) need a valid token and the same `Origin`.
   An `Origin` check does not stop a non-browser client from forging it;
   it keeps other sites from using the gateway from their visitors'
   browsers, and the per-session limits bound what a forged session can do.
   A per-IP rate limit on `/session` (Cloudflare's rate limiting binding or
   a zone rule) is the next step if abuse shows up.
5. **No content logging.** The gateway logs no bytes, no destinations and
   no names: only per-lane counters at the end (connections, bytes,
   refusals by reason, duration). TLS is end to end, so it could not read
   HTTPS content anyway.
6. **Kill switch.** `GATEWAY_ENABLED` (a Worker variable): with `0`,
   `/session` and `/lane` answer 503 and the session ledger refuses every
   grant, so open lanes stop within one grant (≤ 256 KiB). Deploying with
   the variable at `0` is one command (`relay/kill.sh`); deploying again
   with `1` turns it back on. Rotating `TOKEN_SECRET` invalidates every
   token. The lane life bound (1 h) is the worst case for a connection that
   escaped both.

### Web app
- The default is **online, with no user choice**: when the app has a
  gateway (`DEFAULT_GATEWAY` in `web/node/relay.mjs`, or `?gateway=URL`)
  the Worker puts the machine online at start and after every restore.
  `?net=offline` keeps the sinkhole (analysis mode); without a configured
  gateway the app stays offline and says so in the status.
- If the gateway cannot be reached, the Worker answers the guest's
  `CONNECT`s itself with `REFUSED` (the guest sees a reset at once instead
  of waiting 75 s) and retries the session later.
- The real clock bounds guest time (`realtime`, the default): with the
  network online, idle guest time does not run ahead of the round trips.

### Cloudflare specifics
- Each Worker invocation may hold only six connections at once (sockets
  and fetches). A lane carries at most `laneConns` connections (5 on
  Cloudflare, announced in the gateway's `hello`), and the page opens
  further lanes as needed, each one a separate invocation, up to the
  session's lane limit.
- Outbound sockets to Cloudflare's own address ranges are blocked by the
  platform: such connections are refused with the reason `platform`, so
  sites behind Cloudflare do not load through the Workers gateway. The
  Node gateway has no such restriction, and the protocol is the same: if
  this matters in practice, the Node gateway runs on a small VM behind the
  same hostname. To be measured after the deploy.
- The Workers Paid plan is needed for the CPU time of long-lived lanes
  (the free plan's 10 ms per request does not fit a data pump).

## Rejected alternatives
- **Raw frames to the gateway (a remote slirp):** the gateway would see and
  have to parse IP packets, and the inspector and determinism would move
  out of the machine. Byte streams are smaller and simpler to police.
- **DNS answered locally with fake addresses, connect by name only:** it
  hides the real addresses from the guest, but breaks apps that resolve and
  compare addresses, and DNS records other than A. The gateway answers DNS
  (raw queries over DoH); connect by name remains only for the sinkhole's
  fake addresses.
- **Recording at the socket level (outside the machine):** replay would
  need the stack to run differently; recording the gateway's frames as
  inputs keeps ADR 0019's model.
- **TLS interception at the gateway:** never; plaintext comes from the
  guest-side hooks (ADR 0029), and TLS stays between app and server.
- **A user toggle for online/offline:** the owner wants online by default
  with no choice; the sinkhole stays reachable for analysis with a URL
  parameter.

## Verification
- `node relay/test/unit.mjs`: protocol encode/decode and the shared
  vectors; destination policy (every rejected range, edges, ports);
  tokens (signature, expiry, origin, renewal); the ledger (connections,
  lanes, bandwidth wait, total bytes, DNS rate, kill switch); the lane
  logic against fake sockets (connect, data with windows and acks, half
  close, refusals, idle timeout, limits).
- `node relay/test/node.mjs`: the Node gateway over a real WebSocket with
  a local TCP server, the page's `GatewayClient` against it.
- `cargo test -p vetro-net`: the `Uplink` (online and offline, credit and
  acks, DNS through the gateway, `CONNECT` by name for fake addresses,
  `DOWN`, snapshot with and without `GWAY`), the vectors.
- `cargo test --release -p vetro-boot-tests --test relay` (BusyBox guest,
  build VM): `wget` through the protocol, recording and identical replay.
- `VETRO_ANDROID=1 node tests/web/android-online.mjs` (build VM, local
  Node gateway): Android in Chrome loads a real HTTPS page in the browser
  app and a catalog app makes a real request.
