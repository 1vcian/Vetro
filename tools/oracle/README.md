# Oracolo QEMU

L'oracolo è `qemu-aarch64` (QEMU user mode), che esiste solo su Linux.

- **Linux / CI**: `apt-get install qemu-user`. I test lo trovano nel PATH.
- **macOS**: con Docker attivo,

  ```sh
  export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"
  cargo test -p vetro-diff
  ```

  Il primo avvio costruisce l'immagine `vetro-oracle` da questo Dockerfile.

Senza oracolo i test differenziali stampano `SKIP` e passano. Con
`VETRO_REQUIRE_ORACLE=1` (impostato in CI) falliscono: in CI uno skip non
deve mai sembrare un successo.
