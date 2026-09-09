# Decisions

Judgement calls, with the reasoning that produced them. Not a changelog: this is the file for
things somebody will otherwise re-litigate in six months, or quietly undo without knowing what it
was for.

Newest first.

---

## 2026-09-09: the parameter trail is written before the row it is compared against

`refresh` detects a change in an indexer's cut by comparing the incoming value against the one
already in `indexers`, so the two writes are ordered and the order is a choice with a losing side.

Detect, insert the history, then upsert. If the upsert fails after the history has landed, the next
run reads the same previous row, re-detects the same change and writes it a second time - and
`provenance_rows` already collapses that with `DISTINCT ON (param_name, old_value, new_value,
epoch)`, so the reader never sees it. The other order loses the change permanently: once the upsert
has succeeded there is nothing left to compare against, and a cut that moved is simply never
recorded by anyone. A duplicate the reader already dedupes beats a silence nothing can recover.

The trail takes `INSERT` and nothing else. A recorded change is a thing that happened. Repairing bad
history - which 2026-09-07 left behind, and `deploy/repair-parameter-changes.sh` cleans up - is a
deliberate act by a person at a superuser prompt, not a privilege a daemon carries about with it.

`parameter_changes_written_total` and `parameter_changes_failed_total` are both registered at zero
every run. The failure this whole episode consisted of was a writer that stopped while every log
said the service was healthy, and a counter that only exists once it fires makes silence look like
health. Same lesson as chain liveness, below, and as `nest_busy_total` before it.

## 2026-09-08: the TAP signing key comes to the box, under a fixed GRT ceiling

`tap-provision` reads claimed bounties, checks `PaymentsEscrow.getBalance` and **deposits GRT**
where the balance is under 1 GRT, using `TAP_SIGNER_PRIVATE_KEY`. It was held out of the migration
because that is a custody question rather than a port (#11). The answer is yes: the key comes to
Nuremberg and this service signs.

**The ceiling is 1,000 GRT a month, with a 50 GRT daily burst limit.** The monthly figure is the
budget - $20 at $0.0194967, the price `/api/price` served on 2026-09-08. The daily figure is not a
thirtieth of it; it is there to stop a bad `getBalance` read spending the month in an afternoon. A
per-run cap wants the deposit size and the run cadence, which are still in the Node implementation.

**The ceiling is denominated in GRT, and nothing may recompute it from a live price.** A cap of
"$20" enforced against a feed widens exactly when the feed reads low, which is the failure it
exists to prevent. The feed in question was reporting `stale: true` at the moment this was written.
When the price moves enough to matter, a human edits the number here and the reasoning above it
stays visible.

The key loads through systemd `LoadCredential=` rather than the `EnvironmentFile=` the rest of the
config uses. That is a second mechanism for one value, and worth it: an environment variable is
readable from `/proc/<pid>/environ` and is inherited by every child, in a process that also serves
two public HTTP surfaces.

**This unparks `indexing-status`.** The 2026-09-07 entry below holds it on Next because its serving
probe signs TAP receipts against funded escrow, "the same custody question as `tap-provision` (#11)
in different clothes". That question is now answered, so the route is portable and belongs in #23
rather than in this file as an exception.

## 2026-09-07: the rewrite list and the parity table must be one thing

**The failure.** Thirty-six routes moved to this service. Eight of them shipped with the wrong
shape and answered `200` for two hours in production. `/api/subgraph-history` returned
`{allocations, signals}` where the panel reads `{history}`; `/api/indexer-stake-history` returned
raw cutoffs and wei where it plots dates and GRT; `/api/grt-flow` was aliased to `network-stats`
and answered with the protocol row.

**Why nothing caught it.** `src/proxy.ts`'s `MIGRATED` list decided what moved. The parity
harness's route table decided what was checked. They were two lists, nobody had connected them, and
a route in the first and not the second went to production entirely unchecked. Before the cutover I
reported "24 matching, 0 breaking" and treated it as coverage. It was 24 of the 32 I was about to
move, and I drew the wrong conclusion from a true number, repeatedly.

**The decision.** `every_route_the_edge_serves_is_compared_or_excused` reads the router source at
compile time and fails the build unless every `/api/...` path is either in the harness table or in
`UNCHECKED` with a written reason. There are four excused, each explained. An unchecked route is now
a decision somebody wrote down rather than an omission nobody noticed.

**A lesson inside the lesson.** The first version of that extractor matched `.route("` and silently
missed every registration rustfmt had wrapped onto its own line, excusing four routes by accident.
A check that cannot see the whole truth is decoration, so there is now a test for the multi-line
case specifically.

## 2026-09-07: the harness must not grade its own homework

Once the rewrite went live, `www.lodestar-dashboard.com` **is** this service for every migrated
route, so a harness pointed at it compares the service with itself and reports perfect parity for
entirely the wrong reason. Green, staying green, measuring nothing.

Three changes. The default incumbent is a **frozen** Vercel deployment made an hour before
`src/proxy.ts` existed: deployment URLs are permanent and an old deployment cannot start behaving
differently, which is the property a reference wants. The run **exits 2** if the incumbent answers
with any `x-kittiwake-*` header. And it paces itself at 700 ms with a retry on `429`, because a full
run is ~170 requests from one address against a 120-a-minute limit and it was tripping the defence
it helped build.

## 2026-09-07: `indexing-status` stays on Next

> **Superseded 2026-09-08.** The custody question was answered yes; see the entry at the top.

The other seven broken ports were finished the same day. This one was not, deliberately.

It needs the live serving probe, which **signs TAP receipts against funded escrow**, and the
multi-round servability persistence behind the RFC-006 verdict - 547 lines of library with a signing
key in it. That is the same custody question as `tap-provision` (#11) in different clothes, and it
is not a shape fix. It works on Next, so it stays there until the Dock moves and the custody
question is answered on its own terms.

## 2026-09-07: one figure for L1 blocks per year

The dashboard carried two, about 0.7% apart, and had since both were written: 2,610,223 for the
issuance percentage a reader sees, 2,628,000 for the scores those readers rank indexers by. The
number on the page and the number behind the ranking were never the same number.

Unified on **2,610,223** (365.25 days at 12.09 seconds), because 12.09 is the measured average slot
time including missed slots and 12 is the nominal one.

**No ranking moves**, and this was checked rather than assumed: issuance scales every indexer's APR
by the same factor, and `score_delegator_apy` interpolates monotonically across its bands, so a
uniform multiplier preserves order exactly. An absolute threshold anywhere in that function would
have made it a very different change.

I filed the issue quoting 2,610,224, having read the intent of `Math.round((365.25 * 24 * 3600) /
12.09)` rather than evaluating it. A derivation test - the constant asserted against the arithmetic
it comes from, not merely against itself - caught it before my mistake became canonical.

## 2026-09-07: only our own edge may name a client

After the rewrite the chain is browser → Vercel → Caddy → kittiwake, and `X-Forwarded-For` is the
only thing carrying the browser's address. It is also a header anyone can send. Trusting it lets a
stranger choose their rate-limit bucket; refusing it puts **every visitor in the world** into one,
against a limit of 120 a minute.

The edge names the client in `X-Lodestar-Client`, believed only when `X-Lodestar-Edge` matches the
configured `edge_secret`, compared in constant time.

**Why not `X-Forwarded-For`.** Caddy **replaces** the forwarded chain with its own peer unless that
peer is a configured `trusted_proxies`. That is correct of it - preserving an arbitrary caller's
chain is exactly how spoofing works - and it means the address the edge writes never survives the
hop. The alternative was maintaining Vercel's egress ranges in the Caddyfile, and that is a list
which goes stale silently.

`/api/whoami` exists because diagnosing this took an afternoon of firing three hundred requests at
the box and inferring the answer from how many came back 429.

## 2026-09-07: kittiwake gets its own Postgres role

Rather than recovering the shared `DATABASE_URL`, which is a Sensitive Vercel variable nobody can
read back. `deploy/grant-database.sh` mints a password on the box and writes it into the config; it
is printed nowhere and never leaves the machine.

Same shape as the Caddy nest users: one credential per consumer, so rotating kittiwake's cannot
strand Vercel and vice versa. The shared password is precisely what made the previous rotation
impossible to sequence.

The grant **names tables** rather than using `ALL TABLES` - `push_subscriptions`, the Dock's
sessions and the Scuttlebutt tables are user data no route here reads - and it does **not** grant
write on the platform's ingest tables, so #1 is gated by Postgres as well as by the flag and an
accidental flip fails loudly.

## 2026-09-07: the rollback is an environment variable

`LODESTAR_API_ORIGIN`. Unset means no rewrite happens at all, so clearing it in Vercel returns every
route to its Next handler on the next request, with no revert and no build. The moment you need a
rollback is the moment you least want to wait for one.

The Postgres schema is unchanged apart from three additive tables nothing else reads
(`chain_head_records`, `indexer_chains`, `live_answers`), so there is no data to undo either. That
property is worth protecting: the first change that alters an existing table stops the rollback
being free, and should be treated as a one-way door.

## 2026-09-07: a live job's answer outlives the process

The jobs write into this process's memory, which is what removes Redis from the request path. The
cost is that memory does not survive a deploy, and a thirty-minute job leaves its route with nothing
to say for up to thirty minutes. The service this replaces did not have that hole, because its Redis
outlived its functions.

`live_answers` is that durability without putting the hop back in the read path: the store answers
every request, and the table is consulted only between a restart and the job's next run. What comes
back is marked stale with its true age.

## 2026-09-07: cron rows say which system wrote them

Both systems append to `cron_runs` under otherwise identical job names, so "when did
`refresh-chain-health` last succeed" answered with whichever ran most recently and gave no way to
tell which - precisely the question a shadow run is asking. Ours are prefixed `kittiwake:`. Worth
keeping after the platform crons are gone: a row that says where it came from stays true, and one
that does not has to be dated to be understood.

## 2026-09-07: scheduler gating is about the store, not the schedule

`scheduler_enabled` was too coarse. The question is not "is this a cron" but "does this write to a
store the other system also writes to". Jobs touching only this process's memory, or tables of their
own, run always. Only the shared-Postgres writers stay behind the flag.

## 2026-09-07: chain liveness is absolute, not relative

Every staleness signal in the stack measures distance to chain head. When a chain **stops**, the
head freezes, the distance goes to zero, and every one of those signals reports perfect health -
correctly, and to entirely the wrong conclusion. Moonbeam stopped producing blocks around
2026-08-10 and every subgraph on it entered exactly that state.

So `refresh-chain-health` tracks whether the head we observe has actually changed over wall-clock
time, and the history lives in Postgres rather than in memory because the history **is** the signal:
a chain dead for three days must still read as dead after a restart.
