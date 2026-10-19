//! A manifest and its modules, all the way through to a report and a diff between two of them.

use std::collections::HashMap;

use kittiwake_disasm::diff::{self, HandlerStatus};
use kittiwake_disasm::report::{self, Fetched};
use kittiwake_disasm::types::{DisassemblyReport, HostCategory};

const WASM_A: &str = "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
const WASM_B: &str = "QmTgnLRCTd1Nx1cKMuUyXqL4vfWMSwLGyKVBgrfDPBeUZG";

fn manifest(wasm: &str, extra_handler: bool) -> String {
    let extra = if extra_handler {
        "\n        - event: Approval(address)\n          handler: handleApproval"
    } else {
        ""
    };
    format!(
        r#"specVersion: 0.0.5
schema:
  file: /ipfs/{WASM_B}
dataSources:
  - kind: ethereum/contract
    name: Registry
    network: mainnet
    source:
      address: "0xdEaDbeef"
      startBlock: 42
    mapping:
      apiVersion: 0.0.7
      file: /ipfs/{wasm}
      eventHandlers:
        - event: Transfer(address,address,uint256)
          handler: handleTransfer{extra}
"#
    )
}

/// A module whose `handleTransfer` reaches the given host functions.
fn module(imports: &[(&str, &str)], also_approval: bool) -> Vec<u8> {
    let mut wat = String::from("(module\n");
    for (i, (module_name, name)) in imports.iter().enumerate() {
        wat.push_str(&format!(
            "  (import \"{module_name}\" \"{name}\" (func $f{i}))\n"
        ));
    }
    let calls: String = (0..imports.len())
        .map(|i| format!(" (call $f{i})"))
        .collect();
    wat.push_str(&format!("  (func $handleTransfer{calls})\n"));
    wat.push_str("  (export \"handleTransfer\" (func $handleTransfer))\n");
    if also_approval {
        wat.push_str(
            "  (func $handleApproval)\n  (export \"handleApproval\" (func $handleApproval))\n",
        );
    }
    wat.push(')');
    wat::parse_str(&wat).expect("test module assembles")
}

fn build(yaml: &str, bytes: Vec<u8>) -> DisassemblyReport {
    let plan = report::plan(yaml).expect("a manifest");
    let mut fetched: Fetched = HashMap::new();
    for hash in &plan.modules {
        fetched.insert(hash.clone(), Ok(bytes.clone()));
    }
    report::assemble("QmDeployment", plan, &fetched)
}

#[test]
fn a_manifest_and_its_module_become_a_report() {
    let r = build(
        &manifest(WASM_A, false),
        module(&[("store", "set"), ("ethereum", "call")], false),
    );

    assert_eq!(r.deployment_id, "QmDeployment");
    assert_eq!(r.manifest.spec_version, "0.0.5");
    assert_eq!(r.manifest.api_versions, ["0.0.7"]);
    assert_eq!(r.totals.data_sources, 1);
    assert_eq!(r.totals.handlers, 1);
    assert_eq!(r.totals.resolved_handlers, 1);
    assert_eq!(
        r.totals.host_categories,
        [HostCategory::Ethereum, HostCategory::Store]
    );

    let handler = &r.data_sources[0].handlers[0];
    assert!(handler.resolved);
    assert_eq!(handler.host_imports, ["ethereum.call", "store.set"]);

    // The eth_call flag, and the one caveat every report carries.
    assert!(
        r.scorecard
            .flags
            .iter()
            .any(|f| f.title == "eth_call in handlers")
    );
    assert!(r.caveats[0].starts_with("Static analysis only"));
}

/// A module that cannot be fetched is reported as that data source's error, and the rest of the
/// report is still produced. A gateway having a bad minute must not blank the page.
#[test]
fn a_module_that_could_not_be_fetched_is_named_rather_than_hidden() {
    let plan = report::plan(&manifest(WASM_A, false)).unwrap();
    let mut fetched: Fetched = HashMap::new();
    fetched.insert(WASM_A.to_string(), Err("ipfs cat: HTTP 504".into()));
    let r = report::assemble("QmDeployment", plan, &fetched);

    assert_eq!(
        r.data_sources[0].error.as_deref(),
        Some("ipfs cat: HTTP 504")
    );
    assert!(r.data_sources[0].wasm.is_none());
    assert!(r.data_sources[0].handlers.is_empty());
    assert_eq!(r.totals.wasm_bytes, 0);
}

/// A manifest handler with no matching export. The caveat is the point: silence here would read
/// as "this handler reaches nothing", which is a different and much more comforting claim.
#[test]
fn an_unresolved_handler_earns_a_caveat() {
    let r = build(&manifest(WASM_A, true), module(&[("store", "set")], false));
    let handler = r.data_sources[0]
        .handlers
        .iter()
        .find(|h| h.handler == "handleApproval")
        .unwrap();
    assert!(!handler.resolved);
    assert!(
        r.caveats
            .iter()
            .any(|c| c.contains("1 manifest handler(s) were not found")),
        "{:?}",
        r.caveats
    );
}

#[test]
fn something_that_is_not_a_manifest_is_refused_by_name() {
    let err = report::plan("just: some yaml\n").unwrap_err();
    assert!(err.0.contains("not a subgraph manifest"), "{}", err.0);
}

#[test]
fn a_report_against_itself_is_identical() {
    let r = build(&manifest(WASM_A, false), module(&[("store", "set")], false));
    let d = diff::diff(&r, &r);
    assert!(d.identical);
    assert_eq!(d.summary.handlers_unchanged, 1);
    assert!(d.strings.added.is_empty());
}

/// The diff a reader actually wants: a handler that gained a host capability between versions.
#[test]
fn a_handler_that_gains_a_capability_is_the_headline() {
    let before = build(&manifest(WASM_A, false), module(&[("store", "set")], false));
    let after = build(
        &manifest(WASM_A, false),
        module(&[("store", "set"), ("ethereum", "call")], false),
    );
    let d = diff::diff(&before, &after);

    assert!(!d.identical);
    assert_eq!(d.summary.handlers_changed, 1);
    assert_eq!(d.handlers[0].status, HandlerStatus::Changed);
    assert_eq!(d.handlers[0].categories_added, [HostCategory::Ethereum]);
    assert!(d.handlers[0].categories_removed.is_empty());
    assert_eq!(d.host_surface.added, [HostCategory::Ethereum]);
    assert!(d.scorecard.risk_delta > 0);
    assert!(
        d.scorecard
            .flags_added
            .iter()
            .any(|f| f.title == "eth_call in handlers")
    );
}

/// A handler added in the newer version, and the ordering rule: changed and added sort above
/// unchanged, so the reader does not scroll past forty untouched handlers to find the one.
#[test]
fn added_handlers_sort_above_unchanged_ones() {
    let before = build(&manifest(WASM_A, false), module(&[("store", "set")], false));
    let after = build(&manifest(WASM_A, true), module(&[("store", "set")], true));
    let d = diff::diff(&before, &after);

    assert_eq!(d.summary.handlers_added, 1);
    assert_eq!(d.summary.handlers_unchanged, 1);
    assert_eq!(d.handlers[0].status, HandlerStatus::Added);
    assert_eq!(d.handlers[0].handler, "handleApproval");
    assert_eq!(d.handlers[1].status, HandlerStatus::Unchanged);
}

/// A different module behind the same manifest. The strings diff is what tells a reader an
/// entity name or an event signature changed without the handler list moving.
#[test]
fn a_changed_module_shows_up_in_the_recovered_strings() {
    let with_string = |text: &str| {
        let escaped: String = text
            .chars()
            .flat_map(|c| [c as u8, 0u8])
            .map(|b| format!("\\{b:02x}"))
            .collect();
        let wat = format!(
            r#"(module
                 (import "store" "set" (func $set))
                 (memory 1)
                 (data (i32.const 1024) "{escaped}")
                 (func $handleTransfer (call $set))
                 (export "handleTransfer" (func $handleTransfer)))"#
        );
        wat::parse_str(wat).unwrap()
    };
    let before = build(&manifest(WASM_A, false), with_string("EntityBefore"));
    let after = build(&manifest(WASM_A, false), with_string("EntityAfter"));
    let d = diff::diff(&before, &after);

    assert!(
        d.strings.added.contains(&"EntityAfter".to_string()),
        "{:?}",
        d.strings
    );
    assert!(d.strings.removed.contains(&"EntityBefore".to_string()));
    assert!(!d.strings.truncated);
    // The handler surface did not move, so this is not a change in what it can do.
    assert_eq!(d.summary.handlers_changed, 0);
}

#[test]
fn a_manifest_naming_too_many_modules_is_capped_and_says_so() {
    let b58 = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K'];
    let sources: String = (0..20)
        .map(|i| {
            format!(
                r#"  - kind: ethereum/contract
    name: Source{i}
    network: mainnet
    source:
      address: "0xdEaDbeef"
    mapping:
      apiVersion: 0.0.7
      file: /ipfs/QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPb{}{}
      eventHandlers:
        - event: Transfer(address,address,uint256)
          handler: handleTransfer
"#,
                b58[i / 10],
                b58[i % 10]
            )
        })
        .collect();
    let yaml =
        format!("specVersion: 0.0.5\nschema:\n  file: /ipfs/{WASM_B}\ndataSources:\n{sources}");
    let plan = report::plan(&yaml).unwrap();
    assert_eq!(plan.modules.len(), 16);
    let r = report::assemble("QmDeployment", plan, &HashMap::new());
    assert!(
        r.caveats
            .iter()
            .any(|c| c.contains("20 distinct mapping modules")),
        "{:?}",
        r.caveats
    );
}
