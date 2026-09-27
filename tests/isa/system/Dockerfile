# System-mode oracle: qemu-system-aarch64 and AArch64 binutils.
# Only needed to regenerate probe.bin and probe.expected (build.sh), not for CI.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends qemu-system-arm binutils-aarch64-linux-gnu \
 && rm -rf /var/lib/apt/lists/*
