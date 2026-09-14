//! What an indexer actually earns, netted against what it costs to run.
//!
//! The sibling of the delegator arithmetic next door. Two revenue streams, both read from the nest
//! by the event that paid them: query fees as collected, and indexing rewards as collected. Each is
//! what the indexer received - query fees after the protocol's cut, the curators' share and the
//! delegators' cut, rewards after the delegators' share - with the collected figure beside it. The
//! cost side is a model rather than a measurement, and saying so is part of the answer. #67

use std::collections::BTreeMap;

use serde::Serialize;

/// The windows the route accepts. A caller asking for anything else gets 30 rather than an error,
/// because the incumbent does and a dashboard that 400s on a stale bookmark is worse than one that
/// shows the default month.
pub const REVENUE_WINDOWS: &[u32] = &[7, 30, 90, 365];

pub fn revenue_window(requested: Option<u32>) -> u32 {
    match requested {
        Some(days) if REVENUE_WINDOWS.contains(&days) => days,
        _ => 30,
    }
}

/// A chain's archive-node cost, per month.
///
/// **Maintained estimates, not gospel.** Archive cost varies wildly by client, era and provider;
/// Arbitrum One archive alone is cited anywhere from 3.3 TB on PathDB to about 38 TB. The figures
/// exist so an operator has somewhere to start, and every one of them can be overridden per
/// request. Nothing here should be presented as authoritative.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ChainCost {
    pub key: &'static str,
    pub label: &'static str,
    /// Rough archive footprint in TB. Informational, and not used in any sum.
    #[serde(rename = "storageTb")]
    pub storage_tb: Option<f64>,
    #[serde(rename = "monthlyUsd")]
    pub monthly_usd: f64,
}

const fn chain(
    key: &'static str,
    label: &'static str,
    storage_tb: f64,
    monthly_usd: f64,
) -> ChainCost {
    ChainCost {
        key,
        label,
        storage_tb: Some(storage_tb),
        monthly_usd,
    }
}

pub const DEFAULT_CHAIN_COSTS: &[ChainCost] = &[
    chain("mainnet", "Ethereum", 18.0, 700.0),
    chain("arbitrum", "Arbitrum One", 3.3, 1500.0),
    chain("base", "Base", 6.0, 800.0),
    chain("optimism", "Optimism", 4.0, 500.0),
    chain("polygon", "Polygon", 12.0, 1200.0),
    chain("gnosis", "Gnosis", 2.0, 300.0),
    chain("bsc", "BNB Chain", 15.0, 1500.0),
    chain("avalanche", "Avalanche", 5.0, 600.0),
    chain("celo", "Celo", 2.0, 300.0),
    chain("fantom", "Fantom", 3.0, 400.0),
    chain("scroll", "Scroll", 1.0, 300.0),
    chain("linea", "Linea", 1.0, 300.0),
];

/// graph-node, the indexer agent, monitoring: what an operator pays before any chain at all.
pub const DEFAULT_BASE_OVERHEAD_USD: f64 = 300.0;

/// The defaults as the response carries them: an object keyed by chain, because that is the shape
/// the incumbent returns and the shape the cost editor indexes into. A JSON array here would be a
/// silent break in a consumer that reads `defaultChainCosts.arbitrum`.
pub fn default_chain_costs_map() -> serde_json::Map<String, serde_json::Value> {
    DEFAULT_CHAIN_COSTS
        .iter()
        .map(|c| {
            (
                c.key.to_string(),
                serde_json::to_value(c).expect("a ChainCost serializes"),
            )
        })
        .collect()
}

fn known(key: &str) -> Option<&'static ChainCost> {
    DEFAULT_CHAIN_COSTS.iter().find(|c| c.key == key)
}

#[derive(Debug, Clone, Serialize)]
pub struct CostLine {
    pub key: String,
    pub label: String,
    #[serde(rename = "monthlyUsd")]
    pub monthly_usd: f64,
    #[serde(rename = "isOverride")]
    pub is_override: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostModel {
    pub lines: Vec<CostLine>,
    #[serde(rename = "baseOverheadUsd")]
    pub base_overhead_usd: f64,
    #[serde(rename = "totalMonthlyUsd")]
    pub total_monthly_usd: f64,
}

/// Build a cost model from the chains an operator says it runs.
///
/// A chain nobody has a default for is kept only if the caller priced it themselves. Skipping it
/// silently would understate the cost; inventing a price for it would be worse.
pub fn resolve_cost_model(
    chains: &[String],
    overrides: &BTreeMap<String, f64>,
    base_overhead_usd: Option<f64>,
) -> CostModel {
    let base_overhead_usd = base_overhead_usd.unwrap_or(DEFAULT_BASE_OVERHEAD_USD);
    let mut lines = Vec::new();
    for key in chains {
        let default = known(key);
        let overridden = overrides.get(key).copied();
        let monthly_usd = match (overridden, default) {
            (Some(price), _) => price,
            (None, Some(default)) => default.monthly_usd,
            (None, None) => continue,
        };
        lines.push(CostLine {
            key: key.clone(),
            label: default.map(|d| d.label.to_string()).unwrap_or(key.clone()),
            monthly_usd,
            is_override: overridden.is_some(),
        });
    }
    let total_monthly_usd = base_overhead_usd + lines.iter().map(|l| l.monthly_usd).sum::<f64>();
    CostModel {
        lines,
        base_overhead_usd,
        total_monthly_usd,
    }
}

/// `rav_grt` and `indexing_rewards_grt` keep the incumbent's names and now mean what the indexer
/// received. They used to be the collected query fees and the rewards including the delegators'
/// share, which is why the `*_gross_grt` fields exist: the old figures are still there, named.
#[derive(Debug, Clone, Serialize)]
pub struct RevenueDay {
    pub date: String,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
    pub query_fees_gross_grt: f64,
    pub indexing_rewards_gross_grt: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentRevenue {
    pub deployment_id: Option<String>,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
    pub query_fees_gross_grt: f64,
    pub indexing_rewards_gross_grt: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexerRevenue {
    pub indexer: String,
    #[serde(rename = "windowDays")]
    pub window_days: u32,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
    pub query_fees_gross_grt: f64,
    pub indexing_rewards_gross_grt: f64,
    pub daily: Vec<RevenueDay>,
    #[serde(rename = "byDeployment", skip_serializing_if = "Option::is_none")]
    pub by_deployment: Option<Vec<DeploymentRevenue>>,
}

/// A wei figure the nest sent as a decimal string, in GRT. Absent is zero: a day or deployment row
/// exists only because something was paid on it, so a missing column is a column with nothing in it.
fn grt(row: &serde_json::Value, key: &str) -> f64 {
    match row.get(key) {
        Some(serde_json::Value::String(s)) => s.parse::<f64>().unwrap_or(0.0) / 1e18,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0) / 1e18,
        _ => 0.0,
    }
}

/// A UTC day start as `YYYY-MM-DD`, without a date library for one conversion (Hinnant's
/// civil-from-days).
fn iso_day(day_start: i64) -> String {
    let z = day_start.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The revenue answer from the nest's rows: `daily` from `lodestar_indexer_daily`, and
/// `by_deployment` from `lodestar_indexer_deployment_daily` when it was asked for.
pub fn revenue_from_nest(
    indexer: &str,
    window_days: u32,
    answer: &serde_json::Value,
) -> IndexerRevenue {
    let rows = |key: &str| {
        answer
            .get(key)
            .and_then(serde_json::Value::as_array)
            .cloned()
    };
    let daily: Vec<RevenueDay> = rows("daily")
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let (fees, rewards) = (grt(r, "fees_net"), grt(r, "indexer_rewards"));
            RevenueDay {
                date: iso_day(
                    r.get("day_start")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0),
                ),
                rav_grt: fees,
                indexing_rewards_grt: rewards,
                total_grt: fees + rewards,
                query_fees_gross_grt: grt(r, "fees_gross"),
                indexing_rewards_gross_grt: grt(r, "total_rewards"),
            }
        })
        .collect();
    let by_deployment = rows("by_deployment").map(|rows| {
        rows.iter()
            .map(|r| {
                let (fees, rewards) = (grt(r, "fees_net"), grt(r, "indexer_rewards"));
                DeploymentRevenue {
                    deployment_id: r
                        .get("deployment")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    rav_grt: fees,
                    indexing_rewards_grt: rewards,
                    total_grt: fees + rewards,
                    query_fees_gross_grt: grt(r, "fees_gross"),
                    indexing_rewards_gross_grt: grt(r, "total_rewards"),
                }
            })
            .collect()
    });
    let sum = |f: fn(&RevenueDay) -> f64| daily.iter().map(f).sum::<f64>();
    IndexerRevenue {
        indexer: indexer.to_string(),
        window_days,
        rav_grt: sum(|d| d.rav_grt),
        indexing_rewards_grt: sum(|d| d.indexing_rewards_grt),
        total_grt: sum(|d| d.total_grt),
        query_fees_gross_grt: sum(|d| d.query_fees_gross_grt),
        indexing_rewards_gross_grt: sum(|d| d.indexing_rewards_gross_grt),
        daily,
        by_deployment,
    }
}

/// The first UTC day of a window that ends today, as a day start.
pub fn window_start(now: i64, window_days: u32) -> i64 {
    (now.div_euclid(86_400) - (i64::from(window_days) - 1)) * 86_400
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentPnl {
    pub deployment_id: Option<String>,
    pub revenue_grt: f64,
    pub revenue_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexerPnl {
    #[serde(rename = "windowDays")]
    pub window_days: u32,
    #[serde(rename = "grtPrice")]
    pub grt_price: Option<f64>,
    pub revenue_grt: f64,
    pub revenue_usd: Option<f64>,
    /// The monthly model prorated to the window.
    pub infra_cost_usd: f64,
    pub infra_monthly_usd: f64,
    pub net_usd: Option<f64>,
    pub margin_pct: Option<f64>,
    /// The GRT price at which this window breaks even.
    pub breakeven_grt_price: Option<f64>,
    #[serde(rename = "perDeployment")]
    pub per_deployment: Vec<DeploymentPnl>,
}

/// Net revenue against cost.
///
/// Every USD figure is `None` without a price, so the GRT view still answers when the price feed
/// does not. Reporting a dollar total computed from a missing price is the failure this shape
/// exists to prevent.
pub fn compute_pnl(
    revenue: &IndexerRevenue,
    cost: &CostModel,
    grt_price: Option<f64>,
    window_days: u32,
) -> IndexerPnl {
    let infra_cost_usd = cost.total_monthly_usd * (f64::from(window_days) / 30.0);
    let revenue_grt = revenue.total_grt;
    let revenue_usd = grt_price.map(|p| revenue_grt * p);
    let net_usd = revenue_usd.map(|usd| usd - infra_cost_usd);
    let margin_pct = match (revenue_usd, net_usd) {
        (Some(usd), Some(net)) if usd > 0.0 => Some(net / usd * 100.0),
        _ => None,
    };
    IndexerPnl {
        window_days,
        grt_price,
        revenue_grt,
        revenue_usd,
        infra_cost_usd,
        infra_monthly_usd: cost.total_monthly_usd,
        net_usd,
        margin_pct,
        // Dividing by a zero total would be an infinity presented as a price.
        breakeven_grt_price: (revenue_grt > 0.0).then(|| infra_cost_usd / revenue_grt),
        per_deployment: per_deployment(revenue, grt_price),
    }
}

/// Deployments by what they earned, most first.
///
/// The order is the answer here: the panel renders the head of this list as "where the revenue came
/// from", and the source collection is keyed by deployment id, so leaving it alone puts whichever
/// deployment sorts first alphabetically at the top of a table about money. The same ninety-two
/// deployments with the same figures in a different order is still the wrong page.
fn per_deployment(revenue: &IndexerRevenue, grt_price: Option<f64>) -> Vec<DeploymentPnl> {
    let mut out: Vec<DeploymentPnl> = revenue
        .by_deployment
        .iter()
        .flatten()
        .map(|d| DeploymentPnl {
            deployment_id: d.deployment_id.clone(),
            revenue_grt: d.total_grt,
            revenue_usd: grt_price.map(|p| d.total_grt * p),
        })
        .collect();
    // The id breaks a tie, so two deployments that earned the same amount keep a stable order
    // between one request and the next.
    out.sort_by(|a, b| {
        b.revenue_grt
            .total_cmp(&a.revenue_grt)
            .then_with(|| a.deployment_id.cmp(&b.deployment_id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn costs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn chains(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn the_default_model_is_overhead_plus_each_chain() {
        let model = resolve_cost_model(&chains(&["mainnet", "arbitrum"]), &BTreeMap::new(), None);
        assert_eq!(model.total_monthly_usd, 300.0 + 700.0 + 1500.0);
        assert!(model.lines.iter().all(|l| !l.is_override));
    }

    #[test]
    fn an_override_replaces_the_default_and_says_so() {
        let model = resolve_cost_model(&chains(&["arbitrum"]), &costs(&[("arbitrum", 99.0)]), None);
        assert_eq!(model.lines[0].monthly_usd, 99.0);
        assert!(model.lines[0].is_override);
        assert_eq!(model.total_monthly_usd, 399.0);
    }

    /// A chain we have no figure for is priced by the caller or left out. Guessing at one would
    /// put a number nobody chose into somebody's cost base.
    #[test]
    fn an_unknown_chain_counts_only_when_the_caller_prices_it() {
        let unpriced = resolve_cost_model(&chains(&["hedera"]), &BTreeMap::new(), None);
        assert!(unpriced.lines.is_empty());
        assert_eq!(unpriced.total_monthly_usd, 300.0);

        let priced = resolve_cost_model(&chains(&["hedera"]), &costs(&[("hedera", 250.0)]), None);
        assert_eq!(priced.lines[0].label, "hedera", "its own key stands in");
        assert_eq!(priced.total_monthly_usd, 550.0);
    }

    /// An allocation collected on the 1st and the 2nd while open and closed on the 3rd. Dated by
    /// close, all of it lands on the 3rd and neither day it was paid appears.
    #[test]
    fn each_day_is_the_day_it_was_paid() {
        let answer = serde_json::json!({"daily": [
            {"day_start": 1_788_220_800, "indexer_rewards": "60000000000000000000", "total_rewards": "100000000000000000000", "fees_net": "0", "fees_gross": "0"},
            {"day_start": 1_788_307_200, "indexer_rewards": "30000000000000000000", "total_rewards": "50000000000000000000", "fees_net": "0", "fees_gross": "0"},
        ]});
        let r = revenue_from_nest("0xaa", 30, &answer);
        let dates: Vec<&str> = r.daily.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, ["2026-09-01", "2026-09-02"]);
        assert_eq!(r.indexing_rewards_grt, 90.0);
        assert_eq!(r.indexing_rewards_gross_grt, 150.0);
    }

    /// What the indexer kept is the revenue, and what was collected is beside it rather than
    /// instead of it. The figures are one real `GraphPaymentCollected` split.
    #[test]
    fn revenue_is_what_the_indexer_received_and_the_collected_figure_is_kept() {
        let answer = serde_json::json!({"daily": [
            {"day_start": 1_788_393_600, "indexer_rewards": "0", "total_rewards": "0",
             "fees_net": "7105116309131241", "fees_gross": "8860352050294604"},
        ], "by_deployment": [
            {"deployment": "0xd1", "indexer_rewards": "0", "total_rewards": "0",
             "fees_net": "7105116309131241", "fees_gross": "8860352050294604"},
        ]});
        let r = revenue_from_nest("0xaa", 30, &answer);
        assert_eq!(r.rav_grt, 0.007_105_116_309_131_241);
        assert_eq!(r.query_fees_gross_grt, 0.008_860_352_050_294_604);
        assert_eq!(r.total_grt, r.rav_grt);
        let by = r.by_deployment.unwrap();
        assert_eq!(by[0].deployment_id.as_deref(), Some("0xd1"));
        assert_eq!(
            by[0].total_grt, r.total_grt,
            "the table and the chart are one definition"
        );
    }

    /// A collection that paid nothing is a day with nothing earned, not an absent day and not an
    /// error; and a deployment table is absent unless it was asked for.
    #[test]
    fn a_zero_reward_collection_is_a_day_that_earned_nothing() {
        let answer = serde_json::json!({"daily": [
            {"day_start": 1_788_480_000, "indexer_rewards": "0", "total_rewards": "0", "fees_net": "0", "fees_gross": "0"},
        ]});
        let r = revenue_from_nest("0xaa", 7, &answer);
        assert_eq!(r.daily.len(), 1);
        assert_eq!(r.daily[0].date, "2026-09-04");
        assert_eq!(r.total_grt, 0.0);
        assert!(r.by_deployment.is_none());
    }

    #[test]
    fn a_window_ends_today_and_starts_at_a_utc_midnight() {
        let now = 1_789_317_526; // 2026-09-13 16:38:46 UTC
        assert_eq!(window_start(now, 1), 1_789_257_600);
        assert_eq!(window_start(now, 30), 1_789_257_600 - 29 * 86_400);
        assert_eq!(iso_day(window_start(now, 30)), "2026-08-15");
        assert_eq!(iso_day(0), "1970-01-01");
        assert_eq!(iso_day(951_782_400), "2000-02-29");
    }

    fn revenue(total: f64) -> IndexerRevenue {
        IndexerRevenue {
            indexer: "0xaa".into(),
            window_days: 30,
            rav_grt: total,
            indexing_rewards_grt: 0.0,
            total_grt: total,
            query_fees_gross_grt: total,
            indexing_rewards_gross_grt: 0.0,
            daily: Vec::new(),
            by_deployment: None,
        }
    }

    /// Without a price every dollar figure is absent rather than zero. A zero net would read as
    /// breaking even, which is a different claim from not knowing.
    #[test]
    fn no_price_means_no_dollars_rather_than_zero_dollars() {
        let model = resolve_cost_model(&chains(&["arbitrum"]), &BTreeMap::new(), None);
        let pnl = compute_pnl(&revenue(1000.0), &model, None, 30);
        assert_eq!(pnl.revenue_grt, 1000.0);
        assert!(pnl.revenue_usd.is_none());
        assert!(pnl.net_usd.is_none());
        assert!(pnl.margin_pct.is_none());
        // The breakeven price needs no price, which is the whole point of it.
        assert_eq!(pnl.breakeven_grt_price, Some(1800.0 / 1000.0));
    }

    #[test]
    fn the_cost_is_prorated_to_the_window_rather_than_charged_monthly() {
        let model = resolve_cost_model(&[], &BTreeMap::new(), Some(300.0));
        assert_eq!(
            compute_pnl(&revenue(1.0), &model, None, 30).infra_cost_usd,
            300.0
        );
        assert_eq!(
            compute_pnl(&revenue(1.0), &model, None, 7).infra_cost_usd,
            70.0
        );
        assert_eq!(
            compute_pnl(&revenue(1.0), &model, None, 90).infra_cost_usd,
            900.0
        );
    }

    /// An indexer that earned nothing has no breakeven price, rather than an infinite one.
    #[test]
    fn nothing_earned_has_no_breakeven_price() {
        let model = resolve_cost_model(&chains(&["arbitrum"]), &BTreeMap::new(), None);
        let pnl = compute_pnl(&revenue(0.0), &model, Some(0.09), 30);
        assert_eq!(pnl.breakeven_grt_price, None);
        assert_eq!(pnl.margin_pct, None, "no revenue is not a margin of zero");
        assert_eq!(pnl.net_usd, Some(-1800.0), "the cost is still real");
    }

    /// The incumbent's JSON mixes camelCase and snake_case field by field, and a consumer
    /// destructures the exact spelling. #26 is what happens when this is assumed rather than
    /// checked, so it is checked.
    #[test]
    fn the_wire_names_are_the_incumbents_and_not_rusts() {
        let model = resolve_cost_model(&chains(&["arbitrum"]), &costs(&[("arbitrum", 1.0)]), None);
        let json = serde_json::to_value(&model).unwrap();
        for key in ["lines", "baseOverheadUsd", "totalMonthlyUsd"] {
            assert!(json.get(key).is_some(), "CostModel is missing {key}");
        }
        for key in ["key", "label", "monthlyUsd", "isOverride"] {
            assert!(
                json["lines"][0].get(key).is_some(),
                "CostLine is missing {key}"
            );
        }

        let mut revenue = revenue(1.0);
        revenue.by_deployment = Some(Vec::new());
        let json = serde_json::to_value(&revenue).unwrap();
        for key in [
            "indexer",
            "windowDays",
            "rav_grt",
            "indexing_rewards_grt",
            "total_grt",
            "query_fees_gross_grt",
            "indexing_rewards_gross_grt",
            "daily",
            "byDeployment",
        ] {
            assert!(json.get(key).is_some(), "IndexerRevenue is missing {key}");
        }

        let json = serde_json::to_value(compute_pnl(&revenue, &model, Some(1.0), 30)).unwrap();
        for key in [
            "windowDays",
            "grtPrice",
            "revenue_grt",
            "revenue_usd",
            "infra_cost_usd",
            "infra_monthly_usd",
            "net_usd",
            "margin_pct",
            "breakeven_grt_price",
            "perDeployment",
        ] {
            assert!(json.get(key).is_some(), "IndexerPnl is missing {key}");
        }

        // Keyed by chain rather than a list, because the editor indexes into it.
        let defaults = default_chain_costs_map();
        assert!(defaults.contains_key("arbitrum"));
        assert_eq!(defaults["arbitrum"]["monthlyUsd"], 1500.0);
        assert_eq!(defaults["arbitrum"]["storageTb"], 3.3);
    }

    #[test]
    fn a_window_that_is_not_offered_falls_back_to_a_month() {
        assert_eq!(revenue_window(Some(7)), 7);
        assert_eq!(revenue_window(Some(365)), 365);
        assert_eq!(revenue_window(Some(31)), 30);
        assert_eq!(revenue_window(Some(0)), 30);
        assert_eq!(revenue_window(None), 30);
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    fn revenue(pairs: &[(&str, f64)]) -> IndexerRevenue {
        IndexerRevenue {
            indexer: "0xabc".into(),
            window_days: 30,
            rav_grt: 0.0,
            indexing_rewards_grt: 0.0,
            total_grt: pairs.iter().map(|(_, grt)| grt).sum(),
            query_fees_gross_grt: 0.0,
            indexing_rewards_gross_grt: 0.0,
            daily: Vec::new(),
            by_deployment: Some(
                pairs
                    .iter()
                    .map(|(id, grt)| DeploymentRevenue {
                        deployment_id: Some((*id).to_string()),
                        rav_grt: 0.0,
                        indexing_rewards_grt: *grt,
                        total_grt: *grt,
                        query_fees_gross_grt: 0.0,
                        indexing_rewards_gross_grt: *grt,
                    })
                    .collect(),
            ),
        }
    }

    fn ids(out: &[DeploymentPnl]) -> Vec<&str> {
        out.iter()
            .map(|d| d.deployment_id.as_deref().unwrap_or(""))
            .collect()
    }

    /// The panel shows the head of this list as where the revenue came from. The source is keyed
    /// by deployment id, so without this the top of a table about money is whichever id sorts
    /// first, and every figure on the page is right while the page is wrong.
    #[test]
    fn deployments_are_ordered_by_what_they_earned() {
        let out = per_deployment(
            &revenue(&[("0xaa", 5.0), ("0xbb", 500.0), ("0xcc", 50.0)]),
            None,
        );
        assert_eq!(ids(&out), ["0xbb", "0xcc", "0xaa"]);
    }

    /// Two deployments on the same figure keep a stable order between requests, so a reader
    /// refreshing the page does not watch them swap.
    #[test]
    fn a_tie_is_broken_by_the_deployment_id() {
        let out = per_deployment(&revenue(&[("0xbb", 10.0), ("0xaa", 10.0)]), None);
        assert_eq!(ids(&out), ["0xaa", "0xbb"]);
    }
}
