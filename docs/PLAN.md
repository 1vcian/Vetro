# Vetro — plan and status

Source: original plan plan.docx (2026-09-24, not versioned). This file is the working copy:
milestones are marked complete **only** when the exit command passes in CI.

## Vision in brief

Android 15 (API 35) arm64 from AOSP, with microG, emulated in the browser by our own
ARM64 CPU (interpreter + JIT to WASM), on a platform that copies QEMU's
"virt" board. Analysis (syscalls, Binder, TLS, ART) happens from the emulator,
invisible to apps. First target: Chrome/Edge desktop.

### 1.0 success criteria

- Home screen in under 15 s from a cached snapshot, on Chrome or Edge desktop.
- Dropped APK: it installs and is usable with mouse and keyboard.
- Reference set of 30 apps (20 F-Droid, 10 well-known ones without Play Integrity):
  launch, login, navigation.
- Every network request in clear text, attributed to process and library, tied
  on the timeline to the user action.
- Every access to sensitive data recorded by decoding Binder.
- Export to HAR, pcap and JSON.

### Out of scope for 1.0

Play Store / original GMS / Play Integrity; heavy 3D games; iPhone and
mobile Safari; any mandatory cloud component.

## Status

| Milestone | Status | Exit command |
|---|---|---|
| M0 Scaffolding and oracle | **completed** (CI green, 2026-09-24) | `VETRO_REQUIRE_ORACLE=1 tools/ci.sh` |
| M1 AArch64 CPU interpreter | **completed** (CI green, 2026-09-24) | per-instruction tests + 200 random programs identical to qemu-aarch64 (ADR 0006) |
| M2 Linux user syscalls | **completed** (CI green, 2026-09-25) | 355 LTP tests, BusyBox and RISU identical to the oracle in the linux job (ADR 0010) |
| M3 System mode and kernel | **completed** (CI green, 2026-09-25) | Linux 6.18 up to the shell with a log identical to QEMU; 95 kselftests with identical outcomes (ADR 0011) |
| M4 JIT to WASM | **completed** (CI green, 2026-09-26) | M1/M2 tests with the JIT, interpreter-JIT parity, boot with the JIT in V8 3.85 s ≤ native interpreter 4.36 s (ADR 0012, 0013) |
| M5 Android boot | **in progress** (our AOSP 15 image, ADR 0022; in Chrome, ADR 0028: home screen drawn 44.5 min after a cold start with 2 GiB, adb from the page, a dropped APK installed, opened and reacting to a click; a first visit resumes from the prebuilt snapshot on R2 instead of the 45-minute boot, ADR 0031; the Android criteria run in the nightly CI job from the prebuilt snapshot, the cold boot weekly: waiting for the first green run) | home in Chrome, adb install di un APK |
| M6 Snapshots and installation | **in progress** (full machine save/restore, ADR 0015; persistent disks and cached snapshots in the browser, ADR 0017; Android: chunked snapshots, second start in Chrome ready in 4.8–5.0 s, ADR 0028; snapshot format 4 with a small level and the prebuilt home-screen snapshot, ADR 0031; nightly CI job added, waiting for the first green run) | home < 15 s da snapshot, APK trascinato |
| M7 Network and timeline | **in progress** (capture, pcapng, HTTP, decoders, HAR on the BusyBox guest, ADR 0016; network inspector and input→effects timeline in the web app on the Linux guest, ADR 0023; guest introspection from the outside, ADR 0027; TLS hooks and Binder+privacy decoder from the outside, ADR 0029; missing: the TLS endpoint and the test on Android from a snapshot) | 10 apps with HTTPS in clear text and tied to the action, HAR reopenable |
| M8 Binder and privacy | **in progress** (file manager on the Linux guest: daemon over vsock, client, web panel, ADR 0020; editing SQLite rows with SQL in the guest, WAL, SharedPreferences as a table, non-UTF-8 names, ADR 0021) | test app: every access detected, decoy identifier tracked, app files visible and editable live |
| M9 Code tracing and scripting | — | method hook, dynamic dex saved, adapted Frida script |
| M10 Record & replay, 1.0 | **in progress** (core: record & replay and jump to a machine instruction, ADR 0019; in the browser: record, downloadable and reloadable log, keyframes in OPFS, identical replay, jump with registers and memory, ADR 0023) | identical replay, 30 apps, 1.0 criteria |

## Milestones

### M0 — Scaffolding and oracle
- **Goal:** repository, CI and comparison with QEMU working from the start.
- **Deliverables:** repo structure, CLAUDE.md, native and WASM CI, QEMU and RISU
  callable from tests.
- **Exit:** `cargo test` green; a test that launches QEMU and reads its output
  passes in CI.

### M1 — AArch64 CPU interpreter
- **Goal:** run user ARM64 code, slow but correct.
- **Deliverables:** decoder and interpreter for integers, memory, branches, conditions;
  static ELF loader; the CLI runner executes a binary.
- **Exit:** per-instruction suite green; on at least 200 random RISU programs
  the final registers match qemu-aarch64.

### M2 — Linux user syscalls
- **Goal:** real arm64 Linux programs in user mode.
- **Deliverables:** SVC handling, basic syscalls (files, memory, threads, clock)
  mapped onto the host; first syscall tracer.
- **Exit:** user mode LTP selection green; static arm64 BusyBox runs the
  main commands; correct syscall log.

### M3 — System mode and kernel boot
- **Goal:** arm64 Linux kernel up to the shell.
- **Deliverables:** EL0/EL1, stage 1 MMU, GICv3, timer, UART, minimal virtio-blk and
  virtio-net, device tree, kernel loader.
- **Exit:** kernel with initramfs to the shell with a scripted boot; kselftest
  selection green in the guest.

### M4 — JIT to WASM
- **Goal:** workable performance.
- **Deliverables:** block translation into WASM modules, cache, invalidation on
  modified code, fallback to the interpreter in hard cases.
- **Exit:** M1 and M2 tests green with the JIT; kernel boot under the threshold set
  in M3; no interpreter-JIT difference on the differential set.

### M5 — Android boot
- **Goal:** Android home screen in the browser.
- **Deliverables:** AOSP 15 arm64 images; block disk via HTTP Range with
  OPFS cache; virtio-gpu 2D on WebGPU; virtio-input; adb over a virtio channel;
  network sinkhole.
- **Exit:** home in Chrome; adb sees the device; adb install of a simple
  APK, the app opens and reacts to touch.

### M6 — Snapshots and installation
- **Deliverables:** full save/restore; snapshot of booted Android;
  APK drag and drop; copy-on-write layer.
- **Exit:** from the second boot, home < 15 s; a dropped APK installs and
  opens without a command line.

### M7 — Network analysis and timeline
- **Deliverables:** TLS hooks on BoringSSL and Conscrypt; JSON, protobuf and
  form decoding; network inspector; input→effects timeline; HAR and pcap export.
- **Exit:** on 10 apps from the set every HTTPS call is in clear text and tied
  to the action; the HAR reopens in another tool.

### M8 — Binder and privacy
- **Deliverables:** Binder decoder with AIDL mapping; privacy inspector; decoy
  data tracked all the way to the network; file manager for the foreground app.
- **File manager:** panel next to the screen that follows the foreground
  app (detected by the Binder decoder on ActivityTaskManager) and shows
  its file tree: `/sdcard/Android/data/<package>`,
  `/sdcard/Android/media/<package>` and the private data
  `/data/data/<package>` (`/data/user/0`, `/data/user_de/0`). Live
  updates when the app creates or changes files; opening with viewers for
  text, JSON, SharedPreferences XML, SQLite (tables), images and
  hex; live editing with immediate save in the guest.
  Constraints: reads and writes go through the guest kernel (a Vetro daemon on
  virtio-vsock with the root privileges of the userdebug image, not direct
  access to the ext4/f2fs image, which with the guest running would corrupt the
  file system); the file's owner, permissions and SELinux context are
  preserved; every user edit is an input recorded at the single
  point of M10, so replay stays identical. The mechanism becomes an ADR
  before the code.
- **Exit:** on our own test app every expected access shows up; a
  decoy identifier is detected when it goes out on the network; the file manager shows the
  files the test app writes within 1 s, and an edit made from the panel
  (a value in the SharedPreferences and a row of a SQLite database) is
  read by the app after the activity restarts.

### M9 — Code tracing and scripting
- **Deliverables:** ART introspection from the emulator; native tracing with
  symbols; detection of dynamic dex/libraries; Frida-like scripting API.
- **Exit:** a script hooks a method and records the calls; a dex
  loaded at runtime is saved; an example Frida script runs with
  minimal changes.

### M10 — Record & replay and 1.0
- **Deliverables:** deterministic recording and replay; jump to an event with
  the memory and registers of that moment; device profiles; reports; user docs.
- **Exit:** identical replay with return to the exact moment of a call;
  30 apps pass the basic flows; 1.0 criteria met.

## Suggested apps (owner's list, 2026-09-27; delivered through the in-page catalog)

Apps offered to users (as APKs with pinned versions and SHA-256, like
microG), each with its licence recorded
and, where required, its source published next to the image:

| App | Source | Licence | Notes |
|---|---|---|---|
| **Jenny** (local-first AI agent) | https://github.com/flagdizero/jenny-android-ai-agent (release APK, v0.11.0 as of 2026-09-27) | AGPL-3.0 | We redistribute it unmodified: publish the exact source tag with the image. It may want network access and an API key or a local model; check it runs in the emulator (CPU features, 64-bit only, RAM). |
| **Chromium** (latest stable) | official Chromium builds for Android arm64 (e.g. the Chromium snapshot `ChromePublic.apk`), or a trusted open build | BSD-3 plus third-party notices | Large APK and heavy at runtime (its own JIT): measure the effect on image size, snapshot size and speed. Name it Chromium, never Chrome. Keep the version current with each image release. |
| **A couple of games** | to be chosen later; prefer open-source games (F-Droid) with redistributable licences and 64-bit arm64 builds | per game | Pick light 2D games that run well with software rendering (SwiftShader) and one CPU. |

Rules: only apps whose licence allows redistribution; pinned hashes; no
Google apps or trademarks; each addition is measured (image size, snapshot
size, boot time) and gets a short note in `docs/progress/M5.md`. The list is
built into the next AOSP rebuild (the VM is started by the owner when a
batch is ready).


**Update (2026-09-27): in-page app catalog instead of preinstalling
everything.** One base snapshot for everyone (small, fast to download);
the web app shows a catalog of suggested apps with an **Install** button
that downloads the APK and installs it into the running guest through the
in-page ADB client (the same path as drag and drop), with a progress bar.
- APKs mirrored on our R2 (the origins don't send CORS headers), pinned
  versions and SHA-256, verified before install.
- Catalog = a JSON file on R2 (name, icon, description, size, licence,
  source link, minimum image version), updatable without touching the site
  or the snapshot.
- Installed apps persist in the user's disk overlay and local snapshot
  (OPFS).
- Only redistributable apps; AGPL/GPL apps show the source link in the card.
- Nothing is preinstalled beyond the system components (microG). Jenny,
  Chromium and games are all offered through the catalog ("advanced" for
  heavy ones). The table above lists the catalog's first entries.
- Plans: Free gets the basic catalog; Pro gets curated analysis sets.

**Done (2026-09-27, ADR 0033):** the Apps panel in the Android mode and
`tools/catalog/add.mjs` (fetch from the official origin, verify, read
package/version/icon, upload to R2, update `catalog/v1.json`). First
entries on R2: Jenny 0.11.0 (AGPL-3.0-only, unmodified release, source tag
linked; its trademark policy allows the name for unmodified builds, never
implying endorsement), Flowit 4.3 (GPL-3.0-only) and Minesweeper 1.2.3
(Privacy Friendly, GPL-3.0-or-later) from F-Droid with the exact source
tarball linked, and Chromium 155.0.8059.0 (BSD-3-Clause, snapshot 1697569
at the 155 stable branch point, 364 MiB, "advanced").

## Product track: open core, server and plans (decided 2026-09-26)

The owner's direction: this repository becomes **private** (the full
product), and a new **public open-source repository** carries the community
edition. A server provides accounts, plans and the paid features. Domain:
**vetro.lol** (the owner is buying it). Everything is in English.

### Repositories

| Repository | Visibility | Contents |
|---|---|---|
| `vetro` (this one, renamed e.g. `vetro-pro`) | private | everything: core, Pro analysis modules, server, infrastructure, AOSP build |
| `vetro` (new) | public, open source | core emulator (CPU, MMU, JIT, platform, machine, net, snapshot), community web app, Android image build scripts, docs |

- The public repository is **generated from the private one** (an export
  script with an allow-list of paths and a clean history), not maintained by
  hand, so the two never drift. Public contributions come back through the
  same script, under a CLA.
- Licence of the public repo: an OSI licence (to be decided by ADR:
  **AGPL-3.0** protects against someone hosting it as a competing service;
  Apache-2.0 maximises adoption). Pro code stays proprietary.
- GPL (kernel) and Apache (AOSP, microG) obligations apply to every edition.

### Architecture split (needs an ADR before code)

- A **plugin boundary** in the core: capture, tracing hooks and introspection
  primitives stay open; the high-level Pro analyses (TLS plaintext, Binder
  decoding and privacy inspector, ART introspection, scripting, replay
  jump-to-event UI, reports) become separate crates and a separate WebAssembly
  module.
- The Pro module is **downloaded after login**, served by the server with a
  signed, expiring licence token. The client can be cracked like any
  client-side code, so the real value sits in the service (below), and
  licence terms cover the rest.

### Server (vetro.lol)

| Host | Role |
|---|---|
| `vetro.lol` | landing page, pricing, docs |
| `app.vetro.lol` | the web app (with COOP/COEP headers, which also unlocks WASM threads/multi-core) |
| `api.vetro.lol` | accounts, auth, plans, licence tokens, sessions, team sharing, reports |
| `assets.vetro.lol` | images, prebuilt snapshots and Pro modules (Cloudflare R2 behind a custom domain) |

- Suggested stack, cheap and close to what we already use: Cloudflare (DNS,
  Pages for the site and app with custom headers, Workers + D1/Durable Objects
  for the API, R2 for assets). Alternative: a small VPS for the API.
- Payments through a merchant of record (Paddle or Lemon Squeezy) so EU VAT
  is handled for us; Stripe if we prefer to handle VAT ourselves.
- Auth: email magic link plus GitHub/Google login; teams with roles.
- Privacy by design: emulation and analysis stay in the user's browser;
  the server stores only what the user explicitly saves or shares.

### Plans (starting hypothesis, to validate with pricing research)

| Plan | Price (hypothesis) | For whom | Includes |
|---|---|---|---|
| **Free** | €0 | curious users, students | Vetro in the browser with our Android image, APK drag and drop, basic network capture, file manager, local snapshots |
| **Pro** | ~€15/month (≈€150/year) | security researchers, privacy auditors, app developers | HTTPS in clear text, Binder/privacy inspector, ART and scripting, replay with jump-to-event, HAR/pcap/JSON reports, cloud-saved sessions, always-updated images and prebuilt snapshots |
| **Team** | ~€40/user/month | agencies, QA and security teams | Pro + shared sessions and reports, team workspace, device profiles, curated app sets, priority support |
| **Enterprise** | on request | companies, regulated environments | self-hosted/on-premise build, custom images and profiles, SSO, SLA, commercial licence |

Possible extras: an education discount, a free Pro trial, pay-per-report for
one-off audits.

### Phases

1. **B1 — Preparation (now, no user-visible change):** licence ADR, plugin
   boundary ADR, export script for the public repo, trademark search for
   "Vetro", acceptable use policy, CLA text.
2. **B2 — Domain and hosting:** vetro.lol on Cloudflare DNS; site and app on
   Cloudflare Pages with COOP/COEP; R2 on `assets.vetro.lol`. GitHub Pages is
   retired (it's only free for public repositories).
3. **B3 — Public repository:** first export, CI on public runners, README
   and docs for contributors.
4. **B4 — Server and plans:** API (accounts, teams, licence tokens), payment
   provider, Pro module delivery, cloud sessions.
5. **B5 — Launch:** landing page, pricing page, docs, launch after M8 (Pro
   features working on real apps).

### Before making this repository private

- **CI cost:** GitHub Actions is free and unlimited only for public
  repositories. Private repositories get a monthly minute quota, and the
  arm64 runners used by the `linux` and `boot` jobs are billed. Options: keep
  the heavy CI on the public repository (the core lives there), self-hosted
  runners (for example the build VM), or a paid plan.
- **GitHub Pages** needs a paid plan on private repositories: move the site
  to Cloudflare Pages first (phase B2).
- R2 artifacts and the build VM are unaffected.

## Agent team

| Agent | Folders | Active in |
|---|---|---|
| Architect | `docs/`, interfaces between crates | all |
| CPU | `crates/vetro-cpu`, `crates/vetro-mmu`, `tests/isa` | M1–M4 |
| JIT | `crates/vetro-jit`, `tests/diff` | M4, then maintenance |
| Platform | `crates/vetro-platform`, `guest/kernel`, `tests/boot` | M3, M5 |
| Guest | `guest/aosp`, `guest/image` | M5–M6 |
| Network | `crates/vetro-net`, `relay/` | M3, M7 |
| Analysis | `crates/vetro-analysis`, `crates/vetro-snapshot` | M2, M7–M10 |
| Web | `web/` | M5–M10 |
| Quality | `tests/`, `.github/`, `tools/` | all |

Up to M4: a single agent per session. Parallelism from M5.

## Main risks

JIT with subtle bugs (parity mandatory); AOSP boot blocked by missing
devices (BusyBox → kernel → Android); performance (JIT early, snapshots);
browser memory (memory64, configurable RAM); fragile AOSP build
(dedicated machine, versioned artifacts); emulator detection
(credible profiles, invisible introspection); proprietary TLS (search by
signatures); legal issues (AOSP + microG only, kernel sources published).
Full detail in the original plan (plan.docx, not versioned).
