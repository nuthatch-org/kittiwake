//! What an indexer actually earns, netted against what it costs to run.
//!
//! The sibling of the delegator arithmetic next door. Two revenue streams, both already in
//! Postgres: query fees realised as RAV redemptions, and indexing rewards realised when an
//! allocation closes. The cost side is a model rather than a measurement, and saying so is part of
//! the answer. #67

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

#[derive(Debug, Clone, Serialize)]
pub struct RevenueDay {
    pub date: String,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentRevenue {
    pub deployment_id: Option<String>,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexerRevenue {
    pub indexer: String,
    #[serde(rename = "windowDays")]
    pub window_days: u32,
    pub rav_grt: f64,
    pub indexing_rewards_grt: f64,
    pub total_grt: f64,
    pub daily: Vec<RevenueDay>,
    #[serde(rename = "byDeployment", skip_serializing_if = "Option::is_none")]
    pub by_deployment: Option<Vec<DeploymentRevenue>>,
}

/// Fold two per-day series into one.
///
/// A day present in either stream is a day in the answer. Intersecting them instead would drop
/// every day an indexer collected fees and closed no allocation, which is most of them.
pub fn merge_daily(rav: &[(String, f64)], rewards: &[(String, f64)]) -> Vec<RevenueDay> {
    let mut by_date: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for (date, grt) in rav {
        by_date.entry(date).or_default().0 += grt;
    }
    for (date, grt) in rewards {
        by_date.entry(date).or_default().1 += grt;
    }
    by_date
        .into_iter()
        .map(|(date, (rav_grt, indexing_rewards_grt))| RevenueDay {
            date: date.to_string(),
            rav_grt,
            indexing_rewards_grt,
            total_grt: rav_grt + indexing_rewards_grt,
        })
        .collect()
}

/// The same fold, per deployment. `None` is a real key: a redemption can carry no deployment, and
/// collapsing those into one another's totals would misattribute them.
pub fn merge_by_deployment(
    rav: &[(Option<String>, f64)],
    rewards: &[(Option<String>, f64)],
) -> Vec<DeploymentRevenue> {
    let mut by_deployment: BTreeMap<Option<String>, (f64, f64)> = BTreeMap::new();
    for (id, grt) in rav {
        by_deployment.entry(id.clone()).or_default().0 += grt;
    }
    for (id, grt) in rewards {
        by_deployment.entry(id.clone()).or_default().1 += grt;
    }
    by_deployment
        .into_iter()
        .map(
            |(deployment_id, (rav_grt, indexing_rewards_grt))| DeploymentRevenue {
                deployment_id,
                rav_grt,
                indexing_rewards_grt,
                total_grt: rav_grt + indexing_rewards_grt,
            },
        )
        .collect()
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
        per_deployment: revenue
            .by_deployment
            .iter()
            .flatten()
            .map(|d| DeploymentPnl {
                deployment_id: d.deployment_id.clone(),
                revenue_grt: d.total_grt,
                revenue_usd: grt_price.map(|p| d.total_grt * p),
            })
            .collect(),
    }
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

    #[test]
    fn a_day_in_either_stream_is_a_day_in_the_answer() {
        let daily = merge_daily(
            &[("2026-09-01".into(), 10.0), ("2026-09-02".into(), 5.0)],
            &[("2026-09-02".into(), 2.0), ("2026-09-03".into(), 7.0)],
        );
        assert_eq!(daily.len(), 3, "three distinct days");
        assert_eq!(daily[0].total_grt, 10.0);
        assert_eq!(daily[1].total_grt, 7.0, "the day both streams touched");
        assert_eq!(daily[2].indexing_rewards_grt, 7.0);
        // Sorted, because a chart drawn in map order is a chart drawn wrong.
        assert_eq!(daily[0].date, "2026-09-01");
    }

    /// A redemption with no deployment is its own bucket rather than somebody else's.
    #[test]
    fn an_unattributed_redemption_keeps_its_own_line() {
        let by = merge_by_deployment(
            &[(None, 4.0), (Some("Qm1".into()), 6.0)],
            &[(Some("Qm1".into()), 1.0)],
        );
        assert_eq!(by.len(), 2);
        let unattributed = by.iter().find(|d| d.deployment_id.is_none()).unwrap();
        assert_eq!(unattributed.total_grt, 4.0);
        let qm1 = by
            .iter()
            .find(|d| d.deployment_id.as_deref() == Some("Qm1"))
            .unwrap();
        assert_eq!(qm1.total_grt, 7.0);
    }

    fn revenue(total: f64) -> IndexerRevenue {
        IndexerRevenue {
            indexer: "0xaa".into(),
            window_days: 30,
            rav_grt: total,
            indexing_rewards_grt: 0.0,
            total_grt: total,
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
