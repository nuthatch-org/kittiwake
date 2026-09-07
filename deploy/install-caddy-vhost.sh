#!/usr/bin/env bash
# Put api.lodestar-dashboard.com in front of kittiwake.
#
#   bash deploy/install-caddy-vhost.sh
#
# Run it AFTER the DNS A record exists and has propagated. Caddy asks Let's Encrypt for a
# certificate on reload and the ACME HTTP-01 challenge needs the name to resolve to this box; run it
# too early and Caddy retries on a backoff that gets slow enough to look broken.
#
# Idempotent. It appends the vhost if it is absent, validates the whole file before touching the
# live one, and leaves a timestamped backup either way.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
NAME=${NAME:-api.lodestar-dashboard.com}
FRAGMENT=$(dirname "$0")/Caddyfile.fragment

[ -r "$FRAGMENT" ] || { echo "no $FRAGMENT" >&2; exit 1; }

# Resolve first, and refuse rather than let Caddy spend an hour backing off against a name that
# does not exist yet. Checked from here, which is not where Let's Encrypt checks from, so this is a
# guard against the obvious mistake rather than a guarantee.
resolved=$(dig +short "$NAME" A | head -1)
if [ -z "$resolved" ]; then
  echo "$NAME does not resolve yet. Add the A record first; DNS is step 2, this is step 3." >&2
  exit 1
fi
echo "$NAME resolves to $resolved"

# The fragment travels as an environment variable rather than on stdin, because stdin is already
# carrying the script itself. Feeding ssh two of them silently gets you one.
FRAG_TEXT=$(cat "$FRAGMENT")

ssh -i "$KEY" "$HOST" "NAME='$NAME' FRAG=$(printf %q "$FRAG_TEXT") bash -s" <<'BOX'
set -euo pipefail
CADDYFILE=/etc/caddy/Caddyfile

if grep -q "^$NAME {" "$CADDYFILE"; then
  echo "$NAME is already in the Caddyfile; leaving it alone"
  exit 0
fi

cp -a "$CADDYFILE" "$CADDYFILE.bak.$(date -u +%Y%m%dT%H%M%SZ)"
TMP=$(mktemp)
cat "$CADDYFILE" > "$TMP"
printf '\n%s\n' "$FRAG" >> "$TMP"

# Validate before the live file is touched. A Caddyfile that does not parse takes every vhost on
# this box down, including api.studiozemya.com, and this box has no second chance at that.
caddy validate --config "$TMP" --adapter caddyfile
install -m 0644 -o root -g root "$TMP" "$CADDYFILE"
rm -f "$TMP"

systemctl reload caddy
sleep 5
echo "certificate and vhost:"
curl -fsS --max-time 30 "https://$NAME/health" | head -c 200 || \
  echo "  not answering yet - a first certificate can take a minute; check: journalctl -u caddy -n 30"
BOX
