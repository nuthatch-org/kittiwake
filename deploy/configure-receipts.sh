#!/usr/bin/env bash
# Give kittiwake the key that signs SQL receipts, copied from Vercel rather than typed.
#
#   bash deploy/configure-receipts.sh
#
# Run from a machine logged into both Vercel and the box. The value is read from the Lodestar
# project's production environment and written into /etc/kittiwake/config.toml over ssh. It is never
# echoed, and the pulled env file is deleted on exit including on failure.
#
# WHY IT MUST BE THE INCUMBENT'S EXACT VALUE
#
# The issuer key is an ed25519 seed, and its public half is what a receipt is checked against.
# Every receipt this deployment has already issued names that public key. Generate a new one and
# those receipts do not become invalid-looking - they become unverifiable, signed by a key nobody
# can look up any more, which is a worse failure because it looks like the receipt was forged.
#
# Absent is a valid posture, and the route says so rather than erroring: "this deployment does not
# issue receipts; sign the answer yourself with tattler attest-named". That is what it answers now.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

# `vercel env pull` reads the project linked to the working directory, and the linked project is
# lodestar's; this repository is linked to nothing.
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

val() { grep -m1 "^$1=" "$ENVFILE" | cut -d= -f2- | sed -E 's/^"(.*)"$/\1/'; }

ISSUER=$(val TATTLER_ISSUER_KEY)
[ -n "$ISSUER" ] || { echo "TATTLER_ISSUER_KEY is not set in the production environment" >&2; exit 1; }

# The service's own rule, mirrored: 32 bytes of hexadecimal. Checked here so a bad value is a
# sentence from this script rather than a process that will not start after the config is replaced.
case "$ISSUER" in
  *[!0-9a-fA-F]*) echo "TATTLER_ISSUER_KEY is not hexadecimal" >&2; exit 1 ;;
esac
[ "${#ISSUER}" -eq 64 ] || {
  echo "TATTLER_ISSUER_KEY is ${#ISSUER} hex characters; kittiwake wants 64 (32 bytes)" >&2; exit 1; }

echo "writing [receipt] to $HOST"

# The value travels as the first line of the remote script's stdin, ahead of the script itself.
# Not as a command-line argument, which `ps` shows to every user on the box for as long as the
# command runs, and not through `SendEnv`, which sends nothing when the server has no `AcceptEnv`.
{
  printf '%s\n' "$ISSUER"
  cat <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
umask 077

[ -f "$CFG" ] || { echo "no $CFG - run deploy/install.sh first" >&2; exit 1; }
[ -n "${ISSUER_KEY:-}" ] || { echo "the key did not reach the box" >&2; exit 1; }

cp -p "$CFG" "$CFG.bak.$(date +%s)"

TMP=$(mktemp)
python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys

# The section is replaced wholesale rather than edited key by key: a half-written section is what
# the service refuses to start on, and a rewrite cannot leave one.
#
# `(?m)` and deliberately not `(?s)`. With DOTALL the `.*` swallows the rest of the file in one
# bite and the per-line lookahead never fires, so removing [receipt] would take [nest] and its
# credentials with it.
src = open(sys.argv[1]).read()
src = re.sub(r'(?m)^\[receipt\]\s*\n(?:(?!^\[).*\n?)*', '', src)
src = src.rstrip() + '\n\n[receipt]\nissuer_key = "%s"\n' % os.environ['ISSUER_KEY']
sys.stdout.write(src)
PY

grep -q '^\[receipt\]' "$TMP" || { echo "the rewrite produced no [receipt]" >&2; rm -f "$TMP"; exit 1; }
grep -q '^\[nest\]'    "$TMP" || { echo "the rewrite lost [nest]" >&2; rm -f "$TMP"; exit 1; }

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"
echo "config written; a timestamped backup of the previous one is beside it"

systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake

# Starting is not the proof. An absent key starts perfectly happily and answers "this deployment
# does not issue receipts" for ever, which is the same thing it said before this script ran.
echo "receipt route (want a signed answer, not 'does not issue receipts'):"
curl -fsS --max-time 30 -X POST http://127.0.0.1:8080/api/sql/receipt \
  -H 'content-type: application/json' \
  -d '{"name":"delegations_to_indexer","args":{}}' 2>/dev/null | head -c 200 || true
echo
BOX
} | ssh -i "$KEY" "$HOST" 'read -r ISSUER_KEY; export ISSUER_KEY; bash -s'

cat <<'NEXT'

If the route still says it issues no receipts, the key did not take: check
`journalctl -u kittiwake -n 50` for a config error, and the timestamped backup beside
/etc/kittiwake/config.toml is the previous file.
NEXT
