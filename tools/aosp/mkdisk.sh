#!/bin/sh
# Composes on the Mac the disk of Vetro's AOSP image (docs/specs/guest-image.md):
# a GPT file with the partitions that the fstab (fstab.vetro) and init's first
# stage look up by name in /dev/block/by-name (misc, frp, metadata, super,
# userdata; layout in tools/aosp/remote/disk-layout.sh, shared with the build
# VM's tools/aosp/remote/qemu.sh).
# Input: target/aosp/out (tools/aosp/fetch.sh). Output: target/aosp/disk.img.
# Idempotent: redone only if the inputs changed (SHA256SUMS and the scripts).
# Tools (sgdisk, mkfs.ext4, simg2img) in a Debian container
# (Dockerfile.tools): the Mac doesn't have them.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
in="$out/out"
disk="$out/disk.img"
stamp="$out/disk.img.inputs"
layout="$here/remote/disk-layout.sh"
[ -f "$in/super.img" ] && [ -f "$in/userdata.img" ] || { echo "super.img/userdata.img missing in $in (tools/aosp/fetch.sh)" >&2; exit 1; }
want="$(grep -E ' \./(super|userdata)\.img$' "$in/SHA256SUMS" | sort; cat "$here/mkdisk.sh" "$layout" | shasum -a 256)"
if [ -f "$disk" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
  echo "disk already up to date: $disk"
  exit 0
fi
image=vetro-aosp-tools:latest
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f "$here/Dockerfile.tools" "$here" >&2
rm -f "$disk" "$stamp"
docker run --rm -i -v "$out:/w" -w /w "$image" sh -eu -s < "$layout"
echo "$want" > "$stamp"
ls -lhs "$disk"
