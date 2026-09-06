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
| Config, logging, metrics, health and readiness | `config`, `obs` | done |

Three routes are live as the pattern for the rest: `/api/network-stats`, `/api/indexers`,
`/api/epochs`, plus `/api/sql/catalog` and `/api/sql/query`.

Not built yet, in the migration's own order: the remaining `/alloc` reads, the other nests' reads,
the seventeen ingest jobs, the chain and IPFS clients, then the product layer (the Dock,
Scuttlebutt, push, Lodie, signatures, the wasm disassembler). See `docs/migration.md`.

## Measured

Against a stand-in nest whose fold takes 1.5s, on a laptop:

| | |
|---|---|
| Cold read | 1.513 s (the fold itself) |
| Warm read | 0.99 ms |
| 20 concurrent cold readers of one uncached key | **1** nest query, 19 coalesced |
| 200 requests against a 120/min budget | 25 served, 175 refused with `retry-after` |

The real bar is in `docs/design.md`. The honest part of it: the nest's intrinsic cold fold does not
get faster, because the nests are the contract and are off limits. The win is that a reader never
meets one, and that each fold runs at most once per freshness window.

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
  read/     the cached read layer and the ported SQL
  sqlplay/  public SQL: allowlist and screening
  sched/    the ingest scheduler
  warmer/   keeping the expensive folds hot
  edge/     axum router, budgets, headers, error mapping
  kittiwake-bin/  wiring and shutdown
```

## Deployment

Nuremberg, beside Postgres and Redis, not Helsinki beside the nests: that box is 7.7 GB, already
carries every nest with the protocol nest capped at 2 GB, and has been memory-exhausted once.
`deploy/kittiwake.service` targets 768 MB soft, 1 GB hard. `deploy/Caddyfile.fragment` terminates
TLS and is the only thing that may set `X-Forwarded-For`, which is why the service honours that
header only from a loopback peer.
