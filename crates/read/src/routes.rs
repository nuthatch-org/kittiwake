//! Every nest-backed read the dashboard makes.
//!
//! Each route is a description of which queries to run against which nest and how fresh the answer
//! has to be. The thinking - readiness, coalescing, staleness labelling - lives once in
//! [`crate::ReadCtx::cached`], because it is a property of reading a nest rather than of any
//! particular panel.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesOrdered, StreamExt};
use kittiwake_cache::CachePolicy;
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_types::{ApiError, NestId, Provenance};

use crate::dips_agreements as dips_agreements_mod;
use crate::{Fetcher, Read, ReadCtx, shape, sql};

/// One query inside a composed answer.
pub struct Part {
    pub name: &'static str,
    pub nest: NestId,
    pub sql: String,
    /// Fold to a single row rather than an array. For the one-row views.
    pub one: bool,
    /// Left out of the answer when it cannot be read, rather than failing it. See [`many`].
    pub optional: bool,
}

impl Part {
    pub fn rows(name: &'static str, nest: NestId, sql: String) -> Self {
        Self {
            name,
            nest,
            sql,
            one: false,
            optional: false,
        }
    }
    pub fn one(name: &'static str, nest: NestId, sql: String) -> Self {
        Self {
            name,
            nest,
            sql,
            one: true,
            optional: false,
        }
    }
    pub fn optional(self) -> Self {
        Self {
            optional: true,
            ..self
        }
    }
}

/// The older of two provenances, so a composed answer is dated by its stalest part rather than its
/// freshest. Claiming the newer would be flattering and wrong.
pub(crate) fn older_of(a: &Provenance, b: &Provenance) -> Provenance {
    let older = |x: Option<i64>, y: Option<i64>| match (x, y) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (v, None) | (None, v) => v,
    };
    Provenance {
        as_of: older(a.as_of, b.as_of),
        sealed_through: older(a.sealed_through, b.sealed_through),
        registry_hash: a.registry_hash.clone().or_else(|| b.registry_hash.clone()),
        nid: a.nid.clone().or_else(|| b.nid.clone()),
        source: a.source.clone().or_else(|| b.source.clone()),
        truncated: a.truncated || b.truncated,
        degraded: a.degraded || b.degraded,
        partial: a.partial || b.partial,
    }
}

/// Run several queries and return one object keyed by part name.
///
/// Each part is coalesced on its own statement, so two routes that happen to want the same fold
/// cost the nest one query between them rather than one each. They run concurrently; the gate is
/// what bounds how many actually reach the nest at once.
/// [`many`], with the combined object folded into its final shape.
///
/// The parts are what the nest can answer; the fold is what the panel reads. Keeping them separate
/// means the SQL stays a description of the data rather than of the chart.
pub fn many_folded(
    parts: Vec<Part>,
    timeout: Duration,
    fold: fn(&serde_json::Value) -> serde_json::Value,
) -> Fetcher {
    let inner = many(parts, timeout);
    Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            Ok((fold(&v), p))
        })
    })
}

/// Run every part of a fold, never asking for more permits at once than the lane admits.
///
/// ## The deadlock this exists to prevent
///
/// A permit is taken per query and released when that query finishes. `join_all` started all of
/// them at once, so a fold with more parts than its nest has permits **starved itself**: it took
/// every permit, its remaining parts queued for one, and the permits it held were not released
/// until every part had finished. They could not. The pool only freed when the waiters hit the
/// gate timeout and the whole request failed.
///
/// `/api/indexer` and `/api/indexer-status` are six `alloc` parts each, against four permits. Both
/// deadlocked on every cold call - and because a wedged request held the pool for the full gate
/// timeout, raising that timeout to give slow folds room made it *worse*, not better: three
/// seconds of starvation became twenty.
///
/// `lane_width` is the width the gate admits, and one fewer than that leaves a permit for the
/// coalescer and the warmer rather than letting one fold own the nest. It is a width and not a
/// reservation, so this does not stop contention with other requests - it stops a request
/// contending with itself, which is the part that could never resolve.
///
/// A part marked [`Part::optional`] that cannot be read is left out, named with its error's code
/// under `degraded`, and the answer marked degraded and partial. Any other part fails the answer.
pub fn many(parts: Vec<Part>, timeout: Duration) -> Fetcher {
    let parts = Arc::new(parts);
    Arc::new(move |gate: NestGate, lane: Lane| {
        let parts = parts.clone();
        Box::pin(async move {
            // The widest lane any part of this fold uses. Folds are single-nest in practice; taking
            // the minimum is the safe reading when one ever is not.
            let width = parts
                .iter()
                .map(|p| gate.lane_width(p.nest, lane))
                .min()
                .unwrap_or(1)
                .saturating_sub(1)
                .max(1);

            let futures = parts.iter().map(|p| {
                let gate = gate.clone();
                let mut spec = QuerySpec::read(p.nest, p.sql.clone()).with_timeout(timeout);
                spec.lane = lane;
                let key = format!(
                    "{}:{}",
                    p.nest.as_str(),
                    blake3::hash(p.sql.as_bytes()).to_hex()
                );
                async move { gate.query_coalesced(&key, spec).await }
            });

            // Admission is controlled here rather than by `buffer_unordered`, which would lose the
            // ordering the zip below depends on. `FuturesOrdered` polls everything it holds, so the
            // bound has to be on what goes *in*: at most `width` in flight, one more admitted each
            // time one completes.
            let mut pending = futures;
            let mut queue: FuturesOrdered<_> = FuturesOrdered::new();
            let mut answers = Vec::with_capacity(parts.len());
            for _ in 0..width {
                match pending.next() {
                    Some(f) => queue.push_back(f),
                    None => break,
                }
            }
            while let Some(a) = queue.next().await {
                answers.push(a);
                if let Some(f) = pending.next() {
                    queue.push_back(f);
                }
            }
            let mut out = serde_json::Map::new();
            let mut provenance: Option<Provenance> = None;
            let mut degraded = Vec::new();

            for (part, answer) in parts.iter().zip(answers) {
                let answer = match answer.map_err(ApiError::from) {
                    Ok(answer) => answer,
                    Err(e) if part.optional => {
                        tracing::warn!(part = part.name, error = %e, "part left out of the answer");
                        degraded.push(serde_json::json!({"part": part.name, "reason": e.code()}));
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let value = if part.one {
                    answer
                        .rows
                        .first()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::Value::Array(answer.rows.clone())
                };
                out.insert(part.name.to_string(), value);
                provenance = Some(match provenance {
                    None => answer.provenance.clone(),
                    Some(p) => older_of(&p, &answer.provenance),
                });
            }

            let mut provenance = provenance.unwrap_or_default();
            if !degraded.is_empty() {
                provenance.degraded = true;
                provenance.partial = true;
                out.insert("degraded".to_string(), serde_json::Value::Array(degraded));
            }
            Ok((serde_json::Value::Object(out), provenance))
        })
    })
}

/// One query whose rows are shaped and returned, optionally nested under a key.
///
/// The nesting is not decoration: the dashboard's hooks read `data.indexers`, `data.epoches` and
/// so on, and a bare array where an object is expected is as broken as a missing field.
pub fn rows_shaped(
    nest: NestId,
    sql: String,
    timeout: Duration,
    map: fn(&serde_json::Value) -> serde_json::Value,
    under: Option<&'static str>,
) -> Fetcher {
    let inner = rows_only(nest, sql, timeout);
    Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let mapped: Vec<serde_json::Value> = v
                .as_array()
                .map(|rows| rows.iter().map(map).collect())
                .unwrap_or_default();
            let out = match under {
                Some(key) => serde_json::json!({ key: mapped }),
                None => serde_json::Value::Array(mapped),
            };
            Ok((out, p))
        })
    })
}

/// One query whose rows are the whole answer.
pub fn rows_only(nest: NestId, sql: String, timeout: Duration) -> Fetcher {
    let part = Part::rows("rows", nest, sql);
    let inner = many(vec![part], timeout);
    Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            Ok((v.get("rows").cloned().unwrap_or(serde_json::Value::Null), p))
        })
    })
}

/// Freshness by how fast the underlying thing actually moves. These are the cadences of the nests
/// and crons that feed each view, not numbers chosen to feel about right.
mod cadence {
    use std::time::Duration;
    /// Follows the chain closely: directory figures, network totals, live allocations.
    pub const LIVE: Duration = Duration::from_secs(60);
    /// Moves with a cron rather than with the chain.
    pub const SLOW: Duration = Duration::from_secs(300);
    /// Effectively historical within a session: closed allocations, epoch history, archives.
    pub const ARCHIVAL: Duration = Duration::from_secs(600);
}

/// How long the slow answers behind an indexer's page may be served stale. `kittiwake-warmer`
/// refreshes them before this runs out, for every indexer in the directory.
pub const INDEXER_PAGE_STALE_FOR: Duration = Duration::from_secs(60 * 60);

fn indexer_page(poll: Duration) -> CachePolicy {
    CachePolicy::from_poll_interval(poll).kept_warm(INDEXER_PAGE_STALE_FOR)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const T_FOLD: Duration = Duration::from_secs(45);
const T_QUICK: Duration = Duration::from_secs(20);

macro_rules! addr_or_400 {
    ($e:expr, $what:literal) => {
        $e.ok_or_else(|| ApiError::BadRequest(concat!("not a valid ", $what).into()))?
    };
}

// ---------------------------------------------------------------------------------------------
// The directory and the network
// ---------------------------------------------------------------------------------------------

pub struct IndexersParams {
    pub first: i64,
    pub skip: i64,
    pub order_by: String,
    pub descending: bool,
}

/// `/api/indexers`. Cold on the nest this is a whole-history fold measured at 23.5 seconds, which
/// is exactly why it is the first thing the warmer keeps hot.
pub async fn indexers(ctx: &ReadCtx, p: IndexersParams) -> Result<Read, ApiError> {
    let sql = sql::indexers_sql(p.first, p.skip, &p.order_by, p.descending);
    ctx.cached(
        "indexers",
        NestId::Alloc,
        serde_json::json!({"first": p.first, "skip": p.skip, "orderBy": p.order_by, "desc": p.descending}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_shaped(NestId::Alloc, sql, T_FOLD, crate::shape::indexer, Some("indexers")),
    )
    .await
}

pub async fn curators(ctx: &ReadCtx, first: i64, skip: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "curators",
        NestId::Alloc,
        serde_json::json!({"first": first, "skip": skip}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_shaped(
            NestId::Alloc,
            sql::curators_sql(first, skip),
            T_FOLD,
            crate::shape::curator,
            None,
        ),
    )
    .await
}

pub async fn epochs(ctx: &ReadCtx, first: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "epochs",
        NestId::Alloc,
        serde_json::json!({"first": first}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        rows_shaped(
            NestId::Alloc,
            sql::epochs_sql(first),
            T_QUICK,
            crate::shape::epoch,
            Some("epoches"),
        ),
    )
    .await
}

/// `/api/network-stats`, and `/api/grt-flow` off the same fold.
///
/// The shape here is not ours to choose: it is what the dashboard's hooks are written against, so
/// the nest's snake_case columns are mapped to the camelCase the frontend reads, wei stays a
/// decimal string, and counts stay integers. `maxAllocationEpochs` and `thawingPeriod` are null
/// because Horizon has no equivalent - null rather than zero, which would read as "no thawing
/// period" instead of "the concept does not apply".
pub async fn network_stats(ctx: &ReadCtx) -> Result<Read, ApiError> {
    let chain = ctx.chain.clone();
    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        let chain = chain.clone();
        Box::pin(async move {
            let mut net = QuerySpec::read(NestId::Alloc, sql::network_sql());
            let mut par = QuerySpec::read(NestId::Alloc, sql::network_params_sql());
            net.lane = lane;
            par.lane = lane;

            let (net, par) = tokio::join!(
                gate.query_coalesced("alloc:network", net),
                gate.query_coalesced("alloc:network-params", par),
            );
            let net = net.map_err(ApiError::from)?;
            let par = par.map_err(ApiError::from)?;
            let n = net.rows.first().cloned().unwrap_or(serde_json::Value::Null);
            let p = par.rows.first().cloned().unwrap_or(serde_json::Value::Null);

            // Three contract reads across two chains. Absent rather than partial when any fails:
            // a global supply computed from two of three figures is a plausible wrong number.
            let supply = match &chain {
                Some(c) => match c.grt_supply_breakdown().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        // `.ok()` here cost an afternoon: the supply block went null every minute
                        // and nothing said why. A failure worth reporting to the reader as an
                        // absence is worth reporting to the operator as a line.
                        tracing::warn!(error = %e, "grt supply breakdown unavailable");
                        None
                    }
                },
                None => None,
            };

            let graph_network = serde_json::json!({
                "totalTokensStaked": str_of(&n, "total_tokens_staked"),
                "totalDelegatedTokens": str_of(&n, "total_delegated_tokens"),
                "totalTokensSignalled": str_of(&n, "total_tokens_signalled"),
                "totalTokensAllocated": str_of(&n, "total_tokens_allocated"),
                "totalIndexingRewards": str_of(&n, "total_indexing_rewards"),
                "totalQueryFees": str_of(&n, "total_query_fees"),
                "currentEpoch": int_of(&n, "current_epoch"),
                "epochLength": int_of(&p, "epoch_length"),
                "lastLengthUpdateEpoch": int_of(&p, "last_length_update_epoch"),
                "lastLengthUpdateBlock": int_of(&p, "last_length_update_block"),
                "indexerCount": int_of(&n, "indexer_count"),
                "stakedIndexersCount": int_of(&n, "staked_indexers_count"),
                "delegatorCount": int_of(&n, "delegator_count"),
                "activeDelegatorCount": int_of(&n, "active_delegator_count"),
                "curatorCount": int_of(&n, "curator_count"),
                "activeCuratorCount": int_of(&n, "active_curator_count"),
                "subgraphCount": int_of(&n, "subgraph_count"),
                "activeSubgraphCount": int_of(&n, "active_subgraph_count"),
                "delegationRatio": int_of(&p, "delegation_ratio"),
                "protocolFeePercentage": int_of(&p, "protocol_payment_cut"),
                "delegationTaxPercentage": int_of(&p, "curation_tax_percentage"),
                // No Horizon equivalent. Null, not zero.
                "maxAllocationEpochs": serde_json::Value::Null,
                "thawingPeriod": serde_json::Value::Null,
                "totalSupply": str_of(&n, "total_supply"),
                "networkGRTIssuancePerBlock": str_of(&n, "issuance_per_block"),
            });

            let value = serde_json::json!({
                "graphNetwork": graph_network,
                // The block the nest answered as of, which is the only honest date for this row.
                "_meta": { "block": { "number": net.provenance.as_of } },
                "grtSupply": supply,
            });
            Ok((value, older_of(&net.provenance, &par.provenance)))
        })
    });

    ctx.cached(
        "network-stats",
        NestId::Alloc,
        serde_json::json!({}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        fetch,
    )
    .await
}

/// `/api/grt-flow`: where the supply came from and where it went.
///
/// The same two nest reads and the same contract calls as [`network_stats`], shaped into the flow
/// figures rather than the raw protocol row. It is a different question over the same data, which
/// is why it shares the reads and not the shape - and why aliasing this route to `network_stats`,
/// as it was until now, produced a 200 that the panel could not read.
///
/// Every figure is GRT rather than wei, because these are magnitudes for a chart and not balances
/// for arithmetic. The two that must not be approximated - the supply and the totals it is
/// computed from - are summed as integers first and divided once at the end.
pub async fn grt_flow(ctx: &ReadCtx) -> Result<Read, ApiError> {
    let chain = ctx.chain.clone();
    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        let chain = chain.clone();
        Box::pin(async move {
            let mut net = QuerySpec::read(NestId::Alloc, sql::network_sql());
            let mut par = QuerySpec::read(NestId::Alloc, sql::network_params_sql());
            net.lane = lane;
            par.lane = lane;
            let (net, par) = tokio::join!(
                gate.query_coalesced("alloc:network", net),
                gate.query_coalesced("alloc:network-params", par),
            );
            let net = net.map_err(ApiError::from)?;
            let par = par.map_err(ApiError::from)?;
            let n = net.rows.first().cloned().unwrap_or(serde_json::Value::Null);
            let p = par.rows.first().cloned().unwrap_or(serde_json::Value::Null);

            let supply = match &chain {
                Some(c) => match c.grt_supply_breakdown().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(error = %e, "grt supply breakdown unavailable for the flow");
                        None
                    }
                },
                None => None,
            };
            let value = shape::grt_flow(&n, &p, supply.as_ref(), net.provenance.as_of.unwrap_or(0));
            Ok((value, older_of(&net.provenance, &par.provenance)))
        })
    });

    ctx.cached(
        "grt-flow",
        NestId::Alloc,
        serde_json::json!({}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

/// A nest column as the string the frontend expects. Wei never becomes a number on this path.
fn str_of(v: &serde_json::Value, key: &str) -> serde_json::Value {
    match v.get(key) {
        Some(serde_json::Value::String(s)) => serde_json::Value::String(s.clone()),
        Some(serde_json::Value::Number(n)) => serde_json::Value::String(n.to_string()),
        _ => serde_json::Value::Null,
    }
}

/// A nest column as an integer, accepting the string form the nest sometimes returns.
fn int_of(v: &serde_json::Value, key: &str) -> serde_json::Value {
    match v.get(key) {
        Some(serde_json::Value::Number(n)) => serde_json::Value::Number(n.clone()),
        Some(serde_json::Value::String(s)) => s
            .parse::<i64>()
            .map(|i| serde_json::Value::Number(i.into()))
            .unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::Null,
    }
}

/// `/api/token-metrics`: issuance against burn, one point per epoch, oldest first.
///
/// The nest returns newest first because that is what every other epoch view wants; a chart reads
/// left to right, so the series is reversed here rather than asking the nest twice.
pub async fn token_metrics(ctx: &ReadCtx, first: i64) -> Result<Read, ApiError> {
    let inner = rows_only(NestId::Alloc, sql::epochs_sql(first), T_QUICK);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let mut points: Vec<serde_json::Value> = v
                .as_array()
                .map(|rows| rows.iter().map(crate::shape::token_metric).collect())
                .unwrap_or_default();
            points.reverse();
            Ok((serde_json::Value::Array(points), p))
        })
    });

    ctx.cached(
        "token-metrics",
        NestId::Alloc,
        serde_json::json!({ "first": first }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// One indexer
// ---------------------------------------------------------------------------------------------

/// `/api/indexer/[address]`: the whole profile. Six folds, measured at 17.9 seconds cold, which is
/// why the warmer keeps the hot ones filled.
///
/// The indexer's row and the delegation ratio its stake figures are derived from are required. The
/// operators, delegators and both allocation lists are sections, left out and named under
/// `degraded` when the nest refuses them: on 2026-09-14 the delegators ran the allocation nest out
/// of memory and took the whole page with them for half an hour.
pub async fn indexer(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "indexer",
            NestId::Alloc,
            addr_or_400!(sql::indexer_detail_sql(addr), "address"),
        ),
        Part::rows(
            "operators",
            NestId::Alloc,
            addr_or_400!(sql::indexer_operators_sql(addr), "address"),
        )
        .optional(),
        Part::rows(
            "delegators",
            NestId::Alloc,
            addr_or_400!(sql::indexer_delegators_sql(addr, 100), "address"),
        )
        .optional(),
        Part::rows(
            "allocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_active_allocations_sql(addr), "address"),
        )
        .optional(),
        Part::rows(
            "closedAllocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_closed_allocations_sql(addr, 500), "address"),
        )
        .optional(),
        Part::one(
            "params",
            NestId::Alloc,
            sql::DELEGATION_RATIO_SQL.to_string(),
        ),
    ];
    let inner = many(parts, T_FOLD);
    let naming = ctx.naming.clone();
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        let naming = naming.clone();
        Box::pin(async move {
            let (v, p) = inner(gate.clone(), lane).await?;
            let section = |k: &str| v.get(k).and_then(|x| x.as_array()).map(Vec::as_slice);
            let row = v.get("indexer").cloned().unwrap_or(serde_json::Value::Null);
            // The protocol's delegation ratio, defaulting to sixteen if the view is unreadable.
            let ratio = v
                .get("params")
                .and_then(|p| p.get("delegation_ratio"))
                .and_then(|r| r.as_f64())
                .unwrap_or(16.0);

            let mut shaped = crate::shape::indexer_detail(
                &row,
                section("operators"),
                section("allocations"),
                section("closedAllocations"),
                ratio,
            );
            if let Some(delegators) = section("delegators") {
                shaped["delegators"] = delegators.iter().map(crate::shape::delegator).collect();
            }
            let mut value = serde_json::json!({ "indexer": shaped });
            if let Some(degraded) = v.get("degraded") {
                value["degraded"] = degraded.clone();
            }
            // Named inside the fetch, so the cached answer carries the names.
            if let Some(naming) = &naming {
                naming.indexer_page(&gate, lane, &mut value).await;
            }
            Ok((value, p))
        })
    });

    ctx.cached(
        "indexer",
        NestId::Alloc,
        serde_json::json!({ "address": addr }),
        indexer_page(cadence::LIVE),
        fetch,
    )
    .await
}

pub struct DelegatorsParams {
    pub first: i64,
    pub skip: i64,
    pub order_by: String,
    pub descending: bool,
}

/// `/api/indexer/[address]/delegators`: every delegator, paged and sorted by the nest.
/// `/api/indexer/[address]` keeps its hundred for the callers that still read them there.
pub async fn indexer_delegators(
    ctx: &ReadCtx,
    addr: &str,
    p: DelegatorsParams,
) -> Result<Read, ApiError> {
    let first = sql::clamp(p.first, 1, 1000);
    let skip = sql::clamp(p.skip, 0, 1_000_000);
    let order = sql::indexer_delegators_order_column(&p.order_by);
    let sql = addr_or_400!(
        sql::indexer_delegators_page_sql(addr, first, skip, &p.order_by, p.descending),
        "address"
    );
    let inner = rows_only(NestId::Alloc, sql, T_FOLD);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, prov) = inner(gate, lane).await?;
            let rows = v.as_array().map(Vec::as_slice).unwrap_or_default();
            Ok((shape::indexer_delegators_page(rows, skip), prov))
        })
    });

    ctx.cached(
        "indexer-delegators",
        NestId::Alloc,
        serde_json::json!({
            "address": addr, "first": first, "skip": skip, "order": order, "descending": p.descending,
        }),
        CachePolicy::from_poll_interval(cadence::LIVE),
        fetch,
    )
    .await
}

/// `/api/indexer-status/[address]`: the subset the profile header needs, measured at 7.7 seconds.
pub async fn indexer_status(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "indexer",
            NestId::Alloc,
            addr_or_400!(sql::indexer_detail_sql(addr), "address"),
        ),
        Part::rows(
            "activeAllocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_active_allocations_sql(addr), "address"),
        ),
    ];
    ctx.cached(
        "indexer-status",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        indexer_page(cadence::LIVE),
        many(parts, T_FOLD),
    )
    .await
}

/// `/api/apr-provenance/[address]`: the detail row behind the APR figure, so a reader can see how
/// it was arrived at rather than being asked to trust it.
pub async fn apr_provenance(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "indexer",
            NestId::Alloc,
            addr_or_400!(sql::indexer_detail_sql(addr), "address"),
        ),
        Part::rows(
            "activeAllocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_active_allocations_sql(addr), "address"),
        ),
        Part::one(
            "networkParams",
            NestId::Alloc,
            sql::DELEGATION_RATIO_SQL.to_string(),
        ),
    ];
    ctx.cached(
        "apr-provenance",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(parts, T_FOLD),
    )
    .await
}

/// `/api/indexer-stake-history/[address]`: one statement for the whole series, weekly over `days`.
///
/// Keyed on the window, with the cutoffs taken when it is fetched. Keyed on the cutoffs, the key
/// moved every second and no request was ever answered from the cache.
pub async fn indexer_stake_history(ctx: &ReadCtx, addr: &str, days: i64) -> Result<Read, ApiError> {
    addr_or_400!(
        sql::indexer_stake_history_sql(addr, &stake_history_cutoffs(unix_now(), days)),
        "address or window"
    );
    let addr_owned = addr.to_string();
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let cutoffs = stake_history_cutoffs(unix_now(), days);
        let sql = sql::indexer_stake_history_sql(&addr_owned, &cutoffs).unwrap_or_default();
        rows_shaped(
            NestId::Alloc,
            sql,
            T_FOLD,
            shape::stake_history_point,
            Some("history"),
        )(gate, lane)
    });
    ctx.cached(
        "indexer-stake-history",
        NestId::Alloc,
        serde_json::json!({"address": addr, "days": days}),
        indexer_page(cadence::ARCHIVAL),
        fetch,
    )
    .await
}

/// Weekly cutoffs from `days` before `end` up to it.
pub fn stake_history_cutoffs(end: i64, days: i64) -> Vec<i64> {
    let start = end - days * 86_400;
    (0..)
        .map(|i| start + i * 86_400 * 7)
        .take_while(|t| *t <= end)
        .collect()
}

/// `/api/indexer/[address]/trends`: the daily rewards and fees the indexer page charts.
pub async fn indexer_trends(ctx: &ReadCtx, addr: &str, since: i64) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::indexer_daily_sql(addr, since), "address");
    ctx.cached(
        "indexer-trends",
        NestId::Alloc,
        serde_json::json!({"address": addr, "since": since}),
        indexer_page(cadence::ARCHIVAL),
        many_folded(
            vec![Part::rows("rows", NestId::Alloc, sql)],
            T_FOLD,
            shape::indexer_trends,
        ),
    )
    .await
}

/// `/api/indexer/[address]/revenue` and the P&L built on it: earnings per day, and per deployment
/// when asked, from `since`.
pub async fn indexer_revenue(
    ctx: &ReadCtx,
    addr: &str,
    since: i64,
    by_deployment: bool,
) -> Result<Read, ApiError> {
    let mut parts = vec![Part::rows(
        "daily",
        NestId::Alloc,
        addr_or_400!(sql::indexer_revenue_daily_sql(addr, since), "address"),
    )];
    if by_deployment {
        parts.push(Part::rows(
            "by_deployment",
            NestId::Alloc,
            addr_or_400!(
                sql::indexer_revenue_by_deployment_sql(addr, since),
                "address"
            ),
        ));
    }
    ctx.cached(
        "indexer-revenue",
        NestId::Alloc,
        serde_json::json!({"address": addr, "since": since, "byDeployment": by_deployment}),
        indexer_page(cadence::SLOW),
        many(parts, T_FOLD),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Portfolios
// ---------------------------------------------------------------------------------------------

pub async fn delegator_portfolio(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "delegator",
            NestId::Alloc,
            addr_or_400!(sql::delegator_sql(addr), "address"),
        ),
        Part::rows(
            "stakes",
            NestId::Alloc,
            addr_or_400!(sql::delegator_stakes_sql(addr, 200, false), "address"),
        ),
        Part::one(
            "networkParams",
            NestId::Alloc,
            sql::DELEGATION_RATIO_SQL.to_string(),
        ),
    ];
    ctx.cached(
        "portfolio-delegator",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(parts, T_FOLD),
    )
    .await
}

pub async fn curator_portfolio(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "curator",
            NestId::Alloc,
            addr_or_400!(sql::curator_sql(addr), "address"),
        ),
        Part::rows(
            "signals",
            NestId::Alloc,
            addr_or_400!(sql::curator_signals_sql(addr, 200), "address"),
        ),
    ];
    ctx.cached(
        "portfolio-curator",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(parts, T_FOLD),
    )
    .await
}

/// `/api/rewards-history`: the active positions a rewards series is built from.
pub async fn rewards_history(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::delegator_stakes_sql(addr, 100, true), "address");
    ctx.cached(
        "rewards-history",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Provisions, payments, proofs
// ---------------------------------------------------------------------------------------------

/// `/api/provisions`, by indexer.
///
/// Two folds: the indexer's own provisions, and the network-wide totals for each data service they
/// provision to. The totals are a property of the service rather than of this indexer, so they
/// cannot come from the first query - and reporting a per-indexer figure under a network-wide name
/// would be worse than leaving it out.
pub async fn provisions_by_indexer(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::provisions_by_indexer_sql(addr), "address");
    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        let sql = sql.clone();
        Box::pin(async move {
            let mut spec = QuerySpec::read(NestId::Alloc, sql).with_timeout(T_FOLD);
            spec.lane = lane;
            let key = format!("alloc:{}", blake3::hash(spec.sql.as_bytes()).to_hex());
            let rows = gate
                .query_coalesced(&key, spec)
                .await
                .map_err(ApiError::from)?;

            let services: Vec<String> = {
                let mut v: Vec<String> = rows
                    .rows
                    .iter()
                    .filter_map(|r| r.get("data_service").and_then(|s| s.as_str()))
                    .map(str::to_string)
                    .collect();
                v.sort();
                v.dedup();
                v
            };

            // The service thawing period is a sampled contract call rather than an event, so the
            // nest stores the raw ABI word and we decode it. One value serves both the minimum and
            // the maximum, because the protocol has one.
            let mut thawing: Option<String> = None;
            {
                let mut tspec = QuerySpec::read(NestId::Alloc, sql::MAX_THAWING_PERIOD_SQL)
                    .with_timeout(T_QUICK);
                tspec.lane = lane;
                if let Ok(t) = gate.query_coalesced("alloc:max-thawing", tspec).await {
                    thawing = t
                        .rows
                        .first()
                        .and_then(|r| r.get("result"))
                        .and_then(|v| v.as_str())
                        .and_then(crate::shape::decode_uint_word);
                }
            }

            let mut totals: std::collections::HashMap<String, serde_json::Value> =
                Default::default();
            if let Some(tsql) = sql::data_service_totals_sql(&services) {
                let mut tspec = QuerySpec::read(NestId::Alloc, tsql).with_timeout(T_FOLD);
                tspec.lane = lane;
                let tkey = format!("alloc:{}", blake3::hash(tspec.sql.as_bytes()).to_hex());
                if let Ok(t) = gate.query_coalesced(&tkey, tspec).await {
                    for row in &t.rows {
                        if let Some(id) = row.get("data_service").and_then(|s| s.as_str()) {
                            totals.insert(id.to_string(), row.clone());
                        }
                    }
                }
            }

            let mapped: Vec<serde_json::Value> = rows
                .rows
                .iter()
                .map(|r| {
                    let svc = r
                        .get("data_service")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default();
                    crate::shape::provision_with_totals(r, totals.get(svc), thawing.as_deref())
                })
                .collect();
            Ok((
                serde_json::json!({ "provisions": mapped }),
                rows.provenance.clone(),
            ))
        })
    });

    ctx.cached(
        "provisions-indexer",
        NestId::Alloc,
        serde_json::json!({ "indexer": addr }),
        indexer_page(cadence::LIVE),
        fetch,
    )
    .await
}

pub async fn provisions_by_service(
    ctx: &ReadCtx,
    addr: &str,
    first: i64,
    skip: i64,
) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::provisions_by_service_sql(addr, first, skip), "address");
    ctx.cached(
        "provisions-service",
        NestId::Alloc,
        serde_json::json!({"service": addr, "first": first, "skip": skip}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_shaped(
            NestId::Alloc,
            sql,
            T_FOLD,
            crate::shape::provision,
            Some("provisions"),
        ),
    )
    .await
}

/// `/api/payments`: escrow balances, the transaction tail, tally collections, and the totals over
/// them. Three folds against the same nest, coalesced separately.
pub async fn payments(ctx: &ReadCtx, receiver: Option<&str>) -> Result<Read, ApiError> {
    let accounts_sql = addr_or_400!(sql::escrow_accounts_sql(receiver, 100), "address");
    let tx_sql = addr_or_400!(
        sql::escrow_transactions_sql(receiver, if receiver.is_some() { 100 } else { 50 }),
        "address"
    );
    let tally_sql = addr_or_400!(sql::tally_collected_sql(receiver, 50), "address");
    let by_payer_sql = addr_or_400!(sql::tally_by_payer_sql(receiver), "address");

    let parts = vec![
        Part::rows("accounts", NestId::Alloc, accounts_sql),
        Part::rows("transactions", NestId::Alloc, tx_sql),
        Part::rows("tally", NestId::Alloc, tally_sql),
        Part::rows("byPayer", NestId::Alloc, by_payer_sql),
    ];
    let inner = many(parts, T_FOLD);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let arr = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            Ok((
                crate::shape::payments(
                    &arr("accounts"),
                    &arr("transactions"),
                    &arr("tally"),
                    &arr("byPayer"),
                ),
                p,
            ))
        })
    });

    ctx.cached(
        "payments",
        NestId::Alloc,
        serde_json::json!({ "receiver": receiver }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

/// `/api/poi`: which deployments the network agrees on, and which it does not.
///
/// **Two shapes, one route.** Without `deployment` this is the overview, `{ summary, deployments }`.
/// With one it is that deployment's proof history, `{ deploymentId, ipfsHash, signal, stake,
/// totalAllocations, uniqueIndexers, epochs }`, which is what `/poi/[deployment]` destructures.
///
/// The filter used to be applied to the SQL and then thrown at `poi_overview` regardless, so the
/// detail form answered 200 with the overview shape, no `epochs`, and the page died on
/// `detail.epochs.reduce(...)` (kittiwake#27).
pub async fn poi(ctx: &ReadCtx, deployment: Option<&str>, first: i64) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::poi_allocations_sql(deployment, first), "deployment id");
    let detail = deployment.is_some();
    let inner = rows_only(NestId::Alloc, sql, T_FOLD);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let rows = v.as_array().cloned().unwrap_or_default();
            let shaped = if detail {
                // A deployment nobody has closed a proved allocation against is a 404, not an empty
                // detail page pretending the deployment exists.
                crate::shape::poi_deployment_detail(&rows).ok_or(ApiError::NotFound)?
            } else {
                crate::shape::poi_overview(&rows)
            };
            Ok((shaped, p))
        })
    });

    ctx.cached(
        "poi",
        NestId::Alloc,
        serde_json::json!({ "deployment": deployment, "first": first }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Delegation activity
// ---------------------------------------------------------------------------------------------

/// `/api/delegation-events`: the newest `first` events over the last `days`, keyed on the window
/// for the reason [`indexer_stake_history`] is.
pub async fn delegation_events(
    ctx: &ReadCtx,
    indexer: Option<&str>,
    first: i64,
    days: i64,
) -> Result<Read, ApiError> {
    addr_or_400!(sql::delegation_events_sql(indexer, first, 0), "address");
    let indexer_owned = indexer.map(str::to_string);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let since = unix_now() - days * 86_400;
        let sql =
            sql::delegation_events_sql(indexer_owned.as_deref(), first, since).unwrap_or_default();
        // The incumbent puts `source` inside `data` on this route and at the top level on
        // others. Not a pattern worth rationalising - just one the callers already parse.
        let inner = rows_shaped(
            NestId::Alloc,
            sql,
            T_FOLD,
            crate::shape::delegation_event,
            Some("delegationEvents"),
        );
        Box::pin(async move {
            let (mut v, p) = inner(gate, lane).await?;
            v["source"] = serde_json::json!("nuthatch");
            Ok((v, p))
        })
    });
    ctx.cached(
        "delegation-events",
        NestId::Alloc,
        serde_json::json!({"indexer": indexer, "first": first, "days": days}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

/// `/api/delegation-flows`: daily inflow and outflow, from two nests.
///
/// The pre-Horizon era lives in the frozen archive and the Horizon era in the live nest, split at
/// a fixed instant so the two windows are disjoint and no day is counted twice.
///
/// **Both halves are summed by the nest.** Fetching the events and adding them up here was the
/// first version and it was wrong: a long window runs to hundreds of thousands of events against a
/// 50,000-row cap, so the answer came back truncated and the daily totals were short. A chart that
/// is quietly missing three quarters of its volume looks entirely plausible.
///
/// The answer is dated by the **live** half. The archive is frozen by construction rather than
/// stale, and dating the whole response by an as-of that will never advance would make a current
/// chart look three-quarters of a million blocks out of date for ever.
pub async fn delegation_flows(ctx: &ReadCtx, since: i64) -> Result<Read, ApiError> {
    let boundary = sql::LEGACY_HISTORY_END_EXCLUSIVE;
    let wants_archive = since < boundary;

    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        Box::pin(async move {
            let mut live = QuerySpec::read(
                NestId::Alloc,
                sql::daily_flow_sql(since.max(boundary), i64::MAX, false),
            )
            .with_timeout(T_FOLD);
            live.lane = lane;

            let live_answer = gate
                .query_coalesced(&format!("alloc:flows:{}", since.max(boundary)), live)
                .await
                .map_err(ApiError::from)?;

            let mut days: std::collections::BTreeMap<String, (f64, f64)> = Default::default();
            let mut absorb = |rows: &[serde_json::Value]| {
                for r in rows {
                    let Some(date) = r.get("date").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let f = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let e = days.entry(date.to_string()).or_insert((0.0, 0.0));
                    e.0 += f("inflows");
                    e.1 += f("outflows");
                }
            };

            if wants_archive {
                let mut hist = QuerySpec::read(
                    NestId::LegacyFlows,
                    sql::daily_flow_sql(since, boundary, true),
                )
                .with_timeout(T_FOLD);
                hist.lane = lane;
                let hist_answer = gate
                    .query_coalesced(&format!("legacy:flows:{since}"), hist)
                    .await
                    .map_err(ApiError::from)?;
                absorb(&hist_answer.rows);
            }
            absorb(&live_answer.rows);

            let points: Vec<serde_json::Value> = days
                .into_iter()
                .map(|(date, (inflows, outflows))| {
                    serde_json::json!({
                        "date": date,
                        "inflows": inflows,
                        "outflows": outflows,
                        "net": inflows - outflows,
                    })
                })
                .collect();

            Ok((
                serde_json::Value::Array(points),
                live_answer.provenance.clone(),
            ))
        })
    });

    ctx.cached(
        "delegation-flows",
        NestId::Alloc,
        serde_json::json!({ "since": since }),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        fetch,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Subgraphs: the GNS nest and the deployment views
// ---------------------------------------------------------------------------------------------

/// `/api/developer-activity`: subgraphs published per week over the window.
pub async fn developer_activity(ctx: &ReadCtx, cutoff: i64) -> Result<Read, ApiError> {
    let inner = rows_only(
        NestId::Gns,
        sql::subgraphs_published_since_sql(cutoff),
        T_FOLD,
    );
    let now = cutoff + 12 * 30 * 86_400;
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let rows = v.as_array().cloned().unwrap_or_default();
            Ok((crate::shape::developer_activity(&rows, 12, now), p))
        })
    });

    ctx.cached(
        "developer-activity",
        NestId::Gns,
        serde_json::json!({ "cutoff": cutoff }),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        fetch,
    )
    .await
}

pub async fn subgraph_deployments(
    ctx: &ReadCtx,
    first: i64,
    skip: i64,
    order_by: &str,
    descending: bool,
) -> Result<Read, ApiError> {
    let sql = sql::deployments_list_sql(first, skip, order_by, descending);
    ctx.cached(
        "subgraph-deployments",
        NestId::Alloc,
        serde_json::json!({"first": first, "skip": skip, "orderBy": order_by, "desc": descending}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_shaped(
            NestId::Alloc,
            sql,
            T_FOLD,
            |r| crate::shape::deployment(r, None),
            None,
        ),
    )
    .await
}

/// `/api/subgraph-deployment/{hash}`: one deployment's directory row, by IPFS hash.
///
/// The list route exists and cannot answer this: it is ordered by signal and filtered to
/// deployments above a threshold, so asking it for a specific hash means paging a thousand rows
/// and hoping. A caller that wanted one deployment was reduced to composing three endpoints and
/// still came up short of `stakedTokens` and the allocation count.
///
/// The preview cards are what wanted it. They render on the server for crawlers, so they cannot
/// become client fetches, and reading the nest directly is what kept a nest credential in the
/// frontend's environment.
pub async fn subgraph_deployment(ctx: &ReadCtx, ipfs_hash: &str) -> Result<Read, ApiError> {
    // Converted here rather than trusted: `deployments_by_id_sql` refuses anything that is not a
    // deployment id, and a `Qm...` CID is not one. Getting this wrong returned an empty map for
    // every deployment once already (kittiwake#105).
    let id = crate::shape::ipfs_hash_to_bytes32(ipfs_hash)
        .ok_or_else(|| ApiError::BadRequest("not a deployment hash".into()))?
        .to_lowercase();
    let sql = sql::deployments_by_id_sql(std::slice::from_ref(&id))
        .ok_or_else(|| ApiError::BadRequest("not a deployment hash".into()))?;

    ctx.cached(
        "subgraph-deployment",
        NestId::Alloc,
        serde_json::json!({ "id": id }),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_shaped(
            NestId::Alloc,
            sql,
            T_FOLD,
            |r| crate::shape::deployment(r, None),
            None,
        ),
    )
    .await
}

/// `/api/subgraph-fees-30d`: the directory rows for the deployments that earned fees, each with
/// what it earned in the window.
///
/// Two folds and a join. The fee query alone answers "which deployments earned, and how much",
/// which is not what the panel draws: it draws the deployment with its fee alongside.
pub async fn subgraph_fees_30d(ctx: &ReadCtx, since: i64) -> Result<Read, ApiError> {
    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        Box::pin(async move {
            let mut fees =
                QuerySpec::read(NestId::Alloc, sql::deployment_fees_since_sql(since, 200))
                    .with_timeout(T_FOLD);
            fees.lane = lane;
            let fkey = format!("alloc:{}", blake3::hash(fees.sql.as_bytes()).to_hex());
            let fee_rows = gate
                .query_coalesced(&fkey, fees)
                .await
                .map_err(ApiError::from)?;

            let mut by_id: std::collections::HashMap<String, serde_json::Value> =
                Default::default();
            let mut ids: Vec<String> = Vec::new();
            for r in &fee_rows.rows {
                if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                    if let Some(f) = r.get("query_fees") {
                        by_id.insert(id.to_string(), f.clone());
                    }
                }
            }

            let Some(dsql) = sql::deployments_by_id_sql(&ids) else {
                // No deployments earned in the window. An empty list, not an error.
                return Ok((
                    serde_json::Value::Array(vec![]),
                    fee_rows.provenance.clone(),
                ));
            };
            let mut deps = QuerySpec::read(NestId::Alloc, dsql).with_timeout(T_FOLD);
            deps.lane = lane;
            let dkey = format!("alloc:{}", blake3::hash(deps.sql.as_bytes()).to_hex());
            let dep_rows = gate
                .query_coalesced(&dkey, deps)
                .await
                .map_err(ApiError::from)?;

            let mapped: Vec<serde_json::Value> = dep_rows
                .rows
                .iter()
                .map(|r| {
                    let id = r.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    crate::shape::deployment(r, by_id.get(id))
                })
                .collect();
            Ok((
                serde_json::Value::Array(mapped),
                older_of(&fee_rows.provenance, &dep_rows.provenance),
            ))
        })
    });

    ctx.cached(
        "subgraph-fees-30d",
        NestId::Alloc,
        serde_json::json!({ "since": since }),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        fetch,
    )
    .await
}

/// `/api/subgraph-directory`'s whole set: every listed deployment with its fees since `since`,
/// labelled. The route filters and pages it per request; see [`crate::directory`].
pub async fn subgraph_directory(ctx: &ReadCtx, since: i64) -> Result<Read, ApiError> {
    let naming = ctx.naming.clone();
    let fetch: Fetcher = Arc::new(move |gate: NestGate, lane: Lane| {
        let naming = naming.clone();
        Box::pin(async move {
            let mut deps =
                QuerySpec::read(NestId::Alloc, sql::directory_sql()).with_timeout(T_FOLD);
            deps.lane = lane;
            let mut fees = QuerySpec::read(NestId::Alloc, sql::deployment_fees_window_sql(since))
                .with_timeout(T_FOLD);
            fees.lane = lane;
            let dkey = format!("alloc:{}", blake3::hash(deps.sql.as_bytes()).to_hex());
            let fkey = format!("alloc:{}", blake3::hash(fees.sql.as_bytes()).to_hex());
            let (dep_rows, fee_rows) = tokio::join!(
                gate.query_coalesced(&dkey, deps),
                gate.query_coalesced(&fkey, fees),
            );
            let (dep_rows, fee_rows) = (
                dep_rows.map_err(ApiError::from)?,
                fee_rows.map_err(ApiError::from)?,
            );

            let by_id: std::collections::HashMap<&str, &serde_json::Value> = fee_rows
                .rows
                .iter()
                .filter_map(|r| Some((r.get("id")?.as_str()?, r.get("query_fees")?)))
                .collect();
            let mut rows: Vec<serde_json::Value> = dep_rows
                .rows
                .iter()
                .map(|r| {
                    let id = r.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    crate::directory::row(r, by_id.get(id.to_lowercase().as_str()).copied())
                })
                .collect();
            let unanalysed = match &naming {
                Some(n) => n.label_directory(&gate, lane, &mut rows).await,
                None => rows.len(),
            };
            Ok((
                crate::directory::set(rows, unanalysed),
                older_of(&dep_rows.provenance, &fee_rows.provenance),
            ))
        })
    });

    ctx.cached(
        "subgraph-directory",
        NestId::Alloc,
        serde_json::json!({ "since": since }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

pub async fn subgraph_curation(ctx: &ReadCtx, dep: &str) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::deployment_signals_sql(dep, 100), "deployment id");
    let inner = rows_only(NestId::Alloc, sql, T_FOLD);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let rows = v.as_array().cloned().unwrap_or_default();
            Ok((shape::subgraph_curation(&rows), p))
        })
    });
    ctx.cached(
        "subgraph-curation",
        NestId::Alloc,
        serde_json::json!({"deployment": dep}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        fetch,
    )
    .await
}

/// `/api/subgraph-history/[hash]`: signal transactions and allocation history for the timeline.
pub async fn subgraph_history(ctx: &ReadCtx, dep: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::rows(
            "signals",
            NestId::Alloc,
            addr_or_400!(
                sql::deployment_signal_transactions_sql(dep, 1000),
                "deployment id"
            ),
        ),
        Part::rows(
            "allocations",
            NestId::Alloc,
            addr_or_400!(
                sql::deployment_allocations_history_sql(dep, 1000),
                "deployment id"
            ),
        ),
    ];
    ctx.cached(
        "subgraph-history",
        NestId::Alloc,
        serde_json::json!({"deployment": dep}),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        many_folded(parts, T_FOLD, shape::subgraph_history),
    )
    .await
}

/// `/api/indexing-status/[hash]`: which indexers hold an active allocation on a deployment, and
/// the deployment's own totals. The live probing of those indexers is a separate concern.
pub async fn deployment_status(ctx: &ReadCtx, dep: &str) -> Result<Read, ApiError> {
    let parts = vec![
        Part::one(
            "deployment",
            NestId::Alloc,
            addr_or_400!(sql::deployment_sql(dep), "deployment id"),
        ),
        Part::rows(
            "allocations",
            NestId::Alloc,
            addr_or_400!(
                sql::allocations_by_deployment_sql(dep, 100),
                "deployment id"
            ),
        ),
    ];
    ctx.cached(
        "indexing-status",
        NestId::Alloc,
        serde_json::json!({"deployment": dep}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(parts, T_FOLD),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// DIPS
// ---------------------------------------------------------------------------------------------

/// `/api/dips`: where protocol issuance actually goes.
///
/// Four folds against the DIPS nest, which admits two concurrent queries. The gate is what makes
/// that safe: the Node version fired all four at once and survived only because the client
/// happened to stagger them, which was not a property anyone chose.
/// `/api/subgraph-versions/[hash]`: every version of the subgraph that published this deployment.
///
/// Three queries and they are **dependent**, which is why this does not use [`many`]: the
/// deployment names a subgraph, the subgraph names its versions, and only then is there a set of
/// deployments to ask the allocations nest about. Running them concurrently would mean guessing
/// the second query's input.
///
/// The version labels are not here. They live in IPFS documents, and filling them needs the
/// Postgres document cache rather than a nest, so the caller does it in one bulk read after this
/// answer is cached. That way one cached fold serves every request and the labels cost a single
/// query rather than one fetch per version.
pub fn subgraph_versions_fetcher(dep: String) -> Fetcher {
    Arc::new(move |gate: NestGate, lane: Lane| {
        let dep = dep.clone();
        Box::pin(async move {
            let ask = |gate: NestGate, nest: NestId, sql: String| async move {
                let mut spec = QuerySpec::read(nest, sql.clone()).with_timeout(T_FOLD);
                spec.lane = lane;
                let key = format!(
                    "{}:{}",
                    nest.as_str(),
                    blake3::hash(sql.as_bytes()).to_hex()
                );
                gate.query_coalesced(&key, spec)
                    .await
                    .map_err(ApiError::from)
            };

            let owner_sql = sql::deployment_subgraph_sql(&dep)
                .ok_or_else(|| ApiError::BadRequest("not a valid deployment id".into()))?;
            let owner = ask(gate.clone(), NestId::Gns, owner_sql).await?;
            let mut provenance = owner.provenance.clone();

            let subgraph_id = owner
                .rows
                .first()
                .and_then(|r| r.get("subgraph_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // A deployment nobody has published through the GNS has no versions, which is an
            // answer rather than an error.
            let Some(subgraph_id) = subgraph_id else {
                return Ok((
                    serde_json::json!({ "subgraphId": null, "versions": [] }),
                    provenance,
                ));
            };

            let versions_sql = sql::subgraph_versions_sql(&subgraph_id, 100).ok_or_else(|| {
                ApiError::Internal("the gns nest returned a subgraph id we will not query".into())
            })?;
            let versions = ask(gate.clone(), NestId::Gns, versions_sql).await?;
            provenance = older_of(&provenance, &versions.provenance);

            let deployments: Vec<String> = {
                let mut seen: Vec<String> = Vec::new();
                for row in &versions.rows {
                    if let Some(id) = row.get("deployment_id").and_then(|v| v.as_str()) {
                        let id = id.to_lowercase();
                        if !seen.contains(&id) {
                            seen.push(id);
                        }
                    }
                }
                seen
            };

            let mut stats = serde_json::Map::new();
            if let Some(stats_sql) = sql::deployments_signal_stake_sql(&deployments) {
                let answer = ask(gate, NestId::Alloc, stats_sql).await?;
                provenance = older_of(&provenance, &answer.provenance);
                for row in &answer.rows {
                    if let Some(id) = row.get("subgraph_deployment").and_then(|v| v.as_str()) {
                        stats.insert(id.to_string(), row.clone());
                    }
                }
            }

            let out: Vec<serde_json::Value> = versions
                .rows
                .iter()
                .map(|row| {
                    let id = row
                        .get("deployment_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let stat = stats.get(&id.to_lowercase());
                    let token = |key: &str| {
                        stat.and_then(|s| s.get(key))
                            .and_then(|v| v.as_str())
                            .unwrap_or("0")
                            .to_string()
                    };
                    // The `Qm…` form when it converts and the raw id otherwise, because a version
                    // whose id will not convert is still a version.
                    let ipfs_hash = shape::ipfs_or_id(id);
                    serde_json::json!({
                        "version": row.get("version_number").cloned().unwrap_or(serde_json::Value::Null),
                        "createdAt": row.get("created_at").cloned().unwrap_or(serde_json::Value::Null),
                        "ipfsHash": ipfs_hash,
                        "signalledTokens": token("signalled_tokens"),
                        "stakedTokens": token("staked_tokens"),
                        "isCurrent": ipfs_hash == shape::ipfs_or_id(&dep),
                        // Carried so the caller can fill `label` from the document cache; replaced
                        // before the response is written.
                        "versionMetadata": row.get("version_metadata").cloned().unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect();

            Ok((
                serde_json::json!({ "subgraphId": subgraph_id, "versions": out }),
                provenance,
            ))
        })
    })
}

pub async fn subgraph_versions(ctx: &ReadCtx, dep: &str) -> Result<Read, ApiError> {
    ctx.cached(
        "subgraph-versions",
        NestId::Gns,
        serde_json::json!({ "deployment": dep }),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        subgraph_versions_fetcher(dep.to_string()),
    )
    .await
}

/// `/api/dips/agreements`: the DIPS agreement lifecycle, offer through collection.
///
/// One `SELECT *` per stage table rather than one clever UNION. The nest's own `dips_timeline`
/// view unions its sources and that is the right shape beside the data, but this code cannot be
/// exercised against a populated nest, so it is written to fail loudly rather than subtly: a
/// per-table read has no cross-branch type unification to get wrong, and a mistaken column name
/// surfaces as an absent field rather than as a query returning plausible rubbish.
///
/// The parts run concurrently and each is coalesced on its own statement, so the nine cost one
/// round of gate permits rather than nine sequential ones.
pub async fn dips_agreements(ctx: &ReadCtx, sepolia: bool) -> Result<Read, ApiError> {
    let nest = if sepolia {
        NestId::DipsSepolia
    } else {
        NestId::Dips
    };
    let parts: Vec<Part> = dips_agreements_mod::STAGES
        .iter()
        .map(|stage| {
            Part::rows(
                stage.key(),
                nest,
                // The table name is a constant from the nest's own schema, never a caller's input.
                format!("SELECT * FROM {} LIMIT 5000", stage.table()),
            )
        })
        .collect();

    ctx.cached(
        "dips-agreements",
        nest,
        serde_json::json!({ "sepolia": sepolia }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        many_folded(parts, T_FOLD, dips_agreements_mod::fold),
    )
    .await
}

pub async fn dips(ctx: &ReadCtx, sepolia: bool) -> Result<Read, ApiError> {
    let nest = if sepolia {
        NestId::DipsSepolia
    } else {
        NestId::Dips
    };
    let Some(distributed) =
        sql::dips_latest_per_target_sql("issuance_allocator__issuance_distributed")
    else {
        return Err(ApiError::Internal(
            "dips table allowlist rejected its own entry".into(),
        ));
    };
    let Some(self_minted) =
        sql::dips_latest_per_target_sql("issuance_allocator__issuance_self_mint_allowance")
    else {
        return Err(ApiError::Internal(
            "dips table allowlist rejected its own entry".into(),
        ));
    };

    let parts = vec![
        Part::rows(
            "allocations",
            nest,
            sql::DIPS_CURRENT_ALLOCATION_SQL.to_string(),
        ),
        Part::rows("timeline", nest, sql::DIPS_TIMELINE_SQL.to_string()),
        Part::rows("distributed", nest, distributed),
        Part::rows("selfMinted", nest, self_minted),
    ];
    let inner = many(parts, T_FOLD);
    let fetch: Fetcher = Arc::new(move |gate, lane| {
        let inner = inner.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            let arr = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            Ok((
                crate::shape::dips(
                    &arr("allocations"),
                    &arr("timeline"),
                    &arr("distributed"),
                    &arr("selfMinted"),
                ),
                p,
            ))
        })
    });

    ctx.cached(
        "dips",
        nest,
        serde_json::json!({ "sepolia": sepolia }),
        CachePolicy::from_poll_interval(cadence::SLOW),
        fetch,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The admission bound, as arithmetic rather than as a running fold.
    ///
    /// A fold may never ask for more permits at once than its lane admits, or it starves itself:
    /// it takes every permit, its remaining parts queue for one, and the permits it holds are not
    /// released until every part finishes - which cannot happen. `/api/indexer` is six `alloc`
    /// parts and `alloc` had four permits, so it deadlocked on every cold call and only unwedged
    /// when the waiters hit the gate timeout.
    fn admitted(lane_width: usize) -> usize {
        lane_width.saturating_sub(1).max(1)
    }

    #[test]
    fn a_fold_never_asks_for_more_than_the_lane_admits() {
        // The pool that broke, and the one it was raised to. In both cases the fold must run
        // strictly inside the pool rather than filling it.
        for width in [1usize, 2, 4, 6, 8, 16] {
            assert!(
                admitted(width) < width || width == 1,
                "a fold admitted {} of {width} permits, which leaves nothing for anyone else",
                admitted(width)
            );
            assert!(admitted(width) >= 1, "a fold must be allowed to run at all");
        }
    }

    /// Six parts against four permits is the shape that deadlocked. Whatever the part count, the
    /// number in flight is bounded by the pool and not by the fold.
    #[test]
    fn a_six_part_fold_fits_inside_a_four_permit_pool() {
        assert_eq!(admitted(4), 3);
        // The old behaviour, for contrast: `join_all` ran all six at once.
        assert!(
            admitted(4) < 6,
            "six in flight against four permits is the deadlock"
        );
    }

    /// One permit must still make progress rather than dividing to zero and hanging forever.
    #[test]
    fn a_single_permit_lane_still_runs_one_at_a_time() {
        assert_eq!(admitted(1), 1);
        assert_eq!(admitted(0), 1);
    }

    #[test]
    fn a_composed_answer_is_dated_by_its_stalest_half() {
        let fresh = Provenance {
            as_of: Some(500),
            sealed_through: Some(490),
            ..Default::default()
        };
        let old = Provenance {
            as_of: Some(400),
            sealed_through: Some(395),
            degraded: true,
            ..Default::default()
        };
        let merged = older_of(&fresh, &old);
        assert_eq!(merged.as_of, Some(400));
        assert_eq!(merged.sealed_through, Some(395));
        assert!(
            merged.degraded,
            "a degraded half makes the whole answer degraded"
        );
    }

    #[test]
    fn a_missing_half_does_not_erase_the_other_ones_date() {
        let known = Provenance {
            as_of: Some(500),
            ..Default::default()
        };
        let merged = older_of(&known, &Provenance::default());
        assert_eq!(merged.as_of, Some(500));
    }
}

#[cfg(test)]
mod wiring_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    /// Records every statement the service actually sends, so a test can assert on the wiring
    /// rather than on the query builders in isolation.
    ///
    /// This exists because a builder test passed while the route still called the wrong builder:
    /// a formatting pass had split the call across lines and a search-and-replace missed it. The
    /// SQL was right, the route was wrong, and nothing failed until production answered 400.
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Respond for Recorder {
        fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
            // A serving route consults readiness before it asks anything, so the stand-in has to
            // answer that too or every read is refused before a statement is ever sent.
            if req.url.path().ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            self.0
                .lock()
                .unwrap()
                .push(format!("{} :: {q}", req.url.path()));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 0, "rows": [],
                "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
            }))
        }
    }

    #[tokio::test]
    async fn each_nest_is_asked_a_question_its_own_schema_can_answer() {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Recorder(seen.clone()))
            .mount(&server)
            .await;

        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let gate = NestGate::new(cfg).unwrap();
        let ctx = ReadCtx::new(gate, kittiwake_cache::Store::new(64), 1);

        // A window starting before the boundary, so both halves are asked.
        delegation_flows(&ctx, 100)
            .await
            .expect("both halves answer");

        let seen = seen.lock().unwrap();
        let live = seen
            .iter()
            .find(|s| s.starts_with("/alloc/sql"))
            .expect("the live nest was asked");
        let archive = seen
            .iter()
            .find(|s| s.starts_with("/legacy-flows/sql"))
            .expect("the archive was asked");

        // The live nest indexes Horizon only; the pre-Horizon tables exist solely in the archive,
        // and asking for them here is a 400.
        assert!(live.contains("staking__tokens_delegated"), "live: {live}");
        assert!(
            !live.contains("staking_legacy__"),
            "the live nest has no pre-Horizon tables"
        );
        assert!(
            archive.contains("staking_legacy__stake_delegated"),
            "archive: {archive}"
        );

        // Both halves must be summed by the nest. A raw scan truncates at 50,000 rows and the
        // chart silently loses volume.
        for q in [live, archive] {
            assert!(
                q.contains("GROUP BY 1"),
                "not aggregated at the source: {q}"
            );
            assert!(
                !q.contains("lodestar_delegations"),
                "the flow reads events, not the fold: {q}"
            );
        }
    }

    /// A window that starts after the boundary has no archive half at all, and must not ask the
    /// archive a question whose answer would be empty anyway.
    #[tokio::test]
    async fn a_recent_window_does_not_trouble_the_archive() {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Recorder(seen.clone()))
            .mount(&server)
            .await;

        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let ctx = ReadCtx::new(
            NestGate::new(cfg).unwrap(),
            kittiwake_cache::Store::new(64),
            1,
        );

        delegation_flows(&ctx, sql::LEGACY_HISTORY_END_EXCLUSIVE + 86_400)
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|s| s.starts_with("/alloc/sql")));
        assert!(
            !seen.iter().any(|s| s.starts_with("/legacy-flows/sql")),
            "the archive should not have been asked"
        );
    }

    /// Keyed on its cutoffs or its start, each of these was a new entry every second, so every
    /// request was a statement and nothing could keep them warm.
    #[tokio::test]
    async fn a_moving_window_asked_a_second_later_is_answered_from_the_cache() {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Recorder(seen.clone()))
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let ctx = ReadCtx::new(
            NestGate::new(cfg).unwrap(),
            kittiwake_cache::Store::new(64),
            1,
        );
        let a = "0x00000000000000000000000000000000000000aa";

        indexer_stake_history(&ctx, a, 182).await.unwrap();
        delegation_events(&ctx, Some(a), 100, 30).await.unwrap();
        let asked = seen.lock().unwrap().len();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let history = indexer_stake_history(&ctx, a, 182).await.unwrap();
        let events = delegation_events(&ctx, Some(a), 100, 30).await.unwrap();
        assert_eq!(history.meta.served_from, kittiwake_types::ServedFrom::Cache);
        assert_eq!(events.meta.served_from, kittiwake_types::ServedFrom::Cache);
        assert_eq!(seen.lock().unwrap().len(), asked);
    }

    #[test]
    fn stake_history_cutoffs_are_weekly_and_end_at_the_end() {
        let end = 1_789_000_000;
        assert_eq!(
            stake_history_cutoffs(end, 14),
            vec![end - 14 * 86_400, end - 7 * 86_400, end]
        );
    }

    /// Answers every statement with the same rows, and records what it was asked.
    struct Rows(Arc<Mutex<Vec<String>>>, Vec<serde_json::Value>);

    impl Respond for Rows {
        fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
            if req.url.path().ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            if let Some((_, q)) = req.url.query_pairs().find(|(k, _)| k == "q") {
                self.0.lock().unwrap().push(q.to_string());
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": self.1.len(), "rows": self.1,
                "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
            }))
        }
    }

    async fn ctx_answering(
        rows: Vec<serde_json::Value>,
    ) -> (ReadCtx, Arc<Mutex<Vec<String>>>, MockServer) {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Rows(seen.clone(), rows))
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let ctx = ReadCtx::new(
            NestGate::new(cfg).unwrap(),
            kittiwake_cache::Store::new(64),
            1,
        );
        (ctx, seen, server)
    }

    const INDEXER: &str = "0x2f09092aacd80196fc984908c5a9a7ab3ee4f1ce";

    #[tokio::test]
    async fn the_delegator_page_the_reader_asked_for_is_the_one_the_nest_is_asked() {
        let row = serde_json::json!({
            "id": format!("0x1f5fa305abcc49e014d1dbfc4ed68befad403688-{INDEXER}"),
            "delegator": "0x1f5fa305abcc49e014d1dbfc4ed68befad403688",
            "share_amount": "5017656192609777038808807",
            "current_tokens": "11624834073223557856362496",
            "total_delegated_tokens": "10300000000000000000000000",
            "thawing_tokens": "0", "thawing_until": null, "locked_tokens": "0",
            "delegated_at": 1788253100, "last_change_at": 1788253100,
            "pool_shares": "71439299428911516114298997",
            "pool_tokens": "165509546746463656406107704",
            "total": 2498, "active": 2490,
        });
        let (ctx, seen, _server) = ctx_answering(vec![row]).await;

        let read = indexer_delegators(
            &ctx,
            INDEXER,
            DelegatorsParams {
                first: 50,
                skip: 100,
                order_by: "delegatedAt".into(),
                descending: false,
            },
        )
        .await
        .expect("the nest answers");

        let asked = seen.lock().unwrap();
        let page = asked
            .iter()
            .find(|q| q.contains("lodestar_indexer_pool"))
            .expect("the page was asked for");
        assert!(
            page.ends_with(
                "ORDER BY p.delegated_at ASC NULLS LAST, p.delegator ASC LIMIT 50 OFFSET 100"
            ),
            "{page}"
        );

        let v = &*read.entry.value;
        assert_eq!(v["total"], 2498);
        assert_eq!(v["active"], 2490);
        assert_eq!(v["pool"]["delegatorShares"], "71439299428911516114298997");
        let d = &v["delegators"][0];
        assert_eq!(
            d["delegator"]["id"],
            "0x1f5fa305abcc49e014d1dbfc4ed68befad403688"
        );
        assert_eq!(d["currentTokens"], "11624834073223557856362496");
        assert_eq!(d["totalDelegatedTokens"], "10300000000000000000000000");
        assert_eq!(d["thawingUntil"], serde_json::Value::Null);
        assert_eq!(d["delegatedAt"], 1788253100);
    }

    /// Past the end there is no row to carry the totals. Zero there would tell the reader the
    /// indexer has no delegators; only an empty first page may say that.
    #[tokio::test]
    async fn a_page_past_the_end_does_not_claim_there_are_no_delegators() {
        let (ctx, _seen, _server) = ctx_answering(Vec::new()).await;
        let page = |skip| DelegatorsParams {
            first: 25,
            skip,
            order_by: "stake".into(),
            descending: true,
        };

        let past = indexer_delegators(&ctx, INDEXER, page(5000)).await.unwrap();
        assert_eq!(past.entry.value["total"], serde_json::Value::Null);
        assert_eq!(past.entry.value["delegators"], serde_json::json!([]));

        let none = indexer_delegators(&ctx, INDEXER, page(0)).await.unwrap();
        assert_eq!(none.entry.value["total"], 0);
        assert_eq!(none.entry.value["pool"], serde_json::Value::Null);
    }
}
