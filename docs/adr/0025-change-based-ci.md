# ADR 0025 — Selective CI based on changed files

Status: accepted (2026-09-26)

## Context
The full CI run takes about 50 minutes: full LTP twice (interpreter and
JIT) on the arm64 runner, kernel boot under QEMU and under Vetro,
kselftest (about 20 minutes on their own), web tests and site. A change to
a JavaScript file or to a document touches nothing of what LTP or the
kselftests verify, but it redid them all; the site publication also waited
for those 50 minutes.

## Decision
A `changes` job (tools/ci-changes.sh) compares the push with the previous
commit (or the PR with its base) and decides which jobs are needed:
- `native`, `wasm`: Rust code, Rust tests, toolchain;
- `linux`: CPU, MMU, JIT, vetro-cli (Linux syscalls), linux/diff/isa tests,
  LTP, RISU, guest binaries, oracle;
- `boot`: any crate, tests/boot, guest kernel and its tools;
  the kselftests only if the guest kernel changes (configuration, initramfs,
  kselftest, build scripts);
- reduced `boot` job (kernel from cache, WebAssembly in Node, web tests) if
  only the web part changes;
- `pages`/`deploy`: what ends up in the site; it is published if no job
  failed (skipped ones do not block).
Everything runs anyway: every night (schedule), by hand (workflow_dispatch), on
the first push of a branch, if the CI itself changes, or with `[ci full]` in
the commit message.

## Consequences
- The golden rule stands: a milestone is closed on a green full run
  (the nightly one or one with `[ci full]`), not on a partial one.
- The risk is an unforeseen dependency between areas: the nightly run
  finds it within a day. The categories are deliberately broad (for example
  every crate triggers `boot`).
