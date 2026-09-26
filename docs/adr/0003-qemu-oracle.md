# ADR 0003 — Oracolo QEMU e ELF di test generati in Rust

- Stato: accettata (M0, 2026-09-24)

## Contesto
Il confronto con QEMU è l'oracolo principale. QEMU user mode
(`qemu-aarch64`) e RISU esistono solo su Linux; la macchina di sviluppo è
macOS arm64, la CI è Ubuntu x86_64. Non vogliamo dipendere da un
cross-compilatore per i test di base.

## Decisione
- I binari di test minimi si generano in Rust (`vetro-diff::elf` e
  `vetro-diff::a64`): ELF64 statico con un segmento PT_LOAD. Nessun toolchain
  esterno per l'oracolo di base.
- L'oracolo si trova con `VETRO_QEMU_AARCH64` oppure `qemu-aarch64` nel PATH.
  Su macOS `tools/oracle/qemu-aarch64-docker.sh` lo esegue in un container
  Debian con `qemu-user`.
- Senza oracolo i test stampano `SKIP` e passano; con
  `VETRO_REQUIRE_ORACLE=1` falliscono. La CI imposta sempre la variabile.

## Conseguenze
Lo sviluppo su macOS senza Docker resta possibile, ma nessun lavoro si
dichiara chiuso senza un giro con l'oracolo obbligatorio. RISU (M1) seguirà
lo stesso schema: nativo in CI, container su macOS.
