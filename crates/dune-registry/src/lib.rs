//! Dune query ownership, cadence and conservative credit admission. No network or database types.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub monthly_budget_microcredits: u64,
    pub allocations: BTreeMap<String, u64>,
    pub query: Vec<Query>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub id: String,
    pub dune_query_id: u64,
    pub owner: String,
    pub workstream: String,
    pub enabled: bool,
    pub schedule_seconds: u64,
    pub poll_seconds: u64,
    pub execution_estimate_microcredits: u64,
    pub export_estimate_microcredits: u64,
    pub result_limit: usize,
    pub target_table: String,
    pub freshness_seconds: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("registry IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid registry TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid registry: {0}")]
    Invalid(&'static str),
}

impl Registry {
    pub fn parse(text: &str) -> Result<Self, Error> {
        Ok(toml::from_str(text)?)
    }

    pub fn load_file(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn load_dir(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let mut entries = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| e.path());
        if entries.is_empty() {
            return Err(Error::Invalid("no registry files found in directory"));
        }
        let mut combined: Option<Registry> = None;
        for entry in entries {
            let reg = Self::load_file(entry.path())?;
            if let Some(ref mut c) = combined {
                if c.monthly_budget_microcredits != reg.monthly_budget_microcredits {
                    return Err(Error::Invalid(
                        "mismatched monthly_budget_microcredits across registry files",
                    ));
                }
                for (k, v) in reg.allocations {
                    if let Some(existing) = c.allocations.get(&k) {
                        if *existing != v {
                            return Err(Error::Invalid("conflicting allocation in registry files"));
                        }
                    } else {
                        c.allocations.insert(k, v);
                    }
                }
                c.query.extend(reg.query);
            } else {
                combined = Some(reg);
            }
        }
        combined.ok_or(Error::Invalid("no registry files loaded"))
    }

    /// `tables` must come from migration/schema validation, not from the registry itself.
    /// Disabled pilots are structurally checked but cannot consume scheduled allocations.
    pub fn validate(&self, tables: &BTreeSet<String>) -> Result<BTreeMap<String, u64>, Error> {
        let allocated = self
            .allocations
            .values()
            .try_fold(0u64, |sum, n| sum.checked_add(*n))
            .ok_or(Error::Invalid("allocation overflow"))?;
        if allocated > self.monthly_budget_microcredits {
            return Err(Error::Invalid("allocations exceed budget"));
        }
        let mut ids = BTreeSet::new();
        let mut remote_ids = BTreeSet::new();
        let mut targets = BTreeSet::new();
        let mut totals = BTreeMap::<String, u64>::new();
        for q in &self.query {
            if !identifier(&q.id)
                || !identifier(&q.target_table)
                || q.owner.trim().is_empty()
                || !ids.insert(&q.id)
                || !targets.insert(&q.target_table)
            {
                return Err(Error::Invalid("invalid or duplicate query identity/table"));
            }
            if q.dune_query_id != 0 && !remote_ids.insert(q.dune_query_id) {
                return Err(Error::Invalid("duplicate Dune query"));
            }
            if q.schedule_seconds == 0
                || q.poll_seconds == 0
                || q.result_limit == 0
                || q.poll_seconds > q.schedule_seconds
                || q.freshness_seconds < q.schedule_seconds
                || q.execution_estimate_microcredits == 0
                || q.export_estimate_microcredits == 0
            {
                return Err(Error::Invalid(
                    "invalid cadence, result limit or cost estimate",
                ));
            }
            let allocation = self
                .allocations
                .get(&q.workstream)
                .ok_or(Error::Invalid("unknown workstream"))?;
            if !q.enabled {
                continue;
            }
            if q.dune_query_id == 0 || !tables.contains(&q.target_table) {
                return Err(Error::Invalid(
                    "enabled query requires Dune ID and migrated table",
                ));
            }
            // A 31-day horizon and one startup run cover short-month and phase-offset assumptions.
            let runs = (31 * 86400u64)
                .div_ceil(q.schedule_seconds)
                .checked_add(1)
                .unwrap();
            let polls = (31 * 86400u64)
                .div_ceil(q.poll_seconds)
                .checked_add(1)
                .unwrap();
            let cost = runs
                .checked_mul(q.execution_estimate_microcredits)
                .and_then(|n| {
                    polls
                        .checked_mul(q.export_estimate_microcredits)
                        .and_then(|p| n.checked_add(p))
                })
                .ok_or(Error::Invalid("cost overflow"))?;
            let total = totals.entry(q.workstream.clone()).or_default();
            *total = total
                .checked_add(cost)
                .ok_or(Error::Invalid("cost overflow"))?;
            if *total > *allocation {
                return Err(Error::Invalid("workstream exceeds allocation"));
            }
        }
        Ok(totals)
    }
}

fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|c| c.is_ascii_lowercase())
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> Registry {
        Registry::parse(include_str!("../../../registry/dune/lodestar.toml")).unwrap()
    }
    fn tables() -> BTreeSet<String> {
        BTreeSet::from(["dune_grt_price_daily".into()])
    }

    #[test]
    fn disabled_pilot_needs_no_migration() {
        assert!(registry().validate(&BTreeSet::new()).unwrap().is_empty());
    }
    #[test]
    fn enabled_pilot_requires_real_table() {
        let mut r = registry();
        r.query[0].enabled = true;
        assert!(r.validate(&BTreeSet::new()).is_err());
        assert_eq!(r.validate(&tables()).unwrap()["A"], 64_000_000);
    }
    #[test]
    fn export_and_startup_costs_count_towards_allocation() {
        let mut r = registry();
        r.query[0].enabled = true;
        r.allocations.insert("A".into(), 63_999_999);
        assert!(r.validate(&tables()).is_err());
    }
    #[test]
    fn missed_schedule_cannot_be_hidden_by_slow_poll() {
        let mut r = registry();
        r.query[0].poll_seconds = 172800;
        assert!(r.validate(&tables()).is_err());
    }
    #[test]
    fn overflow_is_an_error() {
        let mut r = registry();
        r.query[0].enabled = true;
        r.query[0].execution_estimate_microcredits = u64::MAX;
        assert!(r.validate(&tables()).is_err());
    }
    #[test]
    fn typo_in_configuration_is_not_ignored() {
        assert!(Registry::parse("monthly_budget_microcredits = 4000\nenabld = true").is_err());
    }
    #[test]
    fn load_file_and_load_dir_read_workspace_registry() {
        let file_reg = Registry::load_file("../../registry/dune/lodestar.toml").unwrap();
        assert_eq!(file_reg.monthly_budget_microcredits, 4_000_000_000);
        let dir_reg = Registry::load_dir("../../registry/dune").unwrap();
        assert_eq!(dir_reg.monthly_budget_microcredits, 4_000_000_000);
        assert_eq!(dir_reg.query.len(), file_reg.query.len());
    }
}
