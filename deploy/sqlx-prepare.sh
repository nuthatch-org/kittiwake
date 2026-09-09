#!/usr/bin/env bash
# Rebuild `.sqlx/`, the offline data the `query!` macros check themselves against.
#
#   bash deploy/sqlx-prepare.sh
#
# Run it whenever a checked statement changes, and commit what it writes. CI sets `SQLX_OFFLINE`,
# so a query edited without this being re-run fails the build rather than reaching production
# unchecked - which is the whole point of #2.
#
# It builds a throwaway Postgres from the schema rather than pointing at the live one. Two reasons:
# nobody should need production credentials to compile, and the schema this checks against is then
# a thing in the repository rather than a thing on a box. The DDL still lives in
# `nightswatchhq/lodestar` (#31), so this script reads it from a sibling checkout - which is the
# split ownership that issue is about, made visible rather than worked around.
#
# WHAT IS AND IS NOT CHECKED
#
# Statements carrying wei stay as runtime `sqlx::query`, deliberately. A wei figure does not fit an
# `f64`, so those bind decimal strings and let Postgres cast to `numeric`; the macro would want the
# `bigdecimal` feature or a `float8` parameter, and the second rounds money to buy a green tick.
# So do the statements binding `Vec<Option<String>>`, because `text[]` cannot express a per-element
# null to the macro and filling the gaps with "" would write empty strings where the schema says
# NULL. Each of those carries a comment saying so. Checking what can be checked honestly beats
# checking everything dishonestly.
set -euo pipefail

LODESTAR=${LODESTAR:-$(cd "$(dirname "$0")/../.." && pwd)/lodestar}
PORT=${PORT:-55432}
PGBIN=${PGBIN:-/opt/homebrew/opt/postgresql@15/bin}
WORK=$(mktemp -d)
trap 'pg_ctl_stop' EXIT

export PATH="$PGBIN:$PATH"
pg_ctl_stop() {
  pg_ctl -D "$WORK/pg" stop -m immediate >/dev/null 2>&1 || true
  rm -rf "$WORK"
}

[ -f "$LODESTAR/scripts/schema.sql" ] || {
  echo "no schema at $LODESTAR/scripts/schema.sql" >&2
  echo "clone nightswatchhq/lodestar beside this repo, or set LODESTAR=/path/to/lodestar" >&2
  exit 1
}
command -v initdb >/dev/null || { echo "no postgres in $PGBIN; set PGBIN" >&2; exit 1; }

initdb -D "$WORK/pg" -U postgres --auth=trust >/dev/null
# TCP only. A unix socket path under a temp directory routinely exceeds the 103-byte limit macOS
# imposes on it, and the failure reads as "connection refused" rather than as what it is.
pg_ctl -D "$WORK/pg" -l "$WORK/pg/log" \
  -o "-p $PORT -c unix_socket_directories= -c listen_addresses=127.0.0.1" start >/dev/null
for _ in $(seq 1 20); do
  psql -h 127.0.0.1 -p "$PORT" -U postgres -tAc 'select 1' >/dev/null 2>&1 && break
  sleep 0.3
done

export PGHOST=127.0.0.1 PGPORT=$PORT PGUSER=postgres
psql -q -c 'CREATE ROLE lodestar LOGIN' -c 'CREATE DATABASE lodestar OWNER lodestar' >/dev/null

echo "applying the platform schema from $LODESTAR"
for f in "$LODESTAR"/scripts/schema.sql "$LODESTAR"/scripts/migrate-*.sql; do
  psql -q -d lodestar -f "$f" >/dev/null 2>&1 || {
    echo "  failed: $(basename "$f")" >&2
    exit 1
  }
done

# The four tables that are ours. Kept in step with `ensure_chain_health_tables` and
# `ensure_rolling_apy_table` in `crates/db`, which create them on the box at start-up.
echo "applying our own tables"
psql -q -v ON_ERROR_STOP=1 -d lodestar >/dev/null <<'SQL'
CREATE TABLE IF NOT EXISTS indexer_rolling_apy (
  indexer_address TEXT PRIMARY KEY,
  rolling_apy_30d NUMERIC,
  rolling_apy_90d NUMERIC,
  computed_at TIMESTAMPTZ NOT NULL);
CREATE TABLE IF NOT EXISTS live_answers (
  key TEXT PRIMARY KEY, value JSONB NOT NULL, computed_at TIMESTAMPTZ NOT NULL);
CREATE TABLE IF NOT EXISTS chain_head_records (
  network TEXT PRIMARY KEY, head BIGINT NOT NULL,
  head_first_seen_at TIMESTAMPTZ NOT NULL, last_checked_at TIMESTAMPTZ NOT NULL);
CREATE TABLE IF NOT EXISTS indexer_chains (
  address TEXT PRIMARY KEY, chains TEXT[] NOT NULL, captured_at TIMESTAMPTZ NOT NULL);
SQL

cd "$(dirname "$0")/.."
command -v cargo-sqlx >/dev/null || cargo install sqlx-cli --no-default-features --features postgres,rustls
DATABASE_URL="postgres://postgres@127.0.0.1:$PORT/lodestar" cargo sqlx prepare --workspace -- --all-targets

echo
echo "wrote .sqlx/ - commit it. Then check it stands on its own:"
echo "  SQLX_OFFLINE=true cargo check --workspace --all-targets"
