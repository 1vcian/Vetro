#!/bin/sh
# Compila i kselftest del kernel guest (gruppi in guest/kernel/kselftest/
# targets.txt) e prepara target/guest-kernel/initramfs-kselftest.cpio.gz:
# l'initramfs normale più /kselftest. Con `vetro.kselftest` sulla riga di
# comando /init li esegue (run_kselftest.sh) e spegne la macchina.
#
# I kselftest presuppongono glibc: si compilano in Debian arm64, statici
# (--allow-multiple-definition per valid-adjtimex, che ridefinisce
# clock_adjtime). Servono i sorgenti già estratti da tools/guest-kernel/build.sh
# nel volume Docker.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KVER=6.18.53
VOLUME="${VETRO_GUEST_KERNEL_VOLUME:-vetro-guest-kernel-build}"
OUT="$ROOT/target/guest-kernel"
[ -f "$OUT/Image" ] || "$ROOT/tools/guest-kernel/build.sh"
TARGETS="$(grep -v '^#' "$ROOT/guest/kernel/kselftest/targets.txt" | tr '\n' ' ')"

docker build -q -t vetro-kselftest:latest -f "$ROOT/tools/guest-kernel/Dockerfile.kselftest" \
  "$ROOT/tools/guest-kernel" >/dev/null
docker run --rm --platform linux/arm64 -v "$VOLUME:/build" -e KVER="$KVER" -e TARGETS="$TARGETS" \
  vetro-kselftest:latest sh -euc '
  src=/build/linux-$KVER; obj=/build/obj-$KVER
  rm -rf /build/ks-obj /build/ks-install
  # Header UAPI già installati da build.sh (dentro Alpine).
  [ -f "$obj/usr/include/linux/types.h" ] || { echo "manca $obj/usr/include: esegui build.sh"; exit 1; }
  echo "==> kselftest: $TARGETS"
  make -s -C "$src/tools/testing/selftests" TARGETS="$TARGETS" ARCH=arm64 \
    CC="gcc -static -Wl,--allow-multiple-definition" KHDR_INCLUDES="-isystem $obj/usr/include" \
    O=/build/ks-obj install INSTALL_PATH=/build/ks-install >/build/ks.log 2>&1 \
    || { tail -30 /build/ks.log; exit 1; }
  gcc --version | head -n1 > /build/ks-install/VERSIONS
'

# Initramfs: la lista normale più ogni file di /kselftest (in ordine, con
# permessi e link come nell'installazione).
IMAGE="${VETRO_GUEST_KERNEL_IMAGE:-vetro-guest-kernel:latest}"
docker run --rm --platform linux/arm64 -v "$ROOT:/src" -v "$VOLUME:/build" -e KVER="$KVER" "$IMAGE" sh -euc '
  out=/src/target/guest-kernel
  cd /build/ks-install
  {
    sed "s|^\(file [^ ]*\) \([^ ]*\)|\1 /src/\2|" /src/guest/kernel/initramfs/files.list
    echo "dir /kselftest 0755 0 0"
    find . -mindepth 1 | LC_ALL=C sort | while IFS= read -r p; do
      n="/kselftest/${p#./}"
      m=$(stat -c %a "$p")
      if [ -L "$p" ]; then echo "slink $n $(readlink "$p") 0777 0 0"
      elif [ -d "$p" ]; then echo "dir $n $m 0 0"
      else echo "file $n /build/ks-install/${p#./} $m 0 0"; fi
    done
  } > /build/ks-files.list
  /build/obj-$KVER/usr/gen_init_cpio -t 0 /build/ks-files.list | gzip -n -9 > "$out/initramfs-kselftest.cpio.gz"
  cp /build/ks-install/kselftest-list.txt "$out/kselftest-list.txt"
  cat /build/ks-install/VERSIONS >> "$out/sources/VERSIONS" || true
'
ls -l "$OUT/initramfs-kselftest.cpio.gz"
wc -l < "$OUT/kselftest-list.txt"
