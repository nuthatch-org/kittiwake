#!/usr/bin/env bash
#
# Drop what push left behind.
#
# Push notifications were discontinued on 2026-09-06. The crons that fed them were struck the same
# week, the routes and the UI went in nightswatchhq/lodestar#136 on 2026-09-09, and these tables
# have had no reader or writer since. `docs/migration.md` said they could stay "until someone
# decides to drop them"; this is that decision.
#
# **One way.** Unlike every other statement in `db/schema.sql`, this cannot be re-run into
# existence: the rows are gone. Take a dump first if the history is worth anything to anybody.
#
#   pg_dump "$DATABASE_URL" -t device_tokens -t push_subscriptions -t notification_log > push-backup.sql
#   bash deploy/drop-push-tables.sh
#
# **Order matters, and getting it wrong stops the disputes ingest.** The binary released before
# 2026-09-10 names `push_notified` in its `INSERT INTO disputes`. Drop the column while that binary
# is the one running and every disputes run fails on a column that no longer exists. So:
#
#   1. Deploy a build that contains this script.
#   2. Confirm `cron_runs` has a green `kittiwake:ingest-disputes` from the new binary.
#   3. Then run this.
#
# The reverse order is not a rollback either: putting the column back leaves the rows it recorded
# gone. Deploy first.
set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the database to change}"

echo "This drops device_tokens, push_subscriptions and notification_log, and the push_notified"
echo "columns on disputes and parameter_changes. The rows do not come back."
read -r -p "Type the database host to continue: " confirm
host=$(printf '%s' "$DATABASE_URL" | sed -E 's#.*@([^:/]+).*#\1#')
[ "$confirm" = "$host" ] || { echo "that is not $host; nothing done"; exit 1; }

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 <<'SQL'
BEGIN;

-- The index goes with the column it is predicated on; dropping the column alone would leave
-- Postgres to drop it silently, which works and is not something to rely on.
DROP INDEX IF EXISTS idx_disputes_unnotified;
ALTER TABLE disputes          DROP COLUMN IF EXISTS push_notified;
ALTER TABLE parameter_changes DROP COLUMN IF EXISTS push_notified;

DROP TABLE IF EXISTS device_tokens;
DROP TABLE IF EXISTS push_subscriptions;
DROP TABLE IF EXISTS notification_log;

COMMIT;
SQL

echo "done. db/schema.sql no longer declares any of it."
