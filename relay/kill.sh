#!/bin/sh
# Kill switch of the network gateway (ADR 0046): redeploys it with
# GATEWAY_ENABLED=0. New sessions and lanes get 503, and open lanes stop at
# their next ledger grant (at most 256 KiB later): the Durable Objects
# restart with the new code and refuse everything.
#
#   relay/kill.sh          off
#   relay/kill.sh --on     back on (the same as relay/deploy.sh)
#
# Credentials as in relay/deploy.sh (~/.config/vetro/cloudflare.env).
set -eu
cd "$(dirname "$0")"
if [ "${1:-}" = --on ]; then exec ./deploy.sh; fi
[ -d node_modules/wrangler ] || npm ci --no-audit --no-fund >/dev/null
env_file=$HOME/.config/vetro/cloudflare.env
[ -f "$env_file" ] || { echo "missing $env_file" >&2; exit 2; }
set -a
. "$env_file"
set +a
npx wrangler deploy --var GATEWAY_ENABLED:0 2>&1 | grep -vi token
echo "gateway disabled (relay/kill.sh --on to enable it again)"
