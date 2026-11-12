# What each remaining route is waiting on

Twenty of the twenty-two routes lodestar still serves are written here, registered, and answering
in production. The work left is not porting. It is a cutover: a line in lodestar's `MIGRATED`, and
for most of them a privilege or a credential that only somebody with access to Nuremberg can grant.

This file is that list. It was written on 2026-09-11 after diffing the two services route by route,
which the parity harness excuses these from doing, and finding three faults in the first afternoon.

## The blocker almost everything shares

`kittiwake` has **no privilege at all** on the tables the Dock reads and writes.
Checked against the live database, not inferred:

| table | kittiwake has | kittiwake needs |
|---|---|---|
| `sync_bounties` | none | SELECT, INSERT, UPDATE |
| `studio_subgraphs` | none | SELECT, INSERT, UPDATE, DELETE |
| `studio_deploy_keys` | none | SELECT, INSERT, UPDATE |
| `clickthrough_events` | none | INSERT |
| `parameter_changes` | SELECT | INSERT |

**All of the above were granted on 2026-09-11** and the table is kept as the record of what each
needs and why. `/health` now reports five remaining gaps - `community_votes`, `delegations`,
`indexer_scores`, `roadmap_community_updates`, `servability_rounds` - which are declared in
`db/schema.sql` because that file describes the whole database, not the subset this service uses.
Whether those want granting or want excluding from the check is an open question.

The verbs are derived from the statements in `crates/db`, not chosen generously.

`parameter_changes` is not waiting for a cutover at all: the directory refresh writes the parameter
trail on every run and has been failing on it, logging "the parameter trail could not be written;
the directory refresh continues without it" and carrying on. That one is a live fault today.

```sh
GRANT_DOCK=1 bash deploy/grant-database.sh
```

`deploy/grant-database.sh` is the sanctioned path and now carries these, with the verbs derived
from the statements in `crates/db` rather than chosen generously, and a line of reasoning beside
each. The SQL was dry-run against the live database inside a transaction and rolled back, so the
table and sequence names are known good.

Two things to know before running it. It rotates the role's password and restarts the unit on every
run - by design, and safe, because the new password goes straight into the config - so it is not a
grant on its own. It also refuses outright when `scheduler_enabled` is true, and that check now runs
before anything is changed rather than after the password has already been rotated: run on
2026-09-11 it refused at the end, leaving Postgres holding a new password and the config the old
one, which does not fail until the next restart because the pool's open connections keep working.
And `GRANT_WRITER=1` wants running too: the ingest tables have their writes, but
`parameter_changes` does not, which means the writer block was applied by hand at some point and
that line was missed. That is why the directory refresh has been failing to write the parameter
trail on every run.

Run as a role that owns them (`lodestar`, or `postgres`). Nothing in this service asks for DDL
rights and nothing should: `schema_gaps` checks the schema and refuses to reshape it.

## Per block

### The Dock — nine routes

Grants above, plus `session_secret` under `[studio]` in `/etc/kittiwake/config.toml`, matching
Vercel's `SESSION_SECRET` **exactly**. It is written by:

```sh
bash deploy/configure-dock.sh
```

It reads the value from the Lodestar project's production environment, writes it straight
to the box over ssh, and echoes it nowhere. It travels as the first lines of the remote script's
stdin rather than as arguments, which `ps` would show to every user on the box, or through
`SendEnv`, which silently sends nothing when the server has no matching `AcceptEnv`.

Then, before the switch and not after:

```sh
DOCK_SESSION_SECRET='…' DOCK_SESSION_TOKEN='<a real studio_session cookie>' \
  cargo run -p kittiwake-dock --bin verify-session
```

The cookie is stateless and signed. A secret differing by a trailing newline signs every logged-in
user out the moment the rewrite points here, and nothing errors. That binary exists because this
failure is silent.

`/api/studio/auth` already answers `{"address":null}` signed out, and its shape matches lodestar's
exactly. The other Dock GETs answer `permission denied` today, which is the table above.

### The disassembler — two of three

`/api/disassembly` and `/api/disassembly/diff` need no credential. What they needed was a look, and
it found the `signalledGrt` casing that #99 fixed. Once that deploys, `diff`'s shape is identical to
lodestar's; the report route still omits `sourceHint`, whose only reader lodestar#144 was to delete
and has not, so that is a live regression until the issue lands or the field is ported.

`/api/disassembly/verify` is the one genuinely unported route: it needs the sandbox microVM.

### `/api/sql/receipt`

`TATTLER_ISSUER_KEY` on the box. A custody decision rather than a port, the same question as
`/api/cron/tap-provision`, and worth noting that moving it here turns the tattler signer from a WASM
bundle loaded through two layers of deliberate bundler evasion into an ordinary Rust dependency.

### `/api/indexer/present-poi`

Pinned in lodestar's `NEVER_FORWARD`. The original reason is gone - it was being parsed as an
address, and kittiwake now registers the literal path and answers with its own validation message.
The reason that stands is that nothing exercises a POST that queues an action on a live indexer
agent, and validating a bad payload is not the same as submitting a good one.

## How the faults above were found

Two curls and a script: fetch the same path from `www.lodestar-dashboard.com` and
`api.lodestar-dashboard.com`, compare the field names and types, ignore the values. The parity
harness cannot do this for these routes, which is why they are excused from it - and being excused
from the check is not the same as passing it.
