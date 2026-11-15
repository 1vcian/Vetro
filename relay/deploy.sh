#!/bin/sh
# Deploys the network gateway to Cloudflare Workers (ADR 0048) with the
# wrangler pinned in relay/package.json, on its workers.dev subdomain.
#
#   relay/deploy.sh            deploy (sets TOKEN_SECRET on the first run)
#   relay/deploy.sh --check    only the bundle (wrangler deploy --dry-run)
#
# Credentials: ~/.config/vetro/cloudflare.env with CLOUDFLARE_API_TOKEN and
# CLOUDFLARE_ACCOUNT_ID (the token needs Workers Scripts: Edit and, for the
# first deploy, Account Settings: Read and Workers Subdomain: Edit). They are
# exported to wrangler and never printed. The token secret of the sessions
# is generated here once and kept in ~/.config/vetro/gateway-secret
# (rotating it: delete the file and run again; every token becomes invalid).
# Prints the gateway's URL at the end.
set -eu
cd "$(dirname "$0")"
[ -d node_modules/wrangler ] || npm ci --no-audit --no-fund >/dev/null
if [ "${1:-}" = --check ]; then
  exec npx wrangler deploy --dry-run --outdir "${TMPDIR:-/tmp}/vetro-gateway-check"
fi
env_file=$HOME/.config/vetro/cloudflare.env
[ -f "$env_file" ] || { echo "missing $env_file (CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID)" >&2; exit 2; }
set -a
. "$env_file"
set +a
[ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ] || { echo "$env_file must set CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID" >&2; exit 2; }
secret_file=$HOME/.config/vetro/gateway-secret
new_secret=0
if [ ! -s "$secret_file" ]; then
  umask 077
  node -e "process.stdout.write(require('node:crypto').randomBytes(32).toString('base64url'))" > "$secret_file"
  new_secret=1
fi
npx wrangler deploy --var GATEWAY_ENABLED:1 2>&1 | tee "${TMPDIR:-/tmp}/vetro-gateway-deploy.log" | grep -vi token || true
grep -q "Deployed vetro-gateway\|Current Version ID" "${TMPDIR:-/tmp}/vetro-gateway-deploy.log" || { echo "deploy failed (log: ${TMPDIR:-/tmp}/vetro-gateway-deploy.log)" >&2; exit 1; }
if [ $new_secret = 1 ] || ! npx wrangler secret list 2>/dev/null | grep -q TOKEN_SECRET; then
  npx wrangler secret put TOKEN_SECRET < "$secret_file" >/dev/null
fi
url=$(grep -o 'https://vetro-gateway\.[a-z0-9-]*\.workers\.dev' "${TMPDIR:-/tmp}/vetro-gateway-deploy.log" | head -n 1)
echo "gateway: $url"
