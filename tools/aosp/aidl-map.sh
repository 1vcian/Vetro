#!/bin/sh
# Rigenera la mappa AIDL (descrittore, codice -> metodo) dell'immagine AOSP
# di Vetro: crates/vetro-analysis/src/introspect/aidl_aosp15.tsv (M8).
# Serve target/aosp/out/super.img (tools/aosp/fetch.sh), Docker
# (vetro-aosp-tools: simg2img, fsck.erofs) e dexdump dell'SDK Android
# (DEXDUMP, predefinito build-tools 35.0.0).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
dexdump="${DEXDUMP:-$HOME/Library/Android/sdk/build-tools/35.0.0/dexdump}"
docker build -q -t vetro-aosp-tools:latest -f "$here/Dockerfile.tools" "$here" >/dev/null
[ -f "$a/super.raw" ] || docker run --rm -v "$a:/a" vetro-aosp-tools:latest simg2img /a/out/super.img /a/super.raw
python3 "$here/lp-extract.py" "$a/super.raw" system_a "$a/system.img"
rm -rf "$a/fs/system"
mkdir -p "$a/fs"
docker run --rm -v "$a:/a" vetro-aosp-tools:latest fsck.erofs --extract=/a/fs/system /a/system.img
t="$a/dex"
rm -rf "$t"
mkdir -p "$t"
for j in "$a"/fs/system/system/framework/*.jar; do
  d="$t/$(basename "$j" .jar)"
  mkdir -p "$d"
  unzip -oq "$j" 'classes*.dex' -d "$d" 2>/dev/null || true
done
find "$t" -name '*.dex' | sort | xargs python3 "$here/aidl-map.py" "$dexdump" \
  > "$root/crates/vetro-analysis/src/introspect/aidl_aosp15.tsv"
wc -l "$root/crates/vetro-analysis/src/introspect/aidl_aosp15.tsv"
