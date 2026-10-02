#!/bin/sh
# Publishes the prefetch list of a prebuilt Android snapshot (ADR 0044, made
# by tools/aosp/prefetch-list.mjs) next to the snapshot on R2:
#   aosp/<version>/snapshots/<key>.blocks.json
# The snapshot's info file must already be there (same key). The list can be
# replaced when a better one is made (it only orders downloads the app would
# make anyway): no-cache, replaced without VETRO_REPLACE.
# Usage: VETRO_AOSP_VERSION=<version> tools/aosp/upload-prefetch.sh DIR/<key>.blocks.json
# Credentials in ~/.config/vetro/r2.env, never printed.
set -eu
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
list="${1:?usage: VETRO_AOSP_VERSION=<version> $0 DIR/<key>.blocks.json}"
[ -n "${VETRO_AOSP_VERSION:-}" ] || { echo "VETRO_AOSP_VERSION is needed" >&2; exit 1; }
key="$(basename "$list" .blocks.json)"
[ "$(node -e "console.log(JSON.parse(require('fs').readFileSync(process.argv[1], 'utf8')).key)" "$list")" = "$key" ] \
  || { echo "$list: the key inside is not $key" >&2; exit 1; }
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }
prefix="aosp/$VETRO_AOSP_VERSION/snapshots"
s3 s3api head-object --bucket "$R2_BUCKET" --key "$prefix/$key.json" > /dev/null \
  || { echo "ERROR: $prefix/$key.json is not on R2" >&2; exit 1; }
sha="$(shasum -a 256 "$list" | cut -d' ' -f1)"
s3 s3 cp --only-show-errors --metadata "sha256=$sha" --content-type application/json --cache-control no-cache "$list" "s3://$R2_BUCKET/$prefix/$key.blocks.json"
echo "uploaded: $R2_PUBLIC_URL/$prefix/$key.blocks.json"
