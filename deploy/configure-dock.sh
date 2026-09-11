#!/usr/bin/env bash
# Give kittiwake the Dock's and Scuttlebutt's secrets, copied from Vercel rather than typed.
#
#   bash deploy/configure-dock.sh
#
# Run from a machine logged into both Vercel and the box. The four values are read from the
# Lodestar project's production environment and written straight into /etc/kittiwake/config.toml
# over ssh. They are never echoed, and the pulled file is deleted on exit including on failure.
#
# WHY THEY MUST BE THE INCUMBENT'S EXACT VALUES
#
# Each one signs or derives something that already exists in somebody's browser.
#
#   SESSION_SECRET            signs the studio_session cookie, which is stateless. A secret
#                             differing by one byte does not error: it silently signs out every
#                             logged-in user the moment the edge points here.
#   SCUTTLEBUTT_TRIP_SALT     derives the tripcode that identifies a poster across posts. Change it
#                             and every existing tripcode stops matching its author.
#   SCUTTLEBUTT_IP_PEPPER     derives the hash a ban is recorded against. Change it and every
#                             existing ban stops matching anybody.
#   SCUTTLEBUTT_ADMIN_SECRET  the moderator login.
#
# So this copies. It does not generate. `verify-session` at the end is what proves the copy took,
# and it is worth running before the routes are switched rather than after.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

# `vercel env pull` reads the project linked to the working directory, and the linked project is
# lodestar's - this repository is not linked to anything, so running it here fails with "your
# codebase isn't linked to a project". Default to the sibling checkout and say so plainly when it
# is somewhere else.
LODESTAR_DIR=${LODESTAR_DIR:-$(cd "$(dirname "$0")/../../lodestar" 2>/dev/null && pwd || true)}
if [ -z "$LODESTAR_DIR" ] || [ ! -d "$LODESTAR_DIR/.vercel" ]; then
  echo "cannot find a Vercel-linked lodestar checkout." >&2
  echo "Set LODESTAR_DIR to it, e.g. LODESTAR_DIR=~/Projects/lodestar bash $0" >&2
  exit 1
fi

ENVFILE=$(mktemp)
trap 'rm -f "$ENVFILE"' EXIT
chmod 600 "$ENVFILE"

echo "reading the production environment from Vercel ($LODESTAR_DIR)"
( cd "$LODESTAR_DIR" && vercel env pull "$ENVFILE" --environment=production --yes >/dev/null )

# Everything after the first `=`, with surrounding quotes removed. A value may legitimately contain
# `=`, so `cut -f2` would truncate one and the failure would look like a wrong secret.
val() { grep -m1 "^$1=" "$ENVFILE" | cut -d= -f2- | sed -E 's/^"(.*)"$/\1/'; }

. "$(dirname "$0")/secret-guard.sh"

# Three of these four are marked Sensitive, so the pull returns `[SENSITIVE]` for them and the loop
# below refuses it. That leaves no way to set them from here at all, which is why each value falls
# back to being typed: `ask_for` reads from stdin with echo off, never an argument.
#
# Pasting the incumbent's own values is the point. A fresh trip salt orphans every tripcode in the
# room's history; the originals restore it, and only the window since 2026-09-11 is lost.
ask_for() {
  local name="$1"
  printf 'paste %s (input hidden): ' "$name" >&2
  local v
  read -rs v
  printf '\n' >&2
  printf '%s' "$v"
}

declare -A SECRET
for k in SESSION_SECRET SCUTTLEBUTT_TRIP_SALT SCUTTLEBUTT_IP_PEPPER SCUTTLEBUTT_ADMIN_SECRET; do
  v="$(val "$k")"
  if [ -z "$v" ] || ! refuse_placeholder "$k" "$v" 2>/dev/null; then
    echo "$k could not be read from Vercel." >&2
    v="$(ask_for "$k")"
  fi
  [ -n "$v" ] || { echo "$k is empty; nothing was changed" >&2; exit 1; }
  refuse_placeholder "$k" "$v" || exit 1
  SECRET[$k]="$v"
done

# The service's own rules, mirrored exactly, so a value it will refuse is a sentence from here
# rather than a process that will not start after the config has been replaced.
#
# Only the session secret has a length floor, and that is still right: whatever length the
# incumbent's trip salt happens to be is the length that derives every tripcode in the room, so a
# minimum invented here could refuse a good salt.
#
# What was wrong was concluding from that that a short value must be fine. On 2026-09-11 this
# script refused an eleven-character trip salt, the floor was removed, and eleven characters went
# to production - because `vercel env pull` had returned `[SENSITIVE]` rather than the secret. The
# value was not short, it was absent. `refuse_placeholder` checks for that by name; length is not
# the thing that can catch it.
SS="${SECRET[SESSION_SECRET]}"
[ "${#SS}" -ge 32 ] || {
  echo "SESSION_SECRET is shorter than 32 characters; kittiwake will refuse it" >&2; exit 1; }
unset SS

echo "writing [studio] and [scuttle] to $HOST"

# The four values travel as the first four lines of the remote script's stdin, ahead of the script
# itself. Not as command-line arguments, which `ps` shows to every user on the box for as long as
# the command runs; and not through `SendEnv`, which needs a matching `AcceptEnv` in the server's
# sshd config and silently sends nothing when there isn't one.
{
  printf '%s\n' "${SECRET[SESSION_SECRET]}"
  printf '%s\n' "${SECRET[SCUTTLEBUTT_TRIP_SALT]}"
  printf '%s\n' "${SECRET[SCUTTLEBUTT_IP_PEPPER]}"
  printf '%s\n' "${SECRET[SCUTTLEBUTT_ADMIN_SECRET]}"
  cat <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
umask 077

[ -f "$CFG" ] || { echo "no $CFG - run deploy/install.sh first" >&2; exit 1; }

cp -p "$CFG" "$CFG.bak.$(date +%s)"

TMP=$(mktemp)
python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys

# Both sections are replaced wholesale rather than edited key by key. A half-written section is
# exactly what the service refuses to start on, and a rewrite cannot leave one.
#
# `(?m)` and deliberately not `(?s)`. With DOTALL the `.*` swallows the rest of the file in one
# bite and the per-line lookahead never fires, so removing [studio] took [nest] and its credentials
# with it. Tested against a config carrying [nest], [nest.permits] and a top-level key: all three
# survive, and running it twice does not stack the sections.
src = open(sys.argv[1]).read()
for name in ('studio', 'scuttle'):
    src = re.sub(r'(?m)^\[%s\]\s*\n(?:(?!^\[).*\n?)*' % name, '', src)

src = src.rstrip() + """

[studio]
session_secret = "{session}"

[scuttle]
trip_salt = "{trip}"
ip_pepper = "{pepper}"
admin_secret = "{admin}"
session_secret = "{session}"
""".format(
    session=os.environ['SESSION_SECRET'],
    trip=os.environ['TRIP_SALT'],
    pepper=os.environ['IP_PEPPER'],
    admin=os.environ['ADMIN_SECRET'],
)
sys.stdout.write(src)
PY

grep -q '^\[studio\]' "$TMP" && grep -q '^\[scuttle\]' "$TMP" \
  || { echo "the rewrite produced no [studio]/[scuttle]" >&2; rm -f "$TMP"; exit 1; }

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"
echo "config written; a timestamped backup of the previous one is beside it"

systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake

# Starting is not the proof. A half-configured Dock refuses to expose itself and says so only in
# the log, and the route answers the same "unavailable" it did before.
echo "studio auth (want {\"address\":null} rather than an error):"
curl -fsS --max-time 15 http://127.0.0.1:8080/api/studio/auth | head -c 120; echo
echo "scuttlebutt (want a messages array rather than 'not configured'):"
curl -fsS --max-time 15 http://127.0.0.1:8080/api/scuttlebutt/messages | head -c 120; echo
BOX
} | ssh -i "$KEY" "$HOST" 'read -r SESSION_SECRET; read -r TRIP_SALT; read -r IP_PEPPER; read -r ADMIN_SECRET; export SESSION_SECRET TRIP_SALT IP_PEPPER ADMIN_SECRET; bash -s'

cat <<'NEXT'

Both sections are in. One thing left before the routes are switched, and it is the one that fails
silently if it is skipped: prove this service parses a cookie the incumbent minted.

  1. Mint one with Lodestar's own implementation, on a machine with SESSION_SECRET:

       node -e 'const{createHmac}=require("crypto");
                const a="0x0000000000000000000000000000000000000001";
                const p=`${a}:${Math.floor(Date.now()/1000)}`;
                console.log(`${p}:${createHmac("sha256",process.env.SESSION_SECRET).update(p).digest("hex")}`)'

     A real studio_session cookie out of your own browser is better still, because it proves the
     value that is actually in circulation.

  2. Parse it here:

       DOCK_SESSION_SECRET='<the same secret>' DOCK_SESSION_TOKEN='<the token>' \
         cargo run -p kittiwake-dock --bin verify-session

It checks three things, and the third is what makes the first mean anything: the token parses, a
token with one byte of its signature changed is rejected, and a token with one byte of its payload
changed is rejected too. Green means the two implementations agree and the cutover keeps everyone
signed in.
NEXT
