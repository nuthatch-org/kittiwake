//! Every nest-backed read the dashboard makes.
//!
//! Each route is a description of which queries to run against which nest and how fresh the answer
//! has to be. The thinking - readiness, coalescing, staleness labelling - lives once in
//! [`crate::ReadCtx::cached`], because it is a property of reading a nest rather than of any
//! particular panel.

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use kittiwake_cache::CachePolicy;
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_types::{ApiError, NestId, Provenance};

use crate::{Fetcher, Read, ReadCtx, sql};

/// One query inside a composed answer.
pub struct Part {
    pub name: &'static str,
    pub nest: NestId,
    pub sql: String,
    /// Fold to a single row rather than an array. For the one-row views.
    pub one: bool,
}

impl Part {
    pub fn rows(name: &'static str, nest: NestId, sql: String) -> Self {
        Self {
            name,
            nest,
            sql,
            one: false,
        }
    }
    pub fn one(name: &'static str, nest: NestId, sql: String) -> Self {
        Self {
            name,
            nest,
            sql,
            one: true,
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
    }
}

/// Run several queries and return one object keyed by part name.
///
/// Each part is coalesced on its own statement, so two routes that happen to want the same fold
/// cost the nest one query between them rather than one each. They run concurrently; the gate is
/// what bounds how many actually reach the nest at once.
pub fn many(parts: Vec<Part>, timeout: Duration) -> Fetcher {
    let parts = Arc::new(parts);
    Arc::new(move |gate: NestGate, lane: Lane| {
        let parts = parts.clone();
        Box::pin(async move {
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

            let answers = join_all(futures).await;
            let mut out = serde_json::Map::new();
            let mut provenance: Option<Provenance> = None;

            for (part, answer) in parts.iter().zip(answers) {
                let answer = answer.map_err(ApiError::from)?;
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

            Ok((
                serde_json::Value::Object(out),
                provenance.unwrap_or_default(),
            ))
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
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

pub async fn curators(ctx: &ReadCtx, first: i64, skip: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "curators",
        NestId::Alloc,
        serde_json::json!({"first": first, "skip": skip}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_only(NestId::Alloc, sql::curators_sql(first, skip), T_FOLD),
    )
    .await
}

pub async fn epochs(ctx: &ReadCtx, first: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "epochs",
        NestId::Alloc,
        serde_json::json!({"first": first}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        rows_only(NestId::Alloc, sql::epochs_sql(first), T_QUICK),
    )
    .await
}

/// `/api/network-stats` and, with the same body, `/api/grt-flow`: the totals and the parameters.
pub async fn network_stats(ctx: &ReadCtx) -> Result<Read, ApiError> {
    ctx.cached(
        "network-stats",
        NestId::Alloc,
        serde_json::json!({}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(
            vec![
                Part::one("network", NestId::Alloc, sql::network_sql().to_string()),
                Part::one(
                    "params",
                    NestId::Alloc,
                    sql::network_params_sql().to_string(),
                ),
            ],
            T_FOLD,
        ),
    )
    .await
}

/// `/api/token-metrics`: the epoch series, from which issuance and rewards are derived.
pub async fn token_metrics(ctx: &ReadCtx, first: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "token-metrics",
        NestId::Alloc,
        serde_json::json!({"first": first}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        many(
            vec![
                Part::rows("epochs", NestId::Alloc, sql::epochs_sql(first)),
                Part::one("network", NestId::Alloc, sql::network_sql().to_string()),
            ],
            T_QUICK,
        ),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// One indexer
// ---------------------------------------------------------------------------------------------

/// `/api/indexer/[address]`. Seven folds behind one answer, measured at 17.9 seconds cold. Each
/// part coalesces separately, so a concurrent `/api/indexer-status` for the same address pays for
/// none of the folds they share.
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
        ),
        Part::rows(
            "delegators",
            NestId::Alloc,
            addr_or_400!(sql::indexer_delegators_sql(addr, 100), "address"),
        ),
        Part::rows(
            "activeAllocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_active_allocations_sql(addr), "address"),
        ),
        Part::rows(
            "closedAllocations",
            NestId::Alloc,
            addr_or_400!(sql::indexer_closed_allocations_sql(addr, 500), "address"),
        ),
        Part::rows(
            "provisions",
            NestId::Alloc,
            addr_or_400!(sql::provisions_by_indexer_sql(addr), "address"),
        ),
        Part::one(
            "networkParams",
            NestId::Alloc,
            sql::DELEGATION_RATIO_SQL.to_string(),
        ),
    ];
    ctx.cached(
        "indexer",
        NestId::Alloc,
        serde_json::json!({"address": addr}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        many(parts, T_FOLD),
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
        CachePolicy::from_poll_interval(cadence::LIVE),
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

/// `/api/indexer-stake-history/[address]`: one statement for the whole series.
pub async fn indexer_stake_history(
    ctx: &ReadCtx,
    addr: &str,
    cutoffs: &[i64],
) -> Result<Read, ApiError> {
    let sql = addr_or_400!(
        sql::indexer_stake_history_sql(addr, cutoffs),
        "address or window"
    );
    ctx.cached(
        "indexer-stake-history",
        NestId::Alloc,
        serde_json::json!({"address": addr, "points": cutoffs.len(), "from": cutoffs.first(), "to": cutoffs.last()}),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        rows_only(NestId::Alloc, sql, T_FOLD),
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

pub async fn provisions_by_indexer(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::provisions_by_indexer_sql(addr), "address");
    ctx.cached(
        "provisions-indexer",
        NestId::Alloc,
        serde_json::json!({"indexer": addr}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_only(NestId::Alloc, sql, T_FOLD),
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
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

/// `/api/payments`: escrow balances, the transaction tail, and tally collections. Three folds.
pub async fn payments(ctx: &ReadCtx, receiver: Option<&str>) -> Result<Read, ApiError> {
    let parts = vec![
        Part::rows(
            "accounts",
            NestId::Alloc,
            addr_or_400!(sql::escrow_accounts_sql(receiver, 100), "address"),
        ),
        Part::rows(
            "transactions",
            NestId::Alloc,
            addr_or_400!(
                sql::escrow_transactions_sql(receiver, if receiver.is_some() { 100 } else { 50 }),
                "address"
            ),
        ),
        Part::rows(
            "tally",
            NestId::Alloc,
            addr_or_400!(sql::tally_collected_sql(receiver, 50), "address"),
        ),
    ];
    ctx.cached(
        "payments",
        NestId::Alloc,
        serde_json::json!({"receiver": receiver}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        many(parts, T_FOLD),
    )
    .await
}

pub async fn poi(ctx: &ReadCtx, deployment: Option<&str>, first: i64) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::poi_allocations_sql(deployment, first), "deployment id");
    ctx.cached(
        "poi",
        NestId::Alloc,
        serde_json::json!({"deployment": deployment, "first": first}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Delegation activity
// ---------------------------------------------------------------------------------------------

pub async fn delegation_events(
    ctx: &ReadCtx,
    indexer: Option<&str>,
    first: i64,
    since: i64,
) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::delegation_events_sql(indexer, first, since), "address");
    ctx.cached(
        "delegation-events",
        NestId::Alloc,
        serde_json::json!({"indexer": indexer, "first": first, "since": since}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

/// `/api/delegation-flows`: the Horizon era from the live nest and the pre-Horizon era from the
/// frozen archive. Two nests, one chart. The archive half is exempt from the readiness check by
/// construction, since it reports stalled for ever and is right to.
pub async fn delegation_flows(ctx: &ReadCtx, since: i64) -> Result<Read, ApiError> {
    let parts = vec![
        Part::rows(
            "live",
            NestId::Alloc,
            sql::delegation_events_since_sql(since),
        ),
        Part::rows(
            "historical",
            NestId::LegacyFlows,
            sql::delegation_events_since_sql(since),
        ),
    ];
    ctx.cached(
        "delegation-flows",
        NestId::Alloc,
        serde_json::json!({"since": since}),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        many(parts, T_FOLD),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Subgraphs: the GNS nest and the deployment views
// ---------------------------------------------------------------------------------------------

pub async fn developer_activity(ctx: &ReadCtx, cutoff: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "developer-activity",
        NestId::Gns,
        serde_json::json!({"cutoff": cutoff}),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        rows_only(
            NestId::Gns,
            sql::subgraphs_published_since_sql(cutoff),
            T_FOLD,
        ),
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
        rows_only(NestId::Alloc, sql, T_FOLD),
    )
    .await
}

pub async fn subgraph_fees_30d(ctx: &ReadCtx, since: i64) -> Result<Read, ApiError> {
    ctx.cached(
        "subgraph-fees-30d",
        NestId::Alloc,
        serde_json::json!({"since": since}),
        CachePolicy::from_poll_interval(cadence::ARCHIVAL),
        rows_only(
            NestId::Alloc,
            sql::deployment_fees_since_sql(since, 200),
            T_FOLD,
        ),
    )
    .await
}

pub async fn subgraph_curation(ctx: &ReadCtx, dep: &str) -> Result<Read, ApiError> {
    let sql = addr_or_400!(sql::deployment_signals_sql(dep, 100), "deployment id");
    ctx.cached(
        "subgraph-curation",
        NestId::Alloc,
        serde_json::json!({"deployment": dep}),
        CachePolicy::from_poll_interval(cadence::LIVE),
        rows_only(NestId::Alloc, sql, T_FOLD),
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
        many(parts, T_FOLD),
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

/// `/api/dips`. Four folds against the DIPS nest, which admits two concurrent queries; the gate is
/// what stops that being a problem. The Node version fired all four at once and survived only
/// because the client happened to stagger them, which was not a property anyone chose.
pub async fn dips(ctx: &ReadCtx, sepolia: bool) -> Result<Read, ApiError> {
    let nest = if sepolia {
        NestId::DipsSepolia
    } else {
        NestId::Dips
    };
    let parts = vec![
        Part::rows(
            "allocations",
            nest,
            sql::DIPS_CURRENT_ALLOCATION_SQL.to_string(),
        ),
        Part::rows("timeline", nest, sql::DIPS_TIMELINE_SQL.to_string()),
    ];
    ctx.cached(
        "dips",
        nest,
        serde_json::json!({"sepolia": sepolia}),
        CachePolicy::from_poll_interval(cadence::SLOW),
        many(parts, T_FOLD),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
