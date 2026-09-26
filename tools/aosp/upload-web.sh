#!/bin/sh
# Carica su R2, accanto a una versione già pubblicata dell'immagine AOSP di
# Vetro, i file per il browser (ADR 0028): la mappa del disco e i suoi
# blocchi (tools/aosp/web-disk.mjs), in aosp/<versione>/web/:
#   web/disk.json      mappa del disco GPT ricomposto da super.img e
#                      userdata.img (sparsi) già pubblicati
#   web/disk-head.bin  GPT e metadata (pochi KiB)
# I file della versione non cambiano: si aggiungono solo quelli di web/. Un
# oggetto già presente con lo stesso sha256 non si ricarica; uno diverso è un
# errore (stessa regola di upload.sh).
# Uso: VETRO_AOSP_VERSION=<versione> tools/aosp/upload-web.sh
# (default: la versione del manifest in target/aosp/out/manifest.json).
# Credenziali in ~/.config/vetro/r2.env, mai stampate.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp/out"
env_file="${VETRO_R2_ENV:-$HOME/.config/vetro/r2.env}"
[ -f "$out/web/disk.json" ] && [ -f "$out/web/disk-head.bin" ] || { echo "manca out/web: node tools/aosp/web-disk.mjs" >&2; exit 1; }
if [ -z "${VETRO_AOSP_VERSION:-}" ]; then
  VETRO_AOSP_VERSION="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$out/manifest.json" | head -1)"
fi
[ -n "$VETRO_AOSP_VERSION" ] || { echo "versione sconosciuta (VETRO_AOSP_VERSION)" >&2; exit 1; }
# shellcheck disable=SC1090
. "$env_file"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto
unset R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }
prefix="aosp/$VETRO_AOSP_VERSION"
# La mappa punta ai file della versione: devono esserci, con la stessa dimensione.
for f in super.img userdata.img; do
  want="$(stat -f %z "$out/$f")"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$prefix/$f" --query ContentLength --output text)"
  [ "$want" = "$have" ] || { echo "ERRORE: $prefix/$f su R2 ha $have byte, in locale $want" >&2; exit 1; }
done
put() {
  sha="$(shasum -a 256 "$1" | cut -d' ' -f1)"
  have="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$2" --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$have" = "$sha" ]; then
    echo "già presente: $2"
  elif [ -n "$have" ] && [ "$have" != None ]; then
    echo "ERRORE: $2 esiste con uno sha256 diverso ($have)" >&2
    exit 1
  else
    s3 s3 cp --only-show-errors --metadata "sha256=$sha" --content-type "$3" "$1" "s3://$R2_BUCKET/$2"
    echo "caricato: $2"
  fi
}
put "$out/web/disk-head.bin" "$prefix/web/disk-head.bin" application/octet-stream
put "$out/web/disk.json" "$prefix/web/disk.json" application/json
echo "$R2_PUBLIC_URL/$prefix/web/disk.json"
