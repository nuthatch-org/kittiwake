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
    // Ninety days, not thirty. `rolling_apy` filters to its own window, so the wider fetch still
    // gives a correct 30-day figure; fetching thirty and dividing by a ninety-day year understates
    // it threefold, which is worse than the dash it would replace.
    let month_ago = now - 30 * 86_400;
    let quarter_ago = now - 90 * 86_400;

    // Alloc-nest reads plus the dips issuance split. Each is coalesced on its own statement by
    // the gate, and all of them draw from the background lane, so a page view never waits behind
    // this job.
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
        sql::closed_allocations_since_sql(quarter_ago),
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
    // The pool's exchange rate as it stood a month and a quarter ago, summed from the ledger. This
    // is the primary APY source and the closed-allocation figure is the fallback, which is the
    // order the handler being replaced used.
    let rates_30 = page(
        gate,
        NestId::Alloc,
        sql::exchange_rates_as_of_sql(month_ago),
        lane,
        "exchange rates 30d",
    )
    .await?;
    let rates_90 = page(
        gate,
        NestId::Alloc,
        sql::exchange_rates_as_of_sql(quarter_ago),
        lane,
        "exchange rates 90d",
    )
    .await?;
    // Not `?`: a dips outage must not fail the directory. Instant APR then falls back to
    // protocol-total issuance, which is 1.25× high since GIP-0089.
    let dips_allocs = page(
        gate,
        NestId::Dips,
        sql::DIPS_CURRENT_ALLOCATION_SQL.into(),
        lane,
        "dips allocation",
    )
    .await;

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
    let protocol_issuance_per_block = grt(s(&net, "issuance_per_block"));
    let indexing_per_block = match dips_allocs {
        Ok(answer) => {
            let rate = kittiwake_read::shape::indexing_rate_from_rows(&answer.rows);
            if rate > 0.0 {
                rate
            } else {
                tracing::warn!(
                    "dips has no RewardsManager rate; Instant APR is using total protocol issuance"
                );
                protocol_issuance_per_block
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "dips nest unavailable; Instant APR is using total protocol issuance"
            );
            protocol_issuance_per_block
        }
    };
    let annual_issuance = indexing_per_block * L1_BLOCKS_PER_YEAR;
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
    // The ledger's pool figure already excludes thawing, so the thawing argument is zero here and
    // not in the current-rate call below.
    let rates_at = |rows: &[serde_json::Value]| -> HashMap<String, f64> {
        rows.iter()
            .filter_map(|r| {
                let id = s(r, "indexer")?.to_lowercase();
                let rate =
                    rw::pool_exchange_rate(grt(s(r, "pool_tokens")), 0.0, grt(s(r, "pool_shares")));
                (rate > 0.0).then_some((id, rate))
            })
            .collect()
    };
    let rate_30 = rates_at(&rates_30.rows);
    let rate_90 = rates_at(&rates_90.rows);

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
    let names = known_names(pool).await;

    let mut rows = Vec::with_capacity(indexers.rows.len());
    let mut apys: Vec<(String, Option<f64>, Option<f64>)> = Vec::with_capacity(indexers.rows.len());
    let mut breakdowns: Vec<(String, serde_json::Value)> = Vec::with_capacity(indexers.rows.len());
    for r in &indexers.rows {
        let Some(id) = s(r, "id").map(|v| v.to_lowercase()) else {
            continue;
        };

        let self_stake = grt(s(r, "staked_tokens"));
        let locked = grt(s(r, "locked_tokens"));
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

        // With nobody usably delegated there is no cut to feel, and the raw one stands.
        let effective_cut = rw::effective_cut(
            reward_cut_ppm,
            self_stake - locked,
            delegated,
            delegation_ratio,
        )
        .unwrap_or(reward_cut_ppm / 1_000_000.0);

        let empty: Vec<rw::Allocation> = Vec::new();
        let allocs = allocs_by_indexer.get(&id).unwrap_or(&empty);
        let apr = instant_apr(
            allocs,
            reward_cut_ppm,
            delegated,
            thawing,
            total_signal,
            annual_issuance,
            delegated_ratio,
        );

        // Growth of the pool's exchange rate first, closed allocations as the fallback. That is
        // the order the handler being replaced used, and the two measure different things: the
        // first is what a share actually gained, the second what was paid out.
        let current_rate = rw::pool_exchange_rate(delegated, thawing, shares);
        let apy = |window: f64, historical: &HashMap<String, f64>| -> Option<f64> {
            historical
                .get(&id)
                .map(|h| rw::exchange_rate_apy(current_rate, *h, window))
                .filter(|v| *v > 0.0)
                .or_else(|| {
                    closed_by_indexer
                        .get(&id)
                        .map(|c| rw::rolling_apy(c, delegated - thawing, window, now))
                        .filter(|v| *v > 0.0)
                })
        };
        let rolling = apy(30.0, &rate_30);
        let rolling_90 = apy(90.0, &rate_90);
        apys.push((id.clone(), rolling, rolling_90));

        let (delegations_7d, undelegations_7d, net_flow) =
            flow_by_indexer.get(&id).copied().unwrap_or((0, 0, 0.0));

        let reo = match eligibility.get(&id) {
            Some(Some(true)) => sc::Reo::Eligible,
            Some(Some(false)) => sc::Reo::Ineligible,
            _ => sc::Reo::Unknown,
        };
        let (has_ens, has_display_name) = transparency(names.get(&id));

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
            has_ens,
            has_url: s(r, "url").is_some_and(|u| !u.is_empty()),
            has_display_name,
            reward_cut_ppm,
            query_fee_cut_ppm,
            effective_cut_percent: Some(effective_cut * 100.0),
            query_fees_collected_grt: grt(s(r, "query_fees_collected")),
            served_gap: None,
            net_flow_grt_7d: net_flow,
            delegated_grt: delegated,
            rolling_apy_30d: rolling,
            delegator_apr: apr.apr,
            distinct_data_services: services_by_indexer.get(&id).copied().unwrap_or(0),
            now,
        });
        if let Ok(b) = serde_json::to_value(score.breakdown) {
            breakdowns.push((id.clone(), b));
        }

        rows.push(Row {
            id: id.clone(),
            url: s(r, "url"),
            geohash: s(r, "geohash"),
            self_stake,
            delegated,
            allocated,
            provisioned,
            // PPM, which is what the protocol uses, what `parameter_changes` holds across 4,136
            // rows, and what the platform wrote here before the cutover. The directory read
            // divides by ten thousand so the wire is unchanged. #47.
            reward_cut: reward_cut_ppm,
            query_fee_cut: query_fee_cut_ppm,
            effective_cut: effective_cut * 100.0,
            // Forward-looking, as every page that shows it says. The trailing APYs have their own
            // columns; putting the 30-day one here gave ellipfra a 39% "APR" against 26.9%.
            delegator_apr: apr.apr,
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
            exchange_rate: current_rate,
            data_services: services_by_indexer.get(&id).copied().unwrap_or(0),
            reo_status: match reo {
                sc::Reo::Eligible => "eligible",
                sc::Reo::Ineligible => "ineligible",
                sc::Reo::Unknown => "unknown",
            },
        });
    }

    // The parameter trail, recorded before the upsert overwrites the row it is compared against.
    //
    // The order is deliberate. Detect, insert the history, then upsert. If the upsert then fails,
    // the next run reads the same previous row, re-detects the same change and writes it twice -
    // and `provenance_rows`' `DISTINCT ON (param_name, old_value, new_value, epoch)` collapses the
    // duplicate on the way out. The other order loses the change for ever: once the upsert has
    // landed there is nothing left to compare against, and a cut that moved is simply never
    // recorded. A duplicate the reader already dedupes beats a silence nothing can recover.
    let epoch = i(&net, "current_epoch").map(|e| e as i32);
    let previous = previous_cuts(pool).await?;
    let changes = detect_changes(&previous, &rows, epoch);
    record_parameter_changes(pool, &changes).await;

    write_rows(pool, &rows).await?;
    kittiwake_db::write_rolling_apy(pool, &apys).await?;
    kittiwake_db::write_score_breakdown(pool, &breakdowns).await?;
    crate::state::touch(pool, "indexers").await?;
    Ok(Ingested {
        rows: rows.len() as u64,
    })
}

// ---------------------------------------------------------------------------------------------
// The parameter trail
// ---------------------------------------------------------------------------------------------
//
// `parameter_changes` is what `/api/apr-provenance` shows when it explains why an indexer's APR
// moved. The platform wrote it from `src/lib/ingest/indexers.ts`; `refresh` was ported and the
// write was not carried across, so the table had no writer between the cutover and #47. A frozen
// history and a live one look identical from outside - the route answers 200 with a populated
// array either way - which is why it went unnoticed for two days.

/// One recorded movement in an indexer's cuts. Values are PPM, which is what the protocol uses and
/// what the table has held since 2023 - including through the few hours in which the column it is
/// compared against did not. See #48.
struct ParamChange {
    indexer: String,
    param: &'static str,
    old: f64,
    new: f64,
    epoch: Option<i32>,
}

/// Empty until `ingest-names` has run once, so a name the platform left behind is never scored.
const KNOWN_NAMES_SQL: &str = "SELECT address, name, ens_name FROM indexers \
     WHERE EXISTS (SELECT 1 FROM ingestion_state WHERE key = 'kittiwake:names')";

/// Thawing tokens stay in the pool's `tokens` until withdrawn but earn nothing, so the APR is
/// over the rest: the base docs/apr-methodology.md and the browser's provenance panel use. #223
fn instant_apr(
    allocs: &[rw::Allocation],
    reward_cut_ppm: f64,
    delegated: f64,
    thawing: f64,
    total_signal: f64,
    annual_issuance: f64,
    delegated_ratio: Option<f64>,
) -> rw::AprBreakdown {
    rw::delegator_apr_breakdown(
        allocs,
        reward_cut_ppm,
        delegated - thawing,
        total_signal,
        annual_issuance,
        None,
        delegated_ratio,
    )
}

/// `(name, ens_name)` as `ingest-names` last verified them. Read, not looked up: refresh runs every
/// five minutes and the lookups are mainnet round trips, so they live in the hourly job.
async fn known_names(pool: &Pool) -> HashMap<String, (Option<String>, Option<String>)> {
    match sqlx::query_as::<_, (String, Option<String>, Option<String>)>(KNOWN_NAMES_SQL)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(a, name, ens)| (a.to_lowercase(), (name, ens)))
            .collect(),
        Err(e) => {
            tracing::warn!("indexer names unreadable, scoring without them: {e}");
            HashMap::new()
        }
    }
}

/// `(has_ens, has_display_name)`. Both come from verified names only; an unknown indexer has neither.
fn transparency(names: Option<&(Option<String>, Option<String>)>) -> (bool, bool) {
    match names {
        Some((name, ens)) => (ens.is_some(), name.is_some()),
        None => (false, false),
    }
}

/// The cuts as they stood before this run.
///
/// Cast to `float8` in the statement rather than decoded as `NUMERIC`. The values are integral PPM
/// and an `f64` holds every one of them exactly, so the alternative is a decimal crate for a column
/// that has never had a fractional part. It is also the cast `provenance_rows` already uses to read
/// the same numbers back out.
async fn previous_cuts(
    pool: &Pool,
) -> Result<HashMap<String, (Option<f64>, Option<f64>)>, IngestError> {
    let rows: Vec<(String, Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT address, reward_cut::float8, query_fee_cut::float8 FROM indexers")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(a, reward, fee)| (a.to_lowercase(), (reward, fee)))
        .collect())
}

/// Which cuts moved since the last run.
///
/// **Compared as numbers, never as text.** The legacy carries a comment about exactly this: a
/// `NUMERIC` arrives from Postgres as a string, so a raw `!==` is true for `"880000"` against
/// `880000` and records a no-op change on every single refresh. Reading the column as `float8`
/// makes that mistake unavailable rather than merely avoided.
///
/// **An indexer we have not seen before records nothing.** First sight is not a change, and writing
/// one would date every indexer's parameters to whenever we happened to start looking. Likewise a
/// null previous value, which means "never recorded" rather than "was zero".
fn detect_changes(
    prev: &HashMap<String, (Option<f64>, Option<f64>)>,
    rows: &[Row],
    epoch: Option<i32>,
) -> Vec<ParamChange> {
    let mut out = Vec::new();
    for r in rows {
        let Some((prev_reward, prev_fee)) = prev.get(&r.id) else {
            continue;
        };
        for (param, before, after) in [
            ("reward_cut", *prev_reward, r.reward_cut),
            ("query_fee_cut", *prev_fee, r.query_fee_cut),
        ] {
            let Some(before) = before else { continue };
            if before != after {
                out.push(ParamChange {
                    indexer: r.id.clone(),
                    param,
                    old: before,
                    new: after,
                    epoch,
                });
            }
        }
    }
    out
}

/// Write the trail, and count it whether or not it worked.
///
/// A history table that will not take a row must not stop the directory being refreshed, so a
/// failure is logged rather than propagated - which is what the legacy did too. What the legacy did
/// not do is count it. Both counters are registered at zero on every run, because the failure this
/// whole issue describes is a writer that stopped while every log said the service was healthy, and
/// a counter that only exists once it fires makes silence look like health. Same lesson as #40.
async fn record_parameter_changes(pool: &Pool, changes: &[ParamChange]) {
    metrics::counter!("parameter_changes_written_total").increment(0);
    metrics::counter!("parameter_changes_failed_total").increment(0);

    match write_parameter_changes(pool, changes).await {
        Ok(()) => {
            metrics::counter!("parameter_changes_written_total").increment(changes.len() as u64);
            if !changes.is_empty() {
                tracing::info!(count = changes.len(), "recorded parameter changes");
            }
        }
        Err(e) => {
            metrics::counter!("parameter_changes_failed_total").increment(1);
            tracing::warn!(
                error = %e,
                count = changes.len(),
                "the parameter trail could not be written; the directory refresh continues without it"
            );
        }
    }
}

async fn write_parameter_changes(pool: &Pool, changes: &[ParamChange]) -> Result<(), sqlx::Error> {
    if changes.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO parameter_changes (indexer_address, param_name, old_value, new_value, epoch) \
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::float8[]::numeric[], $4::float8[]::numeric[], $5::int[])",
        // Declared `float8[]` and cast to `numeric[]` in the statement. The values are integral PPM
        // held exactly in an `f64`; binding them as numeric would want a decimal type for a column
        // that never has a fractional part, and the cast keeps the stored type unchanged.
        &changes.iter().map(|c| c.indexer.clone()).collect::<Vec<String>>(),
        &changes.iter().map(|c| c.param.to_string()).collect::<Vec<String>>(),
        &changes.iter().map(|c| c.old).collect::<Vec<f64>>() as &[f64],
        &changes.iter().map(|c| c.new).collect::<Vec<f64>>() as &[f64],
        &changes.iter().map(|c| c.epoch).collect::<Vec<Option<i32>>>() as &[Option<i32>]
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Ask the oracle about every indexer, batched into multicalls.
///
/// A failed read is `None` rather than `Some(false)`: "we could not check" and "the protocol says
/// no" are different claims, and only one of them is about the indexer.
async fn read_eligibility(chain: &Chain, ids: &[String]) -> HashMap<String, Option<bool>> {
    // The oracle's network-wide state is read separately, once, because three of its five views
    // take no arguments and reading them per indexer would add nothing.
    let verdicts = chain
        .eligibility_batch(ids)
        .await
        .unwrap_or_else(|_| vec![None; ids.len()]);
    ids.iter()
        .map(|id| id.to_lowercase())
        .zip(verdicts)
        .collect()
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

    // Deliberately not `query!`. `url` and `geo_hash` are `Vec<Option<String>>`: an indexer with no
    // URL is a normal row, and a null inside an array is a thing `text[]` cannot express to the
    // macro, which insists on `&[String]`. Filling the gaps with empty strings to satisfy a
    // compile-time check would write "" where the schema says NULL, and the directory would stop
    // being able to tell "no URL" from "an empty URL". #2.
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
    use super::*;

    #[test]
    fn names_are_not_scored_before_ingest_names_has_run() {
        assert!(KNOWN_NAMES_SQL.contains(&format!(
            "EXISTS (SELECT 1 FROM ingestion_state WHERE key = '{}')",
            kittiwake_db::NAMES_CURSOR_KEY
        )));
    }

    /// Both were hardcoded false, which cost every indexer up to 70 of 100 in Transparency.
    #[test]
    fn transparency_reads_the_verified_names() {
        let ens = (Some("pinax.eth".to_string()), Some("pinax.eth".to_string()));
        let default_only = (Some("grassets-tech-2".to_string()), None);
        assert_eq!(transparency(Some(&ens)), (true, true));
        assert_eq!(transparency(Some(&default_only)), (false, true));
        assert_eq!(transparency(Some(&(None, None))), (false, false));
        assert_eq!(transparency(None), (false, false));
    }

    /// A row carrying only the two fields the trail compares. Everything else is inert here.
    fn row(id: &str, reward_cut: f64, query_fee_cut: f64) -> Row {
        Row {
            id: id.to_string(),
            url: None,
            geohash: None,
            self_stake: 0.0,
            delegated: 0.0,
            allocated: 0.0,
            provisioned: 0.0,
            reward_cut,
            query_fee_cut,
            effective_cut: 0.0,
            delegator_apr: 0.0,
            capacity_pct: 0.0,
            last_param_update: 0,
            score: 0,
            grade: String::new(),
            delegations_7d: 0,
            undelegations_7d: 0,
            net_flow: 0.0,
            rewards_earned: 0.0,
            query_fees: 0.0,
            allocation_count: 0,
            exchange_rate: 0.0,
            data_services: 0,
            reo_status: "unknown",
        }
    }

    fn prev(
        pairs: &[(&str, Option<f64>, Option<f64>)],
    ) -> HashMap<String, (Option<f64>, Option<f64>)> {
        pairs
            .iter()
            .map(|(a, r, q)| (a.to_string(), (*r, *q)))
            .collect()
    }

    #[test]
    fn a_cut_that_moved_is_recorded_with_both_sides_and_the_epoch() {
        let changes = detect_changes(
            &prev(&[("0xaa", Some(880_000.0), Some(100_000.0))]),
            &[row("0xaa", 900_000.0, 100_000.0)],
            Some(1376),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].param, "reward_cut");
        assert_eq!(changes[0].old, 880_000.0);
        assert_eq!(changes[0].new, 900_000.0);
        assert_eq!(changes[0].epoch, Some(1376));
    }

    /// The bug the legacy carries a comment about, and which it had once in production: Postgres
    /// hands back a `NUMERIC` as a string, so `"880000" !== 880000` is true and a no-op change is
    /// recorded on every refresh. Reading the column as `float8` is what stops it, and this pins
    /// the outcome rather than the mechanism.
    #[test]
    fn an_unchanged_cut_records_nothing_however_many_times_it_is_refreshed() {
        let previous = prev(&[("0xaa", Some(880_000.0), Some(1_000_000.0))]);
        for _ in 0..3 {
            assert!(
                detect_changes(&previous, &[row("0xaa", 880_000.0, 1_000_000.0)], Some(1))
                    .is_empty()
            );
        }
    }

    /// The two cuts move independently and both are recorded when both do.
    #[test]
    fn both_cuts_moving_is_two_rows_and_not_one() {
        let changes = detect_changes(
            &prev(&[("0xaa", Some(0.0), Some(0.0))]),
            &[row("0xaa", 500_000.0, 250_000.0)],
            None,
        );
        let mut names: Vec<_> = changes.iter().map(|c| c.param).collect();
        names.sort_unstable();
        assert_eq!(names, ["query_fee_cut", "reward_cut"]);
        assert!(changes.iter().all(|c| c.epoch.is_none()));
    }

    /// First sight of an indexer is not a change. Recording one would date every indexer's
    /// parameters to whenever this service happened to start looking at them.
    #[test]
    fn an_indexer_with_no_previous_row_records_nothing() {
        assert!(detect_changes(&prev(&[]), &[row("0xnew", 100_000.0, 0.0)], Some(1)).is_empty());
    }

    /// A null previous value is "never recorded", not "was zero", so it is not a movement from
    /// zero to whatever the protocol says today.
    #[test]
    fn a_null_previous_value_is_not_a_change_from_zero() {
        let changes = detect_changes(
            &prev(&[("0xaa", None, Some(0.0))]),
            &[row("0xaa", 900_000.0, 250_000.0)],
            Some(1),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].param, "query_fee_cut");
    }

    /// #47's unit trap, pinned. Both sides are PPM after #48, so the boundary that produced 191
    /// phantom `x -> x * 10_000` rows on 2026-09-07 cannot be crossed again by this code: an
    /// indexer on a 100% cut compares 1,000,000 against 1,000,000 and records nothing. If the
    /// column ever reverts to percent, this test is what fails.
    #[test]
    fn a_hundred_percent_cut_is_a_million_on_both_sides_and_moves_nothing() {
        assert!(
            detect_changes(
                &prev(&[("0xaa", Some(1_000_000.0), Some(1_000_000.0))]),
                &[row("0xaa", 1_000_000.0, 1_000_000.0)],
                Some(1376),
            )
            .is_empty()
        );
    }

    /// The issuance constant is load-bearing and differs from the one used for the displayed
    /// percentage. Pinned here so the discrepancy cannot be closed by accident in one place only.
    #[test]
    fn the_issuance_year_is_the_one_the_scores_have_always_used() {
        // Unified with the figure the displayed issuance percentage uses; see
        // `kittiwake_rewards::L1_BLOCKS_PER_YEAR` for why this moved and why no ranking did.
        assert_eq!(super::L1_BLOCKS_PER_YEAR, 2_610_223.0);
    }

    /// ellipfra's pool on 2026-10-19: tokens 156,308,737, tokensThawing 4,929,832.
    #[test]
    fn instant_apr_divides_by_the_stake_that_is_not_thawing() {
        let allocs = [rw::Allocation {
            allocated_grt: 1e8,
            deployment_signal_grt: 1e4,
            deployment_stake_grt: 1e8,
        }];
        let (delegated, thawing) = (156_308_737.0, 4_929_832.0);
        let b = instant_apr(&allocs, 430_000.0, delegated, thawing, 1e6, 3e8, None);
        assert_eq!(b.active_base, delegated - thawing);
        assert!((b.apr - b.annual_delegator_rewards / (delegated - thawing) * 100.0).abs() < 1e-9);
    }
}
