#!/usr/bin/env bash
# Rebuild `.sqlx/`, the offline data the `query!` macros check themselves against.
#
#   bash deploy/sqlx-prepare.sh
#
# Run it whenever a checked statement changes, and commit what it writes. CI sets `SQLX_OFFLINE`,
# so a query edited without this being re-run fails the build rather than reaching production
# unchecked - which is the whole point of #2.
#
# It builds a throwaway Postgres from `db/schema.sql` rather than pointing at the live one. Three
# reasons: nobody should need production credentials to compile, the schema being checked against
# is then a thing in this repository rather than a thing on a box, and it needs no other checkout.
# Until #31 it read the DDL from a sibling `lodestar`, which meant the compile-time check depended
# on somebody else's working tree being present and current.
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

PORT=${PORT:-55432}
PGBIN=${PGBIN:-/opt/homebrew/opt/postgresql@15/bin}
WORK=$(mktemp -d)
trap 'pg_ctl_stop' EXIT

export PATH="$PGBIN:$PATH"
pg_ctl_stop() {
  pg_ctl -D "$WORK/pg" stop -m immediate >/dev/null 2>&1 || true
  rm -rf "$WORK"
}

SCHEMA="$(cd "$(dirname "$0")/.." && pwd)/db/schema.sql"
[ -f "$SCHEMA" ] || { echo "no schema at $SCHEMA" >&2; exit 1; }
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

echo "applying db/schema.sql"
psql -q -v ON_ERROR_STOP=1 -d lodestar -f "$SCHEMA" >/dev/null

cd "$(dirname "$0")/.."
command -v cargo-sqlx >/dev/null || cargo install sqlx-cli --no-default-features --features postgres,rustls
DATABASE_URL="postgres://postgres@127.0.0.1:$PORT/lodestar" cargo sqlx prepare --workspace -- --all-targets

echo
echo "wrote .sqlx/ - commit it. Then check it stands on its own:"
echo "  SQLX_OFFLINE=true cargo check --workspace --all-targets"
