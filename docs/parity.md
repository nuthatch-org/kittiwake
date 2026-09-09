# Parity

The gate before any route moves. Capture what the incumbent returns, assert we return the same
thing, and only then point traffic at us.

```sh
cargo build --release --bin kittiwake-parity
# from somewhere that can reach both, which in practice means the primary
kittiwake-parity --incumbent https://www.lodestar-dashboard.com --ours http://127.0.0.1:8080
```

Exits non-zero when anything breaking is found, so it can gate CI.

## What it compares, and what it deliberately does not

**Shape, not values.** Two calls a second apart legitimately differ: a block advances, an
allocation closes, a price moves. A harness that asserts on values fails constantly and is
therefore ignored, which is worse than no harness at all. What must not differ is the set of paths
in the JSON and the type at each one, because that is what the frontend's hooks are written
against.

A shape is a flat map from path to the set of types seen there. Array indices collapse to `[]`, so
a hundred rows describe one shape and an optional field reads honestly as `{string, null}` rather
than as two incompatible shapes.

Three kinds of difference, and only two of them matter:

| | |
|---|---|
| **missing** | the incumbent has a path and we do not. Breaks consumers. |
| **retyped** | both have it, different types. Breaks consumers *silently*, which is worse. |
| **added** | we have a path they do not. Harmless: consumers tolerate extra keys. |

Numbers carried as strings get their own type tag. The protocol deals in wei, which does not
survive an `f64`, so a field that quietly changed from `"1000"` to `1000` would break every
consumer that parses it, and would otherwise look like a trivial difference.

A wider type is not a break. Returning `{string, null}` where they only ever returned a string is a
superset and safe; narrowing to only-null is not.

## Two differences from the subgraph that are deliberate

Both measured against production, both certain to be reported as regressions by whoever compares
revenue across the cutover if they are not written down first. Neither is a bug and neither is
going to be fixed. #9

**Seventy-five collections with no allocation.** `escrow__escrow_collected` carries 71,567 rows;
71,492 of them pair with a `subgraph_service__query_fees_collected` event on transaction, payer,
service provider and amount. The remaining 75 are collections for a data service that is **not**
SubgraphService, so no query-fee event exists to carry an allocation id.

When #9 was written the nest did not index GraphTallyCollector's own `PaymentCollected`, and the
plan was to join on it once nuthatch#1078 landed. **It has landed** - `tally__payment_collected` is
in the staking nest - and the join was measured before being written:

| | |
|---|---:|
| orphaned collections | 75 |
| of those, named by a `PaymentCollected` row | **75** |
| distinct `collectionId`s among them | 10 |
| of those ten, resolving to a real allocation | **0** |
| distinct data services involved | 5 |

So the join is available and buys nothing. It would populate `allocation_id` with an id that
matches no allocation, `deployment_id` would stay null exactly as it is today, nothing rendered
would change, and the ingest would pay a second nest read on every page for the privilege. **Do not
add it.** If a future data service starts issuing collection ids that *are* allocations, the figure
to re-measure is the last row of that table.

**Nine rows the subgraph drops.** The same query leaves nine collections with neither a fee event
nor a `PaymentCollected` row, and they are one payer and one receiver:
`0xd4b1775640d9c4ba2ce5bf2b3201b35c4031aa47` collecting to
`0x35917c0eb91d2e21bef40940d028940484230c06`. The subgraph drops them because no escrow account
keyed `(payer, collector, receiver)` was ever funded. They are real collections and we keep them.
**Do not "fix" the nine.**

The two figures come from two populations and the arithmetic closes in both. Within
GraphTallyCollector's own collections: **71,567 total, 71,492 paired with a fee event, 75 not**.
Across every collector: **84 unpaired, of which 75 carry a `PaymentCollected` row** - the tally
ones - **and 9 do not**, those nine being the other collector's and the ones the subgraph drops.
Nothing is unaccounted for, which is the property worth having when somebody asks why two revenue
figures differ.

## Where it stands

**2026-09-07: 20 matching, 0 breaking.** Every route the harness covers returns the shape the
dashboard is written against. Run it twice in a row to be sure - the live probes make one route
mildly non-deterministic.

Shape parity is necessary and not sufficient. Three defects got past it and were caught only by
comparing values:

- **POI consensus** returned the right shape with every deployment at 100%, because
  `closed_at_epoch` arrives as the string `"1374"` and a bare `as_i64()` read it as absent. The
  incumbent found 7 divergent deployments; we found 0. Now both find 7 and agree to three decimals.
- **Indexer status** joined nothing, because the nest names a deployment in hex and the indexer's
  own endpoint names it `Qm…`. Every allocation looked unserved. Both are the same 32 bytes; one
  wears a multihash prefix and base58.
- **Health classification** counted `unhealthy` as failed. It means "serving, with errors
  recorded". Four working deployments were reported broken.

So: run the harness, then compare a figure from each route by eye. The harness catches contract
breaks; only a human comparing numbers catches a contract kept and an answer wrong.

## Where it stood on 2026-09-06

First run: **1 matching, 17 breaking, 2 not compared.**

That is the finding this harness exists to produce, and it arrived before any traffic did. The read
layer was passing the nest's own columns straight through - `total_tokens_staked` in snake_case,
flat - where the dashboard expects `totalTokensStaked` nested under `graphNetwork`, alongside a
`_meta.block.number` and a cross-chain `grtSupply` block. Correct data, wrong contract.

`/api/network-stats` has since been shaped and matches, values included, which is the pattern for
the rest: map the nest's columns to the names the frontend reads, keep wei as a decimal string,
keep counts as integers, and use null rather than zero where Horizon has no equivalent.

The two that could not be compared were fixture faults rather than parity ones: the detail route
404s for our own indexer, which is excluded from the directory by design, and `/api/portfolio`
needs a `type`. Both fixed in the route table.
