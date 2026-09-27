# Oracle for macOS: QEMU user mode (and in M1 RISU) exist only on Linux.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends qemu-user \
 && rm -rf /var/lib/apt/lists/*
# For risugen (the RISU generator).
RUN apt-get update \
 && apt-get install -y --no-install-recommends perl liblist-compare-perl \
 && rm -rf /var/lib/apt/lists/*
