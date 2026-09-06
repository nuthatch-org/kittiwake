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
