//! The report's shape. Field names are the wire names, because the disassembly page reads them.

use serde::Serialize;

/// A host-API family graph-node exposes to mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HostCategory {
    Store,
    Ethereum,
    Ipfs,
    Json,
    Crypto,
    BigInt,
    BigDecimal,
    TypeConversion,
    DataSource,
    Log,
    /// `abort` and the AssemblyScript runtime. Never surfaced as a handler signal.
    Control,
    /// Not a host import at all: memory and runtime helpers.
    Other,
}

/// Ordered by the name it is serialised under, not by declaration order.
///
/// Every category list in the report is sorted, and the incumbent sorts the strings. Deriving
/// `Ord` here would order by declaration instead, which puts `store` before `ethereum` and makes
/// every category list on the page a different list from the one it replaced.
impl Ord for HostCategory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for HostCategory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl HostCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            HostCategory::Store => "store",
            HostCategory::Ethereum => "ethereum",
            HostCategory::Ipfs => "ipfs",
            HostCategory::Json => "json",
            HostCategory::Crypto => "crypto",
            HostCategory::BigInt => "bigInt",
            HostCategory::BigDecimal => "bigDecimal",
            HostCategory::TypeConversion => "typeConversion",
            HostCategory::DataSource => "dataSource",
            HostCategory::Log => "log",
            HostCategory::Control => "control",
            HostCategory::Other => "other",
        }
    }

    /// The categories that say something about a handler. `control` and `other` are runtime noise,
    /// and reporting them would drown the signal in `abort`.
    pub fn is_meaningful(self) -> bool {
        !matches!(self, HostCategory::Control | HostCategory::Other)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostImport {
    pub module: String,
    pub name: String,
    /// The canonical dotted label, `ethereum.call`.
    pub label: String,
    pub category: HostCategory,
    /// Index in the module's function index space.
    pub func_index: u32,
    pub is_host: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HandlerKind {
    Event,
    Call,
    Block,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerAnalysis {
    pub handler: String,
    pub kind: HandlerKind,
    /// Event signature, call function or block filter, for display.
    pub trigger: Option<String>,
    /// The manifest's handler was found in the export section.
    pub resolved: bool,
    pub host_imports: Vec<String>,
    pub categories: Vec<HostCategory>,
    /// Reached a `call_indirect`, so reachability past that point is under-counted.
    pub dynamic_dispatch: bool,
    /// A reachable body could not be decoded.
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmModuleInfo {
    pub wasm_hash: String,
    pub byte_size: usize,
    pub function_count: u32,
    pub imported_function_count: u32,
    pub defined_function_count: u32,
    pub host_imports: Vec<HostImport>,
    pub other_import_count: u32,
    /// Readable strings recovered from the data segments: entity types, event signatures, log
    /// formats, and the `ethereum.decode` type strings the audit below reads.
    pub strings: Vec<String>,
    pub named_functions: usize,
    pub incomplete: bool,
    pub notes: Vec<String>,
}

/// One data-segment type string that ethabi accepted and alloy does not.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodeFinding {
    pub raw: String,
    /// The same string with its whitespace made visible, because a leading space is the finding.
    pub display: String,
    /// How ethabi read it, which is what graph-node 0.41 and earlier did.
    pub ethabi: String,
    /// Why alloy refuses it, which is what graph-node 0.42 and later do.
    pub alloy_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodeStatus {
    NoImport,
    Clean,
    Divergent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodeAudit {
    pub uses_decode: bool,
    pub candidates_scanned: usize,
    pub findings: Vec<DecodeFinding>,
    pub status: DecodeStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Abi {
    pub name: String,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSourceReport {
    pub name: String,
    pub kind: String,
    pub is_template: bool,
    pub network: String,
    pub address: Option<String>,
    pub start_block: i64,
    pub api_version: String,
    pub wasm_hash: Option<String>,
    pub wasm: Option<WasmModuleInfo>,
    pub handlers: Vec<HandlerAnalysis>,
    pub abis: Vec<Abi>,
    /// Set when the module could not be fetched or decoded.
    pub error: Option<String>,
    pub decode_audit: Option<DecodeAudit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Graft {
    pub base: String,
    pub block: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployManifestInfo {
    pub spec_version: String,
    pub api_versions: Vec<String>,
    pub network: String,
    pub features: Vec<String>,
    pub schema_hash: Option<String>,
    pub graft: Option<Graft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagLevel {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskFlag {
    pub level: FlagLevel,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryScore {
    pub name: String,
    /// Nought to a hundred, higher being cleaner.
    pub score: i32,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scorecard {
    pub grade: char,
    /// Nought to a hundred, higher being riskier. The opposite way round from the category
    /// scores, which is the incumbent's choice and is kept so the page's colours do not invert.
    pub risk_score: i32,
    pub flags: Vec<RiskFlag>,
    pub categories: Vec<CategoryScore>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub data_sources: usize,
    pub templates: usize,
    pub handlers: usize,
    pub resolved_handlers: usize,
    pub wasm_bytes: usize,
    pub host_categories: Vec<HostCategory>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisassemblyReport {
    pub deployment_id: String,
    pub manifest: DeployManifestInfo,
    pub data_sources: Vec<DataSourceReport>,
    pub scorecard: Scorecard,
    pub totals: Totals,
    /// What the analysis cannot see. A transparency tool that does not list these is worse than
    /// no tool, because the reader assumes the silence means there was nothing to say.
    pub caveats: Vec<String>,
}
