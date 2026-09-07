#!/usr/bin/env bash
# Give kittiwake the Lodestar database, without the URL ever touching argv, a file or a log.
#
# Run it from your laptop. It prompts for the URL with the terminal echo off, pipes it to the
# primary over ssh's stdin, and the box-side half writes it into /etc/kittiwake/config.toml and
# restarts the unit. Nothing is written to disk locally, nothing appears in `ps`, and nothing lands
# in your shell history - which is the whole reason this is a script rather than three commands.
#
#   bash deploy/set-database-url.sh
#
# The value is the one Vercel holds as DATABASE_URL. It is a Sensitive variable there, so
# `vercel env pull` returns "[SENSITIVE]" and you have to copy it out of the dashboard UI. On the
# box Postgres is local, so rewrite the host as 127.0.0.1:5433 - the script insists on that and
# will refuse a URL still pointing at a public hostname.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

printf 'Paste the Lodestar DATABASE_URL (input hidden), then Enter:\n> ' >&2
IFS= read -rs URL
printf '\n' >&2
[ -n "$URL" ] || { echo "nothing entered" >&2; exit 1; }

case "$URL" in
  postgres://*|postgresql://*) ;;
  *) echo "that is not a postgres URL" >&2; exit 1 ;;
esac
case "$URL" in
  *127.0.0.1:5433*|*localhost:5433*) ;;
  *) echo "the host must be 127.0.0.1:5433 - Postgres is local to the box, and pointing kittiwake" >&2
     echo "at the public hostname would send every query out and back through Hetzner's network" >&2
     exit 1 ;;
esac
case "$URL" in
  *sslmode=*) ;;
  *) echo "no sslmode in the URL; Postgres on this box requires TLS. Add ?sslmode=require" >&2
     exit 1 ;;
esac

printf '%s' "$URL" | ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
CFG=/etc/kittiwake/config.toml
URL=$(cat)

[ -w "$CFG" ] || { echo "cannot write $CFG" >&2; exit 1; }

# Keep a copy before touching it. Mode 0640 root:kittiwake, same as the original, because a
# world-readable backup of a file containing two passwords is not a backup, it is a leak.
cp -a "$CFG" "$CFG.bak.$(date -u +%Y%m%dT%H%M%SZ)"

# Replace an existing database_url, or fall back to inserting one where the commented-out line is.
# Written through a temp file with a tight umask and moved into place, so there is no window in
# which the config exists with default permissions.
umask 077
TMP=$(mktemp)
URL="$URL" python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys
url = os.environ["URL"]
line = f'database_url = "{url}"\n'
out, replaced = [], False
for raw in open(sys.argv[1]):
    if re.match(r'^\s*database_url\s*=', raw):
        out.append(line); replaced = True
    elif not replaced and re.match(r'^\s*#\s*database_url\s*=', raw):
        out.append(line); replaced = True
    else:
        out.append(raw)
if not replaced:
    out.append("\n" + line)
sys.stdout.write("".join(out))
PY
grep -q '^database_url' "$TMP" || { echo "the rewrite produced no database_url line" >&2; rm -f "$TMP"; exit 1; }

# The flag that decides whether this process becomes a second writer to tables the platform's crons
# are still writing. Adding a pool must not quietly turn it on, so check rather than assume.
if grep -qE '^\s*scheduler_enabled\s*=\s*true' "$TMP"; then
  echo "refusing: scheduler_enabled is true. A pool plus the scheduler makes kittiwake a second" >&2
  echo "writer to Lodestar's Postgres, which is a decision to take deliberately and separately." >&2
  rm -f "$TMP"; exit 1
fi

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"
unset URL

systemctl restart kittiwake
sleep 4
systemctl --no-pager --lines=0 status kittiwake | head -3
echo

# The proof is not that it started; it is that the pool connects and a Postgres route answers with
# rows. A misconfigured URL starts perfectly happily and serves "unavailable" for ever.
# Scoped to this boot's PID, not to a time window. A window spans the previous start as well, so a
# restart that happened a minute ago reports its warnings as if they were this one's - which is a
# check that can only ever raise a false alarm, and a false alarm is how a real one gets ignored.
echo "startup warnings about the database (want 0):"
journalctl -u kittiwake --no-pager "_PID=$(systemctl show kittiwake -p MainPID --value)" \
  | grep -c "no database_url" | sed 's/^/  /'
echo "indexers-enriched:"
curl -fsS --max-time 20 http://127.0.0.1:8080/api/indexers-enriched?first=3 \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("  rows:", len(d.get("data") or []), "served_from:", (d.get("provenance") or {}).get("served_from"))' \
  || echo "  the route did not answer - check journalctl -u kittiwake"
BOX
