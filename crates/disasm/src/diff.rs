//! Two reports, compared: which handlers appeared, vanished or changed what they can reach.
//!
//! Pure and deterministic. The interesting half is the handler identity, `dataSource ▸ handler ▸
//! kind`, which is stable across versions where a function index is not.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::types::{DisassemblyReport, HandlerAnalysis, HostCategory, RiskFlag};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HandlerStatus {
    Changed,
    Added,
    Removed,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedChange {
    pub from: bool,
    pub to: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerDiffEntry {
    pub key: String,
    pub data_source: String,
    pub handler: String,
    pub kind: crate::types::HandlerKind,
    pub trigger: Option<String>,
    pub status: HandlerStatus,
    pub categories_added: Vec<HostCategory>,
    pub categories_removed: Vec<HostCategory>,
    pub resolved_changed: Option<ResolvedChange>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSourceStatus {
    Added,
    Removed,
    Common,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSourceDiffEntry {
    pub name: String,
    pub status: DataSourceStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardDiff {
    pub grade_from: char,
    pub grade_to: char,
    pub risk_from: i32,
    pub risk_to: i32,
    /// Target minus base, so a positive number is riskier than before.
    pub risk_delta: i32,
    pub flags_added: Vec<RiskFlag>,
    pub flags_removed: Vec<RiskFlag>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDiff {
    pub api_versions_from: Vec<String>,
    pub api_versions_to: Vec<String>,
    pub api_version_changed: bool,
    pub spec_from: String,
    pub spec_to: String,
    pub network_from: String,
    pub network_to: String,
    pub network_changed: bool,
    pub features_added: Vec<String>,
    pub features_removed: Vec<String>,
    pub graft_from: Option<crate::types::Graft>,
    pub graft_to: Option<crate::types::Graft>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StringsDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSurfaceDiff {
    pub added: Vec<HostCategory>,
    pub removed: Vec<HostCategory>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Side {
    pub deployment_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub handlers_added: usize,
    pub handlers_removed: usize,
    pub handlers_changed: usize,
    pub handlers_unchanged: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisassemblyDiff {
    pub base: Side,
    pub target: Side,
    pub identical: bool,
    pub manifest: ManifestDiff,
    pub scorecard: ScorecardDiff,
    pub host_surface: HostSurfaceDiff,
    pub data_sources: Vec<DataSourceDiffEntry>,
    pub handlers: Vec<HandlerDiffEntry>,
    pub strings: StringsDiff,
    pub summary: Summary,
}

const STRING_CAP: usize = 120;

/// `dataSource \u{25b8} handler \u{25b8} kind`, which is stable across versions where a function
/// index is not.
///
/// The incumbent's separator is a literal NUL byte, while the comment directly above it says
/// `\u{25b8}`. A NUL in a JSON string is legal and nothing good comes of one: it survives into the
/// wire format, into logs and into anything that round-trips through a C string. The page uses this
/// value only as a React key, so the character the comment intended is used instead.
fn handler_key(source: &str, handler: &HandlerAnalysis) -> String {
    let kind = match handler.kind {
        crate::types::HandlerKind::Event => "event",
        crate::types::HandlerKind::Call => "call",
        crate::types::HandlerKind::Block => "block",
    };
    format!("{source} \u{25b8} {} \u{25b8} {kind}", handler.handler)
}

/// Every handler in a report by its cross-version key. A repeated key is degenerate, and the
/// first occurrence wins so the two sides agree about which one they are comparing.
fn flatten(report: &DisassemblyReport) -> BTreeMap<String, (&str, &HandlerAnalysis)> {
    let mut out = BTreeMap::new();
    for source in &report.data_sources {
        for handler in &source.handlers {
            out.entry(handler_key(&source.name, handler))
                .or_insert((source.name.as_str(), handler));
        }
    }
    out
}

/// What one side gained and lost, in the order the other side lists them.
fn set_diff<T: Clone + Ord>(from: &[T], to: &[T]) -> (Vec<T>, Vec<T>) {
    let from_set: BTreeSet<&T> = from.iter().collect();
    let to_set: BTreeSet<&T> = to.iter().collect();
    (
        to.iter()
            .filter(|x| !from_set.contains(x))
            .cloned()
            .collect(),
        from.iter()
            .filter(|x| !to_set.contains(x))
            .cloned()
            .collect(),
    )
}

/// Every recovered string in a report, first occurrence order preserved.
///
/// Order matters only when the diff is capped, and it is kept rather than sorted so the hundred
/// and twenty strings shown are the same hundred and twenty the incumbent showed.
fn all_strings(report: &DisassemblyReport) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for module in crate::report::modules(report) {
        for s in &module.strings {
            if seen.insert(s.clone()) {
                out.push(s.clone());
            }
        }
    }
    out
}

pub fn diff(base: &DisassemblyReport, target: &DisassemblyReport) -> DisassemblyDiff {
    let base_handlers = flatten(base);
    let target_handlers = flatten(target);

    let mut handlers: Vec<HandlerDiffEntry> = Vec::new();
    let keys: BTreeSet<&String> = base_handlers.keys().chain(target_handlers.keys()).collect();
    for key in keys {
        let b = base_handlers.get(key);
        let t = target_handlers.get(key);
        // At least one side has it, or the key would not be here.
        let (source, reference) = t.or(b).copied().expect("key came from one of the two");
        let mut entry = HandlerDiffEntry {
            key: key.clone(),
            data_source: source.to_string(),
            handler: reference.handler.clone(),
            kind: reference.kind,
            trigger: reference.trigger.clone(),
            status: HandlerStatus::Unchanged,
            categories_added: Vec::new(),
            categories_removed: Vec::new(),
            resolved_changed: None,
        };

        match (b, t) {
            (Some((_, b)), None) => {
                entry.status = HandlerStatus::Removed;
                entry.categories_removed = b.categories.clone();
            }
            (None, Some((_, t))) => {
                entry.status = HandlerStatus::Added;
                entry.categories_added = t.categories.clone();
            }
            (Some((_, b)), Some((_, t))) => {
                let (added, removed) = set_diff(&b.categories, &t.categories);
                entry.resolved_changed = (b.resolved != t.resolved).then_some(ResolvedChange {
                    from: b.resolved,
                    to: t.resolved,
                });
                entry.status =
                    if !added.is_empty() || !removed.is_empty() || entry.resolved_changed.is_some()
                    {
                        HandlerStatus::Changed
                    } else {
                        HandlerStatus::Unchanged
                    };
                entry.categories_added = added;
                entry.categories_removed = removed;
            }
            (None, None) => unreachable!("key came from one of the two"),
        }
        handlers.push(entry);
    }
    // Changed first, then added, then removed, then the rest, and by key inside each. What
    // changed is what a reader opened the page for.
    handlers.sort_by(|a, b| a.status.cmp(&b.status).then_with(|| a.key.cmp(&b.key)));

    let count = |status: HandlerStatus| handlers.iter().filter(|h| h.status == status).count();
    let summary = Summary {
        handlers_added: count(HandlerStatus::Added),
        handlers_removed: count(HandlerStatus::Removed),
        handlers_changed: count(HandlerStatus::Changed),
        handlers_unchanged: count(HandlerStatus::Unchanged),
    };

    let names = |report: &DisassemblyReport| -> BTreeSet<String> {
        report.data_sources.iter().map(|d| d.name.clone()).collect()
    };
    let base_names = names(base);
    let target_names = names(target);
    let data_sources: Vec<DataSourceDiffEntry> = base_names
        .union(&target_names)
        .map(|name| DataSourceDiffEntry {
            name: name.clone(),
            status: match (base_names.contains(name), target_names.contains(name)) {
                (true, true) => DataSourceStatus::Common,
                (false, true) => DataSourceStatus::Added,
                _ => DataSourceStatus::Removed,
            },
        })
        .collect();

    let titles = |flags: &[RiskFlag]| -> BTreeSet<String> {
        flags.iter().map(|f| f.title.clone()).collect()
    };
    let base_titles = titles(&base.scorecard.flags);
    let target_titles = titles(&target.scorecard.flags);
    let scorecard = ScorecardDiff {
        grade_from: base.scorecard.grade,
        grade_to: target.scorecard.grade,
        risk_from: base.scorecard.risk_score,
        risk_to: target.scorecard.risk_score,
        risk_delta: target.scorecard.risk_score - base.scorecard.risk_score,
        flags_added: target
            .scorecard
            .flags
            .iter()
            .filter(|f| !base_titles.contains(&f.title))
            .cloned()
            .collect(),
        flags_removed: base
            .scorecard
            .flags
            .iter()
            .filter(|f| !target_titles.contains(&f.title))
            .cloned()
            .collect(),
    };

    let (features_added, features_removed) =
        set_diff(&base.manifest.features, &target.manifest.features);
    let manifest = ManifestDiff {
        api_version_changed: base.manifest.api_versions != target.manifest.api_versions,
        api_versions_from: base.manifest.api_versions.clone(),
        api_versions_to: target.manifest.api_versions.clone(),
        spec_from: base.manifest.spec_version.clone(),
        spec_to: target.manifest.spec_version.clone(),
        network_changed: base.manifest.network != target.manifest.network,
        network_from: base.manifest.network.clone(),
        network_to: target.manifest.network.clone(),
        features_added,
        features_removed,
        graft_from: base.manifest.graft.clone(),
        graft_to: target.manifest.graft.clone(),
    };

    let (surface_added, surface_removed) =
        set_diff(&base.totals.host_categories, &target.totals.host_categories);
    let host_surface = HostSurfaceDiff {
        added: surface_added,
        removed: surface_removed,
    };

    let (mut added, mut removed) = set_diff(&all_strings(base), &all_strings(target));
    let truncated = added.len() > STRING_CAP || removed.len() > STRING_CAP;
    added.truncate(STRING_CAP);
    removed.truncate(STRING_CAP);
    added.sort();
    removed.sort();
    let strings = StringsDiff {
        added,
        removed,
        truncated,
    };

    let identical = summary.handlers_added == 0
        && summary.handlers_removed == 0
        && summary.handlers_changed == 0
        && host_surface.added.is_empty()
        && host_surface.removed.is_empty()
        && scorecard.risk_delta == 0
        && scorecard.flags_added.is_empty()
        && scorecard.flags_removed.is_empty()
        && !manifest.api_version_changed;

    DisassemblyDiff {
        base: Side {
            deployment_id: base.deployment_id.clone(),
        },
        target: Side {
            deployment_id: target.deployment_id.clone(),
        },
        identical,
        manifest,
        scorecard,
        host_surface,
        data_sources,
        handlers,
        strings,
        summary,
    }
}
