#!/bin/sh
# Uploads the Vetro AOSP image artifacts to Cloudflare R2, with a
# versioned path and a manifest with the sha256s:
#   aosp/<version>/{boot,vendor_boot,init_boot,super,userdata}.img
#   aosp/<version>/build-info.txt, SHA256SUMS, manifest.json
#   aosp/<version>/sources/...   (GPL sources, tools/aosp/gpl-sources.sh)
# <version> = VETRO_AOSP_VERSION, otherwise
# <AOSP tag>-<BUILD_ID>-<commit of guest/aosp and tools/aosp>, e.g.
# android-15.0.0_r36-BP1A.250505.005.D1-1a2b3c4d.
# Only our own, redistributable artifacts (AOSP Apache/GPL, microG Apache):
# never Google's SDK image. An object already present with the same sha256
# is not re-uploaded (idempotent); a different one under the same version is an
# error (a published version does not change).
# Credentials in ~/.config/vetro/r2.env (R2_ENDPOINT, R2_ACCESS_KEY_ID,
# R2_SECRET_ACCESS_KEY, R2_BUCKET, R2_PUBLIC_URL): never printed.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
[ -f "$a/out/SHA256SUMS" ] || { echo "artifacts missing (tools/aosp/fetch.sh)" >&2; exit 1; }
(cd "$a/out" && shasum -a 256 -c --quiet SHA256SUMS)
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }

if [ -z "${VETRO_AOSP_VERSION:-}" ]; then
  tag="$(sed -n 's/^manifest_tag=//p' "$a/out/build-info.txt")"
  bid="$(sed -n 's/^build_id=//p' "$a/out/build-info.txt")"
  # The Vetro commit the image was built from (sync.sh ->
  # build-info.txt), not the Mac's at upload time. Old builds
  # without vetro_rev use the current, clean commit.
  # "sconosciuta" (unknown) is the value remote/build.sh and pack.sh write: keep it.
  rev="$(sed -n 's/^vetro_rev=//p' "$a/out/build-info.txt")"
  case "$rev" in
    *-dirty) echo "the image was built from uncommitted changes ($rev): commit, re-sync and rebuild" >&2; exit 1 ;;
    ""|sconosciuta)
      rev="$(git -C "$root" log -1 --format=%h -- guest/aosp tools/aosp guest/kernel/initramfs/vetro-files.c)"
      if [ -n "$(git -C "$root" status --porcelain -- guest/aosp tools/aosp)" ]; then
        echo "guest/aosp or tools/aosp have uncommitted changes: commit before publishing" >&2
        exit 1
      fi ;;
  esac
  VETRO_AOSP_VERSION="${tag:-aosp}-${bid}-${rev}"
fi
prefix="aosp/$VETRO_AOSP_VERSION"
echo "version: $VETRO_AOSP_VERSION"

# put FILE KEY: uploads if missing, verifies the sha256 if already there.
put() {
  sha="$(shasum -a 256 "$1" | cut -d' ' -f1)"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$2" --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$have" = "$sha" ]; then
    echo "already present: $2"
  elif [ -n "$have" ] && [ "$have" != None ]; then
    echo "ERROR: $2 exists with a different sha256 ($have)" >&2
    exit 1
  else
    s3 s3 cp --only-show-errors --metadata "sha256=$sha" "$1" "s3://$R2_BUCKET/$2"
    echo "uploaded: $2"
  fi
}

files="boot.img vendor_boot.img init_boot.img super.img userdata.img build-info.txt SHA256SUMS"
for f in $files; do put "$a/out/$f" "$prefix/$f"; done
src_list=""
if [ -f "$a/sources/SHA256SUMS" ]; then
  (cd "$a/sources" && shasum -a 256 -c --quiet SHA256SUMS)
  src_list="$(cd "$a/sources" && find . -type f | sed 's|^\./||' | sort)"
  for f in $src_list; do put "$a/sources/$f" "$prefix/sources/$f"; done
else
  echo "warning: no GPL sources (tools/aosp/gpl-sources.sh): the image must not be distributed without them" >&2
fi

# Manifest: file, size, sha256, public URL.
man="$a/manifest.json"
{
  printf '{\n  "version": "%s",\n  "base_url": "%s/%s",\n  "files": [\n' "$VETRO_AOSP_VERSION" "$R2_PUBLIC_URL" "$prefix"
  first=1
  for f in $files $(for s in $src_list; do echo "sources/$s"; done); do
    p="$a/out/$f"; case "$f" in sources/*) p="$a/$f" ;; esac
    [ $first = 1 ] || printf ',\n'
    first=0
    printf '    {"path": "%s", "size": %s, "sha256": "%s"}' "$f" "$(stat -f %z "$p")" "$(shasum -a 256 "$p" | cut -d' ' -f1)"
  done
  printf '\n  ]\n}\n'
} > "$man"
put "$man" "$prefix/manifest.json"
echo "$R2_PUBLIC_URL/$prefix/manifest.json"
