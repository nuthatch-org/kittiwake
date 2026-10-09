# Decisions

Judgement calls, with the reasoning that produced them. Not a changelog: this is the file for
things somebody will otherwise re-litigate in six months, or quietly undo without knowing what it
was for.

Newest first.

---

## 2026-09-10: the disassembler needs a parser, not a sandbox

#18 asks for the wasm disassembler to move onto wasmtime with fuel and epoch limits, so a submitted
module that loops for ever is stopped by the runtime rather than by a timeout wrapped round it.

Nothing in the two surviving routes executes anything. `src/lib/disassembly/index.ts` says so in its
own header, `types.ts` repeats it, and the whole directory contains exactly one file that shells
out or instantiates: `build-sandbox.ts`, which belongs to `verify` and goes in
nuthatch-org/lodestar#144. Fuel bounds instructions executed; the count here is nought.

So this is `wasmparser`, and the improvement is a different one. The incumbent decodes the sections
by hand, six hundred lines of LEB128 reader and an opcode table, and bails at the first instruction
it does not model. That is why its reports carry a caveat saying SIMD and atomics make reachability
partial. `wasmparser` knows the instruction set, so the caveat goes and `incomplete` comes to mean
what it says: a body that could not be read at all. A transparency tool that no longer has to
admit it looked away is worth more than a sandbox around code nobody runs.

Two smaller consequences, recorded so they are not read as omissions.

**The decode audit stops being able to fail.** The incumbent classifies `ethereum.decode` type
strings by compiling the real ethabi and alloy parsers to WebAssembly and loading the artefact from
Node, and degrades to `unavailable` when that load fails. Here they are ordinary dependencies.
There is no artefact, no loader and no unavailable state, so the field and the caveat that reported
it are both gone. The vectors from `crates/decode-classify` are ported as they stand and pass
against alloy 1.7.2, which the incumbent pinned at 0.8.

**`sourceHint` is not ported.** It exists to pre-fill the repository URL in the "Verify against
source" box, and #144 deletes that box along with the route behind it. Porting a field whose only
reader is being removed in the same week would be carrying a decision across a migration rather
than making one.

**The diff's handler keys are joined with a triangle rather than a NUL byte.** `handlerKey` in
`diff.ts` reads `` `${dsName}\0${h.handler}\0${h.kind}` ``, and the comment directly above it says
the key is `dataSource ▸ handler ▸ kind`. The NUL reaches the wire: every key in a live
`/api/disassembly/diff` response carries two of them. It is legal JSON and nothing good comes of
it, the page uses the value only as a React key, and the character the comment intended is used
here instead. Found by diffing this port against production, which is the reason for doing that
rather than reading the source and believing it.

Verified against production on three deployments, twelve data sources and seventy-five handlers
between them: the manifest, every handler's reachable host surface, the recovered strings, the
decode audit, the scorecard and the caveats are identical, and so is the diff apart from that
separator.

---

## 2026-09-10: `blog/search-index` stays on Next, and it is not a route this service should want

The last of #67's nineteen, and the only one answered by declining it.

It reads `src/content/blog/*.md` out of the lodestar working directory at build time, strips the
markdown and returns lowercased post bodies keyed by slug. `force-static`, immutable per deploy,
cached for a day. Nothing in it touches the protocol, Postgres, the nest or the chain.

Porting it would mean one of two things. Either the blog posts are copied into this repository, so
a writer publishing a post has to deploy a Rust service; or this service reads a checkout of a
repository it does not own, which is a build-time dependency between two deployments dressed up as
an API. Both are worse than the route.

It belongs with `/api/migration` in the same category: the frontend describing its own content,
which is what a frontend serves. #29 asks for lodestar to hold no API routes, and this is not one -
it is a static asset with a build step, and the fact that Next spells that as `route.ts` is Next's
business rather than an argument about where the data lives.

So the nineteen are settled: sixteen answer here, `feed` went with the Intel Feed
(nuthatch-org/lodestar#146, merged), `analytics/clickthrough` is being deleted for never having
received a click (nuthatch-org/lodestar#150, open), and this one stays where it is.

---

## 2026-09-09: the chain probe waits eight seconds, and the coverage difference is now moot

The incumbent's `fetchIndexerDeployments` allowed three seconds; this one allows eight. That made us
see 44 chains against their 32, and 36 sampled indexers on `arbitrum-one` against their 29 - a
visible difference between two dashboards, which is what #7 was opened about.

**Eight stays, and here is the number that decides it.** Measured against production on 2026-09-09:
of the 44 chains this probe sees, **thirteen are served by two indexers or fewer**. A chain with two
indexers loses its whole sample the moment one of them is slow, so a shorter budget does not sharpen
the reading - it deletes the chain. A chain served only by nodes that take four seconds to answer is
still a chain, and reporting nothing about it is a worse answer than reporting it late.

The cost is bounded and sits on no request path: the probe runs from a background job, ten at a
time, and nothing waits on it. The timeout is now its own named constant rather than inherited from
the client's default, which is set for the price and TVL APIs on exactly the opposite argument - a
slow third party must never be why an indexer table is late. The two numbers agreeing today is a
coincidence and should not become a coupling.

**The comparison the issue worried about no longer exists.** `/api/chain-lag` on the frozen
incumbent answers `{"data": null}`: its chain-health cron is descheduled and its cache is empty.
There is one dashboard now, so nobody is putting the two figures side by side and wondering.

**The sample ordering is already in line.** The scored order the incumbent used is what
`History::Db` reads, and production has a database. The by-stake ordering is the in-memory
fallback's only, it exists so that a missing database means a worse sample rather than no panel at
all, and the code says so where it happens.

## 2026-09-09: this repository owns the schema

`db/schema.sql` is authoritative. It moved here from `nuthatch-org/lodestar`'s
`scripts/schema.sql`, and a new column is added here and nowhere else.

The tables are read on nearly every route and, since the crons were ported, written by this service
and no other. A repository that is to hold no server code (#29) cannot go on owning the schema of a
database it does not write to.

**It was never only tidiness.** #24 was a live 200-with-an-empty-array, and diagnosing it meant
reading the authoritative column types out of another repository - which turned up four wrongly
typed columns where the issue had recorded two. None was visible from inside this one, because the
truth about the columns was a file somewhere else. #2 made the queries check themselves against the
schema at compile time; that check now runs against a file in the repository whose code it checks,
which is the half that was missing.

The move proved its own case on the first run. The copied file predated #4, and
`cargo sqlx prepare` refused it: `column "last_timestamp" does not exist`.

**`deploy/sqlx-prepare.sh` no longer needs a sibling checkout.** Until now it read the DDL from
`../lodestar`, so whether the compile-time check could run at all depended on somebody else's
working tree being present and current.

**One table is duplicated on purpose and says so.** `sync_bounties` is the Dock's, and lodestar's
`migrate-studio*.sql` is still its definition, but `reconcile-bounties` writes it from here (#30)
and a statement checked at compile time needs the table to exist in the schema the check runs
against. Two definitions is precisely what this entry is against, so that one is temporary, carries
a comment naming the issue that removes it (#16), and is the copy that decides whether the Rust
compiles. The rest of the Dock's tables come across when the Dock does.


## 2026-09-09: the money is not checked at compile time, and that is the right way round

`.sqlx/` is committed and CI runs with `SQLX_OFFLINE`, so fourteen statements are now checked
against a real schema when they compile rather than when the job next fires. Fifteen are not, and
the line between them is deliberate.

**Anything carrying wei stays a runtime `sqlx::query`.** Those statements bind decimal strings and
let Postgres cast to `numeric`, which keeps every digit; a wei figure does not fit an `f64`, and two
collections differing in the last few places are exactly the pair somebody is trying to tell apart.
`query!` would want either the `bigdecimal` feature or a `float8` parameter, and the second rounds
the protocol's own totals in order to buy a green tick. That is a bad trade at any price. It rules
out the epoch, delegation, allocation, dispute, snapshot and RAV inserts - which is most of the
ingest path, and was not what #2 expected.

**So does anything binding `Vec<Option<String>>`.** An indexer with no URL is an ordinary row, and
`text[]` cannot express a per-element null to the macro, which insists on `&[String]`. Padding the
gaps with empty strings to satisfy it would write `""` where the schema says `NULL` and the
directory would lose the difference between "no URL" and "an empty URL".

Every one of those carries a comment saying which case it is. Checking what can be checked honestly
beats checking everything dishonestly, and an unchecked statement that says why is a decision rather
than an omission.

**It found one thing immediately.** `DelegationEventRow` declared `delegator` as
`Option<String>`; the DDL has `delegator TEXT NOT NULL`. It decoded perfectly well, because an
`Option` accepts a non-null, which is precisely why nothing found it until something read the
schema. That is the same class as the four wrongly-typed columns #31 records, and the argument for
this work in one line.

`deploy/sqlx-prepare.sh` builds a throwaway Postgres from lodestar's DDL rather than pointing at
production, so compiling needs no credentials and the schema being checked against is a thing in a
repository. It reads that DDL from a sibling checkout, which makes the split ownership in #31
visible instead of working around it.

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
