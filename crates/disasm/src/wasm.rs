//! Static disassembly of a compiled mapping module.
//!
//! Imports, exports, the direct call graph and the data-segment strings, and from those which host
//! functions each exported handler can reach.
//!
//! The incumbent decodes the sections by hand: six hundred lines of LEB128 reader and an opcode
//! table that bails on anything post-MVP, which is why its reports carry a caveat about SIMD and
//! atomics making reachability partial. `wasmparser` knows the whole instruction set, so the
//! caveat is not needed and `incomplete` now means what it says: a body this parser could not
//! read at all. It is kept rather than removed because a truncated or corrupt module is still
//! possible, and a transparency tool must say when it could not look.

use std::collections::{BTreeSet, HashMap};

use wasmparser::{DataKind, ExternalKind, Operator, Parser, Payload, TypeRef};

use crate::manifest::ParsedHandler;
use crate::types::{HandlerAnalysis, HostCategory, HostImport, WasmModuleInfo};

/// The namespaces graph-node links into a mapping.
fn host_category(namespace: &str) -> Option<HostCategory> {
    Some(match namespace {
        "store" => HostCategory::Store,
        "ethereum" => HostCategory::Ethereum,
        "ipfs" => HostCategory::Ipfs,
        "json" => HostCategory::Json,
        "crypto" => HostCategory::Crypto,
        "typeConversion" => HostCategory::TypeConversion,
        "bigInt" => HostCategory::BigInt,
        "bigDecimal" => HostCategory::BigDecimal,
        "dataSource" => HostCategory::DataSource,
        "log" => HostCategory::Log,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct Categorised {
    label: String,
    category: HostCategory,
    is_host: bool,
}

/// AssemblyScript emits a host import either as `(module="ethereum", name="call")` or, since the
/// change that puts every import in one module, as `(module=<file>, name="ethereum.call")`.
/// graph-node keys on the dotted name, so both shapes are accepted.
fn categorise(module: &str, name: &str) -> Categorised {
    if let Some(category) = host_category(module) {
        return Categorised {
            label: format!("{module}.{name}"),
            category,
            is_host: true,
        };
    }
    if let Some((namespace, _)) = name.split_once('.')
        && let Some(category) = host_category(namespace)
    {
        return Categorised {
            label: name.to_string(),
            category,
            is_host: true,
        };
    }
    if name == "abort" {
        return Categorised {
            label: "abort".into(),
            category: HostCategory::Control,
            is_host: true,
        };
    }
    Categorised {
        label: if module.is_empty() {
            name.to_string()
        } else {
            format!("{module}.{name}")
        },
        category: HostCategory::Other,
        is_host: false,
    }
}

#[derive(Debug, Default)]
struct FuncBody {
    calls: Vec<u32>,
    indirect: bool,
    incomplete: bool,
}

pub struct ParsedWasm {
    pub info: WasmModuleInfo,
    exports: HashMap<String, u32>,
    bodies: HashMap<u32, FuncBody>,
    imports: HashMap<u32, Categorised>,
}

/// Guards against a hostile deployment reconstructing a multi-gigabyte memory image.
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STRINGS: usize = 1500;
const MIN_STRING: usize = 4;

struct DataSegment {
    /// The `i32.const` address of an active segment, or `None` for a passive one or an offset
    /// that is not a constant, neither of which can be placed in the image.
    offset: Option<i64>,
    bytes: Vec<u8>,
}

/// Printable runs, in both encodings that turn up.
///
/// AssemblyScript stores string literals as UTF-16LE, and everything the audit reads lives there:
/// event signatures, entity names, log formats and the `ethereum.decode` type strings. The plain
/// ASCII pass covers other toolchains and embedded ABI text; the two do not duplicate each other,
/// because UTF-16 content is broken up by its own padding under an ASCII scan.
fn collect_strings(bytes: &[u8], out: &mut BTreeSet<String>) {
    let push = |run: &mut String, out: &mut BTreeSet<String>| {
        if run.len() >= MIN_STRING && out.len() < MAX_STRINGS {
            out.insert(std::mem::take(run));
        } else {
            run.clear();
        }
    };

    let mut run = String::new();
    for pair in bytes.as_chunks::<2>().0 {
        if pair[1] == 0 && (0x20..=0x7e).contains(&pair[0]) {
            run.push(pair[0] as char);
        } else {
            push(&mut run, out);
        }
        if out.len() >= MAX_STRINGS {
            return;
        }
    }
    push(&mut run, out);

    let mut run = String::new();
    for &b in bytes {
        if (0x20..=0x7e).contains(&b) {
            run.push(b as char);
        } else {
            push(&mut run, out);
        }
        if out.len() >= MAX_STRINGS {
            return;
        }
    }
    push(&mut run, out);
}

/// AssemblyScript emits one small segment per string constant, and many have odd byte lengths, so
/// a UTF-16 character's two bytes can straddle a boundary. Scanning segments one at a time drops
/// those characters, which for an ABI tuple means losing the closing bracket and manufacturing a
/// decode finding out of nothing. So place each active segment at its real address and scan once.
fn collect_strings_from_segments(segments: &[DataSegment]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let placeable: Vec<(i64, &[u8])> = segments
        .iter()
        .filter_map(|s| match s.offset {
            Some(offset) if !s.bytes.is_empty() => Some((offset, s.bytes.as_slice())),
            _ => None,
        })
        .collect();

    if !placeable.is_empty() {
        let min = placeable.iter().map(|(o, _)| *o).min().unwrap_or(0);
        let max = placeable
            .iter()
            .map(|(o, b)| o + b.len() as i64)
            .max()
            .unwrap_or(0);
        // An even base, so a real even address stays even in the image and the UTF-16 pass reads
        // the pairs the toolchain actually wrote.
        let base = if min % 2 == 0 { min } else { min - 1 };
        let span = max - base;
        if span > 0 && span <= MAX_IMAGE_BYTES as i64 {
            let mut image = vec![0u8; span as usize];
            for (offset, bytes) in &placeable {
                let at = (offset - base) as usize;
                image[at..at + bytes.len()].copy_from_slice(bytes);
            }
            collect_strings(&image, &mut out);
            for segment in segments.iter().filter(|s| s.offset.is_none()) {
                collect_strings(&segment.bytes, &mut out);
            }
            return out;
        }
    }

    for segment in segments {
        collect_strings(&segment.bytes, &mut out);
    }
    out
}

/// The `i32.const` address of an active segment's offset expression, or `None` when the offset is
/// computed (a `global.get`), in which case the segment has no address we can place it at.
fn const_offset(expr: &wasmparser::ConstExpr) -> Option<i64> {
    let mut reader = expr.get_operators_reader();
    let mut offset = None;
    while let Ok(op) = reader.read() {
        match op {
            Operator::I32Const { value } => offset = Some(value as i64),
            Operator::I64Const { value } => offset = Some(value),
            Operator::End => break,
            _ => return None,
        }
    }
    offset
}

#[derive(Debug)]
pub struct NotAModule(pub String);

impl std::fmt::Display for NotAModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn parse(bytes: &[u8], wasm_hash: &str) -> Result<ParsedWasm, NotAModule> {
    if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
        return Err(NotAModule(
            "Not a WebAssembly module (bad magic)".to_string(),
        ));
    }

    let mut imports: HashMap<u32, Categorised> = HashMap::new();
    let mut host_imports: Vec<HostImport> = Vec::new();
    let mut imported_functions: u32 = 0;
    let mut other_import_count: u32 = 0;
    let mut defined_functions: u32 = 0;
    let mut exports: HashMap<String, u32> = HashMap::new();
    let mut bodies: HashMap<u32, FuncBody> = HashMap::new();
    let mut named_functions = 0usize;
    let mut segments: Vec<DataSegment> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut incomplete = false;
    let mut body_ordinal: u32 = 0;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = match payload {
            Ok(payload) => payload,
            Err(e) => {
                notes.push(format!("Module could not be fully parsed: {e}"));
                incomplete = true;
                break;
            }
        };
        match payload {
            Payload::ImportSection(reader) => {
                // One entry can be a group: since the compact-imports proposal several imports
                // share a module name, and a reader that treats a group as one import would
                // number every function after it wrongly.
                for group in reader.into_iter().flatten() {
                    for item in group {
                        let Ok((_, import)) = item else {
                            notes.push("Import section could not be fully parsed".into());
                            incomplete = true;
                            break;
                        };
                        if !matches!(import.ty, TypeRef::Func(_)) {
                            continue;
                        }
                        let categorised = categorise(import.module, import.name);
                        let func_index = imported_functions;
                        imported_functions += 1;
                        if categorised.is_host && categorised.category != HostCategory::Control {
                            host_imports.push(HostImport {
                                module: import.module.to_string(),
                                name: import.name.to_string(),
                                label: categorised.label.clone(),
                                category: categorised.category,
                                func_index,
                                is_host: true,
                            });
                        } else if !categorised.is_host {
                            other_import_count += 1;
                        }
                        imports.insert(func_index, categorised);
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                defined_functions = reader.count();
            }
            Payload::ExportSection(reader) => {
                for export in reader.into_iter().flatten() {
                    if export.kind == ExternalKind::Func {
                        exports.insert(export.name.to_string(), export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let func_index = imported_functions + body_ordinal;
                body_ordinal += 1;
                let decoded = scan_body(&body);
                if decoded.incomplete {
                    incomplete = true;
                }
                bodies.insert(func_index, decoded);
            }
            Payload::CustomSection(section) if section.name() == "name" => {
                named_functions = count_named_functions(&section);
            }
            Payload::DataSection(reader) => {
                for data in reader.into_iter().flatten() {
                    let offset = match &data.kind {
                        DataKind::Active { offset_expr, .. } => const_offset(offset_expr),
                        DataKind::Passive => None,
                    };
                    segments.push(DataSegment {
                        offset,
                        bytes: data.data.to_vec(),
                    });
                }
            }
            _ => {}
        }
    }

    let strings = collect_strings_from_segments(&segments);
    if incomplete && notes.is_empty() {
        notes.push(
            "At least one function body could not be decoded; reachability is partial".into(),
        );
    }

    let info = WasmModuleInfo {
        wasm_hash: wasm_hash.to_string(),
        byte_size: bytes.len(),
        function_count: imported_functions + defined_functions,
        imported_function_count: imported_functions,
        defined_function_count: defined_functions,
        host_imports,
        other_import_count,
        strings: strings.into_iter().collect(),
        named_functions,
        incomplete,
        notes,
    };

    Ok(ParsedWasm {
        info,
        exports,
        bodies,
        imports,
    })
}

/// The direct call targets of one body, and whether it dispatches indirectly.
fn scan_body(body: &wasmparser::FunctionBody) -> FuncBody {
    let mut out = FuncBody::default();
    let Ok(reader) = body.get_operators_reader() else {
        out.incomplete = true;
        return out;
    };
    for op in reader {
        match op {
            Ok(Operator::Call { function_index }) | Ok(Operator::ReturnCall { function_index }) => {
                out.calls.push(function_index);
            }
            Ok(Operator::CallIndirect { .. }) | Ok(Operator::ReturnCallIndirect { .. }) => {
                out.indirect = true;
            }
            Ok(_) => {}
            Err(_) => {
                out.incomplete = true;
                break;
            }
        }
    }
    out
}

fn count_named_functions(section: &wasmparser::CustomSectionReader) -> usize {
    let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(
        section.data(),
        section.data_offset(),
    ));
    let mut count = 0;
    for subsection in reader.into_iter().flatten() {
        if let wasmparser::Name::Function(names) = subsection {
            count += names.into_iter().flatten().count();
        }
    }
    count
}

struct Reach {
    labels: BTreeSet<String>,
    categories: BTreeSet<HostCategory>,
    indirect: bool,
    incomplete: bool,
}

/// Everything reachable from one function over direct calls. Imports are leaves.
fn reachable(parsed: &ParsedWasm, start: u32) -> Reach {
    let mut out = Reach {
        labels: BTreeSet::new(),
        categories: BTreeSet::new(),
        indirect: false,
        incomplete: false,
    };
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut stack = vec![start];

    while let Some(func) = stack.pop() {
        if !visited.insert(func) {
            continue;
        }
        if let Some(import) = parsed.imports.get(&func) {
            if import.is_host && import.category.is_meaningful() {
                out.labels.insert(import.label.clone());
                out.categories.insert(import.category);
            }
            continue;
        }
        let Some(body) = parsed.bodies.get(&func) else {
            continue;
        };
        out.indirect |= body.indirect;
        out.incomplete |= body.incomplete;
        stack.extend(body.calls.iter().copied().filter(|c| !visited.contains(c)));
    }

    out
}

pub fn analyse_handler(parsed: &ParsedWasm, handler: &ParsedHandler) -> HandlerAnalysis {
    let Some(&index) = parsed.exports.get(&handler.handler) else {
        // The manifest names an export the module does not have. Reported rather than assumed to
        // be a naming skew, because it is also what a manifest pointing at the wrong module looks
        // like, and the two want different fixes.
        return HandlerAnalysis {
            handler: handler.handler.clone(),
            kind: handler.kind,
            trigger: handler.trigger.clone(),
            resolved: false,
            host_imports: Vec::new(),
            categories: Vec::new(),
            dynamic_dispatch: false,
            incomplete: false,
        };
    };
    let reach = reachable(parsed, index);
    HandlerAnalysis {
        handler: handler.handler.clone(),
        kind: handler.kind,
        trigger: handler.trigger.clone(),
        resolved: true,
        host_imports: reach.labels.into_iter().collect(),
        categories: reach.categories.into_iter().collect(),
        dynamic_dispatch: reach.indirect,
        incomplete: reach.incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HandlerKind;

    /// A module in text form, so a test says what it is testing rather than carrying a byte array.
    fn module(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).expect("test module assembles")
    }

    fn handler(name: &str) -> ParsedHandler {
        ParsedHandler {
            handler: name.to_string(),
            kind: HandlerKind::Event,
            trigger: None,
        }
    }

    /// The load-bearing property: a handler is credited with a host call it reaches through
    /// several hops, and not with one it never reaches. Under-reporting makes the tool useless
    /// and over-reporting makes it a liar, so both directions are asserted.
    #[test]
    fn reachability_follows_direct_calls_and_stops_where_they_do() {
        let bytes = module(
            r#"(module
                 (import "store" "set" (func $set))
                 (import "ipfs" "cat" (func $cat))
                 (import "env" "abort" (func $abort))
                 (func $deep (call $set))
                 (func $middle (call $deep))
                 (func $handleTransfer (call $middle) (call $abort))
                 (func $unrelated (call $cat))
                 (export "handleTransfer" (func $handleTransfer))
                 (export "unrelated" (func $unrelated)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();

        let a = analyse_handler(&parsed, &handler("handleTransfer"));
        assert!(a.resolved);
        assert_eq!(a.host_imports, ["store.set"], "three hops to store.set");
        assert_eq!(a.categories, [HostCategory::Store]);
        assert!(!a.dynamic_dispatch);
        assert!(!a.incomplete);

        let b = analyse_handler(&parsed, &handler("unrelated"));
        assert_eq!(b.host_imports, ["ipfs.cat"], "and no store.set here");
    }

    /// `abort` is reachable from almost everything AssemblyScript emits. Reporting it would put
    /// one useless line on every handler in every report.
    #[test]
    fn the_runtime_is_not_a_signal() {
        let bytes = module(
            r#"(module
                 (import "env" "abort" (func $abort))
                 (import "env" "memory" (memory 1))
                 (func $h (call $abort))
                 (export "h" (func $h)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        let a = analyse_handler(&parsed, &handler("h"));
        assert!(a.resolved);
        assert!(a.host_imports.is_empty(), "{:?}", a.host_imports);
        assert!(parsed.info.host_imports.is_empty());
    }

    /// An imported memory or table takes no slot in the function index space. Counting one would
    /// shift every later index and attribute each host call to the wrong import.
    #[test]
    fn a_non_function_import_does_not_shift_the_function_indices() {
        let bytes = module(
            r#"(module
                 (import "env" "memory" (memory 1))
                 (import "env" "table" (table 1 funcref))
                 (import "ethereum" "call" (func $call))
                 (func $h (call $call))
                 (export "h" (func $h)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        assert_eq!(parsed.info.imported_function_count, 1);
        assert_eq!(
            analyse_handler(&parsed, &handler("h")).host_imports,
            ["ethereum.call"]
        );
    }

    /// The other import shape: every host function in one module, named with its namespace.
    #[test]
    fn a_dotted_import_name_is_categorised_by_its_namespace() {
        let bytes = module(
            r#"(module
                 (import "src/mapping.ts" "ethereum.decode" (func $decode))
                 (import "src/mapping.ts" "typeConversion.bytesToHex" (func $hex))
                 (func $h (call $decode) (call $hex))
                 (export "h" (func $h)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        let a = analyse_handler(&parsed, &handler("h"));
        assert_eq!(
            a.host_imports,
            ["ethereum.decode", "typeConversion.bytesToHex"]
        );
        assert_eq!(
            a.categories,
            [HostCategory::Ethereum, HostCategory::TypeConversion]
        );
    }

    /// AssemblyScript dispatches indirectly all over the place. The flag exists so a reader knows
    /// the reachability under it is a lower bound, and it must not be set when nothing does.
    #[test]
    fn an_indirect_call_is_declared_rather_than_followed() {
        let bytes = module(
            r#"(module
                 (import "store" "get" (func $get))
                 (type $void (func))
                 (table 1 funcref)
                 (func $h (i32.const 0) (call_indirect (type $void)))
                 (func $plain (call $get))
                 (export "h" (func $h))
                 (export "plain" (func $plain)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        assert!(analyse_handler(&parsed, &handler("h")).dynamic_dispatch);
        assert!(!analyse_handler(&parsed, &handler("plain")).dynamic_dispatch);
    }

    /// Recursion is ordinary in a mapping and must not spin.
    #[test]
    fn a_cycle_terminates() {
        let bytes = module(
            r#"(module
                 (import "log" "log" (func $log))
                 (func $a (call $b))
                 (func $b (call $a) (call $log))
                 (export "a" (func $a)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        assert_eq!(
            analyse_handler(&parsed, &handler("a")).host_imports,
            ["log.log"]
        );
    }

    /// The improvement over the incumbent, stated as a test. Its hand-rolled opcode table stops at
    /// the first instruction it does not model and marks the report partial; a module using SIMD
    /// is decoded here to the end, so the call after the SIMD instruction is still found.
    #[test]
    fn post_mvp_instructions_do_not_make_the_answer_partial() {
        let bytes = module(
            r#"(module
                 (import "crypto" "keccak256" (func $keccak))
                 (memory 1)
                 (func $h
                   (drop (v128.load (i32.const 0)))
                   (call $keccak))
                 (export "h" (func $h)))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        assert!(!parsed.info.incomplete, "{:?}", parsed.info.notes);
        let a = analyse_handler(&parsed, &handler("h"));
        assert_eq!(a.host_imports, ["crypto.keccak256"]);
        assert!(!a.incomplete);
    }

    #[test]
    fn a_handler_the_module_does_not_export_is_unresolved_rather_than_empty() {
        let bytes = module(r#"(module (func $h) (export "h" (func $h)))"#);
        let parsed = parse(&bytes, "QmTest").unwrap();
        let a = analyse_handler(&parsed, &handler("handleSomethingElse"));
        assert!(!a.resolved);
        assert!(a.host_imports.is_empty());
    }

    #[test]
    fn something_that_is_not_a_module_is_refused() {
        assert!(parse(b"not wasm at all", "QmTest").is_err());
        assert!(parse(b"", "QmTest").is_err());
    }

    /// The straddling case the memory image exists for. Two segments at consecutive addresses
    /// hold one UTF-16 string; scanned separately, the last character is lost, and for an ABI
    /// tuple that means losing the closing bracket and inventing a decode finding.
    #[test]
    fn a_string_split_across_two_segments_is_recovered_whole() {
        let mut wat = String::from("(module (memory 1)\n");
        // "(uint256,address)" in UTF-16LE, cut after an odd number of bytes.
        let text: Vec<u8> = "(uint256,address)"
            .chars()
            .flat_map(|c| [c as u8, 0u8])
            .collect();
        let (first, second) = text.split_at(9);
        let escape =
            |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("\\{b:02x}")).collect() };
        wat.push_str(&format!(
            "(data (i32.const 1024) \"{}\")\n(data (i32.const {}) \"{}\"))",
            escape(first),
            1024 + first.len(),
            escape(second)
        ));
        let parsed = parse(&module(&wat), "QmTest").unwrap();
        assert!(
            parsed.info.strings.iter().any(|s| s == "(uint256,address)"),
            "{:?}",
            parsed.info.strings
        );
    }

    /// A segment placed by a computed offset has no address, so it cannot go in the image. It is
    /// still scanned on its own rather than dropped.
    #[test]
    fn a_passive_segment_is_still_read() {
        let bytes = module(
            r#"(module
                 (memory 1)
                 (data "handleTransfer"))"#,
        );
        let parsed = parse(&bytes, "QmTest").unwrap();
        assert!(parsed.info.strings.iter().any(|s| s == "handleTransfer"));
    }

    #[test]
    fn the_name_section_is_counted_when_it_is_there() {
        let named = parse(
            &module(r#"(module (func $named) (export "named" (func $named)))"#),
            "QmTest",
        )
        .unwrap();
        assert!(named.info.named_functions > 0);
    }
}
