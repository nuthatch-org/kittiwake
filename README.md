# kittiwake

Lodestar's backend. One Rust process that owns every server-side thing the dashboard does, and
fronts the nuthatch nests with a global admission gate, request coalescing and a freshness model.

The frontend gets **all** of its data from here. Not the nests, not Postgres, not an RPC, not a
price API. The only other thing the browser talks to is the user's wallet, for signing.

Built to the design in `docs/design.md`, which is itself an answer to
`lodestar/docs/rust-backend-research-prompt.md`.

## Why it exists

The Node service it replaces had a one-slot gate in front of a nest that admits four concurrent
queries, and the platform ran many copies of that process, so the only thing actually bounding
concurrency was the nest refusing work. Crons collided with page loads and read `server busy`.
Nothing coalesced, so twenty readers of a cold page cost twenty whole-history folds. Freshness was
a TTL picked by hand at each call site.

This is one process on purpose. That is what makes the semaphore in `crates/nest` the truth rather
than a suggestion.

## State of play

Working, tested, and exercised end to end against a stand-in nest:

| Piece | Crate | |
|---|---|---|
| Admission gate, per-nest permits, cron lane, single-flight, negative cache, `/ready` | `nest` | done |
| Freshness model, stale-while-revalidate, cache keys | `cache` | done |
| Cached read layer, provenance propagation | `read` | done |
| HTTP edge, per-IP budgets, error taxonomy, provenance headers | `edge` | done |
| Public SQL: dataset allowlist, read-only screening | `sqlplay` | done |
| Scheduler: stagger, jitter, overlap, `cron_runs` | `sched` | done |
| Warmer | `warmer` | list folds done; per-address hot set pending real traffic |
| Postgres pool and `cron_runs` writer | `db` | done |
| Ingest jobs: epochs, delegations, allocations, disputes, network snapshot | `ingest` | done; RAV and the chain-dependent jobs remain |
| SSRF guard, with the DNS-rebinding check | `ssrf` | done |
| The eleven-dimension composite score | `score` | done |
| Chain reads: the eligibility oracle, GRT supply | `chain` | done |
| Price, TVL, and probing an indexer's own status endpoint | `ext` | done |
| Delegator APR, realised return, pool exchange rate, capacity | `rewards` | done |
| The refresh pipeline that writes the scored directory | `ingest` | done |
| Config, logging, metrics, health and readiness | `config`, `obs` | done |
| The Dock's session scheme, held to tokens the incumbent minted | `dock` | done |

## The written record

| | |
|---|---|
| [`docs/status.md`](docs/status.md) | Per route: parity verdict and timings. **Generated**, never edited. |
| [`docs/decisions.md`](docs/decisions.md) | Judgement calls, with the reasoning. The file for what would otherwise be re-litigated or quietly undone. |
| [`docs/migration.md`](docs/migration.md) | What has moved, what has not, and what is a deletion rather than a port. |
| [`docs/benchmarks.md`](docs/benchmarks.md) | The acceptance bar, including the number that deliberately does not improve. |
| [`docs/parity.md`](docs/parity.md) | How parity is checked, and why by shape rather than value. |
| [`docs/design.md`](docs/design.md) | The gate, coalescing and freshness model. |


**Live in production since 2026-09-07, 11:24 UTC.** The dashboard's `/api/*` is rewritten to this
service for **31 routes**; the rest are still Next handlers. `kittiwake-parity` reports **31
matching, 1 breaking** against a frozen pre-rewrite deployment, and writes
[`docs/status.md`](docs/status.md) on every run - per route, with timings. The one breaking route is
not in production; see [`docs/migration.md`](docs/migration.md).

The cutover was not clean and the record says so: eight routes shipped with the wrong shape and
answered `200` for two hours, because the rewrite list and the parity table were two lists nobody
had connected. Seven were fixed the same day. What that cost and what now prevents it is in
[`docs/decisions.md`](docs/decisions.md).

Not built yet, in the migration's own order: `indexing-status`, then the product layer - the Dock,
Scuttlebutt, the wasm disassembler, the SQL named tier and receipts - and a long tail that wants
triaging rather than porting.

**Push notifications and Lodie were discontinued on 2026-09-06** and are deliberately absent.

## Measured

Release build, against a stand-in nest whose fold takes the 23,500 ms measured on production:

| | |
|---|---|
| Cold read | 23,506 ms - the fold itself, unchanged and unchangeable from here |
| Warm p50 / p95 / p99 / max | 0.26 / 0.52 / 0.63 / 1.35 ms |
| Warm throughput | 108,728 req/s at concurrency 32 |
| 40 concurrent cold readers of one key | **1** nest query, all 40 answered |
| Nest queries per warm request | 0.0002 |

Reproduce with `cargo run --bin kittiwake-bench`. The full comparison against the migration's
acceptance bar, including the one number that deliberately does not improve, is in
[`docs/benchmarks.md`](docs/benchmarks.md).

## Running it

```sh
cargo build --release
KITTIWAKE_CONFIG=./deploy/config.example.toml ./target/release/kittiwake
```

Config is TOML with a `KITTIWAKE_` environment overlay (`KITTIWAKE_NEST__ORIGIN=...`). There is no
default that reaches the network: an unset nest origin is a startup failure, not a service that
answers every protocol route with an error for a week.

```sh
cargo test          # 44 tests, no network or database needed
cargo clippy --all-targets -- -D warnings
```

## Endpoints

| | |
|---|---|
| `GET /health` | is this process answering |
| `GET /ready` | what every nest says about itself; the frozen archive is reported without judgement |
| `GET /metrics` | Prometheus |
| `GET /api/network-stats`, `/api/indexers`, `/api/epochs` | nest-backed reads |
| `GET /api/sql/catalog`, `POST /api/sql/query` | the public SQL surface over five allowlisted datasets |

Every data response carries provenance in the body and in headers: `x-kittiwake-served-from`,
`x-kittiwake-as-of`, `x-kittiwake-sealed-through`, `x-kittiwake-stale`, `x-kittiwake-age`.

Every data response also says `Cache-Control: no-store`. That is not an oversight. Vercel's CDN has
cached rewritten external origins by upstream `Cache-Control` since April 2026, and letting it hold
protocol figures past their freshness budget would hand our staleness model to somebody else's
cache.

## The rules this service keeps

1. **One source per route.** A nest that cannot answer produces an error carrying the nest's own
   reason. There is no fallback, and no code path that reaches for a second source.
2. **Never stale-as-fresh.** A value past its freshness budget is served only while labelled
   `stale`, in the body and the headers, or not at all.
3. **The nest's budget is the budget.** Our permit counts match what each nest admits. Raising them
   is a change to the nest, never to us.
4. **A background job can never take the last permit.** Crons and the warmer draw from an inner
   lane one narrower than the real one.
5. **The browser never learns the nest origin or credentials.**

## Layout

```
crates/
  types/    Provenance, ServedFrom, ApiError - the shared vocabulary
  config/   TOML + env, redacted Debug, validation at startup
  obs/      JSON logs, Prometheus registry
  nest/     the only code that opens a socket to a nest
  cache/    freshness policy, keys, the in-process store
  db/       Postgres pool, cron_runs
  ingest/   the nest-to-Postgres jobs
  ssrf/     what may be fetched, and what may never be
  score/    the composite indexer score
  chain/    Arbitrum and mainnet contract reads
  ext/      price, TVL, and probing an indexer's own endpoint
  rewards/  what a delegator actually earns
  parity/   compares our shape against the incumbent's, route by route
  read/     the cached read layer and the ported SQL
  sqlplay/  public SQL: allowlist and screening
  sched/    the ingest scheduler
  warmer/   keeping the expensive folds hot
  edge/     axum router, budgets, headers, error mapping
  kittiwake-bin/  wiring and shutdown
```

## Deployment

**Deployed on the Nuremberg primary since 2026-09-06**, on loopback behind Caddy, with the
scheduler and warmer off until traffic moves. `deploy/install.sh` is idempotent and takes the nest
credentials from the box rather than asking anyone to paste a secret. `deploy/build-linux.sh`
builds inside Debian bookworm, because the obvious build host has a newer glibc than the target and
its binaries will not run there.

Nuremberg, beside Postgres and Redis, not Helsinki beside the nests: that box is 7.7 GB, already
carries every nest with the protocol nest capped at 2 GB, and has been memory-exhausted once.
`deploy/kittiwake.service` targets 768 MB soft, 1 GB hard. `deploy/Caddyfile.fragment` terminates
TLS and is the only thing that may set `X-Forwarded-For`, which is why the service honours that
header only from a loopback peer.
