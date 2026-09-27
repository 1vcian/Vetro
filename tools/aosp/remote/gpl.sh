#!/bin/bash
# On the build VM (launched by tools/aosp/gpl-sources.sh): archives in
# ~/$WORK/sources the AOSP tree projects licensed GPL or LGPL
# (MODULE_LICENSE_*GPL*, excluding prebuilts/, kernel/ and toolchain/, which do
# not end up in the image or are handled separately) and writes the repo manifest
# with pinned revisions. Idempotent: an archive already made for the same
# revision is not redone.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
o="$work/sources"
mkdir -p "$o"
rm -f "$o"/*.tmp
cd "$tree"
export PATH="$HOME/bin:$PATH"
repo manifest -r -o "$o/manifest-pinned.xml" 2>/dev/null
# Also excluded: GPL projects needed only for the build or tests that do not
# end up in any image (compilers, test suites, Cuttlefish host firmware and
# OpenWRT images); subfolders of a project already listed are not archived
# twice.
skip='^(external/(openwrt-prebuilts|kotlinc|coreboot|flashrom|ltp|linux-kselftest|seccomp-tests|error_prone/.*|wmediumd)|tools/external/.*|frameworks/native/opengl/tests/.*)$'
find . -path ./out -prune -o -path ./prebuilts -prune -o -path ./kernel -prune \
  -o -path ./toolchain -prune -o -path ./.repo -prune \
  -o -type f -name 'MODULE_LICENSE_*GPL*' -print | sed 's|^\./||; s|/MODULE_LICENSE_.*||' | sort -u |
  grep -Ev "$skip" | awk '{ for (p in seen) if (index($0, p "/") == 1) next; seen[$0] = 1; print }' > "$o/gpl-projects.txt"
keep=()
while read -r d; do
  name="$(echo "$d" | tr / _)"
  rev="$(git -C "$d" rev-parse --short=12 HEAD 2>/dev/null || echo norev)"
  f="$name-$rev.tar.xz"
  keep+=("$f")
  [ -f "$o/$f" ] && continue
  nice -n 19 tar -C "$tree" --exclude=.git -cf - "$d" | nice -n 19 xz -T2 -6 > "$o/$f.tmp"
  mv "$o/$f.tmp" "$o/$f"
done < "$o/gpl-projects.txt"
# Removes archives of old revisions.
for f in "$o"/*.tar.xz; do
  b="$(basename "$f")"
  [[ " ${keep[*]} " == *" $b "* ]] || rm -f "$f"
done
wc -l < "$o/gpl-projects.txt"
du -sh "$o"
