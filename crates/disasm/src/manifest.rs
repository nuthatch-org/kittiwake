//! The manifest, read for what the disassembler needs.
//!
//! `kittiwake_read::manifest` already parses one of these, for a sync-difficulty score. It never
//! surfaces the per-data-source module hash, api version, ABIs or individual handlers, which are
//! the whole input here, so this is a second and richer pass over the same YAML rather than a
//! reuse that would bend both.

use serde_yaml_ng::Value;

use crate::types::HandlerKind;

/// A CIDv0, which is the form deployment ids and manifest file references take.
pub fn is_ipfs_hash(s: &str) -> bool {
    s.len() == 46
        && s.starts_with("Qm")
        && s[2..]
            .bytes()
            .all(|b| b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains(&b))
}

/// The first CIDv0 in a string, which is how a `/ipfs/Qm…` reference carries one.
fn ipfs_ref_in(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    (0..bytes.len().saturating_sub(45)).find_map(|i| {
        let candidate = raw.get(i..i + 46)?;
        is_ipfs_hash(candidate).then(|| candidate.to_string())
    })
}

/// A manifest file reference is either `/ipfs/Qm…` or `{ "/": "/ipfs/Qm…" }`.
fn ipfs_ref(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => ipfs_ref_in(s),
        Value::Mapping(m) => m
            .get(Value::String("/".into()))
            .and_then(Value::as_str)
            .and_then(ipfs_ref_in),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHandler {
    pub handler: String,
    pub kind: HandlerKind,
    pub trigger: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedDataSource {
    pub name: String,
    pub kind: String,
    pub is_template: bool,
    pub network: String,
    pub address: Option<String>,
    pub start_block: i64,
    pub api_version: String,
    pub wasm_hash: Option<String>,
    pub abis: Vec<(String, Option<String>)>,
    pub handlers: Vec<ParsedHandler>,
}

#[derive(Debug, Clone)]
pub struct ParsedManifest {
    pub spec_version: String,
    pub network: String,
    pub features: Vec<String>,
    pub schema_hash: Option<String>,
    pub graft: Option<(String, i64)>,
    pub data_sources: Vec<ParsedDataSource>,
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

/// A scalar as a string. YAML gives numbers and booleans where the schema says string often
/// enough that reading only `as_str` loses a `network: 1` or a version written unquoted.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn text_or(value: Option<&Value>, fallback: &str) -> String {
    text(value).unwrap_or_else(|| fallback.to_string())
}

fn number(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn sequence(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::Sequence(items)) => items,
        _ => &[],
    }
}

fn handlers_of(mapping: Option<&Value>) -> Vec<ParsedHandler> {
    let Some(mapping) = mapping else {
        return Vec::new();
    };
    let mut out = Vec::new();

    for (key, kind, trigger_key) in [
        ("eventHandlers", HandlerKind::Event, "event"),
        ("callHandlers", HandlerKind::Call, "function"),
    ] {
        for entry in sequence(get(mapping, key)) {
            let Some(handler) = text(get(entry, "handler")) else {
                continue;
            };
            out.push(ParsedHandler {
                handler,
                kind,
                trigger: text(get(entry, trigger_key)),
            });
        }
    }

    for entry in sequence(get(mapping, "blockHandlers")) {
        let Some(handler) = text(get(entry, "handler")) else {
            continue;
        };
        let filter = get(entry, "filter");
        let trigger = match filter.and_then(|f| text(get(f, "kind"))) {
            Some(kind) => Some(format!("filter: {kind}")),
            None => filter
                .and_then(|f| get(f, "every"))
                .and_then(Value::as_i64)
                .map(|every| format!("every {every} blocks")),
        };
        out.push(ParsedHandler {
            handler,
            kind: HandlerKind::Block,
            trigger,
        });
    }

    out
}

fn data_source(value: &Value, is_template: bool) -> ParsedDataSource {
    let source = get(value, "source");
    let mapping = get(value, "mapping");
    ParsedDataSource {
        name: text_or(get(value, "name"), "unknown"),
        kind: text_or(get(value, "kind"), "unknown"),
        is_template,
        network: text(get(value, "network"))
            .or_else(|| source.and_then(|s| text(get(s, "network"))))
            .unwrap_or_else(|| "unknown".into()),
        address: source.and_then(|s| text(get(s, "address"))),
        start_block: number(source.and_then(|s| get(s, "startBlock"))),
        api_version: text_or(mapping.and_then(|m| get(m, "apiVersion")), "unknown"),
        wasm_hash: ipfs_ref(mapping.and_then(|m| get(m, "file"))),
        abis: sequence(mapping.and_then(|m| get(m, "abis")))
            .iter()
            .map(|abi| {
                (
                    text_or(get(abi, "name"), "unknown"),
                    ipfs_ref(get(abi, "file")),
                )
            })
            .collect(),
        handlers: handlers_of(mapping),
    }
}

#[derive(Debug)]
pub struct NotAManifest(pub String);

impl std::fmt::Display for NotAManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn parse(yaml: &str) -> Result<ParsedManifest, NotAManifest> {
    let doc: Value = serde_yaml_ng::from_str(yaml)
        .map_err(|e| NotAManifest(format!("manifest is not YAML: {e}")))?;
    if !doc.is_mapping() {
        return Err(NotAManifest("manifest is not a mapping".into()));
    }

    let mut data_sources: Vec<ParsedDataSource> = sequence(get(&doc, "dataSources"))
        .iter()
        .map(|d| data_source(d, false))
        .collect();
    data_sources.extend(
        sequence(get(&doc, "templates"))
            .iter()
            .map(|d| data_source(d, true)),
    );

    // The network of the first real data source, not of the first template: a template inherits
    // the network of whatever spawns it, and reading one first mislabels the whole report.
    let network = data_sources
        .iter()
        .find(|d| !d.is_template)
        .or_else(|| data_sources.first())
        .map(|d| d.network.clone())
        .unwrap_or_else(|| "unknown".into());

    Ok(ParsedManifest {
        spec_version: text_or(get(&doc, "specVersion"), "unknown"),
        network,
        features: sequence(get(&doc, "features"))
            .iter()
            .filter_map(|f| text(Some(f)))
            .collect(),
        schema_hash: ipfs_ref(get(&doc, "schema").and_then(|s| get(s, "file"))),
        graft: get(&doc, "graft")
            .and_then(|g| text(get(g, "base")).map(|base| (base, number(get(g, "block"))))),
        data_sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
specVersion: 0.0.5
features:
  - fullTextSearch
  - grafting
schema:
  file:
    /: /ipfs/QmTgnLRCTd1Nx1cKMuUyXqL4vfWMSwLGyKVBgrfDPBeUZG
graft:
  base: QmTgnLRCTd1Nx1cKMuUyXqL4vfWMSwLGyKVBgrfDPBeUZG
  block: 1234567
dataSources:
  - kind: ethereum/contract
    name: Registry
    network: mainnet
    source:
      address: "0xdEaDbeefdEAdbeefdEadbEEFdeadbeEFdEaDbeeF"
      startBlock: 42
    mapping:
      apiVersion: 0.0.7
      file: /ipfs/QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG
      abis:
        - name: Registry
          file: /ipfs/QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG
        - name: NoFile
      eventHandlers:
        - event: Transfer(indexed address,indexed address,uint256)
          handler: handleTransfer
      callHandlers:
        - function: setOwner(address)
          handler: handleSetOwner
      blockHandlers:
        - handler: handleBlock
          filter:
            kind: call
        - handler: handleEvery
          filter:
            every: 100
        - handler: handlePlain
templates:
  - kind: ethereum/contract
    name: Pool
    network: mainnet
    mapping:
      apiVersion: 0.0.6
      file: /ipfs/QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG
"#;

    #[test]
    fn a_manifest_yields_its_sources_templates_and_handlers() {
        let m = parse(MANIFEST).unwrap();
        assert_eq!(m.spec_version, "0.0.5");
        assert_eq!(m.network, "mainnet");
        assert_eq!(m.features, ["fullTextSearch", "grafting"]);
        assert_eq!(
            m.schema_hash.as_deref(),
            Some("QmTgnLRCTd1Nx1cKMuUyXqL4vfWMSwLGyKVBgrfDPBeUZG")
        );
        assert_eq!(m.graft.as_ref().unwrap().1, 1_234_567);

        assert_eq!(m.data_sources.len(), 2);
        let ds = &m.data_sources[0];
        assert!(!ds.is_template);
        assert_eq!(ds.start_block, 42);
        assert_eq!(ds.api_version, "0.0.7");
        assert_eq!(
            ds.wasm_hash.as_deref(),
            Some("QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG")
        );
        assert_eq!(ds.abis.len(), 2);
        assert_eq!(ds.abis[1], ("NoFile".into(), None));
        assert!(m.data_sources[1].is_template);
    }

    /// The three handler kinds and the two shapes a block filter takes. A block handler with no
    /// filter at all is still a handler, and dropping it would under-report the mapping.
    #[test]
    fn every_handler_kind_survives_with_its_trigger() {
        let ds = &parse(MANIFEST).unwrap().data_sources[0];
        let by = |name: &str| {
            ds.handlers
                .iter()
                .find(|h| h.handler == name)
                .unwrap()
                .clone()
        };
        assert_eq!(ds.handlers.len(), 5);
        assert_eq!(by("handleTransfer").kind, HandlerKind::Event);
        assert_eq!(
            by("handleTransfer").trigger.as_deref(),
            Some("Transfer(indexed address,indexed address,uint256)")
        );
        assert_eq!(by("handleSetOwner").kind, HandlerKind::Call);
        assert_eq!(
            by("handleSetOwner").trigger.as_deref(),
            Some("setOwner(address)")
        );
        assert_eq!(by("handleBlock").trigger.as_deref(), Some("filter: call"));
        assert_eq!(
            by("handleEvery").trigger.as_deref(),
            Some("every 100 blocks")
        );
        assert_eq!(by("handlePlain").trigger, None);
    }

    /// A file reference comes as a bare path or as a CID-link mapping, and both appear in the wild.
    #[test]
    fn a_file_reference_is_read_in_either_shape() {
        let m = parse(MANIFEST).unwrap();
        assert!(m.schema_hash.is_some(), "the CID-link mapping form");
        assert!(m.data_sources[0].wasm_hash.is_some(), "the bare path form");
    }

    #[test]
    fn a_cidv0_is_recognised_and_a_near_miss_is_not() {
        assert!(is_ipfs_hash(
            "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
        ));
        assert!(!is_ipfs_hash(
            "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbd"
        ));
        // `0`, `I`, `O` and `l` are not in the base58 alphabet.
        assert!(!is_ipfs_hash(
            "Qm0wAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
        ));
        assert!(!is_ipfs_hash(
            "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"
        ));
    }

    #[test]
    fn something_that_is_not_a_manifest_is_refused_rather_than_emptied() {
        assert!(parse("- just\n- a list\n").is_err());
        assert!(parse("\t\tnot: [valid").is_err());
    }
}
