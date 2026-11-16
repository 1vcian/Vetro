<p align="center"><img src="docs/assets/vetro-icon.png" alt="Vetro" width="160"></p>

# Vetro

**A complete arm64 Android system, emulated inside a browser tab — with an
analysis layer that sees everything apps do, from the outside.**

No installation, no root, no server. Vetro runs an unmodified Android Open
Source Project build (with microG instead of Google services) on an ARM64 CPU
emulator written in Rust and compiled to WebAssembly. Because all
observation happens in the emulator, apps have nothing injected into them to
detect.

**Try it:** [1vcian.me/Vetro](https://1vcian.me/Vetro/) starts the phone in
your browser (Chrome or Edge on a desktop): the project's own AOSP 15 image
with microG, from a prebuilt snapshot. A small Linux guest with a BusyBox
shell is there too, as a [quick demo](https://1vcian.me/Vetro/app/?os=linux).

**User guide:** [docs/user](docs/user/README.md) (also on the site, under
[docs/](https://1vcian.me/Vetro/docs/)): getting started, using the phone,
internet with Vetro Connect, the network inspector and timeline, the file
manager, record and replay, snapshots, device profiles, troubleshooting,
privacy and FAQ.

**Support:** Vetro has no ads and no telemetry. If it is useful to you, you
can [sponsor it on GitHub](https://github.com/sponsors/1vcian) or
[buy me a coffee](https://buymeacoffee.com/1vcian).

> Vetro is an independent project. It is not affiliated with or endorsed by
> Google. Android is a trademark of Google LLC.

## Guiding principle

**Fidelity before speed.** An app must execute exactly its own code, even at
10 fps. Optimisation only comes after behaviour is correct and verified
against a reference (QEMU).

## Goals for 1.0

- Open a URL in desktop Chrome or Edge and reach the Android home screen in
  under 15 seconds, starting from a cached snapshot.
- Drag an APK onto the page: it installs and is usable with mouse and
  keyboard (simulated touch).
- A reference set of 30 apps (20 from F-Droid, 10 popular apps that do not
  require Play Integrity) completes the basic flows: launch, login,
  navigation.
- Every network request is visible in clear text, attributed to process and
  library, and linked on a timeline to the user action that caused it.
- Every access to sensitive data (location, contacts, clipboard,
  identifiers, accounts) is recorded by decoding Binder transactions.
- Sessions export to HAR, pcap and JSON.

**Out of scope for 1.0:** Play Store, original Google Play Services and Play
Integrity; heavy 3D games at playable frame rates; iPhone and mobile Safari;
any mandatory cloud component. The only optional external piece is a network
relay for users who want real server responses.

## How it works

```mermaid
flowchart TD
  G["Guest<br/>Linux kernel + AOSP 15 + microG"]
  C["ARM64 CPU<br/>interpreter + JIT → WASM"]
  D["Virtual devices<br/>GICv3, timer, virtio"]
  H["Host services<br/>screen, input, disk, network"]
  A["Analysis engine<br/>hooks, decoders, traces"]
  U["UI<br/>timeline and inspectors"]
  G --> C --> D --> H --> U
  C --> A
  D --> A
  A --> U
```

- **CPU:** AArch64 only (EL0/EL1), a reference interpreter plus a block JIT
  that emits WebAssembly modules; one Web Worker per virtual core.
- **Platform:** a replica of QEMU's `virt` board (GICv3, generic timer,
  PL011, PL031, virtio-mmio) with a generated device tree.
- **Guest:** GKI kernel and AOSP 15 (API 35) arm64, built from source as
  `userdebug` with full symbols.
- **Host:** virtio-gpu to a canvas, with the guest's OpenGL ES drawn by the
  browser's GPU (gfxstream to WebGL, ADR 0047; SwiftShader in the guest as
  the fallback), virtio-input from pointer events, virtio-blk over HTTP
  Range with an OPFS cache, virtio-net into a WASM TCP/IP stack (a sinkhole,
  or real internet through Vetro Connect, a small gateway on your own
  computer, ADR 0050), adb over a virtio channel.
- **Analysis:** guest-address hooks inserted by the JIT (invisible to apps);
  decoders for syscalls, Binder, TLS (BoringSSL/Conscrypt) and ART.

First target browsers are desktop Chrome and Edge (SharedArrayBuffer with
COOP/COEP, WebGPU), then Firefox.

## Roadmap and status

Each milestone has an exit criterion checked by a command; a milestone is
done only when that command passes in CI.

| | Milestone | Exit criterion | Status |
|---|---|---|---|
| M0 | Scaffolding and oracle | `cargo test` green; a test runs QEMU and reads its output in CI | ✅ done |
| M1 | AArch64 CPU interpreter | per-instruction suite; ≥200 random programs match `qemu-aarch64` | ✅ done |
| M2 | Linux user-mode syscalls | LTP subset, static BusyBox runs | ✅ done |
| M3 | System mode, kernel boot | kernel + initramfs reaches a shell; kselftest subset | ✅ done |
| M4 | JIT to WebAssembly | M1/M2 tests pass with JIT; interpreter/JIT parity | ✅ done |
| M5 | Android boots | home screen in Chrome; `adb install` of an APK works | ✅ done |
| M6 | Snapshots and install | home in < 15 s from snapshot; drag-and-drop APK | ✅ done |
| M7 | Network analysis and timeline | HTTPS in clear text, linked to user actions; HAR export | 🚧 in progress |
| M8 | Binder and privacy | every sensitive access reported; decoy data tracked to the network | 🚧 in progress |
| M9 | Code tracing and scripting | ART method hooks, dynamic dex capture, Frida-like API | — |
| M10 | Record & replay, 1.0 | deterministic replay; all 30 reference apps; 1.0 goals met | 🚧 in progress |

**Where we are.** Vetro runs unmodified Linux arm64 programs in user mode.
It has the complete ARMv8.0 integer, SIMD/FP and crypto instruction set of a
Cortex-A53, and an emulated Linux kernel with processes, threads, signals,
virtual time and `/proc`. These all match `qemu-aarch64` in CI:
- 355 LTP syscall tests;
- the main BusyBox applets;
- the RISU instruction tests.

Tests where QEMU itself deviates from Linux are checked against the real
kernel on an arm64 host (ADR 0010).

It also runs as a complete machine (`vetro-machine`): a CPU at EL0/EL1,
stage-1 MMU, GICv3, generic timer, PL011, PL031 and virtio-mmio. It boots
Linux 6.18 to an interactive shell, with a boot log identical to
`qemu-system-aarch64`. Its 95 in-guest kernel selftests have the same
outcomes as under QEMU. Everything is deterministic: guest time is the
instruction count, so a boot is exactly 98,439,742 instructions, about 2 s on
an M2.

A block JIT translates guest code to WebAssembly. In V8 it boots the
kernel faster than the native interpreter (3.9 s against 4.4 s on the CI
runner), with an identical log and instruction count. Every test runs with
both the interpreter and the JIT, and the two must agree (ADR 0012, 0013).

Android runs in the browser (M5, M6). The project's AOSP 15 image reaches
the home screen in Chrome from a prebuilt snapshot; an APK dropped on the
page installs and opens; a second start from the snapshot saved in the
browser takes under 15 seconds. The phone is online when Vetro Connect runs
on the same computer.

In progress: network analysis on Android (M7), Binder and privacy (M8),
record and replay with the 30 reference apps (M10). It is usable and slow:
an app takes several seconds to open.

Detailed plan and progress log: [`docs/PLAN.md`](docs/PLAN.md),
[`docs/progress/`](docs/progress/), architecture decisions in
[`docs/adr/`](docs/adr/).

## Building and testing

Requires `rustup` (the pinned nightly toolchain is installed automatically
from `rust-toolchain.toml`).

```sh
tools/ci.sh    # fmt, clippy, native tests, wasm32 build — same as CI
```

The differential tests compare Vetro against `qemu-aarch64` (QEMU user mode,
Linux only):

- **Linux:** `sudo apt-get install qemu-user`
- **macOS:** start Docker, then
  `export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"`

Without an oracle those tests print `SKIP`; set `VETRO_REQUIRE_ORACLE=1` to
make a missing oracle a failure (always on in CI).

The GitHub Pages site is built by `tools/pages/build.sh` (web app, WebAssembly
machine, guest kernel and the exact GPL sources of the kernel and BusyBox)
and checked in Chrome with `node tests/web/pages.mjs`; CI publishes it from
`main` when every other job is green.

## Repository layout

```
crates/   vetro-cpu, vetro-mmu, vetro-jit, vetro-platform, vetro-net,
          vetro-analysis, vetro-snapshot, vetro-wasm, vetro-cli
web/      browser app (workers, host services, UI)
guest/    kernel config, AOSP device target, image packaging
relay/    network gateway for real internet (Vetro Connect, Python)
tests/    per-instruction, differential (vs QEMU), boot, apps, e2e
tools/    CI script, QEMU oracle wrapper, benchmarks
docs/     plan, ADRs, component specs, progress log
```

## Support

Vetro is free for noncommercial use, with no ads and no telemetry. Bug
reports and test results help. If you want to support its development, you
can do it through [GitHub Sponsors](https://github.com/sponsors/1vcian) or
[buy me a coffee](https://buymeacoffee.com/1vcian). It is entirely optional.

## License

Vetro is **source-available**, not open source: it is licensed under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md). You may use, modify and
share it for noncommercial purposes; **commercial use is not permitted**
without a separate written agreement. See [`NOTICE`](NOTICE).

Third-party components keep their own licenses — notably the Linux kernel
(GPL-2.0), whose exact sources are published with every distributed guest
image.
