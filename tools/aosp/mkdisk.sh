#!/bin/sh
# Assembles on the Mac the disk of the Vetro AOSP image (docs/specs/guest-image.md):
# a GPT file with the partitions that the fstab (fstab.vetro) and init's first
# stage look up by name in /dev/block/by-name:
#   misc      1 MiB   zeros (bootloader messages, bootctl)
#   frp       1 MiB   zeros (persistent data block)
#   metadata  64 MiB  empty ext4 (keys for the metadata encryption of /data)
#   super     super.img from the build, expanded (logical partitions, slot A)
#   userdata  userdata.img from the build, expanded (empty f2fs; vold encrypts it)
# Partitions are aligned to 1 MiB, the file is sparse.
# Input: target/aosp/out (tools/aosp/fetch.sh). Output: target/aosp/disk.img.
# Idempotent: rebuilt only if the inputs changed (SHA256SUMS).
# Tools (sgdisk, mkfs.ext4, simg2img) in a Debian container
# (Dockerfile.tools): the Mac does not have them.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
in="$out/out"
disk="$out/disk.img"
stamp="$out/disk.img.inputs"
[ -f "$in/super.img" ] && [ -f "$in/userdata.img" ] || { echo "super.img/userdata.img missing in $in (tools/aosp/fetch.sh)" >&2; exit 1; }
want="$(grep -E ' \./(super|userdata)\.img$' "$in/SHA256SUMS" | sort; cat "$here/mkdisk.sh" | shasum -a 256)"
if [ -f "$disk" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
  echo "disk already up to date: $disk"
  exit 0
fi
image=vetro-aosp-tools:latest
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f "$here/Dockerfile.tools" "$here" >&2
rm -f "$disk" "$stamp"
docker run --rm -i -v "$out:/w" "$image" sh -eu -s <<'EOF'
cd /w
tmp=/tmp/parts
mkdir -p "$tmp"
# The build images can be sparse (Android format) or raw.
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
# 1 MiB for the GPT at the start, 1 MiB for the backup one at the end.
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
# Writes each partition at its start (512-byte sectors), skipping zeros.
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
