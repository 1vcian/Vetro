# shellcheck shell=sh
# Common configuration for the tools/aosp scripts (run from the Mac).
# The build machine is a Linux x86_64 VM (docs/specs/guest-image.md):
#   VETRO_AOSP_HOST  user@address (default: the contents of
#                    target/aosp/vm-host, then vetro@34.154.15.143): if the VM
#                    is recreated the IP may change
#   VETRO_AOSP_KEY   ssh key (default: ~/.ssh/vetro_aosp)
#   VETRO_AOSP_TREE  AOSP tree folder on the VM (default: aosp, in the home)
#   VETRO_AOSP_LUNCH lunch target (default: vetro_arm64-bp1a-userdebug)
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
if [ -z "${VETRO_AOSP_HOST:-}" ]; then
  if [ -s "$out/vm-host" ]; then VETRO_AOSP_HOST="$(cat "$out/vm-host")"
  else VETRO_AOSP_HOST=vetro@34.154.15.143; fi
fi
VETRO_AOSP_KEY="${VETRO_AOSP_KEY:-$HOME/.ssh/vetro_aosp}"
VETRO_AOSP_TREE="${VETRO_AOSP_TREE:-aosp}"
VETRO_AOSP_LUNCH="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
# Vetro's working folder on the VM (remote scripts, patches, logs, artifacts).
VETRO_AOSP_WORK="${VETRO_AOSP_WORK:-vetro-aosp}"
ssh_opts="-i $VETRO_AOSP_KEY -o ConnectTimeout=20 -o ServerAliveInterval=30 -o StrictHostKeyChecking=accept-new"

vm() {
  # shellcheck disable=SC2086
  ssh $ssh_opts "$VETRO_AOSP_HOST" "$@"
}

vm_rsync() {
  rsync -e "ssh $ssh_opts" "$@"
}
