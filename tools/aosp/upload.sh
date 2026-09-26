#!/bin/sh
# Carica su Cloudflare R2 gli artefatti dell'immagine AOSP di Vetro, con un
# percorso versionato e un manifest con gli sha256:
#   aosp/<versione>/{boot,vendor_boot,init_boot,super,userdata}.img
#   aosp/<versione>/build-info.txt, SHA256SUMS, manifest.json
#   aosp/<versione>/sources/...   (sorgenti GPL, tools/aosp/gpl-sources.sh)
# <versione> = VETRO_AOSP_VERSION, altrimenti
# <tag AOSP>-<BUILD_ID>-<commit di guest/aosp e tools/aosp>, es.
# android-15.0.0_r36-BP1A.250505.005.D1-1a2b3c4d.
# Solo artefatti nostri e ridistribuibili (AOSP Apache/GPL, microG Apache):
# mai l'immagine SDK di Google. Un oggetto già presente con lo stesso sha256
# non si ricarica (idempotente); uno diverso sotto la stessa versione è un
# errore (una versione pubblicata non cambia).
# Credenziali in ~/.config/vetro/r2.env (R2_ENDPOINT, R2_ACCESS_KEY_ID,
# R2_SECRET_ACCESS_KEY, R2_BUCKET, R2_PUBLIC_URL): mai stampate.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
[ -f "$a/out/SHA256SUMS" ] || { echo "mancano gli artefatti (tools/aosp/fetch.sh)" >&2; exit 1; }
(cd "$a/out" && shasum -a 256 -c --quiet SHA256SUMS)
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }

if [ -z "${VETRO_AOSP_VERSION:-}" ]; then
  tag="$(sed -n 's/^manifest_tag=//p' "$a/out/build-info.txt")"
  bid="$(sed -n 's/^build_id=//p' "$a/out/build-info.txt")"
  rev="$(git -C "$root" log -1 --format=%h -- guest/aosp tools/aosp guest/kernel/initramfs/vetro-files.c)"
  if [ -n "$(git -C "$root" status --porcelain -- guest/aosp tools/aosp)" ]; then
    echo "guest/aosp o tools/aosp hanno modifiche non committate: committa prima di pubblicare" >&2
    exit 1
  fi
  VETRO_AOSP_VERSION="${tag:-aosp}-${bid}-${rev}"
fi
prefix="aosp/$VETRO_AOSP_VERSION"
echo "versione: $VETRO_AOSP_VERSION"

# put FILE CHIAVE: carica se manca, verifica lo sha256 se c'è già.
put() {
  sha="$(shasum -a 256 "$1" | cut -d' ' -f1)"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$2" --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$have" = "$sha" ]; then
    echo "già presente: $2"
  elif [ -n "$have" ] && [ "$have" != None ]; then
    echo "ERRORE: $2 esiste con uno sha256 diverso ($have)" >&2
    exit 1
  else
    s3 s3 cp --only-show-errors --metadata "sha256=$sha" "$1" "s3://$R2_BUCKET/$2"
    echo "caricato: $2"
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
  echo "attenzione: niente sorgenti GPL (tools/aosp/gpl-sources.sh): l'immagine non va distribuita senza" >&2
fi

# Manifest: file, dimensione, sha256, URL pubblico.
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
