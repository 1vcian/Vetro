# ADR 0016 — Network analysis: capture point, pcapng, HTTP and HAR

- Status: accepted (M7, first part, 2026-09-25).

## Context
M7 asks for a network inspector with HAR and pcap exports, body decoding
(JSON, protobuf, form) and, for the exit, cleartext HTTPS tied to the
user's action. The HTTPS part requires hooks on BoringSSL and Conscrypt
inside Android, which does not run yet; everything else can be done today
on the traffic of the M3 guest kernel (BusyBox), which goes through the
`vetro-net` stack (ADR 0007) via virtio-net.

Constraints: determinism (M10 replays sessions: timings and data order
must depend only on the execution), no external dependencies,
compilation for wasm32 (the inspector will end up in the browser), and no
change to the logic of `vetro-net`, which has another owner.

## Decision

### Capture point at the virtio-net boundary
- `NetLink` (the virtio-net backend in `vetro-machine`) copies every
  Ethernet frame in both directions, if capture is on: `FromGuest` in
  `send` (before `Stack::receive`), `ToGuest` in `recv` (after
  `Stack::pop_frame`). The instant is `NetLink::now`, the virtual time the
  machine sets before serving virtio-net (CNTPCT in microseconds): the
  same as the stack's, hence deterministic.
- API: `Machine::net_tap(on) -> bool` and `Machine::net_tap_take() ->
  Vec<TappedFrame>`. They access the backend without marking the devices
  to serve (unlike `Machine::net`): turning capture on, off and draining
  it does not change the execution. Capture is not included in snapshots
  (it is observation, not guest state).
- The point is on the machine side and not inside the stack: it sees
  exactly what the guest sees (including DHCP and ARP), it does not
  require touching `vetro-net`, and it will work the same with the relay
  (M7) or other upstreams.

### `vetro-analysis::net`: everything from frames
The analysis works only on captured frames (`Frame { at_us, dir, data }`),
not on the event log nor on the sinkhole bytes: this way it is the same
for any source (live capture, re-read pcapng file, the browser in the
future) and does not depend on the `vetro-net` types.
- **pcapng** (not classic pcap): one `LINKTYPE_ETHERNET` interface with
  `if_tsresol` = 6 and the direction in `epb_flags` (`outbound` for the
  guest's frames). Time = Unix epoch + guest time (epoch 0 by default: the
  file says "1970-01-01 00:00:01.5" for 1.5 s after power-on). Our own
  reader re-reads the files (round-trip tests).
- **TCP flows** reconstructed by sequence number (unwrapped around the
  current position: it crosses 2^32), retransmissions and overlaps
  discarded, out-of-order data held (up to 16 MiB per direction), holes
  never filled counted; each piece carries the instant it became
  readable, from which the request timings come. Client = whoever sends
  the SYN; a SYN on a closed 4-tuple opens a new flow. **UDP** by
  4-tuple.
- **HTTP/1.1** (RFC 9112): pipelining, `chunked` with extensions and
  trailers, `Content-Length`, until close, no body for HEAD/1xx/204/304,
  intermediate 1xx responses; `Content-Encoding` gzip and deflate with
  our own DEFLATE (in the manner of `puff.c`, CRC-32 and Adler-32
  verified, output capped at 256 MiB). Brotli and zstd are not decoded:
  the body stays as is and the entry notes it.
- **DNS** from flows to port 53: questions, answers (A, AAAA, compressed
  names), matched by id; the resolved name gives the host of the
  connections and the `dns` phase of the first request to that address.
- **Bodies**: JSON (our own parser that preserves order and numbers as
  written), urlencoded form, multipart (with recursive decoding of the
  parts), schemaless protobuf on the wire like `protoc --decode_raw` (a
  length-delimited field is a message if it is one and is not printable
  text, then a UTF-8 string, then bytes), uncompressed gRPC. Chosen from
  the `Content-Type`, otherwise from the content.
- **Inspector**: `NetworkAnalysis::from_frames` → flows, DNS exchanges,
  HTTP requests with decoded body and `Timings` (the HAR phases:
  blocked, dns, connect, send, wait, receive; the sum is the total),
  TLS flows with the SNI from the ClientHello. `requests()` gives the
  rows of the list.
- **HAR 1.2**: one entry per request; `content.text` decoded (base64
  if not UTF-8), decoders' output in the `comment`s, `_encoding` for the
  binary body of a request (HAR has no `encoding` in `postData`),
  `status: 0` with no response, `connection` = flow index.

### Exposure
- `vetro boot --pcap FILE --har FILE --net-requests` (also with `=`):
  capture on from the start, files written on exit, list on stderr.
- vetro-wasm and the web app will follow (same functions, already
  compiled for wasm32).

### Verification with external tools
`tools/analysis/check.sh` (Docker image `tools/analysis/Dockerfile`):
`capinfos` and `tcpdump` count the packets, `tshark` decodes HTTP requests
and responses, `har-validator` 5.1.5 validates the HAR against the HAR
1.2 schema, `haralyzer` reopens it in Python. The test
`tests/boot/tests/analysis.rs` uses it when Docker is available
(`VETRO_REQUIRE_ANALYSIS_TOOLS=1` makes its absence an error).

## Consequences
- M7 stays open until the TLS hooks in Android (BoringSSL, Conscrypt),
  the input→effects timeline and the exit on the 10 apps are there:
  these pieces will use the same model (`HttpExchange` will also come
  from the hooks' cleartext bytes, with the same decoders and the same
  HAR).
- IPv6, IPv4 fragments, HTTP/2 and HTTP/3 are not analysed (the guest and
  the stack do not use them today: see `docs/specs/net.md`); they are
  added as new parsers on top of the same flows.
- Capture keeps all frames in memory until the host takes them: whoever
  turns it on for long sessions must drain it (`vetro boot` does so at
  every quantum).
