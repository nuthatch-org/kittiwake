//! The risk scorecard: determinism, sync cost and index surface, from the manifest and from what
//! the handlers can reach.
//!
//! Every flag is grounded in a real graph-node failure or cost mode rather than in taste. Fulltext
//! is non-deterministic and a known fatal panic; `eth_call` dominates sync time; a data source
//! with no address matches every contract on the chain.

use crate::manifest::ParsedManifest;
use crate::types::{
    CategoryScore, DataSourceReport, FlagLevel, HandlerKind, HostCategory, RiskFlag, Scorecard,
};

fn weight(level: FlagLevel) -> i32 {
    match level {
        FlagLevel::Critical => 30,
        FlagLevel::Warn => 12,
        FlagLevel::Info => 3,
    }
}

fn grade_for(risk: i32) -> char {
    match risk {
        ..10 => 'A',
        10..25 => 'B',
        25..50 => 'C',
        50..75 => 'D',
        _ => 'F',
    }
}

fn clamp(n: i32) -> i32 {
    n.clamp(0, 100)
}

/// A block number with thousands separators, because the incumbent renders this through
/// `Number.prototype.toLocaleString` and the string is what the page shows.
fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The first fourteen characters of a hash, which is how the incumbent abbreviates one.
fn short(s: &str) -> String {
    s.chars().take(14).collect()
}

fn handlers_reaching(sources: &[DataSourceReport], category: HostCategory) -> usize {
    sources
        .iter()
        .flat_map(|d| &d.handlers)
        .filter(|h| h.categories.contains(&category))
        .count()
}

fn any_handler(sources: &[DataSourceReport], kind: HandlerKind) -> bool {
    sources
        .iter()
        .flat_map(|d| &d.handlers)
        .any(|h| h.kind == kind)
}

fn flag(level: FlagLevel, title: &str, detail: String) -> RiskFlag {
    RiskFlag {
        level,
        title: title.to_string(),
        detail,
    }
}

pub fn build(manifest: &ParsedManifest, sources: &[DataSourceReport]) -> Scorecard {
    let eth_call_handlers = handlers_reaching(sources, HostCategory::Ethereum);
    let ipfs_handlers = handlers_reaching(sources, HostCategory::Ipfs);
    let dynamic_handlers = handlers_reaching(sources, HostCategory::DataSource);

    let has_call_handlers = any_handler(sources, HandlerKind::Call);
    let has_block_handlers = any_handler(sources, HandlerKind::Block);
    // A data source with no address and event handlers matches every contract on the chain.
    // Templates are excluded because theirs is filled in when one is spawned.
    let wildcard = sources.iter().any(|d| {
        !d.is_template
            && d.address.is_none()
            && d.handlers.iter().any(|h| h.kind == HandlerKind::Event)
    });

    let feature = |name: &str| manifest.features.iter().any(|f| f == name);
    let has_fulltext = feature("fullTextSearch");
    let non_deterministic_ipfs =
        feature("nonDeterministicIpfs") || feature("ipfsOnEthereumContracts");

    let mut flags: Vec<RiskFlag> = Vec::new();

    if has_fulltext {
        flags.push(flag(
            FlagLevel::Critical,
            "Full-text search enabled",
            "Fulltext indexing is not deterministic and is a known fatal-panic source in graph-node.".into(),
        ));
    }
    if ipfs_handlers > 0 || non_deterministic_ipfs {
        flags.push(flag(
            FlagLevel::Warn,
            "IPFS / file-data access",
            format!("{ipfs_handlers} handler(s) reach ipfs.*; file data sources and on-chain IPFS reads are non-deterministic."),
        ));
    }
    if let Some((base, block)) = &manifest.graft {
        flags.push(flag(
            FlagLevel::Info,
            "Grafted deployment",
            format!(
                "Built on {}\u{2026} at block {}; history is inherited, not re-derived.",
                short(base),
                thousands(*block)
            ),
        ));
    }

    if eth_call_handlers > 0 {
        flags.push(flag(
            FlagLevel::Warn,
            "eth_call in handlers",
            format!("{eth_call_handlers} handler(s) reach ethereum.call; synchronous contract reads dominate indexing time and add revert/determinism risk."),
        ));
    }
    if has_block_handlers {
        flags.push(flag(
            FlagLevel::Warn,
            "Block handlers",
            "Block handlers run per matching block and can sharply increase sync cost; call-filtered variants also require trace data.".into(),
        ));
    }
    if has_call_handlers {
        flags.push(flag(
            FlagLevel::Info,
            "Call handlers",
            "Call handlers require transaction call traces (EXTENDED Firehose / trace-capable RPC), which is heavier to index.".into(),
        ));
    }

    if wildcard {
        flags.push(flag(
            FlagLevel::Warn,
            "Wildcard indexing",
            "A data source has no source.address, so it matches events from ALL contracts, a large index surface.".into(),
        ));
    }
    if dynamic_handlers > 0 {
        flags.push(flag(
            FlagLevel::Info,
            "Dynamic data sources",
            format!("{dynamic_handlers} handler(s) reach dataSource.create*; templates are spawned at runtime, so the indexed surface grows during sync."),
        ));
    }
    if feature("nonFatalErrors") {
        flags.push(flag(
            FlagLevel::Info,
            "Non-fatal errors",
            "Subgraph continues past handler errors; data may be partial without halting.".into(),
        ));
    }

    let risk_score = clamp(flags.iter().map(|f| weight(f.level)).sum());

    let mut determinism = 100;
    if has_fulltext {
        determinism -= 45;
    }
    if ipfs_handlers > 0 || non_deterministic_ipfs {
        determinism -= 30;
    }
    if eth_call_handlers > 0 {
        determinism -= 12;
    }
    if manifest.graft.is_some() {
        determinism -= 8;
    }

    let mut performance = 100;
    if eth_call_handlers > 0 {
        performance -= 45.min(12 + eth_call_handlers as i32 * 4);
    }
    if has_block_handlers {
        performance -= 20;
    }
    if has_call_handlers {
        performance -= 12;
    }
    if wildcard {
        performance -= 15;
    }

    // How legible the deployed artefact is. A stripped module is not a fault, but it is a thing
    // the reader should know about the analysis they are being shown.
    let named: usize = sources
        .iter()
        .filter_map(|d| d.wasm.as_ref())
        .map(|w| w.named_functions)
        .sum();
    let any_incomplete = sources
        .iter()
        .filter_map(|d| d.wasm.as_ref())
        .any(|w| w.incomplete);
    let mut transparency = if named > 0 { 85 } else { 55 };
    if any_incomplete {
        transparency -= 20;
    }

    let categories = vec![
        CategoryScore {
            name: "Determinism".into(),
            score: clamp(determinism),
            note: if has_fulltext || ipfs_handlers > 0 || non_deterministic_ipfs {
                "Non-deterministic surface present".into()
            } else {
                "No non-deterministic host usage found".into()
            },
        },
        CategoryScore {
            name: "Performance".into(),
            score: clamp(performance),
            note: if eth_call_handlers > 0 {
                "Synchronous eth_calls in handlers".into()
            } else {
                "No eth_call hotspots found".into()
            },
        },
        CategoryScore {
            name: "Transparency".into(),
            score: clamp(transparency),
            note: if named > 0 {
                "Function names recovered from name section".into()
            } else {
                "Sparse name section".into()
            },
        },
    ];

    Scorecard {
        grade: grade_for(risk_score),
        risk_score,
        flags,
        categories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HandlerAnalysis, WasmModuleInfo};

    fn manifest(features: &[&str], graft: Option<(&str, i64)>) -> ParsedManifest {
        ParsedManifest {
            spec_version: "0.0.5".into(),
            network: "mainnet".into(),
            features: features.iter().map(|f| (*f).to_string()).collect(),
            schema_hash: None,
            graft: graft.map(|(b, n)| (b.to_string(), n)),
            data_sources: Vec::new(),
        }
    }

    fn handler(kind: HandlerKind, categories: &[HostCategory]) -> HandlerAnalysis {
        HandlerAnalysis {
            handler: "h".into(),
            kind,
            trigger: None,
            resolved: true,
            host_imports: Vec::new(),
            categories: categories.to_vec(),
            dynamic_dispatch: false,
            incomplete: false,
        }
    }

    fn source(address: Option<&str>, handlers: Vec<HandlerAnalysis>) -> DataSourceReport {
        DataSourceReport {
            name: "Registry".into(),
            kind: "ethereum/contract".into(),
            is_template: false,
            network: "mainnet".into(),
            address: address.map(str::to_string),
            start_block: 0,
            api_version: "0.0.7".into(),
            wasm_hash: Some("QmTest".into()),
            wasm: Some(WasmModuleInfo {
                wasm_hash: "QmTest".into(),
                byte_size: 100,
                function_count: 1,
                imported_function_count: 0,
                defined_function_count: 1,
                host_imports: Vec::new(),
                other_import_count: 0,
                strings: Vec::new(),
                named_functions: 3,
                incomplete: false,
                notes: Vec::new(),
            }),
            handlers,
            abis: Vec::new(),
            error: None,
            decode_audit: None,
        }
    }

    #[test]
    fn a_plain_subgraph_scores_an_a_and_carries_no_flags() {
        let card = build(
            &manifest(&[], None),
            &[source(
                Some("0xabc"),
                vec![handler(HandlerKind::Event, &[HostCategory::Store])],
            )],
        );
        assert_eq!(card.grade, 'A');
        assert_eq!(card.risk_score, 0);
        assert!(card.flags.is_empty());
        assert_eq!(card.categories[0].score, 100);
    }

    /// Fulltext alone is thirty points and a critical flag, because it is a fatal panic rather
    /// than a slow sync.
    #[test]
    fn fulltext_is_critical_on_its_own() {
        let card = build(&manifest(&["fullTextSearch"], None), &[]);
        assert_eq!(card.risk_score, 30);
        assert_eq!(card.grade, 'C');
        assert_eq!(card.flags[0].level, FlagLevel::Critical);
        assert_eq!(card.categories[0].score, 55);
    }

    /// A data source with no address and event handlers indexes the whole chain. A template with
    /// no address does not, because one is filled in when it is spawned, and flagging it would
    /// put a warning on most subgraphs that use templates at all.
    #[test]
    fn a_wildcard_source_is_flagged_and_a_template_is_not() {
        let wild = build(
            &manifest(&[], None),
            &[source(None, vec![handler(HandlerKind::Event, &[])])],
        );
        assert!(wild.flags.iter().any(|f| f.title == "Wildcard indexing"));

        let mut template = source(None, vec![handler(HandlerKind::Event, &[])]);
        template.is_template = true;
        let card = build(&manifest(&[], None), &[template]);
        assert!(!card.flags.iter().any(|f| f.title == "Wildcard indexing"));
    }

    /// The eth_call penalty grows with the number of handlers and then stops, so a subgraph with
    /// forty of them does not score below one with twelve.
    #[test]
    fn the_eth_call_penalty_is_capped() {
        let one = build(
            &manifest(&[], None),
            &[source(
                Some("0xabc"),
                vec![handler(HandlerKind::Event, &[HostCategory::Ethereum])],
            )],
        );
        let many = build(
            &manifest(&[], None),
            &[source(
                Some("0xabc"),
                (0..40)
                    .map(|_| handler(HandlerKind::Event, &[HostCategory::Ethereum]))
                    .collect(),
            )],
        );
        assert_eq!(one.categories[1].score, 84);
        assert_eq!(many.categories[1].score, 55, "capped at 45 off");
    }

    /// Every flag at once. Worth pinning as a number: the weights sum to ninety rather than to a
    /// hundred, so the scale never reaches its own top and an F starts at seventy-five.
    #[test]
    fn every_flag_firing_is_ninety_and_an_f() {
        let card = build(
            &manifest(
                &["fullTextSearch", "nonDeterministicIpfs", "nonFatalErrors"],
                Some(("QmBase", 1)),
            ),
            &[source(
                None,
                vec![
                    handler(
                        HandlerKind::Event,
                        &[HostCategory::Ethereum, HostCategory::Ipfs],
                    ),
                    handler(HandlerKind::Block, &[HostCategory::DataSource]),
                    handler(HandlerKind::Call, &[]),
                ],
            )],
        );
        assert_eq!(card.flags.len(), 9, "every flag fires");
        assert_eq!(card.risk_score, 90);
        assert_eq!(card.grade, 'F');
    }

    /// The block number is rendered the way the page renders it, separators and all.
    #[test]
    fn a_graft_names_its_base_and_block_the_way_the_page_shows_them() {
        let card = build(
            &manifest(
                &[],
                Some(("QmTgnLRCTd1Nx1cKMuUyXqL4vfWMSwLGyKVBgrfDPBeUZG", 1_234_567)),
            ),
            &[],
        );
        let graft = card
            .flags
            .iter()
            .find(|f| f.title == "Grafted deployment")
            .unwrap();
        assert_eq!(
            graft.detail,
            "Built on QmTgnLRCTd1Nx1\u{2026} at block 1,234,567; history is inherited, not re-derived."
        );
    }

    #[test]
    fn thousands_separators_match_the_javascript() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-1_234), "-1,234");
    }

    /// A module with no name section is legible enough to analyse and less legible to read, and
    /// the score says so rather than the report staying silent.
    #[test]
    fn a_stripped_module_costs_transparency() {
        let mut stripped = source(Some("0xabc"), Vec::new());
        stripped.wasm.as_mut().unwrap().named_functions = 0;
        let card = build(&manifest(&[], None), &[stripped]);
        assert_eq!(card.categories[2].score, 55);
        assert_eq!(card.categories[2].note, "Sparse name section");
    }
}
