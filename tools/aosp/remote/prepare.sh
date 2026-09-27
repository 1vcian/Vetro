#!/bin/sh
# On the build VM (launched by tools/aosp/sync.sh): applies the patches in
# ~/$WORK/patches to the AOSP tree and downloads microG. Idempotent: a patch already
# applied is skipped, an APK with the right sha256 is not downloaded again.
# Patches live in patches/<project path>/NNNN-*.patch (a/ and b/
# prefixes relative to the project root).
set -eu
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"

# Development CA (ADR 0030, tools/aosp/dev-ca.sh): only the files created by the
# current patches stay in the two trust stores. After a CA change the old patch
# does not "unapply" itself: its (untracked) file is removed here, so
# the image no longer trusts the previous CA. The current patch's files
# are left alone (no needless APEX rebuild).
for d in external/conscrypt:apex/ca-certificates/files system/ca-certificates:files; do
  proj="${d%%:*}"
  sub="${d#*:}"
  keep="$(cat "$work/patches/$proj/"*.patch 2>/dev/null | sed -n 's|^+++ b/||p')"
  git -C "$tree/$proj" ls-files --others --exclude-standard -- "$sub" | while read -r f; do
    if ! printf '%s\n' "$keep" | grep -qxF "$f"; then
      rm -f "$tree/$proj/$f"
      echo "removed $proj/$f (CA no longer in the patches)"
    fi
  done
done

cd "$work/patches"
find . -name '*.patch' | sed 's|^\./||' | sort | while read -r p; do
  proj="$(dirname "$p")"
  patch="$work/patches/$p"
  if git -C "$tree/$proj" apply --check -R "$patch" 2>/dev/null; then
    echo "patch already applied: $p"
  elif git -C "$tree/$proj" apply --check "$patch"; then
    git -C "$tree/$proj" apply "$patch"
    echo "patch applied: $p"
  else
    echo "ERROR: patch $p does not apply to $proj" >&2
    exit 1
  fi
done

lock="$tree/vendor/vetro/microg/microg.lock"
dest="$tree/vendor/vetro/microg/prebuilt"
mkdir -p "$dest"
grep -v '^#' "$lock" | while read -r file sha url; do
  [ -n "$file" ] || continue
  if [ -f "$dest/$file" ] && echo "$sha  $dest/$file" | sha256sum -c --status; then
    echo "microG: $file already present"
    continue
  fi
  curl -fsSL --retry 3 -o "$dest/$file.tmp" "$url"
  if ! echo "$sha  $dest/$file.tmp" | sha256sum -c --status; then
    echo "ERROR: sha256 of $file differs from microg.lock" >&2
    rm -f "$dest/$file.tmp"
    exit 1
  fi
  mv "$dest/$file.tmp" "$dest/$file"
  echo "microG: $file downloaded"
done
