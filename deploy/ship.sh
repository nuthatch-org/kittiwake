#!/usr/bin/env bash
# Install the binary CI built for a commit on main.
#
#   bash deploy/ship.sh            # the newest successful main build
#   bash deploy/ship.sh <run-id>   # a specific one, to roll back
#
# Artifacts are kept for thirty days, so a rollback target older than that has to be rebuilt.
#
# The three-step scp is what this replaces, and the step it removes is the one that goes wrong:
# `scp target/release/kittiwake` ships whatever happens to be in the working tree, which on a Mac
# is a Darwin binary and on a good day is a Linux one built from a branch nobody merged. This takes
# the artifact CI produced from main, so what runs on the box is what the tests ran against.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
REPO=${REPO:-nuthatch-org/kittiwake}

command -v gh >/dev/null || { echo "this needs the gh CLI" >&2; exit 1; }

RUN=${1:-}
if [ -z "$RUN" ]; then
  echo "finding the newest successful CI run on main"
  RUN=$(gh run list --repo "$REPO" --branch main --workflow CI --status success \
          --limit 1 --json databaseId --jq '.[0].databaseId')
  [ -n "$RUN" ] || { echo "no successful CI run on main to install" >&2; exit 1; }
fi

# Printed before anything is touched, because "which commit is on the box" is the question every
# incident starts with and the answer should not have to be reconstructed afterwards.
gh run view "$RUN" --repo "$REPO" --json headSha,displayTitle,createdAt \
  --jq '"run \(.databaseId // "'"$RUN"'")  \(.headSha[0:12])  \(.createdAt)  \(.displayTitle)"' 2>/dev/null \
  || echo "run $RUN"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "downloading the release artifact"
gh run download "$RUN" --repo "$REPO" --name kittiwake-linux-x86_64 --dir "$WORK" >/dev/null

BIN="$WORK/kittiwake"
[ -f "$BIN" ] || { echo "the run has no kittiwake artifact; was it a release build?" >&2; exit 1; }
chmod +x "$BIN"

# A Mach-O here means the artifact came from somewhere unexpected, and the box would reject it with
# a message about a bad ELF header long after the old binary had been replaced.
case "$(file -b "$BIN" 2>/dev/null || echo unknown)" in
  *ELF*) ;;
  unknown) echo "note: no 'file' on this machine, so the binary's format was not checked" >&2 ;;
  *) echo "that artifact is not a Linux binary; nothing was installed" >&2; exit 1 ;;
esac

echo "shipping to $HOST"
scp -i "$KEY" -q "$BIN" "$HOST:/tmp/kittiwake.new"
scp -i "$KEY" -q "$(dirname "$0")/install.sh" "$(dirname "$0")/kittiwake.service" "$HOST:/tmp/"
ssh -i "$KEY" "$HOST" 'bash /tmp/install.sh /tmp/kittiwake.new'

echo
echo "health:"
ssh -i "$KEY" "$HOST" 'curl -fsS --max-time 20 http://127.0.0.1:8080/health' | head -c 600
echo
