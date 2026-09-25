#!/bin/sh
# Costruisce il kernel guest di M3 e il suo initramfs in target/guest-kernel:
#   Image               kernel Linux arm64 (formato Image, avvio diretto)
#   initramfs.cpio.gz   BusyBox statica + /init + autotest
#   config, System.map  configurazione completa e simboli (per il debug)
#   sources/            sorgenti esatti usati (GPL-2.0, vedi CLAUDE.md)
#   VERSIONS            versioni di kernel, compilatore e BusyBox
# La compilazione gira in un container Alpine arm64 (nativo su Apple Silicon e
# sui runner ubuntu-24.04-arm) su un volume Docker: i sorgenti del kernel
# hanno file che differiscono solo per maiuscole, e il file system di macOS
# non li distingue.
#
# La configurazione è `make allnoconfig` + guest/kernel/config/vetro.config.
# Il defconfig risultante deve coincidere con guest/kernel/config/defconfig;
# per aggiornarlo dopo aver cambiato il frammento:
#   VETRO_KERNEL_UPDATE_CONFIG=1 tools/guest-kernel/build.sh
#
# Uso: tools/guest-kernel/build.sh
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KVER=6.18.53
KSHA256=4d6fba95c2244b08a7b4144a4d38b9be4fb31abb5e7682ae40bb5cb11374cfe0
KURL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz"
IMAGE="${VETRO_GUEST_KERNEL_IMAGE:-vetro-guest-kernel:latest}"
VOLUME="${VETRO_GUEST_KERNEL_VOLUME:-vetro-guest-kernel-build}"
OUT="$ROOT/target/guest-kernel"
start=$(date +%s)

# BusyBox statica: la stessa dei test di M2.
if [ ! -f "$ROOT/target/guest-bins/busybox" ]; then
  "$ROOT/tools/guest-bins/build.sh"
fi

mkdir -p "$OUT/sources"
tarball="$OUT/sources/linux-$KVER.tar.xz"
if [ ! -f "$tarball" ]; then
  echo "==> scarico linux-$KVER"
  curl -fL --retry 3 -o "$tarball.part" "$KURL"
  mv "$tarball.part" "$tarball"
fi

docker build -q -t "$IMAGE" -f "$ROOT/tools/guest-kernel/Dockerfile" "$ROOT/tools/guest-kernel" >/dev/null
docker volume create "$VOLUME" >/dev/null

docker run --rm --platform linux/arm64 \
  -v "$ROOT:/src" -v "$VOLUME:/build" -w /src \
  -e KVER="$KVER" -e KSHA256="$KSHA256" \
  -e UPDATE_CONFIG="${VETRO_KERNEL_UPDATE_CONFIG:-0}" \
  "$IMAGE" sh -euc '
  out=/src/target/guest-kernel
  tarball=$out/sources/linux-$KVER.tar.xz
  echo "$KSHA256  $tarball" | sha256sum -c -
  src=/build/linux-$KVER
  obj=/build/obj-$KVER
  if [ "$(cat "$src/.vetro-sha256" 2>/dev/null)" != "$KSHA256" ]; then
    echo "==> estraggo i sorgenti"
    rm -rf "$src" "$obj"
    tar -C /build -xJf "$tarball"
    echo "$KSHA256" > "$src/.vetro-sha256"
  fi
  # Build riproducibile: niente data, utente o host della macchina.
  export ARCH=arm64
  export KBUILD_BUILD_TIMESTAMP="Thu Sep 24 00:00:00 UTC 2026"
  export KBUILD_BUILD_USER=vetro KBUILD_BUILD_HOST=vetro KBUILD_BUILD_VERSION=1
  frag=/src/guest/kernel/config/vetro.config
  mkdir -p "$obj"
  echo "==> configurazione (allnoconfig + vetro.config)"
  make -s -C "$src" O="$obj" KCONFIG_ALLCONFIG="$frag" allnoconfig
  # Ogni opzione del frammento deve essere arrivata nella .config.
  missing=0
  grep -E "^CONFIG_" "$frag" | while IFS= read -r line; do
    grep -qxF "$line" "$obj/.config" || { echo "opzione non applicata: $line"; exit 1; }
  done || missing=1
  [ $missing = 0 ] || { echo "il frammento non si applica: controlla le dipendenze"; exit 1; }
  make -s -C "$src" O="$obj" savedefconfig
  if ! cmp -s "$obj/defconfig" /src/guest/kernel/config/defconfig; then
    if [ "$UPDATE_CONFIG" = 1 ]; then
      cp "$obj/defconfig" /src/guest/kernel/config/defconfig
      echo "==> guest/kernel/config/defconfig aggiornato"
    else
      diff -u /src/guest/kernel/config/defconfig "$obj/defconfig" || true
      echo "defconfig diverso da guest/kernel/config/defconfig (VETRO_KERNEL_UPDATE_CONFIG=1 per aggiornarlo)"
      exit 1
    fi
  fi
  echo "==> compilo Image con $(nproc) processi"
  make -s -C "$src" O="$obj" -j"$(nproc)" Image
  # Header UAPI per i kselftest (tools/guest-kernel/kselftest.sh): si
  # installano qui, perché i programmi di supporto del kernel in $obj sono
  # compilati con musl e non girano nel container Debian dei kselftest.
  make -s -C "$src" O="$obj" headers
  cp "$obj/arch/arm64/boot/Image" "$obj/.config" "$obj/System.map" "$out/"
  mv "$out/.config" "$out/config"

  echo "==> initramfs"
  sed "s|^\(file [^ ]*\) \([^ ]*\)|\1 /src/\2|" /src/guest/kernel/initramfs/files.list \
    > /build/files.list
  "$obj/usr/gen_init_cpio" -t 0 /build/files.list | gzip -n -9 > "$out/initramfs.cpio.gz"

  echo "==> sorgenti per la GPL"
  cp /src/guest/kernel/config/vetro.config /src/guest/kernel/config/defconfig \
     /src/guest/kernel/initramfs/files.list /src/guest/kernel/initramfs/init \
     /src/guest/kernel/initramfs/autotest.sh /src/guest/kernel/initramfs/kselftest.sh \
     /src/tools/guest-kernel/build.sh \
     /src/tools/guest-kernel/Dockerfile "$out/sources/"
  {
    echo "linux $KVER sha256 $KSHA256 (https://cdn.kernel.org/pub/linux/kernel/v6.x/)"
    gcc --version | head -n1
    ld --version | head -n1
    grep -E "^busybox-static-" /src/target/guest-bins/VERSIONS || true
  } > "$out/VERSIONS"
  cp "$out/VERSIONS" "$out/sources/VERSIONS"
  {
    echo "Sorgenti del kernel e dell initramfs di Vetro (GPL-2.0)."
    echo "Kernel: linux-$KVER.tar.xz senza patch, configurato con"
    echo "  make ARCH=arm64 allnoconfig KCONFIG_ALLCONFIG=vetro.config"
    echo "  (risultato minimo: defconfig), poi make Image."
    echo "BusyBox: binario statico del pacchetto Alpine busybox-static (versione"
    echo "  in VERSIONS); sorgenti e patch in https://gitlab.alpinelinux.org/alpine/aports"
    echo "  (main/busybox) al tag della release Alpine 3.22."
  } > "$out/sources/README"
'
end=$(date +%s)
echo "==> fatto in $((end - start)) s"
ls -l "$OUT"
