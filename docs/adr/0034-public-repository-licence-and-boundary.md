# ADR 0034 — Licence of the public repository and the open/closed boundary

- Status: **Proposed — decision by the owner** (product track B1,
  2026-09-27). Nothing is published by this ADR; the export script
  (`tools/export/public.sh`) takes the licence as a parameter, so B1 does
  not wait for the decision.
- Number: on rebase onto main, renumber if 0034 is already taken (and
  update the rule in `tools/export/allow.txt` that names this file).
- Builds on ADR 0004 (PolyForm Noncommercial) and `docs/PLAN.md`,
  "Product track". A separate B1 ADR will define the plugin boundary in
  the core (how Pro code plugs into the machine and the web app); this ADR
  only fixes which paths are public and under which licence.

## Context
The owner's direction (PLAN, "Product track"): this repository becomes
private and carries the full product; a new public repository carries the
community edition, **generated** from this one with an allow-list and a
clean history, and takes contributions back under a CLA. The public
repository needs an OSI licence, which replaces PolyForm Noncommercial for
the public part only. Pro code (advanced analysis, server, infrastructure)
stays proprietary.

Our situation, which drives the choice:
- **The emulator runs in the user's browser.** Hosting the web app means
  sending its JavaScript and WebAssembly to every visitor: that is
  distribution of the program, not only "use over a network".
- **The likely competitors are hosted services**: a company that runs
  Vetro as an online "Android sandbox" or a mobile app analysis SaaS,
  possibly running `vetro-machine` headless on its servers and streaming
  the screen, or folding the emulator into a closed security product.
- **Adoption matters**: the community edition is the funnel for the paid
  plans; its users are security researchers, privacy auditors, students
  and app developers, who mostly *use* the tool rather than embed it.
- **Dual licensing**: the Enterprise plan foresees a commercial licence and
  a self-hosted build, and the Pro module is proprietary code that works
  with the public core.

## Licence: AGPL-3.0 or Apache-2.0

| Criterion | AGPL-3.0 | Apache-2.0 |
|---|---|---|
| Someone hosts the web app unchanged | allowed; the source is already public | allowed |
| Someone hosts a **modified** web app | must offer the modified source to its users (distribution of JS/WASM; GPL-3.0 alone would already require it) | may keep the changes closed |
| Someone runs a modified emulator **on servers** (streamed screen, analysis API) | must offer the source to the remote users (AGPL §13, the case GPL misses) | may keep the changes closed |
| Closed commercial product embedding the core | only with a commercial licence from us | allowed, no obligation beyond notices |
| Corporate adoption and contributors | lower: many companies forbid AGPL internally | highest |
| Individual researchers, students, auditors | no practical difference | no practical difference |
| Patent grant | yes (GPL-3.0 §11) | yes (§3), with retaliation |
| Compatibility with our dependencies (AOSP, microG, wasmtime, wasmparser: Apache-2.0; smoltcp: 0BSD, tests only) | yes (Apache-2.0 code can go into GPL-3.0/AGPL-3.0 works) | yes |
| Guest kernel (GPL-2.0-only) | unaffected: separate program in an aggregate image | unaffected |
| Changing later | can be relaxed to Apache-2.0 later (we hold all rights thanks to the CLA) | one-way: published versions stay Apache-2.0 forever |
| Value of a commercial licence (Enterprise) | high: it is the way out of copyleft | low: nothing to buy for the core |

Precedents: projects in our position (open core, hosted-service risk) chose
AGPL-3.0 — Grafana (2021), MinIO (2021), Element's Synapse (2023),
Plausible; Elastic added AGPL-3.0 as an option in 2024 after years of
non-OSI licences. Projects that started permissive and later feared cloud
competitors (Elastic, HashiCorp, Redis) had to leave OSI licensing, with
community forks as a result.

**Proposal: AGPL-3.0-only, with the CLA** (`docs/legal/CLA.md`) and the
commercial licence of the Enterprise plan for those who cannot accept
copyleft.
- It keeps the one thing Apache-2.0 would give away: a competitor's
  improvements to the emulator come back to the community, whether it
  distributes the app or runs it on servers.
- The cost in adoption falls mostly on companies that would embed the core
  in closed products — exactly the ones the commercial licence is for.
- It is reversible towards Apache-2.0; the opposite is not.
- `-only` rather than `-or-later`: future FSF versions are not accepted in
  advance; with the CLA the owner can still move to a later version.

What changes compared with ADR 0004: under any OSI licence **commercial use
of the community edition is allowed** (for example a consultancy using it
for paid audits). The noncommercial restriction survives only in the
private repository and in the Pro module's terms. The owner must accept
this explicitly; it is the price of being open source.

The CLA is not optional: without it, the owner could not ship the Pro
module and the Enterprise build on top of a core that contains
contributors' AGPL code under proprietary terms.

Rejected: GPL-3.0 (misses the server-side case); MPL-2.0 (file-level
copyleft, closed forks can wrap it); staying on PolyForm (not open source,
PLAN asks for OSI); a non-OSI "source available" licence such as BSL or
SSPL (same objection, and it repels the research community we want).

## Boundary

The rule of thumb: **what runs the machine is public; what gives meaning to
what the machine observes is private.** The allow-list
(`tools/export/allow.txt`) is the authoritative, path-level version of this
table; "transitional" marks private code that still has to be exported
because public code needs it to compile (next section).

| Area | Paths | Side |
|---|---|---|
| Core emulator | `crates/vetro-cpu`, `vetro-mmu`, `vetro-jit`, `vetro-jit-native` | public |
| Platform, machine | `crates/vetro-platform`, `crates/vetro-machine` (boot, devices, record & replay primitive, file manager, **hook points**: `Tracer`, breakpoints, syscall events, `GuestView`) | public |
| Snapshot | `crates/vetro-snapshot` | public |
| Network basics | `crates/vetro-net` (stack, gateway, sinkhole); from `vetro-analysis`: capture ring, packet, flow and DNS parsing, JSON writer, syscall names (the future `vetro-trace` crate) | public |
| Browser machine and CLI | `crates/vetro-wasm`, `crates/vetro-cli` (without their analysis modules) | public |
| Community web app | `web/app` (without the inspector/timeline panels), `web/node`, `web/src` | public |
| Guest images | `guest/` (kernel config, AOSP device, overlays, patches, the dev CA *certificate*), image build scripts in `tools/aosp`, `tools/guest-kernel`, `tools/guest-bins`, `tools/mkbootimg`, `tools/rootfs` | public |
| Test tooling | `tests/` (except Pro tests), `tools/oracle`, `tools/risu`, `tools/ltp`, `tools/a64asm.sh`, `tools/ci.sh`, `tools/web-test.sh`, `tools/wasm-boot.sh` | public |
| Docs | `docs/adr`, `docs/specs`, `docs/research`, `docs/assets`, `docs/legal/CLA.md`, `docs/legal/acceptable-use.md`, README | public |
| Advanced analysis | `vetro-analysis`: TLS plaintext, HTTP parsing and body decoders, inspector and views, HAR/pcapng/JSON reports, input→effects timeline | private |
| Introspection primitives | `vetro-analysis::introspect` (kernel profiles from kallsyms/BTF, Linux task and memory walking, Binder/Parcel/AIDL decoding, privacy classification, Binder strace); `vetro-machine`'s `analysis.rs`, `tls.rs`, `introspect.rs`; `tools/aosp/aidl-map.*` | private |
| Pro UI | `web/app/analysis.mjs` (inspector, timeline, replay jump-to-event) | private |
| Server | `api.vetro.lol` (not in the repository yet) | private |
| Infrastructure | `tools/remote`, `tools/aosp/upload*.sh`, `tools/pages`, `tools/catalog`, `tools/export`, `.github` (until the public CI of B3), `CLAUDE.md`, `.claude` | private |
| Plans and diaries | `docs/PLAN.md` (pricing, server), `docs/progress/`, `docs/legal/trademark.md`, this ADR | private |

Points for the owner to confirm:
1. **"Introspection primitives" is narrower than in PLAN.** PLAN says
   "capture, tracing hooks and introspection primitives stay open"; this
   ADR keeps the *hook points* in the open machine (they are part of the
   execution loop and of determinism) and makes the *introspection
   library* private. Alternative: open the Linux reader (process list,
   memory maps) as a Free feature and keep only Binder/TLS/ART private.
2. **pcapng export.** PLAN's Free plan has "basic network capture", the Pro
   plan "HAR/pcap/JSON reports". `pcapng.rs` only depends on the capture
   ring; making it public costs nothing technically and is what a
   researcher expects from "capture". Proposal: public.
3. **ADRs and specs of Pro features** (0016, 0023, 0027, 0029;
   `specs/analysis.md`, `specs/introspection.md`) are exported for now:
   design documents reveal little and show the project's rigour. They can
   be excluded by adding `-` rules.
4. **The private repository's own licence**: PolyForm Noncommercial on a
   private repository is harmless, but the Pro module handed to paying
   users needs terms of service / an EULA (phase B4).

Guest images stay under their own licences in every edition (GPL-2.0
kernel with published sources; AOSP and microG Apache-2.0). The dev CA's
private key never enters any repository (ADR 0030); the export has a deny
pattern for PEM private keys.

## Coupling: what prevents a strict split today

`tools/export/public.sh --strict` drops the transitional paths and stops on
any link from public to private code. Today it finds 17: 4 path
dependencies on `vetro-analysis` and 13 file-level links (the default,
non-strict export includes the transitional paths, builds and passes its
tests apart from a VM environment problem: see "Verification"):

| Public code | Needs (private) | Why |
|---|---|---|
| `vetro-machine/Cargo.toml` → `vetro-analysis` | `hooks.rs`: `CpuRegs`, `PhysMem` | two small traits/types the hook points expose |
| `vetro-machine/src/lib.rs` | `analysis.rs`, `tls.rs`, `introspect.rs` | Binder/TLS/syscall tracers and the kernel reader live in the machine crate |
| `vetro-wasm/Cargo.toml` → `vetro-analysis`; `lib.rs`, `replay.rs`, `analysis.rs` | `net::{Capture, NetworkAnalysis, view, har, pcapng}`, `timeline`, `json` | the browser API builds the inspector and the timeline inside the machine module |
| `vetro-cli/Cargo.toml` → `vetro-analysis`; `main.rs`, `linux/syscall.rs`, `analysis.rs`, `netcap.rs` | `syscall::{name, format}`, `net::*`, the machine's tracers | `--strace` names syscalls; `--pcap/--har` and the analysis options |
| `tests/boot/Cargo.toml` → `vetro-analysis` | `tests/analysis.rs`, `tests/introspect.rs` | Pro tests in a public test crate |
| `web/app/main.mjs`, `tests/web/unit.mjs` | `web/app/analysis.mjs` | static import of the panels and of their helpers' unit tests |

The module graph inside `vetro-analysis` makes the cut clean:
`capture`, `packet`, `flow`, `dns`, `json` and `pcapng` depend only on each
other, and `syscall` on nothing; the rest (`body`, `inflate`, `http`,
`inspector`, `har`, `view`, `tls`, `timeline`, `introspect`) sits above
them. `vetro-wasm` does not use the machine's TLS/Binder tracers at all.

Plan, in small steps that each keep `tools/ci.sh` green (no big refactor
now; to be done in B3, before the first real export):
1. **New public crate `vetro-trace`** with `syscall` and `net::{capture,
   packet, flow, dns, json, pcapng}` moved out of `vetro-analysis`
   (which re-exports them, so private users do not change).
2. **Hook types into the machine**: `CpuRegs` and `PhysMem` move to
   `vetro-machine::hooks` (or `vetro-trace`); `vetro-analysis` implements
   its readers over them. `vetro-machine` then no longer depends on
   `vetro-analysis`.
3. **Pro tracers out of the machine**: `analysis.rs`, `tls.rs`,
   `introspect.rs` move to a private crate (`vetro-pro`, depending on
   `vetro-machine` and `vetro-analysis`). They already implement the public
   `Tracer` trait, so this is a move; `Machine::linux` becomes an extension
   trait there.
4. **CLI**: `--strace` uses `vetro-trace`; `analysis.rs` and `netcap.rs`
   (pcap/HAR, TLS, Binder) move behind a private binary or a `pro` feature
   whose code lives in the private crate.
5. **Browser API**: `vetro-wasm` keeps the capture ring and exports raw
   frames and input records (`timeline::UserInput` moves next to
   `record.rs`); the inspector, views, HAR and timeline move to the Pro
   build. How the Pro build is delivered (a second WebAssembly module or a
   full `vetro-wasm` build with Pro features served after login, which
   avoids cross-module calls in the hot loop) is the plugin boundary ADR.
6. **Web app**: `main.mjs` loads `analysis.mjs` with a dynamic `import()`
   only when the Pro module is available; the helper tests move to a
   private `tests/web/unit-pro.mjs`.
7. **Pro tests** (`tests/boot/tests/{analysis,introspect}.rs`,
   `vetro-cli/tests/boot_pcap_har.rs`, `tests/web/{inspector,
   browser-analysis}.mjs`, `tools/analysis`) move with their code.
8. **Gate**: once the strict report is empty, the private CI runs
   `tools/export/public.sh --strict --check` so that no new coupling can
   land.

Other findings of the export, not blocking the build:
- 48 exported files mention private paths (`docs/progress`, `docs/PLAN.md`,
  `CLAUDE.md`, `tools/remote`, `tools/pages`, upload scripts...), mostly in
  comments, ADRs and the README. The README needs a public rewrite in B3
  (it describes the GitHub Pages site and the private CI).
- `tools/aosp/common.sh` had the build VM's address as a built-in default;
  it now reads it only from `VETRO_AOSP_HOST` or `target/aosp/vm-host`
  (also from the main checkout of a worktree), and the deny pattern keeps
  the address out of the export.
- ADRs 0001, 0004 and 0022 still mention PolyForm: they are history and
  stay as they are; the public README and NOTICE say what applies.

## Consequences
- B1 is unblocked: the export runs with either licence, and the owner's
  decision is one parameter.
- The CLA and the acceptable use policy (`docs/legal/`) are drafts to be
  checked by a lawyer before the first public contribution.
- On acceptance: ADR 0004 gets "Superseded for the public repository by
  ADR 0034"; CLAUDE.md "Licensing" gets one line about the two
  repositories.

## Verification
- `tools/export/public.sh --license AGPL-3.0-only --check` on the build VM
  (`tools/remote/test.sh`): export, licence switch, coupling report, then in
  the exported tree `cargo fmt --check`, `cargo build --workspace
  --all-targets`, `cargo test --workspace` with the oracle,
  the wasm build of CI and the web unit tests. Result (2026-09-27, details in
  `docs/progress/product.md`): everything green (675 tests, 0 SKIP, 25 web
  unit tests) except 14 `vetro-linux-tests --test busybox` cases, which fail
  identically in the unexported tree on the VM: the VM lacks binfmt_misc
  for qemu-aarch64, so the oracle runs busybox applet symlinks through the
  host shell. An environment problem of the VM, not of the export.
- `tools/export/public.sh --license AGPL-3.0-only --strict` fails today
  with the 17 links above: that is the expected state until the plan is
  done.
