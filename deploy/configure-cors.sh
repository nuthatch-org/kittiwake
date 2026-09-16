#!/usr/bin/env bash
# Tell kittiwake which browser origins may call it directly.
#
#   bash deploy/configure-cors.sh                       # the two dashboard origins
#   ORIGINS="https://a.example,https://b.example" bash deploy/configure-cors.sh
#   PATTERNS="https://graph-dashboard-|.vercel.app" bash deploy/configure-cors.sh   # + previews
#   bash deploy/configure-cors.sh --remove              # back to sending no CORS headers at all
#
# PATTERNS widens a credentialed allowlist and is empty unless you pass it. Each entry is
# `prefix|suffix` and the service refuses one loose enough to name a whole hosting provider. Read
# the note on `matches_pattern` in crates/edge before using it.
#
# Additive and reversible. Every request today arrives through the Vercel edge server-to-server
# with no `Origin` header, so a browser-facing header changes nothing about the traffic that
# exists; it only begins to matter when `src/proxy.ts` is deleted and browsers call this host.
#
# No secrets pass through here, unlike the other configure scripts, so the values are ordinary
# arguments. Nothing in `[cors]` is confidential: the whole point of the header is that it is sent
# to anyone who asks.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
ORIGINS=${ORIGINS:-https://www.lodestar-dashboard.com,https://lodestar-dashboard.com}

REMOVE=0
[ "${1:-}" = "--remove" ] && REMOVE=1

# Refused here as well as in the service, so a bad value is a sentence from this laptop rather than
# a process that will not restart after the config has been replaced.
if [ "$REMOVE" = 0 ]; then
  IFS=',' read -r -a LIST <<< "$ORIGINS"
  for o in "${LIST[@]}"; do
    case "$o" in
      '*')
        echo "\"*\" cannot carry the Dock's session cookie; it would sign users out." >&2; exit 1 ;;
      # A localhost origin was allowed here for local development, and production ended up serving
      # `access-control-allow-origin: http://localhost:3000` with credentials, which any page on a
      # visitor's own localhost could use. Local runs go through a same-origin proxy now (#151).
      http://localhost*)
        echo "$o would let any page on a visitor's localhost read credentialed answers. Run the dev server behind a same-origin proxy instead." >&2; exit 1 ;;
      https://*) ;;
      *) echo "$o is not an https origin" >&2; exit 1 ;;
    esac
    # Everything after the first `://`, whatever the scheme, so a port is not read as a path.
    host=${o#*://}
    case "$host" in
      */*) echo "$o has a path; an origin is scheme://host[:port]" >&2; exit 1 ;;
    esac
  done
  echo "allowing: ${LIST[*]}"
else
  echo "removing [cors]; the service will send no CORS headers"
fi

TOML=""
if [ "$REMOVE" = 0 ]; then
  QUOTED=$(printf '"%s", ' "${LIST[@]}")
  TOML=$(printf '\n[cors]\nallowed_origins = [%s]\n' "${QUOTED%, }")
  if [ -n "${PATTERNS:-}" ]; then
    IFS=',' read -r -a PLIST <<< "$PATTERNS"
    PQ=$(printf '"%s", ' "${PLIST[@]}")
    TOML="$TOML$(printf 'allowed_origin_patterns = [%s]\n' "${PQ%, }")"
    echo "and, by shape: ${PLIST[*]}"
  fi
fi

{
  printf '%s\n' "$REMOVE"
  printf '%s\n' "$(printf '%s' "$TOML" | base64)"
  cat <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
[ -f "$CFG" ] || { echo "no $CFG" >&2; exit 1; }
cp -p "$CFG" "$CFG.bak.$(date +%s)"

TMP=$(mktemp)
# Replaced wholesale rather than edited key by key, and the same `(?m)`-not-`(?s)` regex the other
# scripts use: with DOTALL the `.*` swallows the rest of the file in one bite and removing one
# section takes [nest] and its credentials with it.
SECTION="$(printf '%s' "$TOML_B64" | base64 -d)" python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys
src = open(sys.argv[1]).read()
src = re.sub(r'(?m)^\[cors\]\s*\n(?:(?!^\[).*\n?)*', '', src)
sys.stdout.write(src.rstrip() + '\n' + os.environ.get('SECTION', ''))
PY

if [ "$REMOVE" = 0 ]; then
  grep -q '^\[cors\]' "$TMP" || { echo "the rewrite produced no [cors]" >&2; rm -f "$TMP"; exit 1; }
else
  if grep -q '^\[cors\]' "$TMP"; then
    echo "[cors] survived the removal" >&2; rm -f "$TMP"; exit 1
  fi
fi
# The sections that must survive a rewrite of a different one. `[nest]` carries the credentials.
grep -q '^\[nest\]' "$TMP" || { echo "[nest] did not survive; nothing installed" >&2; rm -f "$TMP"; exit 1; }

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"
systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake
echo "health:"
curl -fsS --max-time 15 http://127.0.0.1:8080/api/health | head -c 160; echo
BOX
} | ssh -i "$KEY" "$HOST" 'read -r REMOVE; read -r TOML_B64; export REMOVE TOML_B64; bash -s'

cat <<'NEXT'

Config written. The thing to check now is the one the unit tests cannot: that a response actually
carries the header. A CorsLayer will not tell you afterwards what it was built to send.

  curl -sI -H 'Origin: https://www.lodestar-dashboard.com' \
    https://api.lodestar-dashboard.com/api/health | grep -i access-control

Want `access-control-allow-origin` echoing the origin back, and `access-control-allow-credentials:
true`.

An origin that is NOT on the list still gets `access-control-allow-credentials` and
`access-control-expose-headers` - tower-http sends those unconditionally - and does **not** get
`access-control-allow-origin`. That last one is the header the browser decides on, and without it
the response is refused no matter what the other two say, so this is untidy rather than permissive.
Checked from a real browser rather than reasoned about: a page on an allowed origin reads the body,
a page on example.com gets `TypeError: Failed to fetch`.
NEXT
