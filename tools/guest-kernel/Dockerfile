# Toolchain for the M3 guest kernel: arm64 Alpine (native on Apple Silicon and
# on the ubuntu-24.04-arm runners), same base as the guest binaries (tools/guest-bins).
# pahole: detached kernel BTF for introspection (ADR 0027).
FROM alpine:3.22
RUN apk add --no-cache build-base bc bison flex perl linux-headers \
    openssl-dev elfutils-dev xz cpio gzip findutils diffutils coreutils \
    pahole
