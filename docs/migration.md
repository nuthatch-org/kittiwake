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
| 6 | the seventeen crons | run both writing the same tables in shadow first, compare row deltas, then disable the platform crons | every data-plane job done; see the ledger below |
| 7 | `/api/price`, `tvl`, `reo`, `indexer-node-health` | chain and third-party reads; the SSRF guard lands here | **done**; `ens`, `manifest`, `service-census`, `operator-preflight`, `delegate/recommend` remain |
| 8 | `/api/studio/*` | sessions, deploy keys, IPFS, on-chain lifecycle | to do |
| 9 | `/api/scuttlebutt/*` | streaming and moderation | to do |
| 10 | the long tail | some of it droppable | to do |
| 11 | frontend hooks | only now do the hooks change | to do |

## The route ledger

Counted rather than estimated, on 2026-09-07. Lodestar has **90 API routes** outside the crons.
This service serves **36**. Of the 54 remaining, **nine are deletions rather than ports** - the
three `push/*`, two `lodie/*`, two `seahorn/*` (dead upstream since July), `provider-liveness`
(goes with Dispatch), and `/api/health`, which exists here as `/health` at the root.

That leaves **45 routes of real work**, in four blocks and one long tail:

| Block | Routes | Character |
|---|---|---|
| The Dock (`studio/*`) | 9 | Sessions, deploy keys, IPFS upload, the on-chain bounty lifecycle. Sessions are done (`crates/dock`); the routes are not. |
| Scuttlebutt | 6 | A **rewrite**, not a port: axum SSE and a broadcast channel, rather than dragging the Node concurrency model across. |
| The disassembler | 3 | Onto wasmtime with fuel and epoch limits. Better sandbox posture than what it replaces. |
| The SQL upper tier | 2 | `sql/named` and `sql/receipt`. The catalogue and ad-hoc query are already here. |
| The long tail | 25 | Mixed. Roughly a third are nest reads that port in an afternoon; the rest touch Postgres, the chain or a third party. |

The long tail is worth triaging rather than working through in order, because some of it should be
dropped rather than carried. `blog/search-index`, `analytics/*`, `vote` and `x402/query` are each
worth a moment's thought about whether anyone reads them.

## The cron ledger

Seventeen crons, and only nine of them were ever jobs this service should run. The count is worth
writing out, because "eleven remaining" was the wrong number for weeks and it made the remaining
work look larger and vaguer than it was.

**Ported and running (9).** `refresh`, `ingest-epochs`, `ingest-delegations`, `ingest-allocations`,
`ingest-disputes`, `snapshot-network`, `ingest-horizon-activity`, `refresh-chain-health`,
`ingest-rav`. The four Postgres walkers plus `refresh`, `snapshot-network` and `ingest-rav` are
behind `scheduler_enabled`, because turning them on makes this a second writer to tables the
platform's crons still own. The other two write only tables of their own and run always.

**Ported but needs a database (1).** `warm-ipfs`. It fills `ipfs_metadata`, which is also what
`/api/subgraph-search` and `/api/subgraph-names` read. Not gated behind `scheduler_enabled` because
both systems write the same document under the same CID and the upsert is idempotent - two writers
racing to the same answer rather than to different ones.

**Struck, because their delivery is gone (4).** `check-dips`, `check-dips-chain`,
`check-nest-health` and `check-provider-liveness` all end in `sendToAddress` from `lib/apns.ts`.
Push was discontinued on 2026-09-06, so all four detect a thing and then post it to nowhere. The
detections are not all worthless - a DIPS allocation moving is genuinely interesting - but the nest
one is now Foghorn's, which alerts to a webhook that works, and the provider one watches Dispatch,
which is being removed. Anything worth keeping belongs in Foghorn, not here.

**Struck with push (1).** `dispatch-notifications`.

**Product layer, not data plane (2).** `reconcile-bounties` and `tap-provision` belong with the
Dock. `tap-provision` additionally holds `TAP_SIGNER_PRIVATE_KEY` and *spends GRT*: porting it means
putting a signing key on this box and having this service send transactions. That is a decision
about custody, not a port, and it should not move before the Dock does.

## Discontinued, so not ported at all

**Push notifications** (`/api/push/*`, APNs, Push Protocol) and **Lodie** (`/api/lodie/*`, the
Ollama-backed assistant) were discontinued on 2026-09-06. Neither is in this service and neither
should be added. Their tables can stay in Postgres until someone decides to drop them; nothing here
reads or writes them.

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
