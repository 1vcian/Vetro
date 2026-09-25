#!/bin/sh
# Prepara in target/aosp/sources/ i sorgenti GPL/LGPL dell'immagine AOSP di
# Vetro, da pubblicare accanto alle immagini (tools/aosp/upload.sh, CLAUDE.md
# "Licenze"):
#  1. kernel GKI prebuilt: kernel/common al commit scritto nella stringa di
#     versione del kernel usato (… -g<commit>-ab<build> …, build-info.txt),
#     e kernel/common-modules/virtual-device al commit dei moduli
#     (scmversion di virtio_mmio.ko): archivi git esatti da
#     android.googlesource.com, con l'id della build di ci.android.com;
#  2. progetti del tree AOSP con licenza GPL o LGPL (file MODULE_LICENSE_*GPL*,
#     esclusi prebuilts/ e kernel/), archiviati sulla VM così come sono stati
#     compilati, più il manifest di repo con le revisioni fissate e le patch
#     di Vetro (guest/aosp/patches).
# Idempotente: un archivio già presente con lo sha256 di SHA256SUMS non si
# rifà. Serve la VM solo per la parte 2 (pochi minuti).
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
info="$out/out/build-info.txt"
[ -f "$info" ] || { echo "manca $info (tools/aosp/fetch.sh)" >&2; exit 1; }
src="$out/sources"
mkdir -p "$src"
kver="$(sed -n 's/^kernel=Linux version \([^ ]*\) .*/\1/p' "$info")"
kbuild="$(sed -n 's/^kernel_build_id=.*"kernel-build-id":\([0-9]*\).*/\1/p' "$info")"
ksha="$(echo "$kver" | sed -n 's/.*-g\([0-9a-f]\{7,\}\)-ab.*/\1/p')"
msha="$(sed -n 's/^modules_scmversion=g\{0,1\}\([0-9a-f]\{7,\}\).*/\1/p' "$info")"
[ -n "$kver" ] && [ -n "$ksha" ] || { echo "versione del kernel non riconosciuta in $info" >&2; exit 1; }
gitiles=https://android.googlesource.com

# full REPO SHA_CORTO: lo sha completo del commit (gitiles risolve i prefissi).
full() {
  curl -fsSL "$gitiles/$1/+/$2?format=JSON" | sed -n 's/^  "commit": "\([0-9a-f]\{40\}\)",$/\1/p'
}
# archive REPO SHA NOME: archivio git esatto (tar.xz) in $src/NOME.tar.xz.
archive() {
  [ -f "$src/$3.tar.xz" ] && grep -q " $3.tar.xz\$" "$src/SHA256SUMS" 2>/dev/null &&
    (cd "$src" && grep " $3.tar.xz\$" SHA256SUMS | shasum -a 256 -c --quiet) && { echo "già pronto: $3.tar.xz"; return; }
  t="$(mktemp -d)"
  git -C "$t" init -q
  git -C "$t" fetch -q --depth 1 "$gitiles/$1" "$2"
  git -C "$t" archive --prefix="$3/" FETCH_HEAD | xz -T0 -9 > "$src/$3.tar.xz.tmp"
  mv "$src/$3.tar.xz.tmp" "$src/$3.tar.xz"
  rm -rf "$t"
  echo "pronto: $3.tar.xz"
}

kfull="$(full kernel/common "$ksha")"
archive kernel/common "$kfull" "linux-$kver"
mfull=""
if [ -n "$msha" ]; then
  mfull="$(full kernel/common-modules/virtual-device "$msha")"
  archive kernel/common-modules/virtual-device "$mfull" "virtual-device-modules-$msha"
fi

# Parte 2, sulla VM.
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK bash $VETRO_AOSP_WORK/remote/gpl.sh"
mkdir -p "$src/aosp"
vm_rsync -a --delete "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/sources/" "$src/aosp/"
mkdir -p "$src/aosp/vetro-patches"
cp -R "$root/guest/aosp/patches/." "$src/aosp/vetro-patches/"

cat > "$src/README" <<EOF
Sorgenti GPL/LGPL dell'immagine AOSP di Vetro (arm64)
=====================================================

Kernel: GKI android15-6.6 prebuilt di AOSP, non modificato.
  versione:   $kver
  build:      ci.android.com, kernel_aarch64, build $kbuild
  sorgenti:   linux-$kver.tar.xz = $gitiles/kernel/common al commit $kfull
  moduli:     virtual-device-modules-$msha.tar.xz = $gitiles/kernel/common-modules/virtual-device al commit $mfull
  configurazione: arch/arm64/configs/gki_defconfig e build.config.gki.aarch64
  nell'archivio del kernel; compilazione con Kleaf (build/kernel, Apache 2.0),
  manifest completo della build su ci.android.com (manifest_$kbuild.xml).

Spazio utente: aosp/ contiene i progetti del tree AOSP con licenza GPL o LGPL
(archivi dei file così come compilati), manifest-pinned.xml (repo, tutte le
revisioni fissate) e vetro-patches/ (le modifiche di Vetro a AOSP).
Tutto il resto di AOSP è su $gitiles alle revisioni del manifest.
EOF
(cd "$src" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | sort | xargs shasum -a 256 > SHA256SUMS)
du -sh "$src"
