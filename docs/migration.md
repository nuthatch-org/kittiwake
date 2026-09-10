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
| 7 | `/api/price`, `tvl`, `reo`, `indexer-node-health` | chain and third-party reads; the SSRF guard lands here | **done** |
| 8 | `/api/studio/*` | sessions, deploy keys, IPFS, on-chain lifecycle | to do |
| 9 | `/api/scuttlebutt/*` | streaming and moderation | to do |
| 10 | the long tail | some of it droppable | **settled**: sixteen answer here, two deleted, one stays on Next. #67 |
| 11 | frontend hooks | only now do the hooks change | to do |

## The route ledger

Recounted on 2026-09-10 by diffing `src/app/api/**/route.ts` in `nightswatchhq/lodestar` against
this service's router, rather than by hand. The counts below are that diff.

Lodestar has **90 API route files**, twelve of which are cron endpoints rather than API surface.

**Answered by this router: 73.** Their parity verdicts and timings are in [status.md](status.md),
which the harness generates.

**In Next and not here: 20.** Twelve are the crons, which are jobs in this process and are
ledgered below. Four are deliberate: `health` judges the deployment including whether kittiwake is
writing, so moving it here would make the thing being checked the checker; `migration` is the
frontend describing itself; `horizon/debug` is cron-authed and wants confirming before it goes;
`provider-liveness` goes with the Dispatch removal, nightswatchhq/lodestar#99. `disassembly/verify`
is being deleted, nightswatchhq/lodestar#144.

That leaves **two genuinely still to port**: `disassembly` and `disassembly/diff`, which are #18.
`blog/search-index` was the last of #67's nineteen and stays on Next, because it reads the blog
posts out of the lodestar working directory at build time and porting it would put a repository
this service does not own into its build. The reasoning is in [decisions.md](decisions.md).

### Ported here and reachable by nobody: 34

The edge forwards a request only if `isMigrated` says so, and that list is `MIGRATED` in lodestar's
`src/lib/migration.ts`. Thirty-three routes answer on this router and are not in it, and
`/api/indexer/present-poi` is in `NEVER_FORWARD` from when this service had no handler for it.
The Dock's nine, scuttlebutt's six, the SQL upper tier's two and sixteen of the long tail are all
written, tested and unreached; every request for them lands on Next.

Porting a route and flipping it are two pull requests in two repositories, and only the second one
delivers anything. That is worth saying plainly here because the first is the enjoyable half.

### What the cutover cost, recorded because it will be asked

Eight of the thirty-two routes moved on 7 September shipped with the wrong shape and answered `200`
in production for two hours. Seven were fixed the same day and are back; one is `indexing-status`.
None of the eight was in the parity harness, which is the defect the routes were a symptom of - see
[decisions.md](decisions.md).

## The cron ledger

Seventeen crons, of which eleven are now jobs here.

**Ported and running (11).** `refresh`, `ingest-epochs`, `ingest-delegations`,
`ingest-allocations`, `ingest-disputes`, `snapshot-network`, `ingest-horizon-activity`,
`refresh-chain-health`, `ingest-rav`, `warm-ipfs`, `reconcile-bounties`.

Of those, the shared-Postgres writers are behind `scheduler_enabled` and **gated by the grant as
well as the flag** - the role has no write on the platform's ingest tables, so an accidental flip
fails loudly rather than quietly making this a second writer. That decision is #1 and wants a shadow
run first. The rest write only their own tables or this process's memory and run always.

**Struck, because their delivery is gone (5).** `check-dips`, `check-dips-chain`,
`check-nest-health`, `check-provider-liveness` and `dispatch-notifications` all ended in
`sendToAddress` from `lib/apns.ts`. Push was discontinued on 2026-09-06, so they detect a thing and
post it nowhere. The nest watch is Foghorn's now; the DIPS watch is worth rehoming there
(nightswatchhq/foghorn#2); the provider watch goes with Dispatch.

**Product layer, not data plane (1).** `tap-provision` holds `TAP_SIGNER_PRIVATE_KEY` and **spends
GRT**: porting it means putting a signing key on this box and having this service send
transactions, which is a decision about custody rather than a port. Settled on 2026-09-08 - the key
comes to the box under a fixed GRT ceiling - and it moves with the Dock. #11

`reconcile-bounties` was the other one, and was in this group only by association. It reads the
BountyBoard contract and corrects `sync_bounties`; there is no key and nothing is spent. Ported
2026-09-09, every ten minutes on the nine, behind `scheduler_enabled` and a `SELECT, UPDATE` grant
on that one table. #30

## Discontinued, so not ported at all

**Push notifications** (`/api/push/*`, APNs, Push Protocol) and **Lodie** (`/api/lodie/*`, the
Ollama-backed assistant) were discontinued on 2026-09-06. Neither is in this service and neither
should be added.

The routes and the interfaces that offered them went in `nightswatchhq/lodestar`#136 on 2026-09-09.
The tables went on 2026-09-10: `device_tokens`, `push_subscriptions` and `notification_log`, and
the `push_notified` columns on `disputes` and `parameter_changes`. Lodie had none. This file used to
say they could stay "until someone decides to drop them", which is how a decision becomes sediment.
`deploy/drop-push-tables.sh` is the one-way script that does it, and it is the only statement in
this repository that is not safe to re-run.

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
