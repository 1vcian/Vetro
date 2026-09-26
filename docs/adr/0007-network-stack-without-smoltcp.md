# ADR 0007 — Host-side network stack written in-house; smoltcp only in tests

- Status: accepted (M3 preparation, 2026-09-24)

## Context
The guest talks to virtio-net; on the other end of the cable we need a virtual
gateway that answers ARP/DHCP/ICMP and that **terminates** every TCP connection
and every UDP flow from the guest towards any destination (like
slirp/libslirp in QEMU), handing their data to a backend: the sinkhole
(everything fake and recorded) or the WebSocket relay (M7, because the browser
does not open sockets). Constraints: it compiles to `wasm32-unknown-unknown`, it
is deterministic (time only as a parameter, no unseeded randomness: needed for
the replay of M10), and its state will have to go into snapshots (M6).

The natural alternative is `smoltcp` (0BSD, no_std, compiles to WASM, mature).

## Decision
The stack is written in `crates/vetro-net`, with no dependencies. `smoltcp`
comes in only as a **dev-dependency**, and only its `wire` module, as an
independent parser that validates every frame produced in the tests (IPv4, TCP,
UDP, ICMP checksums; DHCP and DNS decoded by a different implementation).

Reasons for not using smoltcp in the crate:
- **Termination "at any address".** smoltcp is designed to be a
  host with its own addresses and listening sockets. To do slirp you have to
  intercept every SYN towards an arbitrary IP, create on the fly a socket
  listening on that 4-tuple before handing it the packet, use
  `any_ip` and fake routes, and separately handle a DHCP server (smoltcp has
  only the client), ARP for the gateway's addresses and UDP flows. You end up
  writing half the stack around it anyway.
- **Snapshot and replay.** The connection state (sequence numbers,
  buffers, timers) will have to be serialised into snapshots (M6) and replayed
  identically (M10). With our own structures it is a `derive`; smoltcp's
  sockets have private fields and borrowed buffers.
- **Event log and attribution.** The analysis engine wants exact
  events (new bytes per direction, never counted twice; open, close and
  reason; DNS queries and answers) with virtual time: simpler to
  emit them where the data originates.
- **Flow control towards an asynchronous upstream.** The upstream decides
  how many bytes to accept (`tcp_write` returns the number): the window
  advertised to the guest directly follows the relay's backpressure.

The TCP written here is deliberately small and compliant with RFC 9293: no
window scaling/SACK/timestamps (not advertised in the SYN-ACK, so the guest
does not use them), out-of-order segments dropped with a duplicate ACK,
immediate ACKs, RFC 6298 RTO in virtual time with Karn and doubling (cancelled
by an ACK of new data), go-back-N on timeout, fast retransmit, Reno, zero-window
probes, RFC 5961 challenge ACKs.

## Consequences
- No runtime dependencies: the WASM build does not change.
- TCP correctness is our responsibility: it is covered by the crate's
  tests (handshake, retransmissions, windows, FIN/RST, transfer in
  both directions over a cable that loses 20% of frames) and, from M3, the
  real Linux kernel as guest.
- Throughput limited by the 64 KiB window without scaling: on the
  virtual cable (RTT of microseconds) it is enough; if it is not, we measure it
  and add window scaling with an ADR.
- Dev-dependency: `smoltcp` 0.14 (0BSD) with `managed` (0BSD), `heapless`,
  `hash32`, `stable_deref_trait`, `bitflags`, `cfg-if` (MIT/Apache-2.0) and
  `byteorder` (Unlicense/MIT). None of them goes into the distributed binaries.
