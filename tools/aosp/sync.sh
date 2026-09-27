#!/bin/sh
# Brings guest/aosp to the build VM and prepares the AOSP tree (idempotent):
#   guest/aosp/device/vetro  -> $TREE/device/vetro   (exact copy)
#   guest/kernel/initramfs/vetro-files.c -> $TREE/device/vetro/vetro_arm64/vetro-files/
#   guest/aosp/vendor/vetro  -> $TREE/vendor/vetro   (exact copy, except the downloaded prebuilts)
#   guest/aosp/patches, tools/aosp/remote -> ~/$WORK
# then, on the VM, applies the patches (skipping those already applied) and downloads the
# microG releases with the sha256 from microg.lock (tools/aosp/remote/prepare.sh).
# First it checks the development CA (tools/aosp/dev-ca.sh: certificate and patches
# consistent) and writes to ~/$WORK/sync.rev the synced Vetro commit
# (last commit of guest/aosp, tools/aosp and vetro-files.c, with "-dirty" if
# there are uncommitted changes): the build records it in build-info.txt and
# upload.sh derives the version from it.
# Usage: tools/aosp/sync.sh      (see common.sh for the variables)
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
g="$root/guest/aosp"
"$here/dev-ca.sh" check
paths="guest/aosp tools/aosp guest/kernel/initramfs/vetro-files.c"
# shellcheck disable=SC2086
rev="$(git -C "$root" log -1 --format=%h -- $paths)"
# shellcheck disable=SC2086
if [ -n "$(git -C "$root" status --porcelain -- $paths)" ]; then rev="$rev-dirty"; fi
vm "mkdir -p $VETRO_AOSP_TREE/device/vetro $VETRO_AOSP_TREE/vendor/vetro $VETRO_AOSP_WORK"
vm_rsync -a --delete --exclude '/vetro_arm64/vetro-files/vetro-files.c' \
  "$g/device/vetro/" "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/device/vetro/"
# The file manager daemon (ADR 0020) has a single source, in guest/kernel.
vm_rsync -a "$root/guest/kernel/initramfs/vetro-files.c" \
  "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/device/vetro/vetro_arm64/vetro-files/vetro-files.c"
vm_rsync -a --delete --exclude '/microg/prebuilt/' "$g/vendor/vetro/" "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/vendor/vetro/"
vm_rsync -a --delete "$g/patches/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/patches/"
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK sh $VETRO_AOSP_WORK/remote/prepare.sh"
vm "echo $rev > $VETRO_AOSP_WORK/sync.rev"
echo "synced: Vetro $rev -> $VETRO_AOSP_HOST"
