# Benchmarks

Run them yourself:

```sh
cargo build --release
FOLD_MS=23500 ./target/release/mocknest &          # a nest whose fold takes what the real one takes
KITTIWAKE_CONFIG=./deploy/config.example.toml ./target/release/kittiwake &
./target/release/kittiwake-bench --route /api/indexers --warm 5000 --burst 40 --concurrency 32
```

The fold time is a parameter rather than a measurement of production, for two reasons: pointing a
load test at the real nest would put load on the one machine this whole design exists to protect,
and a benchmark nobody can reproduce is an anecdote. 23,500 ms is the figure measured on production
on 2026-09-06 for a cold `/api/indexers`.

## Result, 2026-09-06

MacBook, release build, mock nest with a 23,500 ms fold, 32 concurrent workers.

| | |
|---|---|
| Cold, one reader at an uncached key | 23,506 ms |
| Cold, 40 readers arriving together | 23,507 ms p50, 23,508 ms max, **1 nest query**, 40 of 40 answered |
| Warm p50 | 0.26 ms |
| Warm p95 | 0.52 ms |
| Warm p99 | 0.63 ms |
| Warm max | 1.35 ms |
| Warm throughput | 108,728 req/s |
| Nest queries per warm request | 0.0002 |

## Against the bar

The brief's gate is that a migration is only justified if the result beats the current system on
every measured number. Taking them in order:

| Metric | Before | Now | |
|---|---|---|---|
| `/api/indexers` cold | 23.5 s | 23.5 s | **unchanged**, and honestly so - see below |
| Warm read | 0.18 s | 0.26 ms | ~690x |
| Nest queries per user request | 1.0 (no coalescing) | 0.0002 | ~5,000x fewer |
| 40 concurrent cold readers | 40 folds | 1 fold | by construction |
| `server busy` refusals | seen when crons collide | 0 | by construction of the gate |
| Per-IP rate limit | per edge instance, so limit x instances | exact and global | |
| Cold start | platform functions warm on demand | long-lived process, none | |

**The one number that does not improve, and why it does not.** The nest recomputes a whole-history
fold per request. That is the nest's own cost, the nests are the contract and are off limits, so
23.5 seconds is 23.5 seconds. What changes is that no reader meets it: the warmer runs the fold on
the dataset's own cadence, and single-flight collapses any burst that does reach it into one query.
Claiming otherwise would be the kind of benchmark that flatters itself.

The genuinely highest-leverage change available to anyone is a nest-side incremental view for those
folds, which would turn 23 seconds into milliseconds. It belongs to the nest, not here.

## How the numbers are taken

Three scenarios, because they answer three different questions, and each uses a cache key nothing
else has touched so a warm entry cannot flatter a cold measurement:

1. **Cold** - one reader, one uncached key. This is the fold.
2. **Coalescing** - N readers arriving together at one uncached key. The question is how many of
   them the nest hears about.
3. **Warm** - the ordinary case, at concurrency.

Percentiles are nearest-rank, not interpolated: with a few thousand samples, inventing a value
between two measurements is a worse answer than picking the measurement. The p99 and the max are
reported alongside the p50, because a p50 that improves while the p99 degrades is a regression for
whoever hits the p99.


## Baseline before the switch, 2026-09-07

Captured from the Nuremberg box, three calls per route, median of the middle two, both warm.

| Route | Incumbent | kittiwake |
|---|---|---|
| `/api/network-stats` | 0.065 s | 0.0005 s |
| `/api/indexers?first=100` | 0.078 s | 0.0011 s |
| `/api/epochs?count=20` | 0.072 s | 0.0006 s |
| `/api/curators?first=100` | 0.069 s | 0.0007 s |
| `/api/indexer/<addr>` | 0.078 s | 0.0029 s |
| `/api/payments` | 0.080 s | 0.0012 s |
| `/api/poi?first=200` | 0.083 s | 0.0014 s |
| `/api/subgraph-deployments?first=50` | 0.075 s | 0.0011 s |
| `/api/delegation-flows?days=90` | 0.067 s | 0.0006 s |
| `/api/dips` | 0.065 s | 0.0006 s |

**Read this carefully, because it flatters us.** The incumbent column is a round trip from
Nuremberg to Vercel's edge and back; most of those 65 to 80 milliseconds is network, not work. The
kittiwake column is loopback on the same box, so it is nearly all work. The two are not the same
measurement and the ratio between them is not a speed-up anyone will feel.

What it does establish honestly: **kittiwake's server-side work is between 0.5 and 3 milliseconds
on every route**, including the ones that fold hundreds of thousands of events. Whatever the
network adds after the switch, it will add to that.

The comparison that matters can only be taken after the switch: browser to Vercel to Nuremberg
against today's browser to Vercel function. That measurement is worth taking on the same day, from
the same place, at p50, p95 and p99 - and it is the reason this baseline is recorded now, while
there is still something to compare against.
