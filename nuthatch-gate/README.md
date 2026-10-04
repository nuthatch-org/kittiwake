# nuthatch-gate

What nuthatch's release gate and its Helsinki roll smoke send to production nests. These files
carry kittiwake's statements and call-site names, so they live here, in a private repo, and
nuthatch (public) reads them by path at run time.

| Path | Consumer |
| --- | --- |
| `alloc-queries.tsv` | nuthatch `scripts/release-gate-run.sh`, through `GATE_SET`. The ThinkPad's `release-gate.service` points it at `~/kittiwake/nuthatch-gate/alloc-queries.tsv`. |
| `smoke/<unit>.sql` | nuthatch `deploy/roll-helsinki-from-mac.sh`, through `SMOKE_DIR` (default `~/Projects/kittiwake/nuthatch-gate/smoke`). It copies the unit's file to the box, and `deploy-nest.sh roll --smoke` runs it against the unit once it is on the new version. `qos-reo-nest.sql` is for the ThinkPad unit, which that script does not roll. |
| `collect/` | The generator for `alloc-queries.tsv`. |

## Regenerating the query set

    nuthatch-gate/collect/collect-queries.sh > nuthatch-gate/alloc-queries.tsv
    git diff nuthatch-gate/alloc-queries.tsv

It compiles `collect/main.rs` against a copy of `crates/read/src/sql.rs` with plain rustc. Statements
built in `sql.rs` refresh themselves; the ones `main.rs` marks `inline` are hand copies of call
sites and must be re-read when those change. CI runs `collect-queries.sh --check`, which fails when
the committed set no longer matches `sql.rs`.

The smoke files are written by hand, one per production unit, from that unit's real consumers.
