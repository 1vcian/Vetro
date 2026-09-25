# shellcheck shell=sh
# Configurazione comune degli script di tools/aosp (eseguiti dal Mac).
# La macchina di build è una VM Linux x86_64 (docs/specs/guest-image.md):
#   VETRO_AOSP_HOST  utente@indirizzo (default: il contenuto di
#                    target/aosp/vm-host, poi vetro@34.154.147.83); la VM è
#                    Spot e al riavvio l'IP può cambiare
#   VETRO_AOSP_KEY   chiave ssh (default: ~/.ssh/vetro_aosp)
#   VETRO_AOSP_TREE  cartella del tree AOSP sulla VM (default: aosp, nella home)
#   VETRO_AOSP_LUNCH target di lunch (default: vetro_arm64-bp1a-userdebug)
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/aosp"
if [ -z "${VETRO_AOSP_HOST:-}" ]; then
  if [ -s "$out/vm-host" ]; then VETRO_AOSP_HOST="$(cat "$out/vm-host")"
  else VETRO_AOSP_HOST=vetro@34.154.147.83; fi
fi
VETRO_AOSP_KEY="${VETRO_AOSP_KEY:-$HOME/.ssh/vetro_aosp}"
VETRO_AOSP_TREE="${VETRO_AOSP_TREE:-aosp}"
VETRO_AOSP_LUNCH="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
# Cartella di lavoro di Vetro sulla VM (script remoti, patch, log, artefatti).
VETRO_AOSP_WORK="${VETRO_AOSP_WORK:-vetro-aosp}"
ssh_opts="-i $VETRO_AOSP_KEY -o ConnectTimeout=20 -o ServerAliveInterval=30 -o StrictHostKeyChecking=accept-new"

vm() {
  # shellcheck disable=SC2086
  ssh $ssh_opts "$VETRO_AOSP_HOST" "$@"
}

vm_rsync() {
  rsync -e "ssh $ssh_opts" "$@"
}
