//! The refresh job: everything the indexer directory shows, recomputed and written to Postgres.
//!
//! This is the widest of the jobs. It folds six nest reads plus, when a chain is configured, an
//! oracle call per indexer, into one row per indexer carrying stake, capacity, cuts, returns and
//! the composite score. The directory then reads rows rather than recomputing on every page view,
//! which is why the page is fast and why this job is worth its permits.

use std::collections::HashMap;

use kittiwake_chain::Chain;
use kittiwake_db::Pool;
use kittiwake_nest::{Lane, NestGate};
use kittiwake_read::sql;
use kittiwake_rewards as rw;
use kittiwake_score as sc;
use kittiwake_types::NestId;

use crate::{IngestError, Ingested, i, page, s};

use kittiwake_rewards::L1_BLOCKS_PER_YEAR;

/// How many indexers we ask the oracle about at once. The oracle is five calls per indexer against
/// one RPC endpoint, so this is politeness rather than throughput.
const REO_CONCURRENCY: usize = 8;

const WEI: f64 = 1e18;

fn grt(v: Option<String>) -> f64 {
    v.and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / WEI
}

pub async fn refresh(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
    chain: Option<&Chain>,
    now: i64,
) -> Result<Ingested, IngestError> {
    let week_ago = now - 7 * 86_400;
    let month_ago = now - 30 * 86_400;

    // Six reads. Each is coalesced on its own statement by the gate, and all of them draw from the
    // background lane, so a page view never waits behind this job.
    let network = page(
        gate,
        NestId::Alloc,
        sql::network_sql().into(),
        lane,
        "network",
    )
    .await?;
    let params = page(
        gate,
        NestId::Alloc,
        sql::network_params_sql().into(),
        lane,
        "params",
    )
    .await?;
    let indexers = page(
        gate,
        NestId::Alloc,
        sql::INDEXERS_ALL_SQL.into(),
        lane,
        "indexers",
    )
    .await?;
    let allocations = page(
        gate,
        NestId::Alloc,
        sql::ACTIVE_ALLOCATIONS_ALL_SQL.into(),
        lane,
        "allocations",
    )
    .await?;
    let flows = page(
        gate,
        NestId::Alloc,
        sql::delegation_events_since_sql(week_ago),
        lane,
        "delegation flow",
    )
    .await?;
    let closed = page(
        gate,
        NestId::Alloc,
        sql::closed_allocations_since_sql(month_ago),
        lane,
        "closed allocations",
    )
    .await?;
    let services = page(
        gate,
        NestId::Alloc,
        sql::DATA_SERVICE_COUNTS_SQL.into(),
        lane,
        "data services",
    )
    .await?;

    let net = network
        .rows
        .first()
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let par = params
        .rows
        .first()
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let total_signal = grt(s(&net, "total_tokens_signalled"));
    let issuance_per_block = grt(s(&net, "issuance_per_block"));
    let annual_issuance = issuance_per_block * L1_BLOCKS_PER_YEAR;
    let delegation_ratio = i(&par, "delegation_ratio").unwrap_or(16) as f64;
    // The protocol payment cut is a network parameter and not part of this fold: the APR is built
    // from each indexer's own reward cut, which is what a delegator to that indexer feels.

    // Group the wide reads by indexer once, rather than scanning them per indexer.
    let mut allocs_by_indexer: HashMap<String, Vec<rw::Allocation>> = HashMap::new();
    for a in &allocations.rows {
        let Some(id) = s(a, "indexer") else { continue };
        allocs_by_indexer
            .entry(id.to_lowercase())
            .or_default()
            .push(rw::Allocation {
                allocated_grt: grt(s(a, "allocated_tokens")),
                deployment_signal_grt: grt(s(a, "signalled_tokens")),
                deployment_stake_grt: grt(s(a, "deployment_staked_tokens")),
            });
    }

    let mut flow_by_indexer: HashMap<String, (i64, i64, f64)> = HashMap::new();
    for f in &flows.rows {
        let Some(id) = s(f, "indexer") else { continue };
        let tokens = grt(s(f, "tokens"));
        let entry = flow_by_indexer
            .entry(id.to_lowercase())
            .or_insert((0, 0, 0.0));
        match s(f, "event_type").as_deref() {
            Some("delegation") => {
                entry.0 += 1;
                entry.2 += tokens;
            }
            Some("undelegation") => {
                entry.1 += 1;
                entry.2 -= tokens;
            }
            _ => {}
        }
    }

    let mut closed_by_indexer: HashMap<String, Vec<(f64, i64)>> = HashMap::new();
    for c in &closed.rows {
        let Some(id) = s(c, "indexer") else { continue };
        closed_by_indexer
            .entry(id.to_lowercase())
            .or_default()
            .push((
                grt(s(c, "indexing_delegator_rewards")),
                i(c, "closed_at").unwrap_or(0),
            ));
    }

    let services_by_indexer: HashMap<String, i64> = services
        .rows
        .iter()
        .filter_map(|r| Some((s(r, "indexer")?.to_lowercase(), i(r, "n").unwrap_or(0))))
        .collect();

    // The oracle, when a chain is configured. Without one, every indexer scores the neutral 50 on
    // the largest-weighted dimension, which is honest: we did not check.
    let ids: Vec<String> = indexers.rows.iter().filter_map(|r| s(r, "id")).collect();
    let (eligibility, oracle) = match chain {
        Some(c) => {
            let ctx = c.oracle_context().await.ok();
            if ctx.is_some_and(|c| c.stale) {
                // Worth a line every run: the largest-weighted dimension of every score is being
                // computed from a verdict older than the period it covers.
                tracing::warn!(
                    "the eligibility oracle has not run within one period; REO scores are stale"
                );
            }
            (read_eligibility(c, &ids).await, ctx)
        }
        None => (HashMap::new(), None),
    };
    let _ = oracle;

    let mut rows = Vec::with_capacity(indexers.rows.len());
    for r in &indexers.rows {
        let Some(id) = s(r, "id").map(|v| v.to_lowercase()) else {
            continue;
        };

        let self_stake = grt(s(r, "staked_tokens"));
        let delegated = grt(s(r, "delegated_tokens"));
        let thawing = grt(s(r, "delegated_thawing_tokens"));
        let allocated = grt(s(r, "allocated_tokens"));
        let provisioned = grt(s(r, "provisioned_tokens"));
        let shares = grt(s(r, "delegator_shares"));
        let reward_cut_ppm = i(r, "indexing_reward_cut").unwrap_or(0) as f64;
        let query_fee_cut_ppm = i(r, "query_fee_cut").unwrap_or(0) as f64;

        let capacity = rw::delegation_capacity(self_stake, delegated, delegation_ratio);

        // The delegators' share of the pool, which is what turns a raw cut into the cut they
        // actually feel.
        let total_stake = self_stake + delegated;
        let delegated_ratio = if total_stake > 0.0 {
            Some(delegated / total_stake)
        } else {
            None
        };

        let empty: Vec<rw::Allocation> = Vec::new();
        let allocs = allocs_by_indexer.get(&id).unwrap_or(&empty);
        let apr = rw::delegator_apr_breakdown(
            allocs,
            reward_cut_ppm,
            delegated,
            total_signal,
            annual_issuance,
            None,
            delegated_ratio,
        );

        let rolling = closed_by_indexer
            .get(&id)
            .map(|c| rw::rolling_apy(c, delegated, 30.0, now))
            .filter(|v| *v > 0.0);

        let (delegations_7d, undelegations_7d, net_flow) =
            flow_by_indexer.get(&id).copied().unwrap_or((0, 0, 0.0));

        let reo = match eligibility.get(&id) {
            Some(Some(true)) => sc::Reo::Eligible,
            Some(Some(false)) => sc::Reo::Ineligible,
            _ => sc::Reo::Unknown,
        };

        let score = sc::calculate(&sc::Input {
            reo,
            self_stake_grt: self_stake,
            last_delegation_parameter_update: i(r, "last_delegation_parameter_update").unwrap_or(0),
            delegator_parameter_cooldown: 0,
            allocation_count: i(r, "allocation_count").unwrap_or(0),
            allocated_grt: allocated,
            provisioned_grt: if provisioned > 0.0 {
                Some(provisioned)
            } else {
                None
            },
            delegation_utilization: capacity.utilization_percent,
            has_ens: false,
            has_url: s(r, "url").is_some_and(|u| !u.is_empty()),
            has_display_name: false,
            reward_cut_ppm,
            query_fee_cut_ppm,
            effective_cut_percent: Some(apr.effective_cut * 100.0),
            query_fees_collected_grt: grt(s(r, "query_fees_collected")),
            served_gap: None,
            net_flow_grt_7d: net_flow,
            delegated_grt: delegated,
            rolling_apy_30d: rolling,
            delegator_apr: apr.apr,
            distinct_data_services: services_by_indexer.get(&id).copied().unwrap_or(0),
            now,
        });

        rows.push(Row {
            id: id.clone(),
            url: s(r, "url"),
            geohash: s(r, "geohash"),
            self_stake,
            delegated,
            allocated,
            provisioned,
            reward_cut: reward_cut_ppm / 10_000.0,
            query_fee_cut: query_fee_cut_ppm / 10_000.0,
            effective_cut: apr.effective_cut * 100.0,
            delegator_apr: rolling.unwrap_or(apr.apr),
            capacity_pct: capacity.utilization_percent,
            last_param_update: i(r, "last_delegation_parameter_update").unwrap_or(0),
            score: score.composite,
            grade: score.grade.as_str().to_string(),
            delegations_7d,
            undelegations_7d,
            net_flow,
            rewards_earned: grt(s(r, "rewards_earned")),
            query_fees: grt(s(r, "query_fees_collected")),
            allocation_count: i(r, "allocation_count").unwrap_or(0),
            exchange_rate: rw::pool_exchange_rate(delegated, thawing, shares),
            data_services: services_by_indexer.get(&id).copied().unwrap_or(0),
            reo_status: match reo {
                sc::Reo::Eligible => "eligible",
                sc::Reo::Ineligible => "ineligible",
                sc::Reo::Unknown => "unknown",
            },
        });
    }

    write_rows(pool, &rows).await?;
    crate::state::touch(pool, "indexers").await?;
    Ok(Ingested {
        rows: rows.len() as u64,
    })
}

/// Ask the oracle about every indexer, a few at a time.
///
/// A failed read is `None` rather than `Some(false)`: "we could not check" and "the protocol says
/// no" are different claims, and only one of them is about the indexer.
async fn read_eligibility(chain: &Chain, ids: &[String]) -> HashMap<String, Option<bool>> {
    use futures::StreamExt;
    // One call per indexer. The oracle's network-wide state is read separately, once, because
    // three of its five views take no arguments and reading them per indexer would triple the cost
    // of a whole-directory pass for no extra information.
    futures::stream::iter(ids.iter().cloned())
        .map(|id| async move {
            let verdict = chain.is_eligible(&id).await.ok();
            (id.to_lowercase(), verdict)
        })
        .buffer_unordered(REO_CONCURRENCY)
        .collect()
        .await
}

struct Row {
    id: String,
    url: Option<String>,
    geohash: Option<String>,
    self_stake: f64,
    delegated: f64,
    allocated: f64,
    provisioned: f64,
    reward_cut: f64,
    query_fee_cut: f64,
    effective_cut: f64,
    delegator_apr: f64,
    capacity_pct: f64,
    last_param_update: i64,
    score: i32,
    grade: String,
    delegations_7d: i64,
    undelegations_7d: i64,
    net_flow: f64,
    rewards_earned: f64,
    query_fees: f64,
    allocation_count: i64,
    exchange_rate: f64,
    data_services: i64,
    reo_status: &'static str,
}

async fn write_rows(pool: &Pool, rows: &[Row]) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let col = |f: fn(&Row) -> f64| rows.iter().map(f).collect::<Vec<f64>>();
    let icol = |f: fn(&Row) -> i64| rows.iter().map(|r| f(r) as i32).collect::<Vec<i32>>();

    sqlx::query(
        "INSERT INTO indexers (address, url, geo_hash, self_stake_grt, delegated_grt, allocated_grt, \
         provisioned_grt, reward_cut, query_fee_cut, effective_cut, delegator_apr, \
         delegation_capacity_pct, last_delegation_param_update, score, score_grade, \
         delegations_in_7d, undelegations_in_7d, net_flow_grt_7d, rewards_earned_grt, \
         query_fees_collected_grt, allocation_count, delegation_exchange_rate, \
         distinct_data_services, reo_status, last_updated) \
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::numeric[], $5::numeric[], \
         $6::numeric[], $7::numeric[], $8::numeric[], $9::numeric[], $10::numeric[], $11::numeric[], \
         $12::numeric[], $13::int[], $14::numeric[], $15::text[], $16::int[], $17::int[], \
         $18::numeric[], $19::numeric[], $20::numeric[], $21::int[], $22::numeric[], $23::int[], \
         $24::text[]) AS t, LATERAL (SELECT NOW()) AS n(now) \
         ON CONFLICT (address) DO UPDATE SET \
         url = EXCLUDED.url, geo_hash = EXCLUDED.geo_hash, self_stake_grt = EXCLUDED.self_stake_grt, \
         delegated_grt = EXCLUDED.delegated_grt, allocated_grt = EXCLUDED.allocated_grt, \
         provisioned_grt = EXCLUDED.provisioned_grt, reward_cut = EXCLUDED.reward_cut, \
         query_fee_cut = EXCLUDED.query_fee_cut, effective_cut = EXCLUDED.effective_cut, \
         delegator_apr = EXCLUDED.delegator_apr, \
         delegation_capacity_pct = EXCLUDED.delegation_capacity_pct, \
         last_delegation_param_update = EXCLUDED.last_delegation_param_update, \
         score = EXCLUDED.score, score_grade = EXCLUDED.score_grade, \
         delegations_in_7d = EXCLUDED.delegations_in_7d, \
         undelegations_in_7d = EXCLUDED.undelegations_in_7d, \
         net_flow_grt_7d = EXCLUDED.net_flow_grt_7d, rewards_earned_grt = EXCLUDED.rewards_earned_grt, \
         query_fees_collected_grt = EXCLUDED.query_fees_collected_grt, \
         allocation_count = EXCLUDED.allocation_count, \
         delegation_exchange_rate = EXCLUDED.delegation_exchange_rate, \
         distinct_data_services = EXCLUDED.distinct_data_services, \
         reo_status = EXCLUDED.reo_status, last_updated = NOW()",
    )
    .bind(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.url.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.geohash.clone()).collect::<Vec<_>>())
    .bind(col(|r| r.self_stake))
    .bind(col(|r| r.delegated))
    .bind(col(|r| r.allocated))
    .bind(col(|r| r.provisioned))
    .bind(col(|r| r.reward_cut))
    .bind(col(|r| r.query_fee_cut))
    .bind(col(|r| r.effective_cut))
    .bind(col(|r| r.delegator_apr))
    .bind(col(|r| r.capacity_pct))
    .bind(icol(|r| r.last_param_update))
    .bind(rows.iter().map(|r| r.score as f64).collect::<Vec<f64>>())
    .bind(rows.iter().map(|r| r.grade.clone()).collect::<Vec<_>>())
    .bind(icol(|r| r.delegations_7d))
    .bind(icol(|r| r.undelegations_7d))
    .bind(col(|r| r.net_flow))
    .bind(col(|r| r.rewards_earned))
    .bind(col(|r| r.query_fees))
    .bind(icol(|r| r.allocation_count))
    .bind(col(|r| r.exchange_rate))
    .bind(icol(|r| r.data_services))
    .bind(rows.iter().map(|r| r.reo_status.to_string()).collect::<Vec<_>>())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The issuance constant is load-bearing and differs from the one used for the displayed
    /// percentage. Pinned here so the discrepancy cannot be closed by accident in one place only.
    #[test]
    fn the_issuance_year_is_the_one_the_scores_have_always_used() {
        // Unified with the figure the displayed issuance percentage uses; see
        // `kittiwake_rewards::L1_BLOCKS_PER_YEAR` for why this moved and why no ranking did.
        assert_eq!(super::L1_BLOCKS_PER_YEAR, 2_610_223.0);
    }
}
