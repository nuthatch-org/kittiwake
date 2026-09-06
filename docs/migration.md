# Migration order

Read layer first: pure functions of nest and cache, easiest parity, biggest latency win. Then the
ingest jobs, which need write parity. Then the product layer, which has sessions, streams and third
parties and is where the real work is.

Each route moves by pointing the frontend's rewrite at this service instead of the Next.js handler.
Rolling back is pointing it back: the Postgres schema does not change, so there is no data to undo.

Parity for every route is a golden-file diff. Capture the current JSON for fixed inputs, assert this
service returns the same shape, ignoring the added `provenance` key.

| # | Routes | Why here | State |
|---|---|---|---|
| 1 | `/health`, `/api/network-stats`, `/api/epochs` | low risk, exercises gate, cache and provenance end to end | **done** |
| 2 | `/api/indexers`, `/api/indexer/[address]`, `/api/indexer-status` | the 23.5, 17.9 and 7.7 second cold routes: the whole point | `/api/indexers` done |
| 3 | the rest of the `/alloc` reads | same pattern, bulk-portable | to do |
| 4 | `/gns`, `/dips`, `/dips-sepolia`, `/legacy-flows` reads | other nests, exercises multi-nest gate config | to do |
| 5 | `/api/sql/*` | needs screening and receipts; independent of ingest | catalog and query done; named tier and receipts to do |
| 6 | the seventeen ingest jobs | run both writing the same tables in shadow first, compare row deltas, then disable the platform crons | scheduler done, job bodies to do |
| 7 | `/api/ens`, `price`, `tvl`, `reo`, `manifest`, `service-census`, `operator-preflight`, `delegate/recommend`, `indexer-node-health` | chain and third-party reads; the SSRF guard lands here | to do |
| 8 | `/api/push/*` | signature verification, APNs, operator keys | to do |
| 9 | `/api/studio/*` | sessions, deploy keys, IPFS, on-chain lifecycle | to do |
| 10 | `/api/scuttlebutt/*` | streaming and moderation | to do |
| 11 | the long tail | some of it droppable | to do |
| 12 | frontend hooks | only now do the hooks change | to do |

## Two that should be rewritten rather than ported

**Scuttlebutt.** The Node stream maps onto axum's SSE and a broadcast channel cleanly, but a
literal port drags the old concurrency model with it. Rewrite around the channel.

**The wasm disassembler.** Onto wasmtime with fuel and epoch limits. The sandbox posture is
genuinely better than what it replaces, so this is an improvement rather than a like-for-like move.

## One that should not be ported at all

`/api/seahorn/*` points at a host that has not answered since July 2026. Return a clear error or
delete the route; do not carry a dead upstream across a migration.

## The Dock's sessions

Reimplement the existing HMAC scheme exactly, or every logged-in user is logged out at cutover.
Verify one captured production cookie against the new implementation before the switch, not after.
