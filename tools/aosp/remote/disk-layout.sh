#!/bin/sh
# Composes the GPT disk of Vetro's AOSP image (docs/specs/guest-image.md) in
# the current directory: reads out/super.img and out/userdata.img (Android
# sparse or raw), writes disk.img. Run by tools/aosp/mkdisk.sh (Mac, inside
# the Debian container of Dockerfile.tools) and by tools/aosp/remote/qemu.sh
# (build VM). Needs simg2img, sgdisk, mkfs.ext4, truncate, dd.
#   misc      1 MiB   zeros (bootloader messages, bootctl)
#   frp       1 MiB   zeros (persistent data block)
#   metadata  64 MiB  empty ext4 (keys of /data's metadata encryption)
#   super     super.img, expanded (logical partitions, slot A)
#   userdata  userdata.img, expanded (empty f2fs; vold encrypts it)
# Partitions aligned to 1 MiB, sparse file.
set -eu
tmp="${TMPDIR:-/tmp}/vetro-disk.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT
# The build's images can be sparse (Android format) or raw.
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
