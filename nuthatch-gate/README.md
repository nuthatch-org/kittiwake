# nuthatch-gate

What nuthatch's release gate and its Helsinki roll smoke send to production nests. These files
carry kittiwake's statements and call-site names, so they live here, in a private repo, and
nuthatch (public) reads them by path at run time.

| Path | Consumer |
| --- | --- |
| `<set>-queries.tsv` | nuthatch `scripts/release-gate-run.sh`. With one nest it reads `GATE_SET`; with several, the ThinkPad's gate config (`~/release-gate/nests.conf`) names one set per nest, all under `~/kittiwake/nuthatch-gate/`. |
| `smoke/<unit>.sql` | nuthatch `deploy/roll-helsinki-from-mac.sh`, through `SMOKE_DIR` (default `~/Projects/kittiwake/nuthatch-gate/smoke`). It copies the unit's file to the box, and `deploy-nest.sh roll --smoke` runs it against the unit once it is on the new version. `qos-reo-nest.sql` is for the ThinkPad unit, which that script does not roll. |
| `collect/` | The generator for the query sets. |

One set per production nest, each from kittiwake's statements for that `NestId`:

| Set | Nest (unit) | `NestId` |
| --- | --- | --- |
| `alloc` | graph-allocations-nest-next | `Alloc` |
| `qos` | qos-reo-nest (ThinkPad) | `Qos` |
| `gns` | graph-gns-nest-next | `Gns` |
| `dips` | nuthatch-dips | `Dips` |
| `data-services` | data-services-nest | `Services` |
| `staking-archive` | graph-staking-legacy-readonly (serve-only) | `LegacyFlows` |

Parameters are pinned to 2026-10-03. The QoS set asks one closed day, 2026-10-01, for both the day
job's statements and the QoS card's live ones, because production asks that nest one day at a time;
it never counts a view's whole history. The staking archive is asked only by delegation-flow windows
that reach back past the archive's end, so its set carries Lodestar's 90-day and one-year windows.

## Regenerating the query sets

    nuthatch-gate/collect/collect-queries.sh --write
    git diff nuthatch-gate/

`collect-queries.sh <set>` prints one set. It compiles `collect/main.rs` against a copy of
`crates/read/src/sql.rs` with plain rustc, plus `extract.rs`, which it cuts from four other files:
the names job's statement, the census services, the dips agreement stages and the SQL page's sample.
Statements built from those refresh themselves; the ones `main.rs` marks `inline` are hand copies of
call sites and must be re-read when those change (`grep -rn 'NestId::' crates`). CI runs
`collect-queries.sh --check`, which fails when any committed set no longer matches the source.

The smoke files are written by hand, one per production unit, from that unit's real consumers.
