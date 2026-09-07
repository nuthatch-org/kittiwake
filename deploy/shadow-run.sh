#!/usr/bin/env bash
# The shadow run: both systems writing, and a way to tell whether they agree.
#
#   bash deploy/shadow-run.sh baseline   # before enabling anything
#   bash deploy/shadow-run.sh seed       # start our cursors where theirs are
#   bash deploy/shadow-run.sh enable     # grant the writes and turn the scheduler on
#   bash deploy/shadow-run.sh compare    # after a cycle or two
#   bash deploy/shadow-run.sh disable    # back out
#
# Why this is a procedure and not a flag. Turning `scheduler_enabled` on makes this service a
# second writer to tables the platform's crons still own. The upserts are idempotent, so both
# writing the same row is safe - but only if they agree about *which* rows, and that is the thing
# being tested rather than assumed.
#
# The cursors are already separate (`kittiwake:` prefixed in `ingestion_state`), so the two walk
# independently. That is what makes the comparison meaningful: if our cursor converges on theirs,
# our walk agrees with theirs. If it lags or overshoots, it does not, and the tables would have
# diverged the moment one system stopped.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
CMD=${1:-}

TABLES="epochs delegation_events allocations disputes network_snapshots rav_redemptions indexers"

case "$CMD" in
seed)
  ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
export LC_ALL=C
# Start our cursors where the platform's already are.
#
# Without this a cold start is not a shadow run, it is a backfill: 608,725 delegation events at
# 1,000 a quarter-hour is six days, 852,326 allocations at 2,000 an hour is seventeen, and RAV
# would re-read every collection ever made. Six days of nest permits spent re-writing rows that
# are already there, because every one of those upserts is idempotent and would change nothing.
#
# What the shadow run is actually testing is whether this service keeps up *from here*, which is
# the only thing that matters after cutover. The history is already in the tables.
#
# `allocations` cannot be copied: the platform tracks it by epoch and this service walks by id, so
# its cursor is seeded from the highest id already ingested instead. That difference is itself a
# finding - two implementations of the same walk with incompatible cursors cannot be handed over
# by copying a row.
sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt lodestar <<'SQL'
INSERT INTO ingestion_state (key, last_epoch, last_block, last_id, updated_at)
SELECT 'kittiwake:' || key, last_epoch, last_block, last_id, NOW()
  FROM ingestion_state WHERE key IN ('epochs','delegation_events','rav','disputes')
ON CONFLICT (key) DO UPDATE SET
  last_epoch = EXCLUDED.last_epoch, last_block = EXCLUDED.last_block,
  last_id = EXCLUDED.last_id, updated_at = NOW();

INSERT INTO ingestion_state (key, last_id, updated_at)
SELECT 'kittiwake:allocations', max(id), NOW() FROM allocations
ON CONFLICT (key) DO UPDATE SET last_id = EXCLUDED.last_id, updated_at = NOW();
SQL
sudo -u postgres psql -At -F'  ' lodestar -c   "SELECT key, COALESCE(last_epoch::text,'-'), COALESCE(last_block::text,'-'), COALESCE(left(last_id,12),'-')
   FROM ingestion_state WHERE key LIKE 'kittiwake:%' ORDER BY key" | sed 's/^/  /'
BOX
  ;;
baseline|compare)
  ssh -i "$KEY" "$HOST" "TABLES='$TABLES' LABEL='$CMD' bash -s" <<'BOX'
set -euo pipefail
export LC_ALL=C
q() { sudo -u postgres psql -At lodestar -c "$1"; }
echo "== $LABEL at $(date -u +%FT%TZ) =="
echo "-- row counts"
for t in $TABLES; do printf "  %-20s %s\n" "$t" "$(q "SELECT count(*) FROM $t")"; done
echo "-- cursors: theirs against ours"
q "SELECT t.key || '|' || COALESCE(t.last_epoch::text,'-') || '|' || COALESCE(t.last_block::text,'-') || '|' || COALESCE(k.last_epoch::text,'-') || '|' || COALESCE(k.last_block::text,'-')
   FROM ingestion_state t LEFT JOIN ingestion_state k ON k.key = 'kittiwake:' || t.key
   WHERE t.key NOT LIKE 'kittiwake:%' ORDER BY t.key" \
  | awk -F'|' 'BEGIN{printf "  %-22s %14s %14s %14s %14s\n","job","their epoch","their block","our epoch","our block"}
               {sub(/^kittiwake:/,"",$1); printf "  %-22s %14s %14s %14s %14s\n",$1,$2,$3,$4,$5}'
echo "-- cron runs in the last hour"
q "SELECT step || '|' || n || '|' || ok FROM (
     SELECT step, count(*) AS n, bool_and(success) AS ok FROM cron_runs
     WHERE started_at > NOW() - INTERVAL '1 hour' GROUP BY step) x ORDER BY step" \
  | awk -F'|' '{printf "  %-34s %3s runs  all-ok=%s\n",$1,$2,$3}'
BOX
  ;;
enable)
  echo "granting writes on the platform's ingest tables"
  GRANT_WRITER=1 bash "$(dirname "$0")/grant-database.sh" >/dev/null 2>&1 || true
  ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
export LC_ALL=C
sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt lodestar <<'SQL'
GRANT INSERT, UPDATE, DELETE ON indexers, indexer_snapshots, epochs, delegation_events,
  allocations, disputes, network_snapshots, rav_redemptions, ingestion_state TO kittiwake;
-- The sequences those inserts draw from. Granting the table without the sequence gets you a job
-- that fails on its first write with `permission denied for sequence`, which is exactly what
-- happened to `snapshot-network` on the first shadow run.
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO kittiwake;
SQL
echo "  writes granted"
CFG=/etc/kittiwake/config.toml
cp -a "$CFG" "$CFG.bak.$(date -u +%Y%m%dT%H%M%SZ)"
sed -i 's/^scheduler_enabled = false/scheduler_enabled = true/' "$CFG"
grep -q '^scheduler_enabled = true' "$CFG" || { echo "the flag did not flip" >&2; exit 1; }
systemctl restart kittiwake
sleep 5
systemctl is-active kittiwake | sed 's/^/  kittiwake: /'
journalctl -u kittiwake --no-pager "_PID=$(systemctl show kittiwake -p MainPID --value)" \
  | grep -c "ingest jobs scheduled" | sed 's/^/  scheduler started (want 1): /'
BOX
  ;;
disable)
  ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
sed -i 's/^scheduler_enabled = true/scheduler_enabled = false/' "$CFG"
systemctl restart kittiwake
# The grant goes too. The flag alone is one lock; this is the other, and leaving it on would mean a
# future accidental flip is enough on its own.
sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt lodestar <<'SQL'
REVOKE INSERT, UPDATE, DELETE ON indexers, indexer_snapshots, epochs, delegation_events,
  allocations, disputes, network_snapshots, rav_redemptions, ingestion_state FROM kittiwake;
SQL
sleep 4
systemctl is-active kittiwake | sed 's/^/  kittiwake: /'
echo "  scheduler off, writes revoked"
BOX
  ;;
*)
  echo "usage: $0 {baseline|seed|enable|compare|disable}" >&2
  exit 2
  ;;
esac
