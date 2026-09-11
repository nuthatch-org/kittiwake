#!/usr/bin/env bash
#
# Drop the tables nothing reads and nothing writes.
#
#   bash deploy/drop-dead-tables.sh
#
# Six tables, checked on 2026-09-11: every one has zero rows, and no statement in either repository
# names it. They are the residue of features that were removed without their storage.
#
#   community_votes            the community vote. Its route and UI went in lodestar#136; the
#                              `/api/vote` helpers and `lib/voting.ts` followed on 2026-09-11.
#   roadmap_community_updates  the same feature's other half.
#   delegations                referenced twenty-nine times across both repositories and by nothing:
#                              every one of those is a field name or a nest view, and no SQL has
#                              ever named the table. `delegation_events` is the live one.
#   push_subscriptions         push, discontinued 2026-09-06.
#   device_tokens              likewise.
#   notification_log           likewise. Eight rows of history from before it stopped.
#
# **One way.** This cannot be re-run into existence and the rows do not come back. Five of the six
# are empty so there is nothing to lose; `notification_log` has eight rows of a discontinued
# feature's history, and if that is worth anything to anybody, dump it first:
#
#   pg_dump "$DATABASE_URL" -t notification_log > notification-log-backup.sql
#
# The declarations have already gone from `db/schema.sql`, so `schema_gaps` stops expecting them the
# moment a build carrying that change is deployed. Running this against an older binary is harmless
# in the other direction: it would report them as missing, which they would be.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
export LC_ALL=C
psql() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt -p 5433 -d lodestar "$@"; }

TABLES="community_votes roadmap_community_updates delegations push_subscriptions device_tokens notification_log"

# Counted here rather than trusted from a note written earlier. `pg_stat_user_tables` is an
# estimate and reported several of these as empty when they were not; this is an exact count, taken
# immediately before the drop, and a single row anywhere stops all six.
echo "counts immediately before dropping:"
ABORT=0
PRESENT=""
for t in $TABLES; do
  exists=$(psql -c "SELECT COUNT(*) FROM pg_tables WHERE schemaname='public' AND tablename='$t'")
  if [ "$exists" = "0" ]; then
    printf "  %-28s already gone\n" "$t"
    continue
  fi
  n=$(psql -c "SELECT COUNT(*) FROM $t")
  printf "  %-28s %s rows\n" "$t" "$n"
  PRESENT="$PRESENT $t"
  # notification_log is allowed its eight rows of dead history; nothing else may have any.
  if [ "$t" != "notification_log" ] && [ "$n" != "0" ]; then ABORT=1; fi
done

if [ "$ABORT" = "1" ]; then
  echo >&2
  echo "REFUSING: a table that should be empty has rows in it. Nothing has been dropped." >&2
  echo "Something is writing to a table this script believes is dead. Find out what first." >&2
  exit 1
fi

for t in $PRESENT; do
  psql -c "DROP TABLE IF EXISTS $t CASCADE" >/dev/null
  echo "  dropped $t"
done

echo
echo "remaining tables: $(psql -c "SELECT COUNT(*) FROM pg_tables WHERE schemaname='public'")"
BOX
