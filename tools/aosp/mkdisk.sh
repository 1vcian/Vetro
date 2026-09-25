#!/bin/sh
# Compone sul Mac il disco dell'immagine AOSP di Vetro (docs/specs/guest-image.md):
# un file GPT con le partizioni che il fstab (fstab.vetro) e la prima fase
# di init cercano per nome in /dev/block/by-name:
#   misc      1 MiB   zeri (messaggi del bootloader, bootctl)
#   frp       1 MiB   zeri (persistent data block)
#   metadata  64 MiB  ext4 vuoto (chiavi della cifratura dei metadati di /data)
#   super     super.img della build, espanso (partizioni logiche, slot A)
#   userdata  userdata.img della build, espanso (f2fs vuoto; vold lo cifra)
# Le partizioni sono allineate a 1 MiB, il file è sparso.
# Ingresso: target/aosp/out (tools/aosp/fetch.sh). Uscita: target/aosp/disk.img.
# Idempotente: si rifà solo se gli ingressi sono cambiati (SHA256SUMS).
# Strumenti (sgdisk, mkfs.ext4, simg2img) in un container Debian
# (Dockerfile.tools): sul Mac non ci sono.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
in="$out/out"
disk="$out/disk.img"
stamp="$out/disk.img.inputs"
[ -f "$in/super.img" ] && [ -f "$in/userdata.img" ] || { echo "mancano super.img/userdata.img in $in (tools/aosp/fetch.sh)" >&2; exit 1; }
want="$(grep -E ' \./(super|userdata)\.img$' "$in/SHA256SUMS" | sort; cat "$here/mkdisk.sh" | shasum -a 256)"
if [ -f "$disk" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
  echo "disco già aggiornato: $disk"
  exit 0
fi
image=vetro-aosp-tools:latest
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f "$here/Dockerfile.tools" "$here" >&2
rm -f "$disk" "$stamp"
docker run --rm -i -v "$out:/w" "$image" sh -eu -s <<'EOF'
cd /w
tmp=/tmp/parts
mkdir -p "$tmp"
# Le immagini della build possono essere sparse (formato Android) o grezze.
raw() {
  if [ "$(od -An -tx4 -N4 "$1" | tr -d ' ')" = ed26ff3a ]; then simg2img "$1" "$2"
  else cp --sparse=always "$1" "$2"; fi
}
raw out/super.img "$tmp/super.raw"
raw out/userdata.img "$tmp/userdata.raw"
truncate -s 64M "$tmp/metadata.raw"
mkfs.ext4 -q -L metadata "$tmp/metadata.raw"
mib() { echo $(( ($(stat -c %s "$1") + 1048575) / 1048576 )); }
super_mib=$(mib "$tmp/super.raw")
data_mib=$(mib "$tmp/userdata.raw")
# 1 MiB per la GPT in testa, 1 MiB per quella di riserva in coda.
total=$(( 1 + 1 + 1 + 64 + super_mib + data_mib + 1 ))
rm -f disk.img
truncate -s "${total}M" disk.img
sgdisk -a 2048 \
  -n 1:1M:+1M -c 1:misc \
  -n 2:0:+1M -c 2:frp \
  -n 3:0:+64M -c 3:metadata \
  -n 4:0:+${super_mib}M -c 4:super \
  -n 5:0:+${data_mib}M -c 5:userdata \
  disk.img >/dev/null
# Scrive ogni partizione al suo inizio (settori da 512 byte), saltando gli zeri.
put() {
  start=$(sgdisk -i "$1" disk.img | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
  dd if="$2" of=disk.img bs=1M seek=$((start / 2048)) conv=notrunc,sparse status=none
}
put 3 "$tmp/metadata.raw"
put 4 "$tmp/super.raw"
put 5 "$tmp/userdata.raw"
sgdisk -p disk.img
EOF
echo "$want" > "$stamp"
ls -lhs "$disk"
