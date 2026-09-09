#!/usr/bin/env bash
# Give kittiwake its own Postgres role, rather than handing it the shared password.
#
# Run it from your laptop; it does all its work on the primary:
#
#   bash deploy/grant-database.sh
#
# Nobody needs to know the Lodestar DATABASE_URL for this. Postgres on that box takes peer auth
# over the local socket, so `sudo -u postgres psql` is enough to create a role, and the password
# this generates never leaves the box: it goes straight into /etc/kittiwake/config.toml (0640
# root:kittiwake) and is printed nowhere.
#
# This is the same shape as the Caddy basic_auth users: one credential per consumer. Rotating
# kittiwake's role cannot strand Vercel, and rotating Vercel's cannot strand kittiwake. The
# alternative - recovering and reusing one shared password - is what made the last rotation
# impossible to sequence.
#
# WHAT IT GRANTS, AND WHAT IT DELIBERATELY DOES NOT
#
#   SELECT on the protocol tables the read routes actually read - named, not ALL TABLES. The Dock's
#     sessions, push_subscriptions and the Scuttlebutt tables are user data that no route here
#     touches, and they are deliberately left out.
#   SELECT and UPDATE on sync_bounties, under the writer flag. `reconcile-bounties` corrects the
#     cached status of a bounty against the BountyBoard contract (#30). No INSERT and no DELETE:
#     the Dock creates and removes bounties, this service only ever says a stored one is wrong.
#   INSERT and UPDATE on ipfs_metadata, because warm-ipfs fills it and both systems write the same
#     document under the same CID, so they race to the same answer.
#   INSERT on cron_runs, because every job records its run there under a `kittiwake:` prefix.
#   CREATE on the schema, because this service creates three tables of its own.
#   ALL on those three tables.
#
# It does NOT grant write on the tables the platform's crons own - indexers, epochs,
# delegation_events, allocations, disputes, network_snapshots, rav_redemptions, ingestion_state,
# and parameter_changes, which takes INSERT only because a recorded change is append-only.
# Those are what `scheduler_enabled` gates, and until that decision is taken deliberately (after a
# shadow run) the database should refuse the write as well as the flag. Two locks on a one-way door
# is not paranoia; it means an accidental flag flip fails loudly instead of quietly becoming a
# second writer.
#
# When the scheduler does go on, run this again with GRANT_WRITER=1.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}
GRANT_WRITER=${GRANT_WRITER:-0}

ssh -i "$KEY" "$HOST" "GRANT_WRITER=$GRANT_WRITER bash -s" <<'BOX'
set -euo pipefail
export LC_ALL=C          # stops perl locale noise on stderr, per the fleet notes
DB=lodestar
ROLE=kittiwake
CFG=/etc/kittiwake/config.toml

psql() { sudo -u postgres psql -v ON_ERROR_STOP=1 -qAt "$@"; }

[ -f "$CFG" ] || { echo "no $CFG - run deploy/install.sh first" >&2; exit 1; }

# A fresh password every run. 32 bytes of urandom, base64, stripped of characters that would need
# percent-encoding in a URL - a password that has to be escaped is a password that eventually is not.
PW=$(head -c 48 /dev/urandom | base64 | tr -d '/+=\n' | cut -c1-32)
[ ${#PW} -ge 24 ] || { echo "could not generate a password" >&2; exit 1; }

if psql -d postgres -c "SELECT 1 FROM pg_roles WHERE rolname='$ROLE'" | grep -q 1; then
  echo "role $ROLE exists; setting a new password"
  psql -d postgres -c "ALTER ROLE $ROLE WITH LOGIN PASSWORD '$PW'"
else
  echo "creating role $ROLE"
  psql -d postgres -c "CREATE ROLE $ROLE WITH LOGIN PASSWORD '$PW'"
fi

psql -d postgres -c "GRANT CONNECT ON DATABASE $DB TO $ROLE"
# Named tables rather than ALL TABLES. The protocol data this service serves, and nothing else:
# push_subscriptions, the Dock's sessions and bounties and the Scuttlebutt tables all hold user data
# that no route here reads, and a grant is a standing permission rather than a statement of intent.
# When the product layer moves (kittiwake#10), grant those then and know why.
psql -d "$DB" <<SQL
GRANT USAGE, CREATE ON SCHEMA public TO $ROLE;

-- Read now.
GRANT SELECT ON indexers, allocations, ipfs_metadata TO $ROLE;
-- Read for the shadow run: the cursors, and the tables both systems will be writing, so the two
-- can be compared from here rather than only as postgres.
GRANT SELECT ON ingestion_state, epochs, delegation_events, disputes, network_snapshots,
  rav_redemptions, indexer_snapshots, cron_runs, parameter_changes TO $ROLE;

GRANT INSERT, UPDATE ON ipfs_metadata TO $ROLE;
GRANT INSERT ON cron_runs TO $ROLE;
-- INSERT on a table with a serial primary key is not enough on its own: the default draws from the
-- sequence, and the sequence is a separate object with its own permissions. Every job run failed to
-- record itself until this was added, and it failed as a warning rather than an error, which is the
-- quietest possible way for an audit trail to stop existing.
-- Every sequence, not one at a time. `INSERT` on a table with a serial primary key draws from a
-- sequence, and the sequence is a separate object with its own permissions. Granting them
-- individually is how this was done first, and it meant `cron_runs` worked while
-- `network_snapshots` failed the moment the scheduler was turned on - the same bug twice, because
-- the fix had been applied to the instance rather than the class. A sequence grant confers
-- `nextval` and nothing else, so there is no reason to be miserly with it.
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO $ROLE;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE ON SEQUENCES TO $ROLE;
SQL

# The three tables that are ours. Created here as the superuser and owned by the role, so the
# service's own CREATE TABLE IF NOT EXISTS at startup is a no-op rather than the first thing that
# needs a privilege.
psql -d "$DB" <<SQL
CREATE TABLE IF NOT EXISTS chain_head_records (
  network TEXT PRIMARY KEY,
  head BIGINT NOT NULL,
  head_first_seen_at TIMESTAMPTZ NOT NULL,
  last_checked_at TIMESTAMPTZ NOT NULL);
CREATE TABLE IF NOT EXISTS indexer_chains (
  address TEXT PRIMARY KEY,
  chains TEXT[] NOT NULL,
  captured_at TIMESTAMPTZ NOT NULL);
CREATE TABLE IF NOT EXISTS live_answers (
  key TEXT PRIMARY KEY,
  value JSONB NOT NULL,
  computed_at TIMESTAMPTZ NOT NULL);
ALTER TABLE chain_head_records OWNER TO $ROLE;
ALTER TABLE indexer_chains OWNER TO $ROLE;
ALTER TABLE live_answers OWNER TO $ROLE;
SQL

if [ "${GRANT_WRITER:-0}" = "1" ]; then
  echo "granting writes on the shared ingest tables (scheduler)"
  psql -d "$DB" <<SQL
GRANT INSERT, UPDATE, DELETE ON indexers, indexer_snapshots, epochs, delegation_events,
  allocations, disputes, network_snapshots, rav_redemptions, ingestion_state TO $ROLE;
-- INSERT and nothing else on the parameter trail. The refresh job appends a row when an indexer's
-- cut moves (#47); a row already there is a thing that happened, and nothing in normal operation
-- should edit or remove one. Repairing bad history is a deliberate act by a human at a superuser
-- prompt, not a privilege this role carries about with it.
GRANT INSERT ON parameter_changes TO $ROLE;
-- The bounty cache. SELECT and UPDATE, nothing else: `reconcile-bounties` reads the BountyBoard
-- contract and corrects a status the UI's best-effort PATCH never received - an indexer claiming
-- through Arbiscan, a developer cancelling directly. Creating and deleting bounties stays with the
-- Dock, so this role cannot do either even by accident.
GRANT SELECT, UPDATE ON sync_bounties TO $ROLE;
SQL
else
  echo "writes on the platform's ingest tables NOT granted; re-run with GRANT_WRITER=1 when the"
  echo "scheduler decision is taken. Until then Postgres refuses them as well as the flag."
fi

# Into the config. Written through a temp file under a tight umask, so there is no window in which
# it exists with default permissions.
umask 077
TMP=$(mktemp)
PGURL="postgresql://$ROLE:$PW@127.0.0.1:5433/$DB?sslmode=require"
DBURL="$PGURL" python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys
line = f'database_url = "{os.environ["DBURL"]}"\n'
out, done = [], False
for raw in open(sys.argv[1]):
    if re.match(r'^\s*database_url\s*=', raw):
        out.append(line); done = True
    elif not done and re.match(r'^\s*#\s*database_url\s*=', raw):
        out.append(line); done = True
    else:
        out.append(raw)
if not done:
    out.append("\n" + line)
sys.stdout.write("".join(out))
PY
unset PGURL PW
grep -q '^database_url' "$TMP" || { echo "the rewrite produced no database_url line" >&2; rm -f "$TMP"; exit 1; }
if grep -qE '^\s*scheduler_enabled\s*=\s*true' "$TMP"; then
  echo "refusing: scheduler_enabled is true, and a pool plus the scheduler makes this a second" >&2
  echo "writer to the platform's tables. Take that decision on its own." >&2
  rm -f "$TMP"; exit 1
fi
install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"

systemctl restart kittiwake
sleep 5
systemctl --no-pager --lines=0 status kittiwake | head -3
echo

# The proof is not that it started. A wrong URL starts perfectly happily and serves "unavailable"
# for ever, so check that the pool connects and a route that needs it answers with rows.
# Scoped to this boot's PID, not to a time window. A window spans the previous start as well, so a
# restart that happened a minute ago reports its warnings as if they were this one's - which is a
# check that can only ever raise a false alarm, and a false alarm is how a real one gets ignored.
echo "startup warnings about the database (want 0):"
journalctl -u kittiwake --no-pager "_PID=$(systemctl show kittiwake -p MainPID --value)" \
  | grep -c "no database_url" | sed 's/^/  /'
echo "indexers-enriched:"
curl -fsS --max-time 20 'http://127.0.0.1:8080/api/indexers-enriched?first=3' \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("  rows:", len(d.get("data") or []), "served_from:", (d.get("provenance") or {}).get("served_from"))' \
  || echo "  no answer - check journalctl -u kittiwake"
echo "subgraph-search:"
curl -fsS --max-time 25 'http://127.0.0.1:8080/api/subgraph-search?q=uniswap' \
  | python3 -c 'import json,sys; print("  hits:", len(json.load(sys.stdin).get("data") or []))' \
  || echo "  no answer yet - warm-ipfs has not run; it fires four minutes after start"
BOX
