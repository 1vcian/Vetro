# Toolchain per il kernel guest di M3: Alpine arm64 (nativo su Apple Silicon e
# sui runner ubuntu-24.04-arm), stessa base dei binari guest (tools/guest-bins).
# pahole: BTF staccato del kernel per l'introspezione (ADR 0027).
FROM alpine:3.22
RUN apk add --no-cache build-base bc bison flex perl linux-headers \
    openssl-dev elfutils-dev xz cpio gzip findutils diffutils coreutils \
    pahole
