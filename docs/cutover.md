# What each remaining route is waiting on

Twenty of the twenty-two routes lodestar still serves are written here, registered, and answering
in production. The work left is not porting. It is a cutover: a line in lodestar's `MIGRATED`, and
for most of them a privilege or a credential that only somebody with access to Nuremberg can grant.

This file is that list. It was written on 2026-09-11 after diffing the two services route by route,
which the parity harness excuses these from doing, and finding three faults in the first afternoon.

## The blocker almost everything shares

`kittiwake` has **no privilege at all** on the tables the Dock and Scuttlebutt read and write.
Checked against the live database, not inferred:

| table | kittiwake has | kittiwake needs |
|---|---|---|
| `sync_bounties` | none | SELECT, INSERT, UPDATE |
| `studio_subgraphs` | none | SELECT, INSERT, UPDATE, DELETE |
| `studio_deploy_keys` | none | SELECT, INSERT, UPDATE |
| `scuttlebutt_messages` | none | SELECT, INSERT, UPDATE |
| `scuttlebutt_bans` | none | SELECT, INSERT |
| `clickthrough_events` | none | INSERT |
| `parameter_changes` | SELECT | INSERT |

The verbs are derived from the statements in `crates/db`, not chosen generously.

`parameter_changes` is not waiting for a cutover at all: the directory refresh writes the parameter
trail on every run and has been failing on it, logging "the parameter trail could not be written;
the directory refresh continues without it" and carrying on. That one is a live fault today.

```sql
GRANT SELECT, INSERT, UPDATE         ON sync_bounties        TO kittiwake;
GRANT SELECT, INSERT, UPDATE, DELETE ON studio_subgraphs     TO kittiwake;
GRANT SELECT, INSERT, UPDATE         ON studio_deploy_keys   TO kittiwake;
GRANT SELECT, INSERT, UPDATE         ON scuttlebutt_messages TO kittiwake;
GRANT SELECT, INSERT                 ON scuttlebutt_bans     TO kittiwake;
GRANT INSERT                         ON clickthrough_events  TO kittiwake;
GRANT INSERT                         ON parameter_changes    TO kittiwake;

-- A table whose id is a SERIAL needs its sequence too, or the INSERT is refused
-- with a message about the sequence rather than the table, which reads as a
-- different fault than it is.
GRANT USAGE, SELECT ON SEQUENCE
  sync_bounties_id_seq,
  studio_subgraphs_id_seq,
  studio_deploy_keys_id_seq,
  scuttlebutt_messages_id_seq,
  scuttlebutt_bans_id_seq,
  clickthrough_events_id_seq,
  parameter_changes_id_seq
  TO kittiwake;
```

Run as a role that owns them (`lodestar`, or `postgres`). Nothing in this service asks for DDL
rights and nothing should: `schema_gaps` checks the schema and refuses to reshape it.

## Per block

### The Dock — nine routes

Grants above, plus `session_secret` in `/etc/kittiwake/config.toml` matching Vercel's
`SESSION_SECRET` **exactly**. The box has no `[dock]` section at all today.

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

### Scuttlebutt — six routes

Grants above, plus a Scuttlebutt config section: the route answers "Scuttlebutt is not configured"
today. It reads the same `scuttlebutt_messages` table and the same room, so a cutover keeps the
history rather than opening an empty one.

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
