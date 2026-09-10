//! Assembling the report, given a manifest and the modules it names.
//!
//! Split in two on purpose. `plan` reads the manifest and says which modules are wanted; the
//! caller fetches them; `assemble` does the rest. Nothing here touches the network, so the whole
//! analysis is a pure function of bytes and can be tested without a gateway.

use std::collections::{BTreeSet, HashMap};

use crate::manifest::{self, ParsedManifest};
use crate::types::{
    Abi, DataSourceReport, DeployManifestInfo, DisassemblyReport, Graft, HostCategory, Totals,
    WasmModuleInfo,
};
use crate::{decode_audit, scorecard, wasm};

/// Caps so a hostile manifest cannot fan out an unbounded number of fetches.
const MAX_DATA_SOURCES: usize = 60;
const MAX_UNIQUE_WASM: usize = 40;

#[derive(Debug)]
pub struct Plan {
    manifest: ParsedManifest,
    /// The data sources in scope, already capped.
    sources: usize,
    /// The unique module hashes to fetch, in manifest order and capped.
    pub modules: Vec<String>,
    caveats: Vec<String>,
}

#[derive(Debug)]
pub struct NotAManifest(pub String);

impl std::fmt::Display for NotAManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the analysis will need, read off the manifest.
pub fn plan(yaml: &str) -> Result<Plan, NotAManifest> {
    // A hash that resolves to something else entirely is the commonest mistake a caller makes,
    // and "not a subgraph manifest" is a better answer than a report of nothing.
    if !yaml.contains("dataSources") && !yaml.contains("specVersion") {
        return Err(NotAManifest(
            "Artifact at that hash is not a subgraph manifest".into(),
        ));
    }
    let parsed = manifest::parse(yaml).map_err(|e| NotAManifest(e.0))?;

    let mut caveats = vec![
        "Static analysis only: host-import reachability is computed from the deployed WASM, not by executing it.".to_string(),
    ];
    let total = parsed.data_sources.len();
    let sources = total.min(MAX_DATA_SOURCES);
    if total > MAX_DATA_SOURCES {
        caveats.push(format!(
            "Manifest has {total} data sources; analysis capped at {MAX_DATA_SOURCES}."
        ));
    }

    let mut seen = BTreeSet::new();
    let modules: Vec<String> = parsed.data_sources[..sources]
        .iter()
        .filter_map(|d| d.wasm_hash.clone())
        .filter(|hash| seen.insert(hash.clone()))
        .take(MAX_UNIQUE_WASM)
        .collect();

    Ok(Plan {
        manifest: parsed,
        sources,
        modules,
        caveats,
    })
}

/// What a fetch produced: the bytes, or why there are none.
pub type Fetched = HashMap<String, Result<Vec<u8>, String>>;

pub fn assemble(deployment_id: &str, plan: Plan, fetched: &Fetched) -> DisassemblyReport {
    let Plan {
        manifest,
        sources,
        modules: _,
        mut caveats,
    } = plan;

    let parsed: HashMap<&str, Result<wasm::ParsedWasm, String>> = fetched
        .iter()
        .map(|(hash, bytes)| {
            let result = match bytes {
                Ok(bytes) => wasm::parse(bytes, hash).map_err(|e| e.0),
                Err(why) => Err(why.clone()),
            };
            (hash.as_str(), result)
        })
        .collect();

    let data_sources: Vec<DataSourceReport> = manifest.data_sources[..sources]
        .iter()
        .map(|ds| {
            let abis = ds
                .abis
                .iter()
                .map(|(name, hash)| Abi {
                    name: name.clone(),
                    hash: hash.clone(),
                })
                .collect();
            let mut report = DataSourceReport {
                name: ds.name.clone(),
                kind: ds.kind.clone(),
                is_template: ds.is_template,
                network: ds.network.clone(),
                address: ds.address.clone(),
                start_block: ds.start_block,
                api_version: ds.api_version.clone(),
                wasm_hash: ds.wasm_hash.clone(),
                wasm: None,
                handlers: Vec::new(),
                abis,
                error: None,
                decode_audit: None,
            };

            let Some(hash) = ds.wasm_hash.as_deref() else {
                report.error = Some("No mapping WASM referenced in manifest".into());
                return report;
            };
            match parsed.get(hash) {
                None => report.error = Some("Mapping WASM not fetched (capped)".into()),
                Some(Err(why)) => report.error = Some(why.clone()),
                Some(Ok(module)) => {
                    report.handlers = ds
                        .handlers
                        .iter()
                        .map(|h| wasm::analyse_handler(module, h))
                        .collect();
                    report.decode_audit = Some(decode_audit::audit(&module.info));
                    report.wasm = Some(module.info.clone());
                }
            }
            report
        })
        .collect();

    if data_sources
        .iter()
        .any(|d| d.wasm.as_ref().is_some_and(|w| w.incomplete))
    {
        caveats.push(
            "At least one module could not be fully decoded; some reachability is partial.".into(),
        );
    }
    if data_sources
        .iter()
        .any(|d| d.handlers.iter().any(|h| h.dynamic_dispatch))
    {
        caveats.push("AssemblyScript uses dynamic dispatch (call_indirect) pervasively; reachability is computed over direct calls and may under-count paths that route exclusively through indirect dispatch.".into());
    }
    let unresolved: usize = data_sources
        .iter()
        .filter(|d| d.wasm.is_some())
        .map(|d| d.handlers.iter().filter(|h| !h.resolved).count())
        .sum();
    if unresolved > 0 {
        caveats.push(format!(
            "{unresolved} manifest handler(s) were not found as exports in their WASM (possible apiVersion/naming skew)."
        ));
    }
    if data_sources
        .iter()
        .filter_map(|d| d.decode_audit.as_ref())
        .any(|a| a.uses_decode)
    {
        caveats.push("Decode Compatibility Audit is a static data-segment scan with no dataflow, so flagged type strings may not be the exact argument passed to ethereum.decode, and dynamically built type strings are not detected.".into());
    }

    let scorecard = scorecard::build(&manifest, &data_sources);

    let mut api_versions: Vec<String> = manifest.data_sources[..sources]
        .iter()
        .map(|d| d.api_version.clone())
        .collect();
    api_versions.sort();
    api_versions.dedup();

    let host_categories: Vec<HostCategory> = data_sources
        .iter()
        .flat_map(|d| &d.handlers)
        .flat_map(|h| &h.categories)
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let totals = Totals {
        data_sources: data_sources.iter().filter(|d| !d.is_template).count(),
        templates: data_sources.iter().filter(|d| d.is_template).count(),
        handlers: data_sources.iter().map(|d| d.handlers.len()).sum(),
        resolved_handlers: data_sources
            .iter()
            .map(|d| d.handlers.iter().filter(|h| h.resolved).count())
            .sum(),
        // Each module once, however many data sources share it: this is the bytes an indexer
        // fetches, not the sum of what every source points at.
        wasm_bytes: parsed
            .values()
            .filter_map(|m| m.as_ref().ok())
            .map(|m| m.info.byte_size)
            .sum(),
        host_categories,
    };

    DisassemblyReport {
        deployment_id: deployment_id.to_string(),
        manifest: DeployManifestInfo {
            spec_version: manifest.spec_version,
            api_versions,
            network: manifest.network,
            features: manifest.features,
            schema_hash: manifest.schema_hash,
            graft: manifest.graft.map(|(base, block)| Graft { base, block }),
        },
        data_sources,
        scorecard,
        totals,
        caveats,
    }
}

/// Every module info in a report, for the diff's string comparison.
pub fn modules(report: &DisassemblyReport) -> impl Iterator<Item = &WasmModuleInfo> {
    report.data_sources.iter().filter_map(|d| d.wasm.as_ref())
}
