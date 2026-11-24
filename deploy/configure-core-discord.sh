#!/usr/bin/env bash
# Point the core subgraph watch at a Discord channel's webhook.
#
#   bash deploy/configure-core-discord.sh             # prompts for the webhook URL, not echoed
#   bash deploy/configure-core-discord.sh --remove    # stop posting; /api/core-subgraphs stays
#
# The URL carries the webhook's token, so it is read without echo and travels to the box as the
# first line of stdin, as configure-tap.sh sends its key: never as an argument `ps` would show.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

REMOVE=0
[ "${1:-}" = "--remove" ] && REMOVE=1

HOOK=""
if [ "$REMOVE" = 0 ]; then
  read -r -s -p "Discord webhook URL: " HOOK; echo
  case "$HOOK" in
    https://discord.com/api/webhooks/*|https://discordapp.com/api/webhooks/*) ;;
    *) echo "that is not a Discord webhook URL" >&2; exit 1 ;;
  esac
fi

{
  printf '%s\n' "$HOOK"
  cat <<BOX
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
umask 077
[ -f "\$CFG" ] || { echo "no \$CFG - run deploy/install.sh first" >&2; exit 1; }
cp -p "\$CFG" "\$CFG.bak.\$(date +%s)"
TMP=\$(mktemp)
REMOVE=$REMOVE python3 - "\$CFG" > "\$TMP" <<'PY'
import os, re, sys
src = open(sys.argv[1]).read()
src = re.sub(r'(?m)^\[core_subgraphs_discord\]\s*\n(?:(?!^\[).*\n?)*', '', src)
if os.environ['REMOVE'] == '0':
    src = src.rstrip() + '\n\n[core_subgraphs_discord]\nurl = "%s"\n' % os.environ['HOOK_URL']
sys.stdout.write(src)
PY
grep -q '^\[nest\]' "\$TMP" || { echo "the rewrite lost [nest]" >&2; rm -f "\$TMP"; exit 1; }
install -m 0640 -o root -g kittiwake "\$TMP" "\$CFG"
rm -f "\$TMP"
systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake
BOX
} | ssh -i "$KEY" "$HOST" 'read -r HOOK_URL; export HOOK_URL; bash -s'

if [ "$REMOVE" = 0 ]; then
  echo "Set. The first run records the current versions without posting; the next new version posts."
fi
