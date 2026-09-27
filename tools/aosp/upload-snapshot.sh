#!/bin/sh
# Publishes a prebuilt Android snapshot (ADR 0031), made by
# tools/aosp/prebuilt-snapshot.mjs, next to its image version on R2:
#   aosp/<version>/snapshots/<key>.snap   the snapshot
#   aosp/<version>/snapshots/<key>.json   its info (key, parts, size, sha256,
#                                         chunk hashes, metadata), uploaded
#                                         last: the app looks for this file
# <version> is the image version in the info file (parts.android). The key
# already says which vetro-wasm snapshot format and machine configuration it
# is for, so snapshots for several vetro-wasm versions can live side by side.
# An object already there with the same sha256 is not uploaded again; a
# different one under the same key is an error unless VETRO_REPLACE=1.
# Before uploading, the snapshot's size and sha256 are checked against the
# info file.
# Usage: tools/aosp/upload-snapshot.sh [DIR/<key>.json]
#   (default: the only *.json in target/aosp/prebuilt)
# Credentials in ~/.config/vetro/r2.env, never printed.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
info="${1:-}"
if [ -z "$info" ]; then
  set -- "$root"/target/aosp/prebuilt/*.json
  [ "$#" = 1 ] && [ -f "$1" ] || { echo "give the info file: $0 target/aosp/prebuilt/<key>.json" >&2; exit 1; }
  info="$1"
fi
snap="${info%.json}.snap"
[ -f "$snap" ] || { echo "missing $snap" >&2; exit 1; }
field() { node -e "const i = JSON.parse(require('fs').readFileSync(process.argv[1], 'utf8')); console.log($1)" "$info"; }
key="$(field i.key)"
version="$(field i.parts.android)"
size="$(field i.size)"
sha="$(field i.sha256)"
[ "$(basename "$info" .json)" = "$key" ] || { echo "the file name is not the key $key" >&2; exit 1; }
[ "$(wc -c < "$snap" | tr -d ' ')" = "$size" ] || { echo "$snap: size differs from the info file" >&2; exit 1; }
[ "$(shasum -a 256 "$snap" | cut -d' ' -f1)" = "$sha" ] || { echo "$snap: sha256 differs from the info file" >&2; exit 1; }
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }
prefix="aosp/$version"
# The image version must be published (the snapshot needs its disk and images).
s3 s3api head-object --bucket "$R2_BUCKET" --key "$prefix/web/disk.json" > /dev/null \
  || { echo "ERROR: $prefix/web/disk.json is not on R2" >&2; exit 1; }
put() {
  file="$1" obj="$2" type="$3" cache="$4"
  fsha="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$obj" --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$have" = "$fsha" ]; then
    echo "already present: $obj"
    return
  fi
  if [ -n "$have" ] && [ "$have" != None ] && [ "${VETRO_REPLACE:-0}" != 1 ]; then
    echo "ERROR: $obj exists with a different sha256 ($have); VETRO_REPLACE=1 to replace it" >&2
    exit 1
  fi
  s3 s3 cp --only-show-errors --metadata "sha256=$fsha" --content-type "$type" --cache-control "$cache" "$file" "s3://$R2_BUCKET/$obj"
  echo "uploaded: $obj"
}
put "$snap" "$prefix/snapshots/$key.snap" application/octet-stream "public, max-age=3600"
put "$info" "$prefix/snapshots/$key.json" application/json "no-cache"
echo "$R2_PUBLIC_URL/$prefix/snapshots/$key.json"
