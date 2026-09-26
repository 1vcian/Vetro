# Spec — vetro-analysis

## Scope
Analysis engine from outside the guest. Today: syscall decoding for
`vetro run --strace` (M2, `syscall` module), network analysis (M7,
`net` module, ADR 0016) and input→effects timeline (M7, `timeline` module,
ADR 0023). TLS, Binder, ART hooks and scripting arrive with M7–M9.

## Allowed dependencies
None (neither at runtime nor in development). Compiles to `wasm32-unknown-unknown`.
No host clock, threads, I/O, `HashMap`: `BTreeMap`/`Vec` tables,
deterministic results. No panic on arbitrary bytes (parsers
return `None`/`Err` or mark incomplete messages).

## `net`: public interface
- `capture`: `Direction { FromGuest, ToGuest }`, `Frame { at_us, dir, data }`
  (guest virtual time in µs), `Capture` (frames in non-decreasing time
  order: `push`, `frames`, `into_frames`, `From<Vec<Frame>>`).
- `pcapng`: `write(&[Frame], &PcapngOptions { epoch_us, interface }) ->
  Vec<u8>`; `read(&[u8]) -> Result<PcapngFile, PcapngError>` (interfaces and
  frames; EPB and SPB, both byte orders, `if_tsresol`).
- `packet`: `parse(&[u8]) -> Packet` (`Tcp`, `Udp`, `OtherIpv4`, `Arp`,
  `Other`) and the `ethernet`, `ipv4`, `tcp`, `udp` headers.
- `flow`: `Flows::from_frames` → `tcp: Vec<TcpFlow>` (client, server,
  times of SYN/SYN-ACK/handshake/close, reset, `client_data` and
  `server_data`: `Stream { bytes, marks, fin, missing }`, `time_at(offset)`)
  and `udp: Vec<UdpFlow>` (datagrams with time and direction), `other_frames`.
- `dns`: `parse(&[u8]) -> Option<Message>`, `exchanges(&[UdpFlow]) ->
  Vec<DnsExchange>` (name, type, query and answer times, rcode,
  records; `ipv4()`), `type_name`.
- `http`: `looks_like_request`, `parse_requests(&[u8])`,
  `parse_responses(&[u8], methods, eof)`; `Request`/`Response` with
  `Headers` (case-insensitive lookup, `has_token`), offset in the stream,
  `complete`, and `Body { wire_len, raw, decoded, content_encoding,
  decode_error, chunked }`.
- `inflate`: `inflate_raw`, `gunzip` (multiple members), `zlib_or_raw`, `crc32`.
- `body`: `decode(content_type, &[u8]) -> Decoded` (`Empty`, `Json`,
  `Form`, `Multipart(Vec<Part>)`, `Protobuf(Vec<Field>)`, `Text`,
  `Binary { len, note }`), `protobuf`, `protobuf_text`, `form`,
  `url_decode`, `media_type`, `param`; `Decoded::kind`, `to_text`.
- `json`: `parse -> Value` (order and numbers preserved), `Value::get`,
  `as_str`, `to_compact`, `to_pretty`, `quote`.
- `inspector`: `NetworkAnalysis::from_frames(&[Frame])` → `frames`,
  `flows`, `dns`, `http: Vec<HttpExchange>`, `tls: Vec<TlsFlow>`;
  `requests() -> Vec<RequestRow>` (with `Display`: one line per request).
  `HttpExchange`: flow, addresses, resolved name, URL, request,
  response, decoded bodies, `Timings` (`started_us`, `blocked_us`,
  `dns_us`, `connect_us`, `send_us`, `wait_us`, `receive_us`,
  `total_us()`), `first_on_connection`. `tls_sni(&[u8])`.
- `har`: `to_har(&NetworkAnalysis, &HarOptions { epoch_us }) -> String`
  (also `NetworkAnalysis::to_har`), `iso8601`, `base64`.
- `view` (ADR 0023): the inspector as JSON for the web app. `requests_json(&a)`:
  `{"frames", "requests": [row], "dns": [{name, type, queryUs, answerUs,
  rcode, addrs}], "tls": [{flow, server, sni, startedUs}]}`; row = `{i, flow,
  method, url, host, path, status, reason, mime, reqBytes, respBytes,
  reqWire, respWire, reqKind, respKind, complete, client, server,
  resolvedName, timings: {startedUs, blockedUs, dnsUs, connectUs, sendUs,
  waitUs, receiveUs, totalUs}}` (`null` for whatever is missing).
  `exchange_json(&x)`: `{row, request: {method, target, version, complete,
  headers: [[k, v]], body}, response: {status, reason, version, complete,
  interim, headers, body} | null}`; body = `{wire, raw, size, encoding,
  decodeError, chunked, contentType, kind, text, textTruncated, base64,
  truncated}` plus `json` (the value), `fields` ([[k, v]] of the form), `parts`
  (multipart: `{name, filename, contentType, size, kind, text, ...}`),
  `note` (binary). `text` is `Decoded::to_text` (at most 512 KiB), `base64`
  the first 256 KiB of the decoded body; `split_url`.

## `net::tls`: plaintext from the TLS hooks (M7, ADR 0029)
- `TlsMessage { at_us, to_server, data }`: one block of `SSL_write`
  (`to_server`) or `SSL_read` of a connection, with the guest time.
- `TlsConversation { client, server, host, pid, tid, process, package,
  library, messages }`: an encrypted connection seen in plaintext by the
  hooks, with its attribution (process and library). `exchanges(flow)`
  reconstructs its requests as `HttpExchange { secure: true, attribution }`.
- `NetworkAnalysis::merge_tls(&[TlsConversation])`: merges the HTTPS
  requests with the plaintext ones (same order, same list, same HAR).
- `HttpExchange.secure` and `.attribution` (`Attribution { pid, tid,
  process, package, library }`); in the HAR as `_secure` and `_vetro`, in the
  `view` as `secure` and `attribution`. The machine fills the
  `TlsConversation`s (`vetro_machine::tls`); here there is only the reconstruction.

## `timeline`: public interface (ADR 0023)
- Time: µs of guest time, `step_us(instructions) = instructions / 100`
  (the same as for captured frames).
- `InputKind` (`Key`, `Pointer`, `Touch`, `Console`, `Files`, `Power`,
  `Display`, `Other`; `name`, `code`/`from_code` = position in `ALL`),
  `UserInput { step, at_us, kind, label, weak, seq }` (`UserInput::new`).
- `EffectKind` (`Http`, `Dns`, `Tls`, `File`, `Console`), `Effect { at_us,
  kind, label, detail, bytes, seq }` (`Effect::new`: `seq = u64::MAX`, after
  the inputs of the same instant).
- `cause(inputs, at_us, seq, window_us, strong_only) -> Option<usize>`.
- `Timeline`: `push_input`, `push_effect` (in time order, `seq` of
  arrival), `push_console(at_us, bytes)` (merges into the last console
  effect if nothing else has arrived in the meantime), `attributed(extra,
  window_us)`, `to_json(extra, window_us)` = `{windowUs, dropped, version,
  inputs: [{i, step, atUs, kind, label, weak, effects}], effects: [{atUs,
  kind, label, ref, bytes, cause}]}`, `clear`, `version`, limits
  `MAX_INPUTS`/`MAX_EFFECTS` (the oldest are discarded, `dropped`).
- `network_effects(&NetworkAnalysis)`: HTTP requests (at
  `timings.started_us`, `detail` = index in the inspector), DNS queries,
  TLS flows.
- `LineEditor` (lines typed at the console: DEL/BS, ^U, ^C, CR/LF, escapes
  ignored), `printable(bytes)`, `key_name(code)`, `is_enter(code)`.

**Attribution rule (heuristic):** the effect belongs to the last input
that precedes it (earlier instant, or equal and arrived first) within the
window (default 3 s); for network and files only command inputs
(`weak == false`: Enter, click, touch, file manager command, power-on), for
the console any input. With no input in the window: no cause.

## Semantics
- Times as seen by the guest, at the virtio-net boundary. Phases: `dns` = query →
  answer of the DNS lookup that gave the address (only the first connection that
  uses it); `connect` = SYN → ACK of the handshake (first request of the
  connection); `send` = first → last byte of the request; `wait` =
  last byte of the request → first of the response (0 if the response
  arrives earlier, as the sinkhole does); `receive` → last byte of the
  response; `blocked` = the rest up to the first byte of the request.
  The total is the sum of the phases.
- URL: `http://` + `Host` (or resolved name, or address) + target; an absolute
  target is used as is; CONNECT → `https://` + authority.
- Connections opened by the host (port forwarding, `Stack::host_connect`):
  the client is 10.0.2.2, the server the guest; HTTP served by the guest is
  analysed the same way.
- HAR: see the header of `src/net/har.rs` and ADR 0016.

## Hooking into the machine
- `vetro-machine`: `Machine::net_tap(on)` / `net_tap_take()`,
  `TappedFrame { at: VirtualTime, dir: FrameDir, data }` (ADR 0016). They
  do not change execution and do not go into snapshots.
- `vetro-cli`: `vetro boot --pcap FILE --har FILE --net-requests`
  (`vetro_cli::netcap`).
- vetro-wasm (ABI 8, ADR 0023): capture in the `Vm` on every `vetro_run`,
  `vetro_capture_*`, `vetro_inspect_*` (list, detail, HAR, pcapng),
  `vetro_timeline_*`; inputs described by `analysis::Describer`
  (`docs/specs/wasm.md`).

## Tests
- `cargo test -p vetro-analysis`: parsers with constructed inputs (Ethernet/IP,
  flows with retransmissions, out of order, sequences beyond 2^32, holes,
  reused ports; DNS with looping and truncated pointers; HTTP pipelining,
  HEAD, 100 Continue, chunked, truncated; host gzip with fixed and
  dynamic codes, multiple members, zlib, uncompressed blocks; valid and invalid
  JSON; form, multipart, protobuf and gRPC; pcapng round trip; HAR with
  timing sums, dates, base64; SNI), and random bytes without panics.
- `vetro-machine`: `cattura_dei_frame_nei_due_versi`.
- `cargo test --release -p vetro-boot-tests --test analysis`: the guest
  kernel makes seven requests with BusyBox wget/nc (JSON, host
  chunked+gzip, JSON→protobuf, form, multipart, protobuf, BusyBox gzip
  body); checks requests, bodies, DNS, timings, HAR, pcapng re-read
  by our reader and, with Docker, by `capinfos`/`tcpdump`/`tshark`,
  HAR validated by `har-validator` (schema 1.2) and opened by `haralyzer`
  (`tools/analysis/check.sh`; `VETRO_REQUIRE_ANALYSIS_TOOLS=1` makes
  the absence of Docker an error); two runs give identical pcapng and
  HAR.
- `cargo test --release -p vetro-cli --test boot_pcap_har`: the
  `vetro boot` options.
- `timeline` and `view`: unit tests in `cargo test -p vetro-analysis`;
  from the API in `cargo test -p vetro-wasm` and `tests/web/inspector.mjs`
  (M3 kernel: wget to the sinkhole, decoded bodies, attribution to the
  command, two identical runs); in the app `tests/web/browser-analysis.mjs`.
