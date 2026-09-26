#!/bin/sh
# Costruisce il kernel guest di M3 e il suo initramfs in target/guest-kernel:
#   Image               kernel Linux arm64 (formato Image, avvio diretto)
#   initramfs.cpio.gz   BusyBox statica + /init + autotest + vetro-dev
#   vetro-dev           prova dei dispositivi di M5 (nell'initramfs)
#   vetro-files         demone del gestore dei file di M8 (nell'initramfs,
#                       ADR 0020) con SQLite linkato (ADR 0021); è anche
#                       /bin/sqlite3 (multi-chiamata, shell ufficiale)
#   config, System.map  configurazione completa e simboli (per il debug)
#   vmlinux.btf         tipi del kernel (BTF staccato) per l'introspezione
#                       dall'esterno (ADR 0027): stessa configurazione più
#                       DEBUG_INFO in una cartella a parte, poi pahole; il
#                       kernel che si avvia resta quello senza debug
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
# SQLite per vetro-files (ADR 0021): amalgamation ufficiale, dominio pubblico.
# sha256 calcolato da noi; il SHA3-256 pubblicato da sqlite.org è
# 628a44cfe82c66aed1ccbbe85a562d2e33ebe64b3288981ed76285612227934e.
SQLITE=sqlite-amalgamation-3530400
SQLITE_SHA256=1e71ddf93849c6a6ecf58b827c0692073d2dd7ee40196158068f7b29f422e87d
SQLITE_URL="https://www.sqlite.org/2026/$SQLITE.zip"
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

sqlite_zip="$OUT/sources/$SQLITE.zip"
if [ ! -f "$sqlite_zip" ]; then
  echo "==> scarico $SQLITE"
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
  # Programma di prova dei dispositivi di M5: gli header di drm/ vengono dal
  # kernel (Alpine non li ha); -idirafter lascia la precedenza a quelli di musl.
  gcc -static -O2 -Wall -Werror -idirafter "$obj/usr/include" \
    -o "$out/vetro-dev" /src/guest/kernel/initramfs/vetro-dev.c
  # Demone del gestore dei file di M8 (ADR 0020), statico come vetro-dev,
  # con SQLite (ADR 0021) e la sua shell (argv[0] sqlite3). Il motore senza
  # thread né estensioni caricabili; i sorgenti di SQLite senza -Werror
  # (non sono nostri).
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

  # Tipi del kernel per l introspezione (ADR 0027). Il kernel di prova non ha
  # CONFIG_DEBUG_INFO_BTF (vorrebbe BPF_SYSCALL, che cambia il kernel): si
  # compila vmlinux con la stessa .config più DEBUG_INFO (che non cambia la
  # disposizione delle strutture) e pahole ne estrae il BTF staccato.
  echo "==> BTF staccato (vmlinux.btf)"
  btfobj=/build/obj-btf-$KVER
  mkdir -p "$btfobj"
  cp "$obj/.config" "$btfobj/.config"
  # DEBUG_KERNEL apre il menu del debug: le opzioni che accenderebbe da sé
  # (DEBUG_MISC, RCU_TRACE) restano spente.
  printf "%s\n" CONFIG_DEBUG_KERNEL=y CONFIG_DEBUG_INFO_DWARF5=y \
    "# CONFIG_DEBUG_MISC is not set" "# CONFIG_RCU_TRACE is not set" >> "$btfobj/.config"
  make -s -C "$src" O="$btfobj" olddefconfig 2>/dev/null
  # Oltre alle opzioni spente, solo quelle delle informazioni di debug.
  if diff "$obj/.config" "$btfobj/.config" | grep -E "^[<>] CONFIG_" \
    | grep -vE "^> CONFIG_(DEBUG_KERNEL|DEBUG_INFO|DEBUG_INFO_[A-Z0-9_]*|PAHOLE_HAS_[A-Z0-9_]*)=y$"; then
    echo "la configurazione del BTF cambia opzioni che non sono di debug"
    exit 1
  fi
  make -s -C "$src" O="$btfobj" -j"$(nproc)" vmlinux
  pahole --btf_encode_detached="$out/vmlinux.btf" "$btfobj/vmlinux"
  # pahole lo crea 0640 (root nel container): i test lo leggono da utente.
  chmod 644 "$out/vmlinux.btf"

  echo "==> initramfs"
  sed "s|^\(file [^ ]*\) \([^ ]*\)|\1 /src/\2|" /src/guest/kernel/initramfs/files.list \
    > /build/files.list
  "$obj/usr/gen_init_cpio" -t 0 /build/files.list | gzip -n -9 > "$out/initramfs.cpio.gz"

  echo "==> sorgenti per la GPL"
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
    echo "$SQLITE sha256 $SQLITE_SHA256 (https://www.sqlite.org/, dominio pubblico)"
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
