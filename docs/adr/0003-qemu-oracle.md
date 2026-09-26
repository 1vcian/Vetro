# ADR 0003 — QEMU oracle and test ELFs generated in Rust

- Status: accepted (M0, 2026-09-24)

## Context
Comparison with QEMU is the main oracle. QEMU user mode
(`qemu-aarch64`) and RISU exist only on Linux; the development machine is
macOS arm64, CI is Ubuntu x86_64. We don't want to depend on a
cross-compiler for the basic tests.

## Decision
- The minimal test binaries are generated in Rust (`vetro-diff::elf` and
  `vetro-diff::a64`): a static ELF64 with one PT_LOAD segment. No external
  toolchain for the basic oracle.
- The oracle is found via `VETRO_QEMU_AARCH64` or `qemu-aarch64` in the PATH.
  On macOS `tools/oracle/qemu-aarch64-docker.sh` runs it in a Debian
  container with `qemu-user`.
- Without an oracle the tests print `SKIP` and pass; with
  `VETRO_REQUIRE_ORACLE=1` they fail. CI always sets the variable.

## Consequences
Development on macOS without Docker remains possible, but no work is
declared closed without a run with the mandatory oracle. RISU (M1) will follow
the same scheme: native in CI, container on macOS.
