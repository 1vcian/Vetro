# Spec — vetro-net

## Scope
Host-side network stack: the "other end of the cable" of virtio-net.
Virtual IPv4 gateway like QEMU's user network, TCP and UDP terminated on the
host (like slirp), trait-based backend (`Upstream`) with sinkhole and relay,
event log for analysis. Choices in ADR 0007.

## Virtual network (`NetConfig` defaults)
| | |
|---|---|
| Guest (via DHCP) | 10.0.2.15/24 |
| Gateway | 10.0.2.2, MAC 52:55:0a:00:02:02 |
| DNS | 10.0.2.3 (same MAC) |
| MTU | 1500, advertised MSS 1460 |
| DHCP lease | 86400 s (T1 half, T2 7/8) |

- **ARP**: replies only for gateway and DNS; never for the guest's address
  (the DHCP client's ARP probes must not see conflicts).
- **DHCP** (RFC 2131): DISCOVER→OFFER, REQUEST→ACK (NAK if the address is
  not the guest's; silence if the server id belongs to another server),
  INFORM→ACK; RELEASE and DECLINE only logged. Replies ≥ 300 bytes, as IP
  broadcast if the client does not have `ciaddr` yet.
- **ICMP**: echo to gateway and DNS always answered; towards the outside
  `Upstream::ping` decides. UDP to gateway ports with no service: ICMP
  port unreachable.
- **UDP**: every 4-tuple is a flow with a `ConnId`; closed after
  `udp_idle_timeout_us` (60 s) of inactivity. DNS is a UDP flow to
  10.0.2.3:53, answered by the upstream; the stack decodes its questions and
  answers (A records) for the log.
- **TCP**: every SYN creates a connection, opened towards the upstream; the
  SYN-ACK is sent only when the upstream answers `Connected` (RST if
  `Refused` or after `tcp_connect_timeout_us`, 75 s). TCP details in
  ADR 0007 and at the top of `src/tcp.rs`.
- **Not supported** (counted in `Stats`, never a panic): IPv6 (the guest
  receives no Router Advertisement and stays with link-local only; the
  sinkhole answers AAAA questions with no records, so apps fall back to IPv4),
  IPv4 fragments (Linux uses DF and PMTU; DNS over UDP stays below the MTU),
  multicast and broadcast other than DHCP, VLAN, TCP options other than MSS.
- With `verify_checksums` (default) packets with a wrong checksum are
  dropped: the platform must not offer `VIRTIO_NET_F_CSUM` to the guest, or
  must complete partial checksums before calling `receive`.

## Public interface
- `Stack<U: Upstream>`:
  - `new(NetConfig, U)`;
  - `receive(now, &frame)`: Ethernet frame transmitted by the guest (also
    runs a `poll`);
  - `poll(now)`: timers (retransmissions, TIME-WAIT, waits, UDP flows) and
    exchanges with the upstream;
  - `pop_frame() -> Option<Vec<u8>>`: next frame for the guest;
  - `next_deadline() -> Option<VirtualTime>`: by when to call `poll` again
    (an asynchronous upstream also needs a `poll` when it has new data);
  - `events()`, `take_events()`, `stats()`, `upstream()`, `upstream_mut()`.
- `VirtualTime`: microseconds of virtual time, non-decreasing, supplied by
  the caller. `ConnId`: `u64` unique across TCP and UDP, increasing from 1.
  `Flow { guest, remote }`: 4-tuple as seen by the guest.
- `trait Upstream` (sans-I/O, never called by the stack reentrantly):
  `tcp_open`, `tcp_status` (`Pending`/`Connected`/`Refused`),
  `tcp_write` (returns the bytes accepted: backpressure),
  `tcp_read` (`Data(n)`/`WouldBlock`/`Eof`/`Reset`), `tcp_shutdown` (guest
  FIN), `tcp_close(reset)` (last call for that id), `udp_send`,
  `udp_recv`, `udp_close`, `ping`.
- `Sinkhole` (`SinkholeConfig`): accepts everything except `refused_ports`;
  `TcpReply { on_connect, on_data, close_after_reply }` responses per port
  (default: empty HTTP 200 on port 80); closes when the guest closes;
  DNS A → fake addresses in 198.18.0.0/15 in order of first question
  (198.18.0.1, .2, …), names lowercased; logs connections
  (`TcpRecord`: resolved name, guest bytes, close), UDP flows and
  DNS questions.
- `Relay` (non-blocking transport of `RelayMessage`), `RelayUpstream<R>`
  (`Upstream` → messages adapter), `MemoryRelay` (test relay: TCP/UDP
  echo, DNS from a table). Wire encoding and host↔relay flow control:
  M7.
- Public format modules (`wire`, `dhcp`, `dns`) for tests and tools.
- Port forwarding (host → guest, section below): `host_connect`,
  `host_send`, `host_recv`, `host_shutdown`, `host_abort`, `host_conn`
  (`HostConnInfo`, `HostConnState`), `host_conns`, `host_release`; module
  `hostfwd` (`HOST_BUFFER`, `FIRST_EPHEMERAL_PORT`).

## Port forwarding: connections from the host to the guest
Like QEMU's `-netdev user,hostfwd=tcp:…`: the host opens a TCP connection
to a guest service (for example adbd on 5555). The stack acts as a TCP
client towards the guest; no real socket in the core, everything synchronous.

- `host_connect(port) -> Option<ConnId>`: SYN from **10.0.2.2** (the gateway,
  the way slirp translates connections from localhost) to
  `guest_ip:port`, from a deterministic ephemeral port (49152, 49153, …,
  skipping 4-tuples in use). The id comes from the same counter as the
  guest's connections. The SYN goes out at the **next `poll`**: all host
  actions take effect there.
- From the SYN-ACK on, the connection is the same state machine as `tcp.rs`
  (the only addition is the `SynSent` state: SYN with MSS 1460 and window
  65535, retransmitted with the RFC 6298 RTO and doubling; gives up after
  `tcp_connect_timeout_us` without RST; valid RST|ACK from the guest =
  `Refused`; wrong ACK = RST; simultaneous open not handled). In place of
  the upstream there is the host side (`HostSide`): two byte queues.
- `host_send` accepts at most `HOST_BUFFER` (256 KiB) in the queue and
  returns how many bytes it took (backpressure towards the host); the stack
  sends them at the pace of the guest's window. `host_recv` reads the guest's
  bytes: this queue also holds at most 256 KiB, beyond which the window
  advertised to the guest closes until the host reads (the window update
  goes out at the next `poll`).
- `host_shutdown`: FIN after the queued bytes (then a 4 s TIME-WAIT if the
  guest closes afterwards). `host_abort`: RST to the guest (`RemoteReset`);
  before the SYN, close without packets. `host_release`: forgets a
  closed connection (if it is alive, it aborts it first).
- `host_conn(id)`: `state` (`Connecting`, `Open`, `Closed(CloseReason)`:
  `Normal`, `Refused`, `GuestReset`, `RemoteReset`, `Timeout`),
  `readable`, `writable`, `guest_eof` (guest FIN and everything read),
  `unsent`, `flow`.
- Log: `TcpConnect { id, flow }` in place of `TcpOpen` (line
  `tcp 1 from host 10.0.2.2:49152 -> 10.0.2.15:5555`), then the same
  `TcpEstablished`, `TcpData` (`ToRemote` = guest bytes towards the host) and
  `TcpClosed`.
- The guest's MAC is needed, learned from its first frame (in practice
  DHCP): before that, segments would go out as broadcast and Linux would
  drop them.
- Determinism: same calls at the same instants → same frames and log. Host
  calls are **inputs**: in `vetro-machine` they go through `Machine::input`
  with `Input::HostNet` (which forces a `poll` before the next instruction)
  and are recorded with the instruction number, like console bytes (M10,
  ADR 0019).

Platforms:
- native: `vetro boot --hostfwd=tcp:[ADDR]:PORT-[10.0.2.15]:GUEST_PORT`
  (repeatable; without an address it listens on 127.0.0.1; port 0 = chosen
  by the system, printed on stderr as `vetro: hostfwd tcp 127.0.0.1:PORT ->
  10.0.2.15:5555`). The real sockets are in `vetro-cli` (`src/hostfwd.rs`):
  one thread accepts, one per connection reads (with a limit of 1 MiB read
  and not yet taken by the stack), one writes (likewise, 1 MiB); everything
  enters the machine **between one quantum and the next** (2 million
  instructions) in the `vetro boot` loop, which touches the stack only if
  there is something to do (without `--hostfwd` connections execution does
  not change). Closes towards the client like slirp: FIN after `Normal` and
  `Refused` (nobody listening: the client sees the end of the stream
  immediately), RST (SO_LINGER 0) after a reset or a timeout; an RST from the
  client becomes `host_abort`.
- browser: vetro-wasm's `vetro_net_*` (ABI 5, `docs/specs/wasm.md`) and
  `GuestSocket` in `web/node/vetro.mjs`.

## How adb will use forwarding (M5/M6)
adbd in the Android guest listens on TCP 5555 (`service.adb.tcp.port=5555`,
`docs/research/m5-android-images.md`).
- Native: `vetro boot … --hostfwd=tcp:127.0.0.1:5555-:5555`, then
  `adb connect 127.0.0.1:5555` with the host's real adb: the ADB protocol
  (CNXN, AUTH with the RSA key from `~/.android/adbkey`, OPEN/WRTE/OKAY/
  CLSE) passes transparently over the forwarded connection. As with QEMU
  (`hostfwd=tcp::5555-:5555`); the Android Studio emulator instead uses
  the 5554/5555 pair for the console, which does not exist here.
- Browser (done, ADR 0028): the JS ADB client in `web/node/adb.mjs` over
  `GuestSocket` (`connectGuest(5555)`): 24-byte messages + data,
  `shell,v2,raw:` (stdout, stderr, exit code), `sync:` for push, install =
  push to `/data/local/tmp` + `pm install -r`, `devices` from the banner and
  `ro.serialno`. AUTH is handled (`AdbKey`: RSA 2048 from WebCrypto, PKCS#1
  v1.5 signature of the token as a SHA-1 digest, public key in the
  `adb_keys` format), but Vetro's userdebug image has `ro.adb.secure=0` and
  adbd already on TCP 5555 (ADR 0022): the handshake is a plain CNXN. Tested
  against a fake adbd (`tests/web/adb.mjs`), against adbd under QEMU over TCP
  (`tests/web/adb-tcp.mjs`), against adbd in the guest from Node
  (`tests/web/android.mjs`) and in the app (`tests/web/android-chrome.mjs`).

## Event log (`NetEvent { at, kind }`)
`Dhcp`, `IcmpEcho`, `TcpOpen`, `TcpEstablished`,
`TcpData { dir, len }` (new bytes only: from the guest when they arrive in
order, towards the guest when the stack takes them from the upstream),
`TcpClosed { reason, bytes_to_remote, bytes_to_guest }`, `UdpOpen`, `UdpData`,
`UdpClosed`, `DnsQuery { txid, name, qtype }`, `DnsAnswer { rcode, addrs }`,
`TcpConnect` (connection opened by the host, port forwarding). The byte
contents are not in the log: the upstream holds them (the sinkhole keeps them).

`NetEvent`, `Flow` and `Mac` implement `Display`: one line per event with the
virtual time in seconds (`[     1.500000] tcp 3 syn 10.0.2.15:40000 ->
198.18.0.1:80`), used by `vetro boot --net-events`.

## Wiring into the machine (`vetro-machine`, M5)
- `Devices::net: Option<NetSetup>` (`mac`, `NetConfig`, `SinkholeConfig`);
  the default has the network, mounted after GPU, keyboard and tablet (slot
  28, like QEMU's fourth `-device`). Default guest MAC
  `52:54:00:12:34:56`, the one QEMU gives to the first `virtio-net-device`.
- virtio-net backend: `NetLink` (`send` → `Stack::receive`, `recv` →
  `Stack::pop_frame`). virtio-net offers only MAC, STATUS and MRG_RXBUF: no
  `VIRTIO_NET_F_CSUM`, so the guest computes all checksums.
- Time: `VirtualTime` = the machine's CNTPCT in microseconds (rounded
  down; the `next_deadline` deadline becomes the first CNTPCT that reaches
  it). No host clock. In `Machine::sync_irqs`, before servicing the devices,
  the stack receives the current instant; at its deadline `poll` is called
  and, if there are frames, virtio-net is serviced. The stack's deadline
  enters the machine's next deadline together with the timer's: it bounds
  the JIT blocks and acts as the wake-up for WFI (an idle guest jumps
  straight there). Same instructions with and without the JIT.
- Host access: `Machine::input(Input::HostNet(..))` and
  `Input::NetFrame` (recorded inputs, ADR 0019),
  `Machine::net(|stack| …)` (mutable, forces a `poll`; during a
  recording it is an opaque event that stops replay) and
  `Machine::net_view(|stack| …)` (read-only, does not change execution:
  log, statistics, `upstream().tcp_connections()`).
- CLI: `vetro boot` has the network by default; `--no-net` removes it, `--net`
  puts it back even with `--no-devices`, `--net-events` prints the log on
  stderr (`vetro-net: …`).
- The relay (M7) will be another upstream behind the same `NetLink`.
- Capture (M7, ADR 0016): `Machine::net_tap(on)` and `net_tap_take()`
  copy the Ethernet frames passing through `NetLink` in both directions, with
  the virtual instant; observation only (does not change execution, outside
  snapshots). The analysis is in `vetro-analysis` (`docs/specs/analysis.md`).

## Invariants
- Compiles to `wasm32-unknown-unknown`; no runtime dependencies; no
  `std::time`, threads, I/O, `HashMap` (tables are `BTreeMap`, the order
  of produced frames is by `ConnId`).
- Determinism: same calls with same arguments → same frames and same
  log. TCP ISNs derive from `NetConfig::seed` and the `ConnId`
  (SplitMix64); IP idents are a counter.
- No panic on arbitrary frames (parsers return `None`).
- Every produced packet has correct checksums, DF, TTL 64.

## Tests
`cargo test -p vetro-net`: a fake guest that builds frames (DHCP with
smoltcp's encoder, ARP, ICMP, DNS, UDP, TCP) and checks replies and
log; every produced frame is validated by `smoltcp::wire` and by checksums
recomputed in the test. TCP: handshake, retransmitted SYN and SYN-ACK,
retransmission with doubling, exhaustion with RST, zero window and probes,
upstream backpressure, guest MSS and window respected, out of
order and overlaps, RST (valid and out of window), challenge ACK,
closes from both sides with TIME-WAIT, simultaneous transfer in
both directions with 20% of frames lost, determinism with the same seed.

With the Linux 6.18 guest kernel (`cargo test --release -p vetro-boot-tests`,
BusyBox in the initramfs, `udhcpc` with `/usr/share/udhcpc/default.script`):
- `vetro.rs` (self-test, compared with QEMU `-netdev user`): DHCP lease
  (10.0.2.15/24, router 10.0.2.2, DNS 10.0.2.3, 86400 s), routing
  table, `resolv.conf`, ping to gateway and DNS: same log;
- `net.rs` (Vetro only, because QEMU sends DNS and TCP to the real network):
  `nslookup` receives 198.18.0.1; `wget` does a short GET, a 300 KB GET
  (`cksum` sum equal to the host's) and a 108 KB POST (body found again
  byte for byte in the sinkhole); ping to the gateway and to a fake
  address; the host checks DHCP, DNS questions and answers, connections with
  resolved name, bytes in the log equal to those in the sinkhole, `Normal`
  closes, no wrong checksums; two runs give the same log, same
  instructions, same event log;
- `crates/vetro-cli/tests/boot_net.rs`: `vetro boot --no-devices --net
  --net-events`, DHCP and a GET, events read from stderr.

Port forwarding:
- `crates/vetro-net/tests/hostfwd.rs` (fake guest server): SYN from the
  gateway with the first ephemeral port, handshake and log, 200 KB echo
  beyond window and queue, backpressure in both directions (window
  closed and reopened), close from the host with TIME-WAIT and from the guest,
  RST|ACK from the guest (`Refused`, and an RST with a wrong ACK ignored),
  retransmitted SYN and timeout at 75 s, reset from the host and from the
  guest, release, determinism;
- `tests/boot/tests/hostfwd.rs` (M3 kernel, direct API between one quantum
  and the next): `nc -n -v -l -p 5555 -e cat` in the guest, line `connect to
  10.0.2.15:5555 from 10.0.2.2:…`, short and 200 KB echo, clean close,
  port with no service (`Refused`), service that writes and closes first,
  reset from the host (`cat: read error: Connection reset by peer` in the
  guest) and from the guest (service that exits with unread data:
  `GuestReset`), two identical runs. Comparison with QEMU
  (`-netdev user,hostfwd=tcp:127.0.0.1:PORT-:5555`, same `nc`): same
  lines `listening on 0.0.0.0:5555 ...` and `connect to 10.0.2.15:5555 from
  10.0.2.2:PORT (10.0.2.2:PORT)` (source port aside) and same echo.
  On macOS the client runs inside the QEMU container (`docker exec`,
  `VETRO_ORACLE_NAME`), because connections forwarded by Docker
  would come from its gateway (172.17.0.1) and not from localhost;
- `crates/vetro-cli/tests/boot_hostfwd.rs`: `vetro boot --hostfwd=
  tcp:127.0.0.1:0-:5555` with a real `TcpStream`: 200 KB echo with
  orderly close in both directions, port with no service (the client sees
  the close), client RST seen by the guest;
- `tests/web/hostfwd.mjs`: the same with `GuestSocket` in Node (JIT and
  interpreter).

## Snapshot (M6, ADR 0015)

`Stack<U: Upstream + Snapshot>` and `Sinkhole` implement
`vetro_snapshot::Snapshot` (files `stack/snapshot.rs`, `tcp/snapshot.rs`,
`sinkhole/snapshot.rs`, child modules so they can see the private fields):
guest MAC, outgoing frames, complete TCP connections (state, sequences,
windows, congestion, in-flight data, RTO and timers), UDP flows, indexes,
`next_id`, `ip_ident`, counters, event log, connections opened by the host
(port forwarding: queues, next ephemeral port) and upstream state. The
configuration is not saved. Whoever adds a field to the state also adds it
there and bumps `vetro_snapshot::FORMAT_VERSION`.
