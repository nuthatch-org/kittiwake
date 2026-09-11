#!/usr/bin/env bash
# Give kittiwake the token that opens an issue in nightswatchhq/graph-support.
#
#   bash deploy/configure-support.sh
#
# Unlike its siblings this does not copy from Vercel, because there is nothing there to copy:
# GRAPH_SUPPORT_ISSUE_TOKEN has never been set in the production environment, which is why the
# incumbent's filing route has answered "not configured" to every request it has ever had. The
# token is typed in here instead, read from the terminal with echo off.
#
# WHAT THE TOKEN SHOULD BE
#
# A fine-grained personal access token, scoped to `nightswatchhq/graph-support` alone, with
# Issues: Read and write and nothing else. Not a classic token: a classic `repo` token can push to
# every repository the account can reach, and this one lives on a box that serves public traffic.
#
# Absent is a valid posture and is the current one. With no token the forms still render and
# `canFile` is false, so the compose page sends people to GitHub's own chooser rather than
# accepting typing it cannot deliver.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

# Read from the terminal rather than from a flag or the environment. An argument is visible in `ps`
# to every user on this machine and lands in shell history; an environment variable is inherited by
# everything this script runs.
printf 'graph-support issue token (input hidden): ' >&2
read -rs TOKEN < /dev/tty
printf '\n' >&2
[ -n "$TOKEN" ] || { echo "no token given; nothing was changed" >&2; exit 1; }

case "$TOKEN" in
  ghp_*)
    echo "that is a classic token. This wants a fine-grained one (github_pat_...) scoped to" >&2
    echo "nightswatchhq/graph-support with Issues: Read and write. Nothing was changed." >&2
    exit 1 ;;
  github_pat_*) ;;
  *)
    echo "that does not look like a GitHub token. Nothing was changed." >&2
    exit 1 ;;
esac

echo "writing [support] to $HOST"

# The value travels as the first line of the remote script's stdin, ahead of the script itself.
# Not as a command-line argument, which `ps` shows to every user on the box for as long as the
# command runs, and not through `SendEnv`, which sends nothing when the server has no `AcceptEnv`.
{
  printf '%s\n' "$TOKEN"
  cat <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
umask 077

[ -f "$CFG" ] || { echo "no $CFG - run deploy/install.sh first" >&2; exit 1; }
[ -n "${ISSUE_TOKEN:-}" ] || { echo "the token did not reach the box" >&2; exit 1; }

cp -p "$CFG" "$CFG.bak.$(date +%s)"

TMP=$(mktemp)
python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys

# The section is replaced wholesale rather than edited key by key: a half-written section is what
# the service refuses to start on, and a rewrite cannot leave one.
#
# `(?m)` and deliberately not `(?s)`. With DOTALL the `.*` swallows the rest of the file in one
# bite and the per-line lookahead never fires, so removing [support] would take [nest] and its
# credentials with it.
src = open(sys.argv[1]).read()
src = re.sub(r'(?m)^\[support\]\s*\n(?:(?!^\[).*\n?)*', '', src)
src = src.rstrip() + '\n\n[support]\nissue_token = "%s"\n' % os.environ['ISSUE_TOKEN']
sys.stdout.write(src)
PY

grep -q '^\[support\]' "$TMP" || { echo "the rewrite produced no [support]" >&2; rm -f "$TMP"; exit 1; }
grep -q '^\[nest\]'    "$TMP" || { echo "the rewrite lost [nest]" >&2; rm -f "$TMP"; exit 1; }

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"
echo "config written; a timestamped backup of the previous one is beside it"

systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake

# Starting is not the proof: an absent token starts perfectly happily and reports canFile:false
# for ever, which is the same thing it said before this ran.
echo "issue forms (want canFile:true):"
curl -fsS --max-time 20 http://127.0.0.1:8080/api/issue-forms \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("canFile:", d.get("canFile"), " templates:", len(d.get("templates", [])))' 2>/dev/null \
  || echo "  the route did not answer; see journalctl -u kittiwake -n 50"
BOX
} | ssh -i "$KEY" "$HOST" 'read -r ISSUE_TOKEN; export ISSUE_TOKEN; bash -s'

cat <<'NEXT'

canFile:true means the compose page will offer to file. The first real filing is the only proof
that the token's scope is right, and it opens a public issue - so make the first one a genuine
report rather than a test, or open it and close it yourself.
NEXT
