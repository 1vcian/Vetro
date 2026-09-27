# QEMU oracle

The oracle is `qemu-aarch64` (QEMU user mode), which exists only on Linux.

- **Linux / CI**: `apt-get install qemu-user`. The tests find it in the PATH.
- **macOS**: with Docker running,

  ```sh
  export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"
  cargo test -p vetro-diff
  ```

  The first run builds the `vetro-oracle` image from this Dockerfile.

Without an oracle the differential tests print `SKIP` and pass. With
`VETRO_REQUIRE_ORACLE=1` (set in CI) they fail: in CI a skip must never
look like a success.
