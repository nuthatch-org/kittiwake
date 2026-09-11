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

# Two ways in, because the first one has failed three times in a row for reasons that never made
# it back off the terminal it printed on. Vercel first when it is available, and the key typed in
# when it is not; a deploy script whose only path runs through another vendor's CLI has two
# failure modes and can only report one of them.
#
#   bash deploy/configure-receipts.sh            # try Vercel, fall back to asking
#   bash deploy/configure-receipts.sh --ask      # skip Vercel entirely
#   bash deploy/configure-receipts.sh --dry-run  # check the key, write nothing
#
# `--dry-run` exists because testing the refusals meant feeding this a fake key, and a fake key
# that passes the checks is one the script then writes to production. That is exactly what
# happened on 2026-09-11: a zeroed test key reached /etc/kittiwake/config.toml and was reverted
# from the backup two minutes later. Anything that validates must be able to stop before it acts.
. "$(dirname "$0")/secret-guard.sh"

ASK_ONLY=0
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --ask) ASK_ONLY=1 ;;
    --dry-run) DRY_RUN=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

RAW=""

if [ "$ASK_ONLY" = "0" ]; then
  LODESTAR_DIR=${LODESTAR_DIR:-$(cd "$(dirname "$0")/../../lodestar" 2>/dev/null && pwd || true)}
  if [ -z "$LODESTAR_DIR" ] || [ ! -d "$LODESTAR_DIR/.vercel" ]; then
    echo "no Vercel-linked lodestar checkout found; will ask for the key instead." >&2
  else
    ENVFILE=$(mktemp)
    trap 'rm -f "$ENVFILE"' EXIT
    chmod 600 "$ENVFILE"

    echo "reading the production environment from Vercel ($LODESTAR_DIR)"
    # Not fatal, and not silent. `set -e` used to abort the whole script here with whatever the
    # CLI happened to print, which is the shape of the failure nobody could describe afterwards.
    if ( cd "$LODESTAR_DIR" && vercel env pull "$ENVFILE" --environment=production --yes >/dev/null 2>&1 ); then
      RAW=$(grep -m1 "^TATTLER_ISSUER_KEY=" "$ENVFILE" | cut -d= -f2- | sed -E 's/^"(.*)"$/\1/')
      if [ -z "$RAW" ]; then
        if grep -q '^TATTLER_ISSUER_KEY=' "$ENVFILE"; then
          echo "Vercel returned TATTLER_ISSUER_KEY with an empty value, which is what a write-only" >&2
          echo "secret looks like from out here." >&2
        else
          echo "TATTLER_ISSUER_KEY is not in the pulled environment. These names are:" >&2
          grep -o '^[A-Z_0-9]*' "$ENVFILE" | sort | sed 's/^/  /' >&2
        fi
      elif ! refuse_placeholder TATTLER_ISSUER_KEY "$RAW"; then
        # Sensitive in the dashboard, so the pull returned a placeholder. Fall through and ask.
        RAW=""
      else
        echo "got the key from Vercel"
      fi
    else
      echo "vercel env pull did not succeed; will ask for the key instead." >&2
    fi
  fi
fi

if [ -z "$RAW" ]; then
  echo >&2
  echo "Paste the issuer key. It is TATTLER_ISSUER_KEY in the Lodestar project's production" >&2
  echo "environment on Vercel: Settings, Environment Variables, reveal, copy." >&2
  echo >&2
  # From the terminal, with echo off. Not an argument, which `ps` shows to every user on this
  # machine and which lands in shell history.
  printf 'issuer key (input hidden): ' >&2
  # Plain stdin: the terminal when run by hand, the pipe when driven by a test. `< /dev/tty` was
  # here and fails outright where there is no controlling terminal, which is every environment
  # that could have checked this path before you ran it.
  read -rs RAW
  printf '\n' >&2
fi

[ -n "$RAW" ] || { echo "no key given; nothing was changed" >&2; exit 1; }

# `0x` is how the value is written in half the places it appears, and `x` is not a hex digit, so a
# prefixed key failed the check below with "not hexadecimal" - which reads as a corrupt key rather
# than a prefix. configure-tap.sh already stripped it; this did not.
ISSUER=${RAW#0x}

# The service's own rule, mirrored: 32 bytes of hexadecimal. Checked here so a bad value is a
# sentence from this script rather than a process that will not start after the config is replaced.
#
# The diagnostics describe the value without printing it: its length, and the offending characters
# reduced to a class. A refusal nobody can act on is the same as a silent one.
case "$ISSUER" in
  *[!0-9a-fA-F]*)
    echo "TATTLER_ISSUER_KEY is not hexadecimal." >&2
    echo "  length: ${#ISSUER} characters" >&2
    echo "  it contains: $(printf '%s' "$ISSUER" | tr -d '0-9a-fA-F' | fold -w1 | sort -u | tr -d '\n' | sed 's/./& /g')" >&2
    echo "  expected: 64 hex characters, an ed25519 seed, with or without a 0x prefix" >&2
    exit 1 ;;
esac
[ "${#ISSUER}" -eq 64 ] || {
  echo "TATTLER_ISSUER_KEY is ${#ISSUER} hex characters; kittiwake wants 64 (32 bytes)." >&2
  echo "If the incumbent is happy with it, kittiwake will not be, and they must agree: every" >&2
  echo "receipt already issued names the public half of this exact key." >&2
  exit 1; }

echo "the key is 64 hex characters, as expected"

if [ "$DRY_RUN" = "1" ]; then
  echo "--dry-run: the key would be accepted. Nothing was written and $HOST was not contacted."
  exit 0
fi

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
#
# Nor is a signed answer the proof. A receipt signed by the *wrong* key is well formed and
# verifies against nothing, which is the failure this whole script exists to avoid, and it looks
# identical to success from here. So the answer's public key is what gets printed.
echo "PUBKEY:"
curl -fsS --max-time 30 -X POST http://127.0.0.1:8080/api/sql/receipt \
  -H 'content-type: application/json' \
  -d '{"name":"delegations_to_indexer","args":{"indexer":"0x0000000000000000000000000000000000000000","before_block":0}}' \
  2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pubkey",""))' 2>/dev/null || echo ""
BOX
} | ssh -i "$KEY" "$HOST" 'read -r ISSUER_KEY; export ISSUER_KEY; bash -s' | tee /tmp/kittiwake-receipt-out.$$ >&2
OURS=$(sed -n '/^PUBKEY:$/{n;p;}' /tmp/kittiwake-receipt-out.$$ | tr -d '[:space:]')
rm -f /tmp/kittiwake-receipt-out.$$

# The incumbent publishes its issuer key in every receipt it signs, so the comparison needs no
# secret: ask it for one and read the public half.
echo
echo "checking the key is the incumbent's, not merely a valid one"
THEIRS=$(curl -fsS --max-time 40 -X POST https://www.lodestar-dashboard.com/api/sql/receipt \
  -H 'content-type: application/json' \
  -d '{"name":"delegations_to_indexer","args":{"indexer":"0x0000000000000000000000000000000000000000","before_block":0}}' \
  2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d.get("data") or d).get("pubkey",""))' 2>/dev/null || echo "")

if [ -z "$OURS" ]; then
  echo "kittiwake did not return a public key; see the output above." >&2
  exit 1
fi
if [ -z "$THEIRS" ]; then
  echo "could not read the incumbent's public key, so the two were not compared." >&2
  echo "kittiwake is now issuing under: $OURS" >&2
  echo "Check that against a receipt the incumbent signed before trusting it." >&2
  exit 1
fi

echo "  kittiwake: $OURS"
echo "  incumbent: $THEIRS"
if [ "$OURS" != "$THEIRS" ]; then
  echo >&2
  echo "THESE DO NOT MATCH. The key that was just installed is not the one the incumbent signs" >&2
  echo "with, so every receipt already issued names a public key this deployment cannot produce." >&2
  echo "Those receipts do not look invalid; they become unverifiable, which is worse." >&2
  echo >&2
  echo "Run this again with the incumbent's own TATTLER_ISSUER_KEY. The previous config is in the" >&2
  echo "timestamped backup beside /etc/kittiwake/config.toml." >&2
  exit 1
fi
echo "  they match: every receipt already issued stays verifiable."

cat <<'NEXT'

If the route still says it issues no receipts, the key did not take: check
`journalctl -u kittiwake -n 50` for a config error, and the timestamped backup beside
/etc/kittiwake/config.toml is the previous file.
NEXT
