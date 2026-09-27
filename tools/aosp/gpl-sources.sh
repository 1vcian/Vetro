#!/bin/sh
# Prepares in target/aosp/sources/ the GPL/LGPL sources of the Vetro AOSP
# image, to be published next to the images (tools/aosp/upload.sh, CLAUDE.md
# "Licenses"):
#  1. prebuilt GKI kernel and virtual-device modules: the build manifest
#     from ci.android.com (id in prebuilt-info.txt, build-info.txt) gives the
#     commits of kernel/common (checked against the -g<commit> in the kernel
#     version string) and of kernel/common-modules/virtual-device; exact git
#     archives from android.googlesource.com, plus the manifest itself;
#  2. AOSP tree projects licensed GPL or LGPL (MODULE_LICENSE_*GPL* files,
#     excluding prebuilts/ and kernel/), archived on the VM exactly as they were
#     built, plus the repo manifest with pinned revisions and Vetro's
#     patches (guest/aosp/patches).
# Idempotent: an archive already present is not redone. The VM is needed only
# for part 2 (a few minutes).
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
info="$out/out/build-info.txt"
[ -f "$info" ] || { echo "$info missing (tools/aosp/fetch.sh)" >&2; exit 1; }
src="$out/sources"
mkdir -p "$src"
kver="$(sed -n 's/^kernel=Linux version \([^ ]*\) .*/\1/p' "$info")"
kbuild="$(sed -n 's/^kernel_build_id=.*"kernel-build-id":\([0-9]*\).*/\1/p' "$info")"
ksha="$(echo "$kver" | sed -n 's/.*-g\([0-9a-f]\{7,\}\)-ab.*/\1/p')"
[ -n "$kver" ] && [ -n "$ksha" ] && [ -n "$kbuild" ] || { echo "kernel version not recognized in $info" >&2; exit 1; }
gitiles=https://android.googlesource.com

# Kernel build manifest (kernel_virt_aarch64 builds GKI and modules).
man="manifest_$kbuild.xml"
if [ ! -s "$src/$man" ]; then
  curl -fsSL -o "$src/$man.tmp" \
    "https://androidbuildinternal.googleapis.com/android/internal/build/v3/builds/$kbuild/kernel_virt_aarch64/attempts/latest/artifacts/$man/url?redirect=true"
  mv "$src/$man.tmp" "$src/$man"
fi
# rev PROJECT_NAME: the project's revision in the manifest.
rev() {
  grep -o "<project [^>]*name=\"$1\"[^>]*>" "$src/$man" | sed -n 's/.* revision="\([0-9a-f]\{40\}\)".*/\1/p' | head -n1
}
# archive REPO SHA NAME: exact git archive (tar.xz) in $src/NAME.tar.xz.
archive() {
  # The file appears only when complete (renamed from .tmp): if it exists, it is the right one.
  [ -f "$src/$3.tar.xz" ] && { echo "already ready: $3.tar.xz"; return; }
  t="$(mktemp -d)"
  git -C "$t" init -q
  git -C "$t" fetch -q --depth 1 "$gitiles/$1" "$2"
  git -C "$t" archive --prefix="$3/" FETCH_HEAD | xz -T0 -9 > "$src/$3.tar.xz.tmp"
  mv "$src/$3.tar.xz.tmp" "$src/$3.tar.xz"
  rm -rf "$t"
  echo "ready: $3.tar.xz"
}

kfull="$(rev kernel/common)"
mfull="$(rev kernel/common-modules/virtual-device)"
msha="$(echo "$mfull" | cut -c1-12)"
case "$kfull" in
  "$ksha"*) ;;
  *) echo "manifest $man has kernel/common $kfull, the kernel says $ksha" >&2; exit 1 ;;
esac
[ -n "$mfull" ] || { echo "virtual-device not found in $man" >&2; exit 1; }
archive kernel/common "$kfull" "linux-$kver"
archive kernel/common-modules/virtual-device "$mfull" "virtual-device-modules-$msha"

# Part 2, on the VM.
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK bash $VETRO_AOSP_WORK/remote/gpl.sh"
mkdir -p "$src/aosp"
vm_rsync -a --delete "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/sources/" "$src/aosp/"
mkdir -p "$src/aosp/vetro-patches"
cp -R "$root/guest/aosp/patches/." "$src/aosp/vetro-patches/"

cat > "$src/README" <<EOF
GPL/LGPL sources of the Vetro AOSP image (arm64)
================================================

Kernel: AOSP prebuilt GKI android15-6.6, unmodified.
  version:    $kver
  build:      ci.android.com $kbuild (kernel_aarch64 and kernel_virt_aarch64)
  sources:    linux-$kver.tar.xz = $gitiles/kernel/common at commit $kfull
  modules:    virtual-device-modules-$msha.tar.xz = $gitiles/kernel/common-modules/virtual-device at commit $mfull
  configuration: arch/arm64/configs/gki_defconfig and build.config.gki.aarch64
  in the kernel archive; built with Kleaf (build/kernel, Apache 2.0),
  full build manifest: $man (all revisions, including
  build/kernel and the toolchain).

User space: aosp/ contains the AOSP tree projects licensed GPL or LGPL
(archives of the files as built), manifest-pinned.xml (repo, all revisions
pinned) and vetro-patches/ (Vetro's changes to AOSP).
Everything else in AOSP is on $gitiles at the manifest revisions.
EOF
(cd "$src" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | sort | xargs shasum -a 256 > SHA256SUMS)
du -sh "$src"
