//! How hard a subgraph is to sync, read from its manifest.
//!
//! A score out of a hundred with the reasoning attached, because the number on its own tells an
//! operator nothing they can act on: the breakdown is what says whether a subgraph is expensive
//! because it starts at block zero or because it polls every block on Arbitrum.
//!
//! Pure, and takes the YAML as a string, so every rule below is exercised by a fixture rather than
//! by finding a subgraph that happens to trigger it. #67
//!
//! **The output shape is the incumbent's**, including two things a Rust port would otherwise get
//! wrong. `blockHandlerInterval` is `Infinity` in JavaScript when a source has no block handlers,
//! and `JSON.stringify` writes that as `null`; and the human-readable `rawValue` labels carry
//! `toLocaleString()`'s thousands separators, so `25000000` reads `25,000,000`.

use serde::Serialize;
use serde_json::Value;

/// Seconds per block, for the chains the scoring is aware of.
///
/// Aliases are included because a manifest names the network however its author spelled it:
/// `xdai` and `gnosis` are the same chain, and so are `matic` and `polygon`.
const CHAIN_BLOCK_TIMES: &[(&str, f64)] = &[
    ("mainnet", 12.0),
    ("gnosis", 5.0),
    ("xdai", 5.0),
    ("celo", 5.0),
    ("bsc", 3.0),
    ("chapel", 3.0),
    ("avalanche", 2.0),
    ("fantom", 1.0),
    ("base", 2.0),
    ("optimism", 2.0),
    ("matic", 2.0),
    ("polygon", 2.0),
    ("arbitrum-one", 0.25),
    ("arbitrum-sepolia", 0.25),
    ("scroll", 3.0),
    ("linea", 3.0),
    ("zksync-era", 1.0),
    ("polygon-zkevm", 5.0),
    ("blast", 2.0),
    ("mode", 2.0),
    ("mantle", 2.0),
    ("berachain", 2.0),
    ("base-sepolia", 2.0),
    ("optimism-sepolia", 2.0),
];

/// The baseline every block range is normalised against.
const ETH_BLOCK_TIME: f64 = 12.0;

/// An unknown chain is assumed to be Ethereum-paced. Guessing faster would inflate the score of
/// every manifest naming a network this list has not heard of.
fn block_time(network: &str) -> f64 {
    CHAIN_BLOCK_TIMES
        .iter()
        .find(|(name, _)| *name == network)
        .map_or(ETH_BLOCK_TIME, |(_, secs)| *secs)
}

/// `toLocaleString()` for a whole number, which is what the incumbent's labels carry.
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Category {
    Light,
    Moderate,
    Heavy,
    Extreme,
}

fn category(score: i64) -> Category {
    match score {
        s if s < 25 => Category::Light,
        s if s < 50 => Category::Moderate,
        s if s < 75 => Category::Heavy,
        _ => Category::Extreme,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dimension {
    pub dimension: &'static str,
    pub raw_value: String,
    pub score: i64,
    pub max_score: i64,
}

/// One source's handler counts, and how often its block handlers fire.
#[derive(Debug, Clone, Copy)]
struct Handlers {
    event: usize,
    call: usize,
    block: usize,
    /// The smallest polling interval among the block handlers. Infinite when there are none, which
    /// is what makes `min` over several sources work without a special case.
    interval: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub name: String,
    pub kind: String,
    pub network: String,
    pub address: Option<String>,
    pub start_block: i64,
    pub event_handlers: usize,
    pub call_handlers: usize,
    pub block_handlers: usize,
    /// `null` when the source has no block handlers. That is JavaScript's `Infinity` surviving
    /// `JSON.stringify`, and consumers are written against it.
    #[serde(serialize_with = "finite_or_null")]
    pub block_handler_interval: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub name: String,
    pub kind: String,
    pub network: String,
    pub event_handlers: usize,
    pub call_handlers: usize,
    pub block_handlers: usize,
    #[serde(serialize_with = "finite_or_null")]
    pub block_handler_interval: f64,
}

fn finite_or_null<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() {
        s.serialize_f64(*v)
    } else {
        s.serialize_none()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Graft {
    pub base: String,
    pub block: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub description: Option<String>,
    pub repository: Option<String>,
    pub score: i64,
    pub category: Category,
    pub breakdown: Vec<Dimension>,
    pub data_sources: Vec<Source>,
    pub templates: Vec<Template>,
    pub features: Vec<String>,
    pub spec_version: String,
    pub network: String,
    pub graft: Option<Graft>,
    pub pruning: Option<String>,
    pub schema_hash: Option<String>,
}

fn as_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as i64).unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn handlers(mapping: &Value) -> Handlers {
    let len = |key: &str| {
        mapping
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    let block_list = mapping
        .get("blockHandlers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // A handler with no `filter.every` fires on every block, which is an interval of one and the
    // most expensive thing a manifest can ask for.
    let mut interval = f64::INFINITY;
    for handler in &block_list {
        match handler
            .get("filter")
            .and_then(|f| f.get("every"))
            .and_then(Value::as_f64)
        {
            Some(every) if every > 0.0 => interval = interval.min(every),
            _ => interval = 1.0,
        }
    }

    Handlers {
        event: len("eventHandlers"),
        call: len("callHandlers"),
        block: block_list.len(),
        interval,
    }
}

fn source_of(ds: &Value) -> Source {
    let source = ds.get("source").cloned().unwrap_or(Value::Null);
    let h = handlers(ds.get("mapping").unwrap_or(&Value::Null));
    Source {
        name: as_str(ds.get("name")).unwrap_or_else(|| "unknown".into()),
        kind: as_str(ds.get("kind")).unwrap_or_else(|| "unknown".into()),
        network: as_str(ds.get("network"))
            .or_else(|| as_str(source.get("network")))
            .unwrap_or_else(|| "unknown".into()),
        address: as_str(source.get("address")),
        start_block: as_i64(source.get("startBlock")),
        event_handlers: h.event,
        call_handlers: h.call,
        block_handlers: h.block,
        block_handler_interval: h.interval,
    }
}

fn template_of(tpl: &Value) -> Template {
    let source = tpl.get("source").cloned().unwrap_or(Value::Null);
    let h = handlers(tpl.get("mapping").unwrap_or(&Value::Null));
    Template {
        name: as_str(tpl.get("name")).unwrap_or_else(|| "unknown".into()),
        kind: as_str(tpl.get("kind")).unwrap_or_else(|| "unknown".into()),
        network: as_str(tpl.get("network"))
            .or_else(|| as_str(source.get("network")))
            .unwrap_or_else(|| "unknown".into()),
        event_handlers: h.event,
        call_handlers: h.call,
        block_handlers: h.block,
        block_handler_interval: h.interval,
    }
}

fn dimension(dimension: &'static str, raw_value: String, score: i64, max_score: i64) -> Dimension {
    Dimension {
        dimension,
        raw_value,
        score,
        max_score,
    }
}

/// Score the manifest, and say why.
///
/// Nine dimensions and two overrides. The overrides exist because two shapes are expensive out of
/// all proportion to what the additive score would give them: a handler on every block near the
/// chain's genesis, and one that indexes every contract on the chain rather than a named one.
fn score_of(
    sources: &[Source],
    templates: &[Template],
    features: &[String],
    pruning: Option<&str>,
    graft: Option<&Graft>,
    network: &str,
) -> (i64, Vec<Dimension>) {
    let mut breakdown = Vec::new();
    let secs = block_time(network);

    // 1. Block range, normalised to Ethereum-equivalent blocks. Twenty-five million blocks on Base
    //    is four million Ethereum blocks of work, and scoring the raw number would call every fast
    //    chain expensive for the wrong reason.
    let start = sources.iter().map(|s| s.start_block).min().unwrap_or(0);
    let eth_equivalent = (start as f64 * (secs / ETH_BLOCK_TIME)).round() as i64;
    let block_score = match eth_equivalent {
        0 => 20,
        e if e < 1_000_000 => 18,
        e if e < 5_000_000 => 14,
        e if e < 10_000_000 => 10,
        e if e < 15_000_000 => 6,
        e if e < 18_000_000 => 3,
        _ => 0,
    };
    let block_label = if secs == ETH_BLOCK_TIME {
        grouped(start)
    } else {
        format!("{} (~{} ETH-eq)", grouped(start), grouped(eth_equivalent))
    };
    breakdown.push(dimension("Block Range", block_label, block_score, 20));

    // 2. Handler types. Event handlers are the cheap case and score nothing.
    let block_intervals: Vec<f64> = sources
        .iter()
        .map(|s| (s.block_handlers, s.block_handler_interval))
        .chain(
            templates
                .iter()
                .map(|t| (t.block_handlers, t.block_handler_interval)),
        )
        .filter(|(count, _)| *count > 0)
        .map(|(_, interval)| interval)
        .collect();
    let has_block = !block_intervals.is_empty();
    let has_call = sources.iter().any(|s| s.call_handlers > 0)
        || templates.iter().any(|t| t.call_handlers > 0);
    let min_interval = block_intervals
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let polling_only = has_block && min_interval > 1.0;

    let mut handler_score = 0;
    if has_block {
        handler_score += match min_interval {
            i if i >= 1000.0 => 4,
            i if i >= 100.0 => 8,
            i if i > 1.0 => 12,
            _ => 18,
        };
    }
    if has_call {
        handler_score += 7;
    }
    let mut labels: Vec<String> = Vec::new();
    if has_block {
        labels.push(if polling_only {
            format!("block (every {})", grouped(min_interval as i64))
        } else {
            "block".into()
        });
    }
    if has_call {
        labels.push("call".into());
    }
    labels.push("event".into());
    breakdown.push(dimension(
        "Handler Types",
        labels.join(", "),
        handler_score,
        25,
    ));

    // 3. Data sources.
    let ds_count = sources.len();
    let ds_score = match ds_count {
        0..=2 => 0,
        3..=5 => 3,
        6..=10 => 6,
        _ => 10,
    };
    breakdown.push(dimension(
        "Data Sources",
        ds_count.to_string(),
        ds_score,
        10,
    ));

    // 4. Templates, which are data sources created at run time and so are unbounded in number.
    let tpl_count = templates.len();
    let tpl_score = match tpl_count {
        0 => 0,
        1..=2 => 3,
        3..=4 => 7,
        _ => 10,
    };
    breakdown.push(dimension("Templates", tpl_count.to_string(), tpl_score, 10));

    // 5. A source with no address indexes every contract on the chain that emits the event.
    let missing_address = sources.iter().any(|s| s.address.is_none());
    breakdown.push(dimension(
        "Missing Address",
        if missing_address {
            "Yes (indexes ALL contracts)".into()
        } else {
            "All specified".into()
        },
        if missing_address { 10 } else { 0 },
        10,
    ));

    // 6. Chain throughput: a faster chain is more blocks to keep up with for the same wall clock.
    let chain_score = match secs {
        s if s < 1.0 => 15,
        s if s <= 2.0 => 12,
        s if s <= 4.0 => 8,
        s if s <= 8.0 => 4,
        _ => 0,
    };
    let chain_label = if secs == ETH_BLOCK_TIME {
        format!("{secs}s blocks")
    } else {
        format!(
            "{secs}s blocks ({}x Ethereum)",
            (ETH_BLOCK_TIME / secs).round() as i64
        )
    };
    breakdown.push(dimension("Chain Throughput", chain_label, chain_score, 15));

    // 7. Features that make indexing do more than read logs.
    const HEAVY: &[&str] = &[
        "ipfsOnEthereumContracts",
        "fullTextSearch",
        "nonDeterministicIpfs",
    ];
    let heavy = features
        .iter()
        .filter(|f| HEAVY.contains(&f.as_str()))
        .count();
    breakdown.push(dimension(
        "Features",
        if features.is_empty() {
            "None".into()
        } else {
            features.join(", ")
        },
        (heavy as i64 * 2).min(5),
        5,
    ));

    // 8. Pruning. `auto` discards history the subgraph does not need; anything else keeps all of it.
    breakdown.push(dimension(
        "Pruning",
        pruning.unwrap_or("absent").to_string(),
        if pruning == Some("auto") { 0 } else { 5 },
        5,
    ));

    // 9. Grafting is informational and weighted zero: it changes where a subgraph starts, not how
    //    hard the syncing is. Kept as a row so the reason it scores nothing is visible.
    breakdown.push(dimension(
        "Graft",
        match graft {
            Some(g) => format!(
                "From {}... at block {}",
                g.base.chars().take(12).collect::<String>(),
                grouped(g.block)
            ),
            None => "None".into(),
        },
        0,
        0,
    ));

    let mut total: i64 = breakdown.iter().map(|d| d.score).sum();
    // Only a handler on *every* block triggers the override; a polling handler does not, which is
    // the distinction the additive score above cannot express.
    let raw_block = has_block && !polling_only;
    if raw_block && eth_equivalent < 5_000_000 {
        total = 100;
    }
    if missing_address && raw_block {
        total = 100;
    }
    (total.min(100), breakdown)
}

/// The IPFS hash of the schema a manifest points at, in either spelling.
///
/// `schema.file` is `{"/": "/ipfs/Qm…"}` in a modern manifest and a bare string in an old one.
fn schema_hash(doc: &Value) -> Option<String> {
    let file = doc.get("schema")?.get("file")?;
    let raw = match file {
        Value::String(s) => s.clone(),
        other => other.get("/").and_then(Value::as_str)?.to_string(),
    };
    let bytes = raw.as_bytes();
    (0..bytes.len().saturating_sub(45)).find_map(|i| {
        let candidate = raw.get(i..i + 46)?;
        (candidate.starts_with("Qm")
            && candidate
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l')))
        .then(|| candidate.to_string())
    })
}

#[derive(Debug, thiserror::Error)]
#[error("that document is not a subgraph manifest: {0}")]
pub struct NotAManifest(String);

/// Parse and score a manifest.
pub fn analyse(yaml: &str) -> Result<Analysis, NotAManifest> {
    let doc: Value = serde_yaml_ng::from_str(yaml).map_err(|e| NotAManifest(e.to_string()))?;

    let array = |key: &str| {
        doc.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let data_sources: Vec<Source> = array("dataSources").iter().map(source_of).collect();
    let templates: Vec<Template> = array("templates").iter().map(template_of).collect();
    let features: Vec<String> = array("features")
        .iter()
        .filter_map(|f| as_str(Some(f)))
        .collect();

    // The manifest's network is its first data source's, because a subgraph indexes one chain.
    let network = data_sources
        .first()
        .map_or_else(|| "unknown".to_string(), |s| s.network.clone());

    let graft = doc.get("graft").and_then(|g| {
        Some(Graft {
            base: as_str(g.get("base"))?,
            block: as_i64(g.get("block")),
        })
    });
    let pruning = as_str(doc.get("indexerHints").and_then(|h| h.get("prune")));

    let (score, breakdown) = score_of(
        &data_sources,
        &templates,
        &features,
        pruning.as_deref(),
        graft.as_ref(),
        &network,
    );

    Ok(Analysis {
        description: as_str(doc.get("description")),
        repository: as_str(doc.get("repository")),
        score,
        category: category(score),
        breakdown,
        data_sources,
        templates,
        features,
        spec_version: as_str(doc.get("specVersion")).unwrap_or_else(|| "unknown".into()),
        network,
        graft,
        pruning,
        schema_hash: schema_hash(&doc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> Analysis {
        analyse(body).expect("fixture parses")
    }

    /// The cheap shape: one named contract, event handlers only, recent start block, on Ethereum.
    const LIGHT: &str = r#"
specVersion: 0.0.5
schema:
  file:
    /: /ipfs/QmUxJ9QYbNbDTNkQPHzq4ivqzGyMbwZBvXhrQfPTLE1Pjq
indexerHints:
  prune: auto
dataSources:
  - name: Token
    kind: ethereum/contract
    network: mainnet
    source:
      address: "0xabc"
      startBlock: 19000000
    mapping:
      eventHandlers:
        - event: Transfer(address,address,uint256)
"#;

    #[test]
    fn a_recent_event_only_subgraph_is_light() {
        let a = manifest(LIGHT);
        assert_eq!(a.category, Category::Light);
        assert_eq!(a.network, "mainnet");
        assert_eq!(a.spec_version, "0.0.5");
        assert_eq!(a.pruning.as_deref(), Some("auto"));
        assert_eq!(
            a.schema_hash.as_deref(),
            Some("QmUxJ9QYbNbDTNkQPHzq4ivqzGyMbwZBvXhrQfPTLE1Pjq")
        );
        assert_eq!(a.data_sources[0].event_handlers, 1);
        assert_eq!(a.data_sources[0].block_handlers, 0);
    }

    #[test]
    fn preserves_optional_manifest_source_metadata() {
        let a = manifest(
            "description: Token balances\nrepository: https://github.com/example/token\ndataSources: []\n",
        );
        assert_eq!(a.description.as_deref(), Some("Token balances"));
        assert_eq!(
            a.repository.as_deref(),
            Some("https://github.com/example/token")
        );

        let a = manifest(LIGHT);
        assert!(a.description.is_none());
        assert!(a.repository.is_none());
    }

    /// The two overrides are the whole reason the additive score is not the last word.
    #[test]
    fn a_handler_on_every_block_near_genesis_is_extreme_whatever_else_it_scores() {
        let a = manifest(
            r#"
specVersion: 0.0.5
dataSources:
  - name: Everything
    network: mainnet
    source:
      address: "0xabc"
      startBlock: 1000
    mapping:
      blockHandlers:
        - handler: handleBlock
"#,
        );
        assert_eq!(a.score, 100);
        assert_eq!(a.category, Category::Extreme);
    }

    #[test]
    fn a_source_with_no_address_and_a_raw_block_handler_is_extreme() {
        let a = manifest(
            r#"
specVersion: 0.0.5
dataSources:
  - name: AllContracts
    network: mainnet
    source:
      startBlock: 20000000
    mapping:
      blockHandlers:
        - handler: handleBlock
"#,
        );
        assert_eq!(a.score, 100);
        assert!(a.data_sources[0].address.is_none());
    }

    /// A polling handler is not a handler on every block, and the override must not fire for it.
    #[test]
    fn a_polling_block_handler_does_not_trigger_the_override() {
        let a = manifest(
            r#"
specVersion: 0.0.5
dataSources:
  - name: Poller
    network: mainnet
    source:
      address: "0xabc"
      startBlock: 1000
    mapping:
      blockHandlers:
        - handler: handlePoll
          filter:
            every: 1000
"#,
        );
        assert!(a.score < 100, "scored {} on a 1000-block poll", a.score);
        let handlers = a
            .breakdown
            .iter()
            .find(|d| d.dimension == "Handler Types")
            .unwrap();
        assert_eq!(
            handlers.score, 4,
            "very infrequent polling is the cheap end"
        );
        assert_eq!(handlers.raw_value, "block (every 1,000), event");
    }

    /// Twenty-five million Arbitrum blocks is not twenty-five million Ethereum blocks of work, and
    /// scoring the raw number would call every fast chain expensive for the wrong reason.
    #[test]
    fn a_block_range_is_normalised_to_ethereum_before_it_is_scored() {
        let a = manifest(
            r#"
specVersion: 0.0.5
dataSources:
  - name: Fast
    network: arbitrum-one
    source:
      address: "0xabc"
      startBlock: 240000000
    mapping:
      eventHandlers:
        - event: Transfer(address,address,uint256)
"#,
        );
        let range = a
            .breakdown
            .iter()
            .find(|d| d.dimension == "Block Range")
            .unwrap();
        // 240,000,000 * (0.25/12) = 5,000,000 Ethereum-equivalent blocks.
        assert_eq!(range.raw_value, "240,000,000 (~5,000,000 ETH-eq)");
        assert_eq!(range.score, 10);

        let chain = a
            .breakdown
            .iter()
            .find(|d| d.dimension == "Chain Throughput")
            .unwrap();
        assert_eq!(chain.raw_value, "0.25s blocks (48x Ethereum)");
        assert_eq!(chain.score, 15);
    }

    /// A network nobody has a block time for is assumed Ethereum-paced. Guessing faster would
    /// inflate every manifest naming a chain this list has not heard of.
    #[test]
    fn an_unknown_network_is_scored_as_ethereum() {
        assert_eq!(block_time("hedera"), ETH_BLOCK_TIME);
        assert_eq!(block_time("xdai"), block_time("gnosis"), "aliases agree");
        assert_eq!(block_time("matic"), block_time("polygon"));
    }

    /// `Infinity` is what JavaScript computes here and `null` is what it serialises to, so a source
    /// with no block handlers must not report an interval of zero or of one.
    #[test]
    fn a_source_with_no_block_handlers_reports_a_null_interval() {
        let a = manifest(LIGHT);
        assert!(a.data_sources[0].block_handler_interval.is_infinite());
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["dataSources"][0]["blockHandlerInterval"], Value::Null);
        assert_eq!(json["dataSources"][0]["startBlock"], 19_000_000);
    }

    /// The labels carry `toLocaleString()`'s separators, which a consumer renders verbatim.
    #[test]
    fn the_labels_are_grouped_the_way_javascript_groups_them() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(25_000_000), "25,000,000");
        assert_eq!(grouped(-1_234), "-1,234");
    }

    #[test]
    fn the_wire_names_are_the_incumbents() {
        let json = serde_json::to_value(manifest(LIGHT)).unwrap();
        for key in [
            "score",
            "category",
            "breakdown",
            "dataSources",
            "templates",
            "features",
            "specVersion",
            "network",
            "graft",
            "pruning",
            "schemaHash",
        ] {
            assert!(json.get(key).is_some(), "Analysis is missing {key}");
        }
        for key in ["dimension", "rawValue", "score", "maxScore"] {
            assert!(json["breakdown"][0].get(key).is_some(), "missing {key}");
        }
        for key in [
            "name",
            "kind",
            "network",
            "address",
            "startBlock",
            "eventHandlers",
            "callHandlers",
            "blockHandlers",
            "blockHandlerInterval",
        ] {
            assert!(json["dataSources"][0].get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["category"], "Light");
    }

    #[test]
    fn a_document_that_is_not_a_manifest_is_refused_rather_than_scored() {
        assert!(analyse(": : not yaml at all : :").is_err());
        // Valid YAML that is not a manifest scores what an empty one would rather than exploding.
        let a = manifest("hello: world");
        assert_eq!(a.network, "unknown");
        assert!(a.data_sources.is_empty());
        assert!(a.schema_hash.is_none());
    }

    #[test]
    fn a_graft_is_reported_and_scores_nothing() {
        let a = manifest(
            r#"
specVersion: 0.0.5
graft:
  base: QmUxJ9QYbNbDTNkQPHzq4ivqzGyMbwZBvXhrQfPTLE1Pjq
  block: 1234567
dataSources: []
"#,
        );
        let graft = a.breakdown.iter().find(|d| d.dimension == "Graft").unwrap();
        assert_eq!(graft.score, 0);
        assert_eq!(graft.max_score, 0);
        assert_eq!(graft.raw_value, "From QmUxJ9QYbNbD... at block 1,234,567");
    }
}
