#!/usr/bin/env bash
# Delete the phantom parameter changes the cutover's unit split wrote on 2026-09-07.
#
#   bash deploy/repair-parameter-changes.sh              # counts them and changes nothing
#   bash deploy/repair-parameter-changes.sh COMMIT=1     # deletes them
#
# WHAT HAPPENED
#
# For a few hours on 2026-09-07 the `indexers.reward_cut` column held two units at once. kittiwake
# had begun writing percent where the platform had always written PPM (#25, #47). The platform's
# `writeIndexers` cron was still running, and it detects a parameter change by comparing the stored
# previous value against the incoming one. So it read kittiwake's `100` - a hundred percent - saw
# the protocol's incoming `1000000` PPM, concluded the indexer had changed its cut, and recorded it.
# Every run. For every indexer.
#
# #48 made the column PPM throughout, so this cannot recur. It did not touch the history the split
# had already written, and `/api/apr-provenance` has been serving it ever since as the explanation
# for why an indexer's APR moved. Measured against production on 2026-09-09: **96 of 96 indexers**
# in the directory serve at least one, 191 of the 248 parameter-change events on the whole site are
# these, all of them dated 2026-09-07, and each is an exact `x -> x * 10000`. The most recent event
# in every indexer's trail is a false one, which is the first thing a reader sees.
#
# HOW THE ROWS ARE IDENTIFIED, AND WHY THAT IS SAFE
#
# Three conditions together, not one:
#
#   new_value = old_value * 10000     the arithmetic signature of a percent read as PPM
#   created_at on 2026-09-07          the only day the column held two units
#   old_value > 0                     0 -> 0 is not a change and would not have been recorded
#
# A genuine change of exactly ten-thousandfold is arithmetically possible - 0.01% to 100% is
# `100 -> 1000000` in honest PPM - which is why the date is part of the test and not decoration.
# Every one of the 191 observed falls on the one day, and the counts this script prints before it
# deletes anything are the check on that: if the "outside 2026-09-07" figure is not zero, stop and
# look rather than widening the window.
#
# It runs as `postgres`, deliberately. The `kittiwake` role has INSERT on this table and nothing
# else (deploy/grant-database.sh), because a recorded change is a thing that happened and repairing
# bad history should be a deliberate act by a person rather than a privilege a daemon carries.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
COMMIT=${COMMIT:-0}
for a in "$@"; do case "$a" in COMMIT=*) COMMIT="${a#COMMIT=}" ;; esac; done

ssh -i "$KEY" "$HOST" "COMMIT=$COMMIT bash -s" <<'BOX'
set -euo pipefail
export LC_ALL=C
DB=lodestar

psql() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt -d "$DB" "$@"; }

WHERE="new_value = old_value * 10000 AND old_value > 0"
DAY="created_at >= TIMESTAMPTZ '2026-09-07 00:00:00+00' AND created_at < TIMESTAMPTZ '2026-09-08 00:00:00+00'"

echo "parameter_changes, before:"
echo "  total rows                          $(psql -c "SELECT count(*) FROM parameter_changes")"
echo "  x10000 on 2026-09-07 (the target)   $(psql -c "SELECT count(*) FROM parameter_changes WHERE $WHERE AND $DAY")"
echo "  indexers affected                   $(psql -c "SELECT count(DISTINCT indexer_address) FROM parameter_changes WHERE $WHERE AND $DAY")"
echo "  x10000 on any OTHER day             $(psql -c "SELECT count(*) FROM parameter_changes WHERE $WHERE AND NOT ($DAY)")"
echo "  any row at all on 2026-09-07        $(psql -c "SELECT count(*) FROM parameter_changes WHERE $DAY")"

# The last two lines are the ones to read. A non-zero "other day" means the signature is catching
# something this repair was not written for. A gap between "any row on 2026-09-07" and the target
# means real changes were recorded that day too, and they must survive.

if [ "${COMMIT:-0}" != "1" ]; then
  echo
  echo "dry run; nothing deleted. Re-run with COMMIT=1 once the counts above look right."
  exit 0
fi

psql -c "BEGIN; DELETE FROM parameter_changes WHERE $WHERE AND $DAY; COMMIT;"
echo
echo "parameter_changes, after:"
echo "  total rows                          $(psql -c "SELECT count(*) FROM parameter_changes")"
echo "  x10000 on 2026-09-07 (should be 0)  $(psql -c "SELECT count(*) FROM parameter_changes WHERE $WHERE AND $DAY")"
BOX
