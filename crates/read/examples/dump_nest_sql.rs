//! Every statement kittiwake sends a nest, printed with marker parameters, `;;` between them.
//!
//! The replay in nuthatch (`shadow_replay_over_a_nest`, `NUTHATCH_SHADOW_SQL`) reads this and puts
//! ids from the nest in place of the markers, so the shadow engine sees the dashboard's real
//! statements rather than `SELECT * FROM view`. Statements for the gns, dips and qos nests are
//! included; on another nest both engines refuse them alike, which is not a difference.
//!
//!     cargo run -q -p kittiwake-read --example dump_nest_sql > dashboard.sql

use kittiwake_read::sql::*;

const ADDR: &str = "0x00000000000000000000000000000000000000a1";
const DELEGATOR: &str = "0x00000000000000000000000000000000000000a2";
const CURATOR: &str = "0x00000000000000000000000000000000000000a3";
const RECEIVER: &str = "0x00000000000000000000000000000000000000a4";
const DEP: &str = "0x00000000000000000000000000000000000000000000000000000000000000d1";
const SUBGRAPH: &str = "1";
const SINCE: i64 = 1_720_000_000;
const CUTOFF: i64 = 1_758_000_000;

fn main() {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<String>| {
        if let Some(s) = s {
            out.push(s);
        }
    };
    push(Some(indexers_sql(50, 0, "stakedTokens", true)));
    push(Some(indexers_sql(50, 50, "allocationCount", false)));
    push(Some(network_sql().to_string()));
    push(Some(network_params_sql().to_string()));
    push(Some(epochs_sql(30)));
    push(delegation_events_sql(None, 100, SINCE));
    push(delegation_events_sql(Some(ADDR), 100, SINCE));
    push(Some(curators_sql(50, 0)));
    push(curator_sql(CURATOR));
    push(curator_signals_sql(CURATOR, 50));
    push(delegator_sql(DELEGATOR));
    push(delegator_stakes_sql(DELEGATOR, 50, true));
    push(delegator_stakes_sql(DELEGATOR, 50, false));
    push(indexer_detail_sql(ADDR));
    push(indexer_operators_sql(ADDR));
    push(indexer_delegators_sql(ADDR, 100));
    push(indexer_delegators_page_sql(
        ADDR,
        50,
        0,
        "stakedTokens",
        true,
    ));
    push(indexer_active_allocations_sql(ADDR));
    push(indexer_closed_allocations_sql(ADDR, 50));
    push(indexer_stake_history_sql(
        ADDR,
        &[CUTOFF - 86_400 * 30, CUTOFF - 86_400 * 7, CUTOFF],
    ));
    push(indexer_daily_sql(ADDR, SINCE));
    push(indexer_revenue_daily_sql(ADDR, SINCE));
    push(indexer_revenue_by_deployment_sql(ADDR, SINCE));
    push(Some(DELEGATION_RATIO_SQL.to_string()));
    push(indexer_allocation_shares_sql(ADDR));
    push(Some(ALLOCATION_SHARES_ALL_SQL.to_string()));
    push(provisions_by_indexer_sql(ADDR));
    push(provisions_by_service_sql(ADDR, 50, 0));
    push(data_service_totals_sql(&[
        ADDR.to_string(),
        RECEIVER.to_string(),
    ]));
    push(Some(MAX_THAWING_PERIOD_SQL.to_string()));
    push(provision_thaw_requests_sql(ADDR));
    push(indexer_horizon_activity_sql(ADDR, 50));
    push(Some(DATA_SERVICE_COUNTS_SQL.to_string()));
    push(poi_allocations_sql(None, 50));
    push(poi_allocations_sql(Some(DEP), 50));
    push(escrow_accounts_sql(None, 50));
    push(escrow_accounts_sql(Some(RECEIVER), 50));
    push(escrow_transactions_sql(None, 50));
    push(escrow_transactions_sql(Some(RECEIVER), 50));
    push(tally_collected_sql(None, 50));
    push(tally_collected_sql(Some(RECEIVER), 50));
    push(tally_by_payer_sql(None));
    push(tally_by_payer_sql(Some(RECEIVER)));
    push(Some(deployments_list_sql(50, 0, "signalledTokens", true)));
    push(Some(deployments_list_sql(50, 50, "stakedTokens", false)));
    push(Some(directory_sql()));
    push(deployments_by_id_sql(&[DEP.to_string()]));
    push(Some(DEPLOYMENT_IDS_SQL.to_string()));
    push(deployment_sql(DEP));
    push(allocations_by_deployment_sql(DEP, 50));
    push(deployment_signals_sql(DEP, 50));
    push(deployment_signal_transactions_sql(DEP, 50));
    push(deployment_allocations_history_sql(DEP, 50));
    push(Some(deployment_fees_since_sql(SINCE, 50)));
    push(Some(deployment_fees_window_sql(SINCE)));
    push(deployment_serving_indexers_sql(DEP, 50));
    push(indexer_url_sql(ADDR));
    push(Some(INDEXERS_ALL_SQL.to_string()));
    push(Some(ACTIVE_ALLOCATIONS_ALL_SQL.to_string()));
    push(Some(delegation_events_since_sql(SINCE)));
    push(Some(daily_flow_sql(SINCE, CUTOFF, true)));
    push(Some(daily_flow_sql(SINCE, CUTOFF, false)));
    push(Some(closed_allocations_since_sql(SINCE)));
    push(Some(exchange_rates_as_of_sql(CUTOFF)));
    push(Some(newest_provisions_sql(50)));
    push(Some(subgraphs_published_since_sql(SINCE)));
    push(deployment_subgraph_sql(DEP));
    push(subgraph_versions_sql(SUBGRAPH, 20));
    push(deployments_signal_stake_sql(&[DEP.to_string()]));
    push(Some(SUBGRAPH_METADATA_SQL.to_string()));
    push(Some(DIPS_CURRENT_ALLOCATION_SQL.to_string()));
    push(Some(DIPS_TIMELINE_SQL.to_string()));
    push(dips_latest_per_target_sql(
        "issuance_allocator__issuance_distributed",
    ));
    push(Some(QOS_PROVENANCE_SQL.to_string()));
    push(Some(QOS_FRESHNESS_SQL.to_string()));
    push(Some(qos_day_probe_sql(20_300)));
    push(Some(qos_indexer_day_sql(20_300)));
    push(Some(qos_seconds_behind_day_sql(20_300)));
    push(qos_indexer_daily_sql(ADDR, 20_300));
    push(qos_indexer_seconds_behind_sql(ADDR, 20_300));
    push(qos_indexer_gateways_sql(ADDR, 20_300));
    println!("{}", out.join("\n;;\n"));
}
