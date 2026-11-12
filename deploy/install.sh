#!/usr/bin/env bash
# Install or upgrade kittiwake on the Lodestar primary (Nuremberg).
#
# Idempotent. Safe to re-run: it replaces the binary, leaves an existing config alone, and restarts
# the unit. The nest credentials are lifted from Foghorn's config on the same box rather than asked
# for, so no secret is typed, pasted or logged anywhere.
#
#   scp target/release/kittiwake root@<primary>:/tmp/kittiwake.new
#   scp deploy/install.sh deploy/kittiwake.service root@<primary>:/tmp/
#   ssh root@<primary> 'bash /tmp/install.sh /tmp/kittiwake.new'
set -euo pipefail

BIN=${1:-/tmp/kittiwake.new}
FOG=${FOGHORN_CONFIG:-/root/foghorn/config.toml}
UNIT=${UNIT_FILE:-$(dirname "$0")/kittiwake.service}
ETC=/etc/kittiwake
OPT=/opt/kittiwake

[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }
[ -r "$UNIT" ] || { echo "no unit file at $UNIT" >&2; exit 1; }

id -u kittiwake >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin kittiwake
install -d -m 0755 "$OPT/bin"
install -d -m 0750 -o root -g kittiwake "$ETC"
install -d -m 0750 -o kittiwake -g kittiwake /var/lib/kittiwake

# Read the existing config with the NEW binary, as startup will, before anything is replaced. A
# config the new build refuses would otherwise be found by the restart below, with the service down.
# The unit's EnvironmentFile is handed to the binary, which reads it as systemd does; it is never
# sourced here, because its values (URLs, passwords) are not shell.
if [ -f "$ETC/config.toml" ]; then
  ENVFILE=$(sed -n 's/^EnvironmentFile=//p' "$UNIT" | head -n1)
  OPTIONAL=no
  case "$ENVFILE" in -*) OPTIONAL=yes; ENVFILE=${ENVFILE#-} ;; esac
  ENVARGS=()
  if [ -n "$ENVFILE" ]; then
    if [ -r "$ENVFILE" ]; then
      ENVARGS=(--env-file "$ENVFILE")
    elif [ "$OPTIONAL" = no ]; then
      echo "could not run the config check: the unit names $ENVFILE and it is not readable" >&2
      exit 1
    fi
  fi
  rc=0
  KITTIWAKE_CONFIG="$ETC/config.toml" "$BIN" --check-config ${ENVARGS[@]+"${ENVARGS[@]}"} || rc=$?
  if [ "$rc" -ne 0 ]; then
    if [ "$rc" -ge 126 ]; then
      echo "could not run the config check (exit $rc); nothing was installed and the old binary is still running" >&2
    else
      echo "the new binary refuses $ETC/config.toml (its message is above); nothing was installed and the old binary is still running" >&2
    fi
    exit 1
  fi
fi

# Replace the binary by rename, never by writing over it: a running process holds the inode and a
# straight copy fails with "Text file busy". The rename is atomic and leaves the old inode open
# for the process still using it.
[ -x "$OPT/bin/kittiwake" ] && cp -p "$OPT/bin/kittiwake" "$OPT/bin/kittiwake.prev"
install -m 0755 "$BIN" "$OPT/bin/kittiwake.new"
mv -f "$OPT/bin/kittiwake.new" "$OPT/bin/kittiwake"

if [ ! -f "$ETC/config.toml" ]; then
  [ -r "$FOG" ] || { echo "no readable $FOG to take nest credentials from" >&2; exit 1; }
  field() {
    awk -v k="$1" '/^\[nest\]/{f=1;next} /^\[/{f=0} f && $0 ~ "^"k"[[:space:]]*=" {
      sub(/^[^=]*=[[:space:]]*/,""); gsub(/^"|"$/,""); print; exit }' "$FOG"
  }
  # Foghorn points at one nest path; kittiwake addresses every nest under the shared origin, so the
  # trailing path segment comes off.
  NEST_ORIGIN=$(field url | sed -E 's#/[a-z-]+/?$##')
  NEST_USER=$(field username)
  NEST_PASS=$(field password)
  [ -n "$NEST_ORIGIN" ] || { echo "could not read the nest origin from $FOG" >&2; exit 1; }

  umask 027
  cat > "$ETC/config.toml" <<CONF
# Written by deploy/install.sh. Edit freely; re-running install will not overwrite it.
bind = "127.0.0.1:8080"

# Both off on a fresh install, and both should be turned on deliberately.
#
# The scheduler makes this process the writer for Lodestar's Postgres, which is a decision rather
# than a default. The warmer issues one fold per dataset per minute against the protocol nest; that
# is a reduction in load once traffic moves here, but pure addition while nothing points at this
# service yet, and the nest it would be adding to has been memory-exhausted once.
scheduler_enabled = false
warmer_enabled = false

# Left unset deliberately: the read routes need no database, and the Postgres password lives in the
# platform's environment rather than on this box. Add it when the ingest jobs move over.
# database_url = "postgresql://lodestar:...@127.0.0.1:5433/lodestar?sslmode=require"
database_max_connections = 16

# The eligibility oracle and the GRT supply need this. Use the same value as the platform's
# ARBITRUM_RPC_URL - a keyed endpoint, not a public one. The refresh job makes one oracle call per
# indexer, and the free endpoints answer a handful per minute before refusing.
# arbitrum_rpc_url = "https://arb-mainnet.g.alchemy.com/v2/..."

[nest]
origin = "$NEST_ORIGIN"
user = "$NEST_USER"
password = "$NEST_PASS"

[nest.permits]
# Two, because that is what the nest admits: 'NUTHATCH_SQL_MAX_CONCURRENCY=2' on the allocations
# nest since its Burrmill switch on 2026-10-01 (it was 4 before). The rule above is not advice - a
# pool wider than the nest means the surplus is sent anyway
# and shed, and 'nest_busy' climbs while every page shows a console error for work the backend was
# never able to take.
#
# It was briefly eight, as a mitigation for a fold that starved itself by needing more permits than
# existed. That is fixed properly in 'many()', which admits at most 'lane_width - 1' at a time, so
# a six-part fold fits inside any pool of two or more and the mitigation is no longer needed. Raising this
# again only moves the shedding from kittiwake's gate to the nest's.
alloc = 2
gns = 2
dips = 2
"dips-sepolia" = 2
"legacy-flows" = 2
qos = 2
supply = 2
"supply-mainnet" = 2

[cache]
max_entries = 8192
schema_version = 1

[limits]
sql_per_minute = 10
read_per_minute = 120
mutation_per_minute = 30
exempt_ips = []
CONF
  chown root:kittiwake "$ETC/config.toml"
  chmod 0640 "$ETC/config.toml"
  echo "wrote $ETC/config.toml"
else
  echo "kept the existing $ETC/config.toml"
fi

install -m 0644 "$UNIT" /etc/systemd/system/kittiwake.service
systemctl daemon-reload
systemctl enable kittiwake
# Restart rather than `enable --now`, which is a no-op on an already-running unit and would leave
# the old binary serving after a perfectly successful-looking install. Learned the obvious way.
systemctl restart kittiwake
sleep 3
if ! curl -fsS --max-time 5 http://127.0.0.1:8080/health >/dev/null && [ -x "$OPT/bin/kittiwake.prev" ]; then
  echo "new binary is not healthy; restoring the previous one" >&2
  mv -f "$OPT/bin/kittiwake.prev" "$OPT/bin/kittiwake"
  systemctl restart kittiwake
  sleep 3
  curl -fsS --max-time 5 http://127.0.0.1:8080/health >/dev/null && echo "previous binary restored and healthy" >&2
  exit 1
fi
systemctl --no-pager --lines=0 status kittiwake | head -5
echo
echo "running: $(readlink -f /proc/$(systemctl show kittiwake -p MainPID --value)/exe 2>/dev/null || echo unknown)"
echo "smoke:"
curl -fsS --max-time 5 http://127.0.0.1:8080/health && echo
curl -fsS --max-time 20 http://127.0.0.1:8080/ready | head -c 400 && echo
