#!/bin/sh
# Builds the M3 guest kernel and its initramfs in target/guest-kernel:
#   Image               arm64 Linux kernel (Image format, direct boot)
#   initramfs.cpio.gz   static BusyBox + /init + autotest + vetro-dev
#   vetro-dev           M5 device test program (in the initramfs)
#   vetro-files         M8 file manager daemon (in the initramfs,
#                       ADR 0020) with SQLite linked in (ADR 0021); it is also
#                       /bin/sqlite3 (multi-call, the official shell)
#   config, System.map  full configuration and symbols (for debugging)
#   vmlinux.btf         kernel types (detached BTF) for introspection
#                       from the outside (ADR 0027): same configuration plus
#                       DEBUG_INFO in a separate folder, then pahole; the
#                       kernel that boots stays the one without debug info
#   sources/            exact sources used (GPL-2.0, see CLAUDE.md)
#   VERSIONS            versions of kernel, compiler and BusyBox
# The build runs in an arm64 Alpine container (native on Apple Silicon and
# on the ubuntu-24.04-arm runners) on a Docker volume: the kernel sources
# have files that differ only in case, and the macOS file system
# doesn't tell them apart.
#
# The configuration is `make allnoconfig` + guest/kernel/config/vetro.config.
# The resulting defconfig must match guest/kernel/config/defconfig;
# to update it after changing the fragment:
#   VETRO_KERNEL_UPDATE_CONFIG=1 tools/guest-kernel/build.sh
#
# Usage: tools/guest-kernel/build.sh
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KVER=6.18.53
KSHA256=4d6fba95c2244b08a7b4144a4d38b9be4fb31abb5e7682ae40bb5cb11374cfe0
KURL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz"
# SQLite for vetro-files (ADR 0021): official amalgamation, public domain.
# sha256 computed by us; the SHA3-256 published by sqlite.org is
# 628a44cfe82c66aed1ccbbe85a562d2e33ebe64b3288981ed76285612227934e.
SQLITE=sqlite-amalgamation-3530400
SQLITE_SHA256=1e71ddf93849c6a6ecf58b827c0692073d2dd7ee40196158068f7b29f422e87d
SQLITE_URL="https://www.sqlite.org/2026/$SQLITE.zip"
IMAGE="${VETRO_GUEST_KERNEL_IMAGE:-vetro-guest-kernel:latest}"
VOLUME="${VETRO_GUEST_KERNEL_VOLUME:-vetro-guest-kernel-build}"
OUT="$ROOT/target/guest-kernel"
start=$(date +%s)

# Static BusyBox: the same as the M2 tests.
if [ ! -f "$ROOT/target/guest-bins/busybox" ]; then
  "$ROOT/tools/guest-bins/build.sh"
fi

mkdir -p "$OUT/sources"
tarball="$OUT/sources/linux-$KVER.tar.xz"
if [ ! -f "$tarball" ]; then
  echo "==> downloading linux-$KVER"
  curl -fL --retry 3 -o "$tarball.part" "$KURL"
  mv "$tarball.part" "$tarball"
fi

sqlite_zip="$OUT/sources/$SQLITE.zip"
if [ ! -f "$sqlite_zip" ]; then
  echo "==> downloading $SQLITE"
  curl -fL --retry 3 -o "$sqlite_zip.part" "$SQLITE_URL"
  mv "$sqlite_zip.part" "$sqlite_zip"
fi

docker build -q -t "$IMAGE" -f "$ROOT/tools/guest-kernel/Dockerfile" "$ROOT/tools/guest-kernel" >/dev/null
docker volume create "$VOLUME" >/dev/null

docker run --rm --platform linux/arm64 \
  -v "$ROOT:/src" -v "$VOLUME:/build" -w /src \
  -e KVER="$KVER" -e KSHA256="$KSHA256" \
  -e SQLITE="$SQLITE" -e SQLITE_SHA256="$SQLITE_SHA256" \
  -e UPDATE_CONFIG="${VETRO_KERNEL_UPDATE_CONFIG:-0}" \
  "$IMAGE" sh -euc '
  out=/src/target/guest-kernel
  tarball=$out/sources/linux-$KVER.tar.xz
  echo "$KSHA256  $tarball" | sha256sum -c -
  echo "$SQLITE_SHA256  $out/sources/$SQLITE.zip" | sha256sum -c -
  src=/build/linux-$KVER
  obj=/build/obj-$KVER
  if [ "$(cat "$src/.vetro-sha256" 2>/dev/null)" != "$KSHA256" ]; then
    echo "==> extracting the sources"
    rm -rf "$src" "$obj"
    tar -C /build -xJf "$tarball"
    echo "$KSHA256" > "$src/.vetro-sha256"
  fi
  # Reproducible build: no date, user or host of the machine.
  export ARCH=arm64
  export KBUILD_BUILD_TIMESTAMP="Thu Sep 24 00:00:00 UTC 2026"
  export KBUILD_BUILD_USER=vetro KBUILD_BUILD_HOST=vetro KBUILD_BUILD_VERSION=1
  frag=/src/guest/kernel/config/vetro.config
  mkdir -p "$obj"
  echo "==> configuration (allnoconfig + vetro.config)"
  make -s -C "$src" O="$obj" KCONFIG_ALLCONFIG="$frag" allnoconfig
  # Every option of the fragment must have made it into the .config.
  missing=0
  grep -E "^CONFIG_" "$frag" | while IFS= read -r line; do
    grep -qxF "$line" "$obj/.config" || { echo "option not applied: $line"; exit 1; }
  done || missing=1
  [ $missing = 0 ] || { echo "the fragment does not apply: check the dependencies"; exit 1; }
  make -s -C "$src" O="$obj" savedefconfig
  if ! cmp -s "$obj/defconfig" /src/guest/kernel/config/defconfig; then
    if [ "$UPDATE_CONFIG" = 1 ]; then
      cp "$obj/defconfig" /src/guest/kernel/config/defconfig
      echo "==> guest/kernel/config/defconfig updated"
    else
      diff -u /src/guest/kernel/config/defconfig "$obj/defconfig" || true
      echo "defconfig differs from guest/kernel/config/defconfig (VETRO_KERNEL_UPDATE_CONFIG=1 to update it)"
      exit 1
    fi
  fi
  echo "==> building Image with $(nproc) processes"
  make -s -C "$src" O="$obj" -j"$(nproc)" Image
  # UAPI headers for the kselftests (tools/guest-kernel/kselftest.sh): they are
  # installed here, because the kernel helper programs in $obj are
  # built with musl and do not run in the Debian container of the kselftests.
  make -s -C "$src" O="$obj" headers
  # M5 device test program: the drm/ headers come from the
  # kernel (Alpine does not have them); -idirafter leaves precedence to the musl ones.
  gcc -static -O2 -Wall -Werror -idirafter "$obj/usr/include" \
    -o "$out/vetro-dev" /src/guest/kernel/initramfs/vetro-dev.c
  # M8 file manager daemon (ADR 0020), static like vetro-dev,
  # with SQLite (ADR 0021) and its shell (argv[0] sqlite3). The engine without
  # threads or loadable extensions; the SQLite sources without -Werror
  # (they are not ours).
  sq=/build/$SQLITE
  rm -rf "$sq"
  unzip -q -d /build "$out/sources/$SQLITE.zip"
  sqdefs="-DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_DQS=0 -DSQLITE_DEFAULT_MEMSTATUS=0"
  gcc -c -O2 -w $sqdefs -o /build/sqlite3.o "$sq/sqlite3.c"
  gcc -c -O2 -w $sqdefs -Dmain=sqlite3_shell_main -o /build/shell.o "$sq/shell.c"
  gcc -c -O2 -Wall -Wextra -Werror -DVETRO_SQLITE_SHELL -I"$sq" -idirafter "$obj/usr/include" \
    -o /build/vetro-files.o /src/guest/kernel/initramfs/vetro-files.c
  gcc -static -o "$out/vetro-files" /build/vetro-files.o /build/shell.o /build/sqlite3.o -lm
  strip "$out/vetro-files"
  cp "$obj/arch/arm64/boot/Image" "$obj/.config" "$obj/System.map" "$out/"
  mv "$out/.config" "$out/config"

  # Kernel types for introspection (ADR 0027). The test kernel does not have
  # CONFIG_DEBUG_INFO_BTF (it would want BPF_SYSCALL, which changes the kernel): we
  # build vmlinux with the same .config plus DEBUG_INFO (which does not change the
  # layout of the structures) and pahole extracts the detached BTF from it.
  echo "==> detached BTF (vmlinux.btf)"
  btfobj=/build/obj-btf-$KVER
  mkdir -p "$btfobj"
  cp "$obj/.config" "$btfobj/.config"
  # DEBUG_KERNEL opens the debug menu: the options it would turn on by itself
  # (DEBUG_MISC, RCU_TRACE) stay off.
  printf "%s\n" CONFIG_DEBUG_KERNEL=y CONFIG_DEBUG_INFO_DWARF5=y \
    "# CONFIG_DEBUG_MISC is not set" "# CONFIG_RCU_TRACE is not set" >> "$btfobj/.config"
  make -s -C "$src" O="$btfobj" olddefconfig 2>/dev/null
  # Besides the options turned off, only those of the debug information.
  if diff "$obj/.config" "$btfobj/.config" | grep -E "^[<>] CONFIG_" \
    | grep -vE "^> CONFIG_(DEBUG_KERNEL|DEBUG_INFO|DEBUG_INFO_[A-Z0-9_]*|PAHOLE_HAS_[A-Z0-9_]*)=y$"; then
    echo "the BTF configuration changes options that are not debug options"
    exit 1
  fi
  make -s -C "$src" O="$btfobj" -j"$(nproc)" vmlinux
  pahole --btf_encode_detached="$out/vmlinux.btf" "$btfobj/vmlinux"
  # pahole creates it 0640 (root in the container): the tests read it as a user.
  chmod 644 "$out/vmlinux.btf"

  echo "==> initramfs"
  sed "s|^\(file [^ ]*\) \([^ ]*\)|\1 /src/\2|" /src/guest/kernel/initramfs/files.list \
    > /build/files.list
  "$obj/usr/gen_init_cpio" -t 0 /build/files.list | gzip -n -9 > "$out/initramfs.cpio.gz"

  echo "==> sources for the GPL"
  cp /src/guest/kernel/config/vetro.config /src/guest/kernel/config/defconfig \
     /src/guest/kernel/initramfs/files.list /src/guest/kernel/initramfs/init \
     /src/guest/kernel/initramfs/autotest.sh /src/guest/kernel/initramfs/kselftest.sh \
     /src/guest/kernel/initramfs/vetro-dev.c /src/guest/kernel/initramfs/vetro-files.c \
     /src/guest/kernel/initramfs/udhcpc.script \
     /src/tools/guest-kernel/build.sh \
     /src/tools/guest-kernel/Dockerfile "$out/sources/"
  {
    echo "linux $KVER sha256 $KSHA256 (https://cdn.kernel.org/pub/linux/kernel/v6.x/)"
    gcc --version | head -n1
    ld --version | head -n1
    grep -E "^busybox-static-" /src/target/guest-bins/VERSIONS || true
    echo "$SQLITE sha256 $SQLITE_SHA256 (https://www.sqlite.org/, public domain)"
  } > "$out/VERSIONS"
  cp "$out/VERSIONS" "$out/sources/VERSIONS"
  {
    echo "Sources of the Vetro kernel and initramfs (GPL-2.0)."
    echo "Kernel: linux-$KVER.tar.xz without patches, configured with"
    echo "  make ARCH=arm64 allnoconfig KCONFIG_ALLCONFIG=vetro.config"
    echo "  (minimal result: defconfig), then make Image."
    echo "BusyBox: static binary of the Alpine package busybox-static (version"
    echo "  in VERSIONS); sources and patches in https://gitlab.alpinelinux.org/alpine/aports"
    echo "  (main/busybox) at the tag of the Alpine 3.22 release."
  } > "$out/sources/README"
'
end=$(date +%s)
echo "==> done in $((end - start)) s"
ls -l "$OUT"
