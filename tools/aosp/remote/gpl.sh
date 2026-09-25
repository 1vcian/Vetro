#!/bin/bash
# Sulla VM di build (lanciato da tools/aosp/gpl-sources.sh): archivia in
# ~/$WORK/sources i progetti del tree AOSP con licenza GPL o LGPL
# (MODULE_LICENSE_*GPL*, esclusi prebuilts/, kernel/ e toolchain/, che non
# finiscono nell'immagine o hanno la loro strada) e scrive il manifest di repo
# con le revisioni fissate. Idempotente: un archivio già fatto per la stessa
# revisione non si rifà.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
o="$work/sources"
mkdir -p "$o"
cd "$tree"
export PATH="$HOME/bin:$PATH"
repo manifest -r -o "$o/manifest-pinned.xml" 2>/dev/null
find . -path ./out -prune -o -path ./prebuilts -prune -o -path ./kernel -prune \
  -o -path ./toolchain -prune -o -path ./.repo -prune \
  -o -type f -name 'MODULE_LICENSE_*GPL*' -print | sed 's|^\./||; s|/MODULE_LICENSE_.*||' | sort -u > "$o/gpl-projects.txt"
keep=()
while read -r d; do
  name="$(echo "$d" | tr / _)"
  rev="$(git -C "$d" rev-parse --short=12 HEAD 2>/dev/null || echo norev)"
  f="$name-$rev.tar.xz"
  keep+=("$f")
  [ -f "$o/$f" ] && continue
  tar -C "$tree" --exclude=.git -cf - "$d" | xz -T0 -6 > "$o/$f.tmp"
  mv "$o/$f.tmp" "$o/$f"
done < "$o/gpl-projects.txt"
# Toglie gli archivi di revisioni vecchie.
for f in "$o"/*.tar.xz; do
  b="$(basename "$f")"
  [[ " ${keep[*]} " == *" $b "* ]] || rm -f "$f"
done
wc -l < "$o/gpl-projects.txt"
du -sh "$o"
