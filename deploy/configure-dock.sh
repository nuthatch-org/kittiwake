#!/usr/bin/env bash
# Give kittiwake the Dock's session secret, copied from Vercel rather than typed.
#
#   bash deploy/configure-dock.sh
#
# Run from a machine logged into both Vercel and the box. The value is read from the Lodestar
# project's production environment and written straight into /etc/kittiwake/config.toml over ssh.
# It is never echoed, and the pulled file is deleted on exit including on failure.
#
# WHY IT MUST BE THE INCUMBENT'S EXACT VALUE
#
# `SESSION_SECRET` signs the studio_session cookie, which is stateless. A secret differing by one
# byte does not error: it silently signs out every logged-in user the moment the edge points here.
# So this copies. It does not generate. `verify-session` at the end is what proves the copy took.
#
# It also REMOVES any `[scuttle]` section it finds. Scuttlebutt is gone, the binary no longer reads
# that section (it only warns about it) - so four unread secrets would otherwise sit in the config
# indefinitely, looking live to anyone who opened it.
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

# A Sensitive variable comes back as the literal `[SENSITIVE]` and `refuse_placeholder` catches it,
# which leaves no way to set it from here at all - hence the fallback to being typed. `ask_for`
# reads from stdin with echo off, never from an argument, which `ps` would show to the whole box.
ask_for() {
  local name="$1"
  printf 'paste %s (input hidden): ' "$name" >&2
  local v
  read -rs v
  printf '\n' >&2
  printf '%s' "$v"
}

# A plain variable rather than an associative array. macOS ships bash 3.2, where `declare -A` is a
# syntax error, and this is a script whose whole job is to be run from a laptop.
SECRET_SESSION=""

collect() {
  local name="$1" v
  v="$(val "$name")"
  if [ -z "$v" ] || ! refuse_placeholder "$name" "$v" 2>/dev/null; then
    echo "$name could not be read from Vercel." >&2
    v="$(ask_for "$name")"
  fi
  [ -n "$v" ] || { echo "$name is empty; nothing was changed" >&2; return 1; }
  refuse_placeholder "$name" "$v" || return 1
  printf '%s' "$v"
}

SECRET_SESSION="$(collect SESSION_SECRET)" || exit 1

# The service's own rules, mirrored exactly, so a value it will refuse is a sentence from here
# rather than a process that will not start after the config has been replaced.
#
# The floor here is the service's own, mirrored. It is worth remembering why it is the only one:
# on 2026-09-11 this script refused an eleven-character trip salt, the floor was argued away, and
# eleven characters went to production - because `vercel env pull` had returned `[SENSITIVE]`
# rather than the secret. The value was not short, it was absent. `refuse_placeholder` checks for
# that by name; length is not the thing that can catch it.
SS="$SECRET_SESSION"
[ "${#SS}" -ge 32 ] || {
  echo "SESSION_SECRET is shorter than 32 characters; kittiwake will refuse it" >&2; exit 1; }
unset SS

echo "writing [studio] to $HOST, and removing [scuttle] if it is still there"

# The value travels as the first line of the remote script's stdin, ahead of the script itself.
# Not as a command-line argument, which `ps` shows to every user on the box for as long as the
# command runs; and not through `SendEnv`, which needs a matching `AcceptEnv` in the server's sshd
# config and silently sends nothing when there isn't one.
{
  printf '%s\n' "$SECRET_SESSION"
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

# `[studio]` is replaced wholesale rather than edited key by key: a half-written section is exactly
# what the service refuses to start on, and a rewrite cannot leave one. `[scuttle]` is removed and
# not written back, which is how the four secrets behind a deleted feature stop living here.
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
""".format(session=os.environ['SESSION_SECRET'])
sys.stdout.write(src)
PY

grep -q '^\[studio\]' "$TMP" || { echo "the rewrite produced no [studio]" >&2; rm -f "$TMP"; exit 1; }
# `if` rather than `grep … && { … }`: under `set -e` the latter kills the script on the success
# path, because a grep that finds nothing exits 1 and that is the status of the whole expression.
if grep -q '^\[scuttle\]' "$TMP"; then
  echo "[scuttle] survived the rewrite; nothing was installed" >&2
  rm -f "$TMP"
  exit 1
fi

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
BOX
} | ssh -i "$KEY" "$HOST" 'read -r SESSION_SECRET; export SESSION_SECRET; bash -s'

cat <<'NEXT'

[studio] is in and [scuttle] is gone. One thing left, and it is the one that fails silently if it
is skipped: prove this service parses a cookie the incumbent minted.

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
