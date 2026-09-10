//! The `ethereum.decode` compatibility audit for the graph-node 0.42 alloy migration.
//!
//! graph-node 0.42 (graphprotocol/graph-node#6063) swapped the ABI type-string parser from ethabi
//! to alloy. alloy is strict where ethabi was absurdly lenient, so `ethereum.decode(type, data)`
//! whose type string ethabi accepted and alloy rejects now returns null - usually as silent data
//! loss rather than an error. The type string is a compile-time constant in the mapping's data
//! segment, so an affected deployment is detectable from bytes we already have.
//!
//! The incumbent compiles the real ethabi and alloy parsers to WebAssembly and loads the artefact
//! from Node, and degrades to "unavailable" when that load fails. Here they are ordinary
//! dependencies, so there is no artefact, no loader and no unavailable state: the audit either
//! ran or the module does not import `decode`.
//!
//! A caveat that survives the move: alloy's own strictness can change between releases, and the
//! version graph-node pins is not necessarily this one. The vectors below are the cases the bug
//! reports name, and they are asserted rather than assumed.

use std::sync::OnceLock;

use alloy_dyn_abi::DynSolType;
use ethabi::param_type::Reader;
use regex::Regex;

use crate::types::{DecodeAudit, DecodeFinding, DecodeStatus, HostCategory, WasmModuleInfo};

/// True when a type string breaks on graph-node >= 0.42: ethabi accepts it, alloy does not.
pub fn is_divergent(s: &str) -> bool {
    Reader::read(s).is_ok() && DynSolType::parse(s).is_err()
}

/// How each parser reads one type string.
pub struct Classification {
    /// How ethabi read it, or `None` if even ethabi refused.
    pub ethabi: Option<String>,
    pub alloy_error: Option<String>,
}

pub fn classify(s: &str) -> Classification {
    Classification {
        ethabi: Reader::read(s).ok().map(|pt| format!("{pt:?}")),
        alloy_error: DynSolType::parse(s).err().map(|e| e.to_string()),
    }
}

/// An ABI-ish shape, bare type or tuple. Deliberately loose: a false positive costs a line in a
/// panel labelled as a static scan, and a false negative costs a deployment nobody warns.
fn candidate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)\A\s*\(?\s*(u?int[0-9]*|bytes[0-9]*|address|bool|string|fixed[0-9x]*|ufixed[0-9x]*)(\[[0-9]*\])*(\s*,\s*\(?[a-zA-Z0-9\[\](), ]*\)?)*\s*\)?\s*\z",
        )
        .expect("constant")
    })
}

/// At least one real ABI keyword, to cut generic-string noise.
fn keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"uint|int|bytes|address|bool|string").expect("constant"))
}

fn trailing_junk_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^A-Za-z0-9\])]+\z").expect("constant"))
}

const MIN_LEN: usize = 4;
const MAX_LEN: usize = 2048;

/// The slice from the first `(` to its matching `)`, or nothing.
///
/// Recovers a clean tuple from a data-segment string that picked up junk on either side, which
/// happens whenever a printable object-header byte lands next to a constant in the reconstructed
/// image.
fn balanced_tuple(s: &str) -> Option<&str> {
    // Every string here came from a printable-ASCII scan, so byte offsets are character offsets.
    let open = s.find('(')?;
    let mut depth = 0usize;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open..open + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The one cleaned form of a recovered string worth classifying, which is not the raw string.
///
/// Memory reconstruction routinely joins a printable header byte onto a constant, and ethabi
/// silently parses *any* unparseable string as `uint8`, which alloy rejects. So the raw
/// junk-suffixed form manufactures a divergence that is not there. Leading whitespace is kept,
/// because a leading space is itself the divergence in graphprotocol/graph-node#6461.
fn candidate_form(s: &str) -> Option<String> {
    if let Some(tuple) = balanced_tuple(s) {
        return Some(tuple.to_string());
    }
    let trimmed = trailing_junk_re().replace(s, "");
    (!trimmed.is_empty()).then(|| trimmed.into_owned())
}

/// The ABI-shaped strings out of everything the data segments held.
pub fn candidates(strings: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for s in strings {
        if s.len() < MIN_LEN || s.len() > MAX_LEN {
            continue;
        }
        let Some(form) = candidate_form(s) else {
            continue;
        };
        if form.len() < MIN_LEN || form.len() > MAX_LEN {
            continue;
        }
        if !keyword_re().is_match(&form) || !candidate_re().is_match(&form) {
            continue;
        }
        if seen.insert(form.clone()) {
            out.push(form);
        }
    }
    out
}

/// Whitespace made visible, so a leading or trailing space is obvious on the page.
pub fn visible_whitespace(s: &str) -> String {
    s.replace(' ', "\u{b7}").replace('\t', "\u{2192}")
}

/// Audit one module.
///
/// Gated on the module actually importing `ethereum.decode`: a divergent-looking string in a
/// module that never decodes is almost certainly an embedded ABI definition, and warning about it
/// would train the reader to ignore the panel.
pub fn audit(wasm: &WasmModuleInfo) -> DecodeAudit {
    // The import shape varies between graph-ts versions, so match on the recognised category plus
    // a `decode` field rather than on one spelling of the label.
    let uses_decode = wasm.host_imports.iter().any(|h| {
        h.category == HostCategory::Ethereum && (h.name == "decode" || h.name.ends_with(".decode"))
    });
    if !uses_decode {
        return DecodeAudit {
            uses_decode: false,
            candidates_scanned: 0,
            findings: Vec::new(),
            status: DecodeStatus::NoImport,
        };
    }

    let candidates = candidates(&wasm.strings);
    let findings: Vec<DecodeFinding> = candidates
        .iter()
        .filter_map(|raw| {
            let c = classify(raw);
            let ethabi = c.ethabi?;
            let alloy_reason = c.alloy_error?;
            Some(DecodeFinding {
                raw: raw.clone(),
                display: visible_whitespace(raw),
                ethabi,
                alloy_reason,
            })
        })
        .collect();

    DecodeAudit {
        uses_decode: true,
        candidates_scanned: candidates.len(),
        status: if findings.is_empty() {
            DecodeStatus::Clean
        } else {
            DecodeStatus::Divergent
        },
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divergent_vectors() {
        for s in [
            "bytes128",
            "bytes33",
            "bytes0",
            "(uint32,uint32,uint32,uint64,bytes32,bytes32,bytes32,bytes128)",
            " address",
            "address ",
            "uint255",
            "uint257",
            "uint8[0]",
            "address[",
        ] {
            assert!(is_divergent(s), "expected DIVERGENT: {s:?}");
        }
    }

    #[test]
    fn fine_vectors() {
        for s in [
            "uint",
            "int",
            "uint32",
            "string",
            "bool[2]",
            "uint256[]",
            "(uint256,address)",
            "(address)",
            "()",
            "tuple(uint256,address)",
        ] {
            assert!(!is_divergent(s), "expected NOT divergent: {s:?}");
        }
    }

    #[test]
    fn ethabi_leniency_is_preserved() {
        assert_eq!(
            Reader::read(" address").unwrap(),
            ethabi::ParamType::Uint(8)
        );
        assert_eq!(
            Reader::read("address[").unwrap(),
            ethabi::ParamType::Uint(8)
        );
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::types::HostImport;

    fn module(names: &[&str], strings: &[&str]) -> WasmModuleInfo {
        WasmModuleInfo {
            wasm_hash: "QmTest".into(),
            byte_size: 0,
            function_count: 0,
            imported_function_count: 0,
            defined_function_count: 0,
            host_imports: names
                .iter()
                .enumerate()
                .map(|(i, name)| HostImport {
                    module: "ethereum".into(),
                    name: (*name).into(),
                    label: format!("ethereum.{name}"),
                    category: HostCategory::Ethereum,
                    func_index: i as u32,
                    is_host: true,
                })
                .collect(),
            other_import_count: 0,
            strings: strings.iter().map(|s| (*s).to_string()).collect(),
            named_functions: 0,
            incomplete: false,
            notes: Vec::new(),
        }
    }

    /// The two real bug reports. `bytes128` is the CCTP case in graph-node#6683 and the leading
    /// space is #6461; both decode to null on 0.42 and neither raises an error.
    #[test]
    fn the_reported_divergences_are_found() {
        let audit = audit(&module(
            &["decode"],
            &[
                "(uint32,uint32,uint32,uint64,bytes32,bytes32,bytes32,bytes128)",
                " address",
                "(uint256,address)",
            ],
        ));
        assert_eq!(audit.status, DecodeStatus::Divergent);
        assert_eq!(audit.findings.len(), 2, "{:?}", audit.findings);
        let leading = audit
            .findings
            .iter()
            .find(|f| f.raw == " address")
            .expect("the leading-space case");
        assert_eq!(leading.display, "\u{b7}address");
        assert_eq!(leading.ethabi, "Uint(8)");
        assert!(!leading.alloy_reason.is_empty());
    }

    /// The gate. A module that never decodes gets no findings whatever its data segments hold,
    /// because an embedded ABI definition is not a call.
    #[test]
    fn a_module_that_does_not_decode_is_not_audited() {
        let audit = audit(&module(&["call"], &["bytes128", " address"]));
        assert_eq!(audit.status, DecodeStatus::NoImport);
        assert!(!audit.uses_decode);
        assert_eq!(audit.candidates_scanned, 0);
    }

    /// The other import spelling. graph-ts has emitted all three over the years, and matching one
    /// of them would silently skip the audit for the rest.
    #[test]
    fn the_import_is_recognised_however_it_is_spelled() {
        for name in ["decode", "ethereum.decode"] {
            assert!(audit(&module(&[name], &["uint256"])).uses_decode, "{name}");
        }
    }

    /// The trap that made the incumbent invent findings. Memory reconstruction joins a printable
    /// byte onto a constant, ethabi parses literally anything as uint8, and the junk-suffixed form
    /// therefore looks divergent when the real argument is fine.
    #[test]
    fn junk_picked_up_from_neighbouring_memory_is_trimmed_before_classifying() {
        let audit = audit(&module(&["decode"], &["(uint256,address)L", "string,"]));
        assert_eq!(audit.status, DecodeStatus::Clean, "{:?}", audit.findings);
    }

    #[test]
    fn only_abi_shaped_strings_are_classified() {
        let picked = candidates(&[
            "uint256".into(),
            "handleTransfer".into(),
            "Transfer(indexed address,indexed address,uint256)".into(),
            "(uint256,address)".into(),
            "ab".into(),
        ]);
        assert!(picked.contains(&"uint256".to_string()));
        assert!(picked.contains(&"(uint256,address)".to_string()));
        assert!(!picked.contains(&"handleTransfer".to_string()));
        assert!(!picked.contains(&"ab".to_string()));
    }

    /// One string, one candidate, however many times it appears.
    #[test]
    fn candidates_are_deduplicated() {
        assert_eq!(candidates(&["uint256".into(), "uint256".into()]).len(), 1);
    }
}
