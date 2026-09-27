# shellcheck shell=sh
# Common configuration for the tools/aosp scripts (run from the Mac).
# The build machine is a Linux x86_64 VM (docs/specs/guest-image.md):
#   VETRO_AOSP_HOST  user@address (default: the contents of
#                    target/aosp/vm-host, in this tree or else in the main
#                    checkout of a worktree): if the VM is recreated the IP
#                    may change. No built-in address, so the scripts can go
#                    to the public repository (ADR 0034)
#   VETRO_AOSP_KEY   ssh key (default: ~/.ssh/vetro_aosp)
#   VETRO_AOSP_TREE  AOSP tree folder on the VM (default: aosp, in the home)
#   VETRO_AOSP_LUNCH lunch target (default: vetro_arm64-bp1a-userdebug)
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
if [ -z "${VETRO_AOSP_HOST:-}" ]; then
  main_out="$(git -C "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's|/\.git$||')/target/aosp"
  if [ -s "$out/vm-host" ]; then VETRO_AOSP_HOST="$(cat "$out/vm-host")"
  elif [ -s "$main_out/vm-host" ]; then VETRO_AOSP_HOST="$(cat "$main_out/vm-host")"
  else VETRO_AOSP_HOST=; fi
fi
need_host() {
  [ -n "$VETRO_AOSP_HOST" ] && return
  echo "no build VM address: set VETRO_AOSP_HOST or write user@address to $out/vm-host" >&2
  exit 3
}
VETRO_AOSP_KEY="${VETRO_AOSP_KEY:-$HOME/.ssh/vetro_aosp}"
VETRO_AOSP_TREE="${VETRO_AOSP_TREE:-aosp}"
VETRO_AOSP_LUNCH="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
# Vetro's working folder on the VM (remote scripts, patches, logs, artifacts).
VETRO_AOSP_WORK="${VETRO_AOSP_WORK:-vetro-aosp}"
ssh_opts="-i $VETRO_AOSP_KEY -o ConnectTimeout=20 -o ServerAliveInterval=30 -o StrictHostKeyChecking=accept-new"

vm() {
  need_host
  # shellcheck disable=SC2086
  ssh $ssh_opts "$VETRO_AOSP_HOST" "$@"
}

vm_rsync() {
  need_host
  rsync -e "ssh $ssh_opts" "$@"
}
