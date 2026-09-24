# Oracolo per macOS: QEMU user mode (e in M1 RISU) esistono solo su Linux.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends qemu-user \
 && rm -rf /var/lib/apt/lists/*
