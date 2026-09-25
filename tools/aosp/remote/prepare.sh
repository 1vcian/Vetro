#!/bin/sh
# Sulla VM di build (lanciato da tools/aosp/sync.sh): applica le patch di
# ~/$WORK/patches al tree AOSP e scarica microG. Idempotente: una patch già
# applicata si salta, un APK con lo sha256 giusto non si riscarica.
# Le patch stanno in patches/<percorso del progetto>/NNNN-*.patch (prefissi
# a/ e b/ relativi alla radice del progetto).
set -eu
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"

cd "$work/patches"
find . -name '*.patch' | sed 's|^\./||' | sort | while read -r p; do
  proj="$(dirname "$p")"
  patch="$work/patches/$p"
  if git -C "$tree/$proj" apply --check -R "$patch" 2>/dev/null; then
    echo "patch già applicata: $p"
  elif git -C "$tree/$proj" apply --check "$patch"; then
    git -C "$tree/$proj" apply "$patch"
    echo "patch applicata: $p"
  else
    echo "ERRORE: la patch $p non si applica a $proj" >&2
    exit 1
  fi
done

lock="$tree/vendor/vetro/microg/microg.lock"
dest="$tree/vendor/vetro/microg/prebuilt"
mkdir -p "$dest"
grep -v '^#' "$lock" | while read -r file sha url; do
  [ -n "$file" ] || continue
  if [ -f "$dest/$file" ] && echo "$sha  $dest/$file" | sha256sum -c --status; then
    echo "microG: $file già presente"
    continue
  fi
  curl -fsSL --retry 3 -o "$dest/$file.tmp" "$url"
  if ! echo "$sha  $dest/$file.tmp" | sha256sum -c --status; then
    echo "ERRORE: sha256 di $file diverso da microg.lock" >&2
    rm -f "$dest/$file.tmp"
    exit 1
  fi
  mv "$dest/$file.tmp" "$dest/$file"
  echo "microG: $file scaricato"
done
