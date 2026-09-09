#!/usr/bin/env bash
# Give the RAV cursor a column that can hold a time.
#
#   bash deploy/migrate-ingestion-cursor.sh
#
# `ingestion_state.last_block` is an `int4`, and the RAV ingest stores a unix **timestamp** in it -
# which is what the platform did and what the port faithfully preserved. Today's timestamps are
# about 1.79e9 against a ceiling of 2,147,483,647, so it works until 19 January 2038.
#
# Then it does something worse than stop. `max_timestamp as i32` truncates in Rust rather than
# saturating, so the stored cursor goes negative, `since` clamps to zero, and every delta run
# silently becomes a full backfill. The job keeps succeeding, works very much harder, and nobody
# is told. #4
#
# Additive and reversible: a nullable column nothing else reads. The platform's own crons do not
# know it exists, so the property `docs/decisions.md` records - that a rollback needs no data undone
# - survives this.
#
# Run it before deploying the binary that uses the column. Run it twice and nothing happens.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

ssh -i "$KEY" "$HOST" bash -s <<'BOX'
set -euo pipefail
export LC_ALL=C
DB=lodestar
ROLE=kittiwake
psql() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt -d "$DB" "$@"; }

echo "before:"
psql -c "SELECT key, last_block, updated_at FROM ingestion_state WHERE key LIKE 'kittiwake:%' ORDER BY key" \
  | sed 's/^/  /'

psql <<'SQL'
ALTER TABLE ingestion_state ADD COLUMN IF NOT EXISTS last_timestamp BIGINT;
-- Seed only our own cursors, and only where the new column is still empty. The platform's rows are
-- its own and their `last_block` may genuinely be a block; ours are prefixed and this one is not.
UPDATE ingestion_state
   SET last_timestamp = last_block
 WHERE key = 'kittiwake:rav'
   AND last_timestamp IS NULL
   AND last_block IS NOT NULL;
SQL

# UPDATE on this table is already in the writer grant; the new column inherits it. Stated rather
# than assumed, because a column added by the superuser is a place a grant can quietly not reach.
psql -c "GRANT SELECT, INSERT, UPDATE ON ingestion_state TO $ROLE" >/dev/null

echo "after:"
psql -c "SELECT key, last_block, last_timestamp, to_timestamp(last_timestamp) FROM ingestion_state WHERE key LIKE 'kittiwake:%' ORDER BY key" \
  | sed 's/^/  /'
BOX
