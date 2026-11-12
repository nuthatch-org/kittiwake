#!/usr/bin/env bash
# Allow one `sync_bounties` row per on-chain bounty.
#
#   bash deploy/unique-bounty-chain-id.sh
#
# The record route looks a bounty up by `chain_bounty_id` and inserts it if absent, so two requests
# for the same bounty arriving together could both insert. This adds a unique index on the column
# where it is set; the route already treats the index's refusal as "recorded by the other request".
#
# Duplicates already present are resolved first, in the same transaction: the lowest `id` for each
# `chain_bounty_id` is kept, because it is the row every lookup by that id has always returned, and
# the later copies are printed in full and then deleted. Nothing else reads them, and the
# reconciliation job rewrites the kept row's status from the chain. The printed rows are the only
# copy, so keep the output if there are any.
#
# Safe on a binary that predates it, and safe to run twice. Run it as the database owner.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

ssh -i "$KEY" "$HOST" bash -s <<'BOX'
set -euo pipefail
export LC_ALL=C
DB=lodestar
psql() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt -d "$DB" "$@"; }

psql <<'SQL'
BEGIN;
LOCK TABLE sync_bounties IN SHARE ROW EXCLUSIVE MODE;
\echo 'duplicates removed (every column):'
DELETE FROM sync_bounties b
 USING sync_bounties keep
 WHERE b.chain_bounty_id IS NOT NULL
   AND keep.chain_bounty_id = b.chain_bounty_id
   AND keep.id < b.id
RETURNING b.*;
CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_bounties_chain_bounty_id
  ON sync_bounties(chain_bounty_id) WHERE chain_bounty_id IS NOT NULL;
COMMIT;
SQL

echo "index:"
psql -c "SELECT indexdef FROM pg_indexes WHERE indexname = 'idx_sync_bounties_chain_bounty_id'" \
  | sed 's/^/  /'
BOX
