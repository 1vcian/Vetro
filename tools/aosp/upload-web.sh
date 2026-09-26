#!/bin/sh
# Uploads to R2, next to an already published version of Vetro's AOSP
# image, the browser files (ADR 0028): the disk map and its blocks
# (tools/aosp/web-disk.mjs), in aosp/<version>/web/:
#   web/disk.json      map of the GPT disk rebuilt from the already published
#                      (sparse) super.img and userdata.img
#   web/disk-head.bin  GPT and metadata (a few KiB)
# The version's files do not change: only web/ files are added. An object
# already present with the same sha256 is not uploaded again; a different one
# is an error (same rule as upload.sh).
# Usage: VETRO_AOSP_VERSION=<version> tools/aosp/upload-web.sh
# (default: the version in target/aosp/out/manifest.json).
# Credentials in ~/.config/vetro/r2.env, never printed.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp/out"
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
[ -f "$out/web/disk.json" ] && [ -f "$out/web/disk-head.bin" ] || { echo "out/web missing: node tools/aosp/web-disk.mjs" >&2; exit 1; }
if [ -z "${VETRO_AOSP_VERSION:-}" ]; then
  VETRO_AOSP_VERSION="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$out/manifest.json" | head -1)"
fi
[ -n "$VETRO_AOSP_VERSION" ] || { echo "unknown version (VETRO_AOSP_VERSION)" >&2; exit 1; }
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }
prefix="aosp/$VETRO_AOSP_VERSION"
# The map points at the version's files: they must be there, with the same size.
for f in super.img userdata.img; do
  want="$(stat -f %z "$out/$f")"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$prefix/$f" --query ContentLength --output text)"
  [ "$want" = "$have" ] || { echo "ERROR: $prefix/$f on R2 has $have bytes, locally $want" >&2; exit 1; }
done
put() {
  sha="$(shasum -a 256 "$1" | cut -d' ' -f1)"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$2" --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$have" = "$sha" ]; then
    echo "already present: $2"
  elif [ -n "$have" ] && [ "$have" != None ]; then
    echo "ERROR: $2 exists with a different sha256 ($have)" >&2
    exit 1
  else
    s3 s3 cp --only-show-errors --metadata "sha256=$sha" --content-type "$3" "$1" "s3://$R2_BUCKET/$2"
    echo "uploaded: $2"
  fi
}
put "$out/web/disk-head.bin" "$prefix/web/disk-head.bin" application/octet-stream
put "$out/web/disk.json" "$prefix/web/disk.json" application/json
echo "$R2_PUBLIC_URL/$prefix/web/disk.json"
