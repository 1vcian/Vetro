# Vetro — rules for all agents

Vetro is a complete ARM64 system emulator written in Rust. It runs in
WebAssembly in the browser and runs Android (AOSP + microG), with analysis
tools that observe it from the outside. The plan, milestones and status live
in `docs/PLAN.md`; the work log lives in `docs/progress/`. Read them at the
start of every session.

"Vetro" is the official name. The product never presents itself as "Android"
(a Google trademark).

## Language

**Everything is in English**: code comments, commit messages, docs (ADRs,
specs, progress logs, plan), UI strings, test output and messages. Existing
Italian text is being translated; any file you touch should end up in English,
and all new text is written in English from the start.

## Principles

- **Fidelity before speed.** Correct and verified first, fast second.
- **QEMU is the oracle.** Every CPU behaviour is compared against
  `qemu-aarch64` / `qemu-system-aarch64`.
- **Determinism from day one.** Clock, randomness, input and device timing
  go through a single recordable point (needed for M10 replay).

## Golden rule

No milestone is complete until its exit command passes in CI (in a full run:
the nightly one, or a commit with `[ci full]`, ADR 0025). Don't declare done
what a test doesn't confirm. A skipped test (`SKIP`) is not a passed test. If
a test is red, keep working until it is green or until the reason is
understood and written down in `docs/progress/`.

## Commands

```sh
tools/ci.sh                          # all CI checks locally
cargo test --workspace               # native tests
cargo test -p vetro-diff             # QEMU oracle and random programs (ADR 0006)
cargo test -p vetro-isa-tests        # per-instruction tests (also against QEMU)
VETRO_DIFF_SEED=<seed> VETRO_DIFF_CASES=1 cargo test -p vetro-diff --test random   # reproduce a case
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --target wasm32-unknown-unknown --workspace --exclude vetro-cli --exclude vetro-diff --exclude vetro-isa-tests --exclude vetro-linux-tests --exclude vetro-jit-native
VETRO_JIT=1 cargo test --release -p vetro-linux-tests   # same tests with the JIT (ADR 0012)
VETRO_JIT=1 cargo test --release -p vetro-boot-tests --test vetro   # kernel boot with the system JIT (ADR 0013)
tools/wasm-boot.sh --jit             # boot in Node: interpreter and JIT in V8, M4 threshold
tools/pages/build.sh && node tests/web/pages.mjs   # GitHub Pages site and its Chrome test
```

Oracle on macOS (QEMU user mode only exists on Linux; Docker must be running):

```sh
export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"
```

Encodings for tests: `printf 'add x0, x1, #1\n' | tools/a64asm.sh` (a real
assembler; it rejects relocations). Never hand-write encodings in tests.

`VETRO_REQUIRE_ORACLE=1` turns an oracle skip into a failure (on in CI). Use
it locally too before declaring work finished.

## Toolchain

Rust nightly pinned in `rust-toolchain.toml` (WASM threads need build-std,
see `docs/adr/0002`). Don't change it without an ADR.

## Non-negotiable rules

- Every bug fix lands with a test that would fail without the fix.
- Every new instruction or syscall comes with its targeted or differential
  test.
- A single difference from QEMU in the differential set blocks the release.
- Performance is measured, not estimated.
- **Never rebuild AOSP in CI.** Images are built on the dedicated Linux
  machine and uploaded as versioned artifacts.

## Boundaries and collaboration

- Every area has an owner (see `.claude/agents/` and `docs/PLAN.md`, section
  "Agent team"). An agent writes only in its own folders.
- Interfaces between crates live in `docs/specs/`. Changing them is an
  architectural decision: ADR in `docs/adr/` first, then code.
- Non-obvious decisions become a numbered ADR, so they aren't reopened.
- Branches `area/description` (e.g. `cpu/decoder-simd`). One PR per unit of
  work, always with the test that proves the criterion.
- Commits cite the milestone, e.g. `M1: ADD/SUB immediate decoder`.

## End of session

Add three lines to `docs/progress/Mx.md`: done, missing, blocked.

## Licensing

Our code: PolyForm Noncommercial 1.0.0 (`LICENSE.md`, see ADR 0004): no
commercial use by third parties; the owner keeps the right to offer
commercial licences (see "Product track" in `docs/PLAN.md`). The Linux
kernel is GPL-2.0: for every distributed image the sources of the kernel used
are published too.
