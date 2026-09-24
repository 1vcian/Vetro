# Oracolo della modalità sistema: qemu-system-aarch64 e binutils AArch64.
# Serve solo a rigenerare probe.bin e probe.expected (build.sh), non alla CI.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends qemu-system-arm binutils-aarch64-linux-gnu \
 && rm -rf /var/lib/apt/lists/*
