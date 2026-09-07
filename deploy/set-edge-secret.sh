#!/usr/bin/env bash
# Generate the shared secret that lets kittiwake believe the edge's X-Forwarded-For.
#
#   bash deploy/set-edge-secret.sh
#
# It mints one on the primary, writes it into /etc/kittiwake/config.toml, restarts, and then prints
# it **once** so you can paste it into Vercel as LODESTAR_EDGE_SECRET. That is the only moment it is
# ever displayed; after this it lives in a 0640 file on one box and in Vercel, and nowhere else.
#
# Why a secret at all: after the rewrite the chain is browser -> Vercel -> Caddy -> kittiwake, and
# `X-Forwarded-For` is the only thing carrying the browser's address. It is also a header anyone can
# send. Trusting it unconditionally lets a stranger choose their own rate-limit bucket; refusing it
# entirely puts every visitor in the world into one. The secret is what separates the two cases.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
CFG=/etc/kittiwake/config.toml
[ -f "$CFG" ] || { echo "no $CFG" >&2; exit 1; }

SECRET=$(head -c 48 /dev/urandom | base64 | tr -d '/+=\n' | cut -c1-40)
[ ${#SECRET} -ge 32 ] || { echo "could not generate a secret" >&2; exit 1; }

cp -a "$CFG" "$CFG.bak.$(date -u +%Y%m%dT%H%M%SZ)"
umask 077
TMP=$(mktemp)
SECRET="$SECRET" python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys
line = f'edge_secret = "{os.environ["SECRET"]}"\n'
out, done = [], False
for raw in open(sys.argv[1]):
    if re.match(r'^\s*#?\s*edge_secret\s*=', raw):
        out.append(line); done = True
    else:
        out.append(raw)
if not done:
    # Top level, so it lands before the first [section] header and not inside one.
    for i, l in enumerate(out):
        if l.lstrip().startswith('['):
            out.insert(i, line + "\n"); done = True; break
    if not done:
        out.append("\n" + line)
sys.stdout.write("".join(out))
PY
grep -q '^edge_secret' "$TMP" || { echo "the rewrite produced no edge_secret line" >&2; rm -f "$TMP"; exit 1; }
install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"

systemctl restart kittiwake
sleep 4
systemctl --no-pager --lines=0 status kittiwake | head -3
echo
echo "warnings about a missing edge_secret in this boot (want 0):"
journalctl -u kittiwake --no-pager "_PID=$(systemctl show kittiwake -p MainPID --value)" \
  | grep -c "no edge_secret" | sed 's/^/  /'
echo
echo "Paste this into Vercel as LODESTAR_EDGE_SECRET, then clear your scrollback:"
echo
echo "    $SECRET"
echo
BOX
