# Design decisions, and where this deviates from the brief

The full research brief is `docs/rust-backend-research-prompt.md` in the Lodestar repo. This file
records what was decided, why, and the five places the implementation departs from the design it
was built to. A decision without its reason is a decision the next person will reverse by accident.

## The load-bearing choice: one process

The gate in `crates/nest` is a semaphore sized to what each nest admits. A semaphore is only the
truth if there is one of it. The Node service's gate was per process and the platform ran many, so
the only thing bounding concurrency was the nest refusing work.

This is why there is no api/worker split, and why the scheduler lives in the same binary as the
router. Splitting them would reintroduce exactly the fault being fixed. If you ever find yourself
reaching for a second deployable, the gate has to move to something shared first, and at that point
you have bought a distributed system to solve a problem one process did not have.

## Where it runs

Nuremberg, beside Postgres and Redis. Not Helsinki beside the nests.

Helsinki is 7.7 GB and already carries every nest, Caddy, the data-service gateway, a TAP
aggregator, a chat relay and the backup target. The protocol nest is capped at `MemoryHigh=2G` and
has been memory-exhausted once. Adding a service with a large cache there risks pushing the nest
into swap, which is the one thing the whole exercise is meant to prevent.

The split option, with a nest proxy on Helsinki and the rest on Nuremberg, buys a loopback hop to
the nests and costs a place where the gate can fragment. The nest hop is cold, cache-fronted and
low-rate by design. Rejected.

## The freshness model

Freshness is derived from the cadence at which the source actually moves, not chosen per call site.
`CachePolicy::from_poll_interval` floors it at 30 seconds, because below that the cache stops
absorbing bursts and the nest feels every page view. Stale-while-revalidate is capped at five
minutes, because past that, waiting for the truth beats serving something the reader would not
accept if they knew its age.

Three outcomes, in order: fresh, served silently; stale, served *labelled* with a refresh behind
it; expired, fetched with everyone else waiting coalesced onto one query.

## Deviations from the design as written

1. **`serde_json::Value` in the cache, not pre-serialized bytes.** The design implies storing what
   the response will be. Values are stored instead, which costs one serialize per warm hit and buys
   a cache that can be introspected and reshaped without a format migration. Warm reads measured
   under a millisecond, so the cost is not the constraint. Revisit if it ever is.
2. **Runtime `sqlx` queries, not the checked macros.** The design mandates compile-time checking.
   That needs either a reachable database or a committed `.sqlx` offline cache, and `DATABASE_URL`
   is a Sensitive production variable that cannot be pulled locally. The queries are runtime for
   now. Generating `.sqlx` against the primary and switching to `query!` is a task, not a change of
   mind, and should happen before the ingest jobs land.
3. **No Redis yet.** The design keeps Redis for pub/sub and for rehydrating the cache after a
   restart. Neither is needed until Scuttlebutt's stream is ported and the warm-set matters across
   restarts. The in-process cache is the whole of L1 today, and a cold start refills it in about a
   minute.
4. **The warmer warms list folds only.** Warming every per-address profile would spend a
   four-permit budget on pages nobody is reading. Following the cache's own access frequency to
   pick the hot N is the next step and wants real traffic to choose N from.
5. **`cron` the crate is a dependency but unused.** Schedules are `Every(Duration)` today, which is
   what all seventeen jobs actually are. The dependency stays for the moment a job needs a real
   cron expression, and should be dropped if that moment does not come.

## The bug the tests caught

The first rate limiter took a burst parameter independent of the budget, so a three-per-minute
limit configured with a burst of forty admitted forty callers in one second. It read as working.
Burst is now clamped to the budget, and
`a_burst_can_never_exceed_the_budget_it_belongs_to` fails if that is ever undone.

## What was deliberately not done

- **No Redis-backed rate limiter or gate.** In one process, in-memory is exact and free. A network
  hop per request would buy a failure mode and no correctness.
- **No attempt to make the nest's cold fold faster.** The nests are the contract and are off
  limits. The fold is engineered out of the request path instead. A nest-side materialized view for
  the whole-history folds would turn 23 seconds into milliseconds and is the single highest-leverage
  change available to anyone, but it belongs to the nest, not here.
- **No second source for a protocol figure, ever.** There is no arm in the error mapping that
  reaches for one. The x402 route, when it lands, is a caller-paid pass-through and must stay walled
  off from `/alloc`.
