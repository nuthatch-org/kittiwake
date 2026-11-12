//! The API described on its own terms, generated from the router.
//!
//! The dashboard has never been the only sensible consumer of this data - indexer scores,
//! delegation flows, allocations, query fees, APR provenance, chain lag - and until now it was the
//! only sanctioned one. Anybody else had to scrape the UI or guess at paths. #37
//!
//! **Generated, not maintained.** The paths come from the same compile-time read of the router that
//! makes the parity harness honest: axum will not tell you its own routes, so the source is read
//! and every `.route("/api/…")` extracted. A hand-written spec is wrong within a fortnight, and
//! `docs/decisions.md` has the receipts on what hand-maintained route lists cost.
//!
//! [`every_served_path_is_published_or_withheld`] is the enforcement. A route added to the router
//! is in the document or in [`WITHHELD`] with a written reason, and the build fails otherwise -
//! the same shape of check, and for the same reason, as the parity harness's coverage test.
//!
//! **Not everything goes out**, which is the actual design question #37 raises rather than an
//! implementation detail. `/api/sql/query` takes arbitrary SQL; the Dock's routes are
//! session-scoped and belong to whoever is signed in. Publishing "whatever the router serves" would
//! describe those as though they were public reads.

/// The router, as source. Read at compile time for the reason the parity harness gives: axum does
/// not expose its own routes, and a document that cannot see the truth is decoration.
const ROUTER: &str = include_str!("lib.rs");

/// Why a served path is not in the published document.
///
/// Every entry is a decision somebody made. A long list is a smell; an empty one would mean the
/// whole surface is public, which it is not.
pub const WITHHELD: &[(&str, &str)] = &[
    (
        "/api/sql/query",
        "takes arbitrary SQL. The named tier at /api/sql/named is the described one: it answers the \
         same questions with a name and typed arguments, and can be documented without documenting \
         a way to ask anything at all",
    ),
    (
        "/api/file-issue",
        "sends a support report to the maintainers' Telegram chat. Documenting it as an anonymous \
         endpoint would be an invitation to write into our chat through us",
    ),
    (
        "/api/health",
        "reports on this deployment rather than on the protocol. It exists under /api because that \
         is the URL the platform's health endpoint had and monitors already point at it",
    ),
    (
        "/api/whoami",
        "a diagnostic that reports the caller's own address back to them; it describes the request \
         rather than the protocol",
    ),
    (
        "/api/studio/auth",
        "a session- and wallet-signature-scoped product route, not a public protocol read",
    ),
    (
        "/api/studio/bounties",
        "the Dock's shared write-side state; reads and mutations belong to signed-in Studio users",
    ),
    (
        "/api/studio/bounties/{id}",
        "reconciles one shared bounty against its on-chain state",
    ),
    (
        "/api/studio/deploy-key",
        "mints an account secret and must not be advertised as anonymous API access",
    ),
    (
        "/api/studio/image",
        "uploads a subgraph image to IPFS for a signed-in Studio user",
    ),
    (
        "/api/studio/ipfs/{*path}",
        "a deploy-key-authenticated IPFS proxy, not a public IPFS gateway",
    ),
    (
        "/api/studio/metadata",
        "uploads metadata to IPFS for a signed-in Studio user",
    ),
    (
        "/api/studio/node",
        "a deploy-key-authenticated graph-cli compatibility endpoint",
    ),
    (
        "/api/studio/subgraphs",
        "lists and mutates the signed-in Studio user's subgraphs",
    ),
    (
        "/api/studio/subgraphs/{id}",
        "mutates one signed-in Studio user's subgraph",
    ),
    (
        "/api/sql/receipt",
        "issues a signed receipt and is a POST; this document describes GET operations, and a \
         receipt is obtained by asking for one rather than by reading a path",
    ),
    (
        "/api/data-services/query",
        "runs a real sample query against a provider and is a POST; it spends a signed receipt \
         rather than reading the protocol",
    ),
    (
        "/api/indexer/present-poi",
        "queues an action on an indexer's own agent and is a POST; it acts on somebody's \
         infrastructure rather than reading the protocol",
    ),
    (
        "/api/foghorn/{*path}",
        "a pass-through to Foghorn's QoS API; its shape is Foghorn's rather than ours, and it is \
         documented where that service is",
    ),
];

/// A summary for one method of a path, where the path's own line fits only the first.
const METHOD_SUMMARIES: &[(&str, &str, &str)] = &[(
    "/api/sql/named",
    "post",
    "Run one named query by name with typed arguments; the answer carries the SQL it ran.",
)];

/// One published operation.
struct Operation {
    path: &'static str,
    summary: &'static str,
}

/// What each published path is, in one line.
///
/// Hand-written on purpose, and the only hand-written part: a summary is prose and cannot be
/// derived from a route table. The *set* of paths is generated, so a route can never be quietly
/// left out - it will fail the test below until somebody writes its line.
const SUMMARIES: &[Operation] = &[
    op(
        "/api/network-stats",
        "Protocol-wide totals: stake, delegation, signal, allocation, supply and the current epoch.",
    ),
    op(
        "/api/epochs",
        "Recent epochs with their rewards and query fees.",
    ),
    op(
        "/api/indexers",
        "The indexer directory, scored and ordered.",
    ),
    op(
        "/api/indexers-enriched",
        "The directory with the computed columns the dashboard ranks by.",
    ),
    op(
        "/api/indexer/{address}",
        "One indexer in full. A section that could not be read is left out and named under `degraded`.",
    ),
    op(
        "/api/indexer-status/{address}",
        "What one indexer's own endpoint says it is serving.",
    ),
    op(
        "/api/indexer-stake-history/{address}",
        "Twenty-six weeks of one indexer's stake and delegation.",
    ),
    op(
        "/api/indexing-status/{hash}",
        "Who is serving a deployment, whether they are synced, and whether anyone can answer a paid query.",
    ),
    op(
        "/api/apr-provenance/{address}",
        "Why an indexer's APR moved: delegation events and cut changes on one timeline.",
    ),
    op(
        "/api/rewards-history",
        "One address's indexing rewards over time.",
    ),
    op("/api/curators", "The curator directory."),
    op(
        "/api/support",
        "The nuthatch-org/graph-support archive, mirrored twice a day.",
    ),
    op(
        "/api/support/templates",
        "The graph-support issue forms, as the repository's own YAML.",
    ),
    op(
        "/api/support/{number}",
        "One graph-support thread: the opening post and every reply.",
    ),
    op(
        "/api/subgraph-deployment/{hash}",
        "One subgraph deployment's directory row, by IPFS hash.",
    ),
    op(
        "/api/issue-forms",
        "The graph-support issue forms, parsed into fields, and whether this deployment can file.",
    ),
    op("/api/provisions", "One indexer's Horizon provisions."),
    op(
        "/api/provisions/detail",
        "One indexer's thaw requests, fee cuts, service ranges and Horizon activity.",
    ),
    op(
        "/api/delegation-events",
        "Individual delegations and undelegations.",
    ),
    op(
        "/api/delegation-flows",
        "Delegation in and out, aggregated by day.",
    ),
    op(
        "/api/payments",
        "Query-fee collections and RAV redemptions.",
    ),
    op("/api/poi", "Proof-of-indexing consensus across indexers."),
    op(
        "/api/developer-activity",
        "Subgraph publishing and versioning activity.",
    ),
    op(
        "/api/subgraph-deployments",
        "Deployments with their signal, stake and fees. With ?hash=Qm…, that one deployment.",
    ),
    op(
        "/api/subgraph-fees-30d",
        "Thirty days of query fees per deployment.",
    ),
    op(
        "/api/subgraph-directory",
        "Every listed deployment, filtered by signal, stake, ratio, fees, network, complexity, category and age, sorted and paged.",
    ),
    op(
        "/api/subgraph-search",
        "Search deployments by name, hash prefix or contract address.",
    ),
    op(
        "/api/subgraph-names",
        "Display names for a set of deployments.",
    ),
    op(
        "/api/subgraph-curation/{hash}",
        "Who has signalled on a deployment, and how much.",
    ),
    op(
        "/api/subgraph-history/{hash}",
        "A deployment's weekly signal and stake.",
    ),
    op("/api/dips", "Where protocol issuance actually goes."),
    op("/api/horizon/activity", "Recent Horizon protocol activity."),
    op("/api/grt-flow", "GRT minted, burned and bridged."),
    op("/api/token-metrics", "Supply and price over time."),
    op("/api/price", "The GRT price in USD, with its age."),
    op("/api/tvl", "Total value locked on Arbitrum."),
    op(
        "/api/chain-lag",
        "Per-chain head, lag and liveness, sampled across indexers.",
    ),
    op(
        "/api/dropped-chains",
        "Chains an indexer has stopped serving since we last looked.",
    ),
    op(
        "/api/reo",
        "The rewards-eligibility oracle's verdict for an address, with its staleness.",
    ),
    op(
        "/api/portfolio",
        "One address's position as a delegator or a curator, chosen by `type`.",
    ),
    op(
        "/api/sql/catalog",
        "The datasets the SQL surface exposes, and what is in them.",
    ),
    op(
        "/api/sql/named",
        "The named-query tier: which queries may be asked, and their arguments.",
    ),
    op(
        "/api/indexer-disputes/{address}",
        "Disputes and slashing raised against one indexer.",
    ),
    op(
        "/api/parameter-history/{address}",
        "When an indexer changed its cut, and what it changed from.",
    ),
    op(
        "/api/indexer/{address}/qos",
        "Edge & Node's gateway QoS for one indexer per UTC day, read from the oracle's own postings: queries, success rate and latency weighted by queries, fees, five-minute buckets seen and how many were bad, and seconds behind the freshest peer.",
    ),
    op(
        "/api/indexer/{address}/qos-score",
        "The QoS Quality score over thirty days of that feed, its four sub-scores and served gap, and the score for each of the last thirty days.",
    ),
    op(
        "/api/indexer/{address}/qos-deployments",
        "The deployments behind the QoS Quality score, heaviest drag first.",
    ),
    op(
        "/api/indexer/{address}/qos-qualifying-days",
        "For each UTC day of the Rewards Eligibility Oracle's 28-day window, the deployments this indexer answered with at least one HTTP 200, and how many of them carried 500 GRT of signal at that day's end. The QoS feed has no per-query latency or freshness, so each count bounds the oracle's from above; `qualifying_test_applied` and `signal_floor_applied` say what was and was not applied, with the reason.",
    ),
    op(
        "/api/indexer/{address}/revenue",
        "What an indexer received over a window ending today, by UTC day and by deployment: query fees after the protocol, curators and delegators, and rewards after the delegators, each dated by the collection that paid it, with the collected figures beside them.",
    ),
    op(
        "/api/indexer/{address}/trends",
        "Indexing rewards, split between the indexer and its delegators, and query fees collected, to curators, to the protocol and what the indexer received after the delegators' cut, per UTC day. Dated by the event that paid them, not by allocation close.",
    ),
    op(
        "/api/indexer/{address}/delegators",
        "Every delegator of one indexer, a page at a time: `first` (at most 1,000), `skip`, `orderBy` one of stake, delegatedAt, lastChangeAt or thawingTokens, and `orderDirection`. Each row carries its shares, what they are worth now, what is thawing and until when, and when the position opened and last changed; `total` counts every listed position and `active` those still holding shares.",
    ),
    op(
        "/api/indexer/{address}/pnl",
        "That revenue netted against a modelled infrastructure cost. The cost side is an estimate the caller may override, not a measurement.",
    ),
    op(
        "/api/dips/agreements",
        "The Direct Indexer Payments agreement lifecycle: offered, accepted, active, collected, ended. Empty on Arbitrum One until an agreement is funded, and it says so.",
    ),
    op(
        "/api/subgraph-versions/{hash}",
        "Every version of the subgraph that published a deployment, with its signal and stake.",
    ),
    op(
        "/api/subgraph-deployment/{hash}/graft-children",
        "Deployments whose cached manifest grafts onto this hash, with graft block and published name and label where known.",
    ),
    op(
        "/api/qos/capture",
        "How allocated stake sits across quality bands. Unscored means not measured, which is most of it.",
    ),
    op(
        "/api/network-stats/subgraph-check",
        "The protocol totals on /api/network-stats beside the Graph Network subgraph's own, field by field.",
    ),
    op(
        "/api/grt-flow/substreams",
        "About a day of Ethereum GRT transfers, read from GraphOps Substreams and from our own nest, compared transfer by transfer.",
    ),
    op(
        "/api/delegate/recommend",
        "Which indexer to delegate to, ranked against four preference sliders, with the reasoning.",
    ),
    op(
        "/api/disassembly",
        "What a deployed subgraph's mappings can actually do, read off the WebAssembly.",
    ),
    op(
        "/api/disassembly/diff",
        "What changed between two deployments: handlers, host surface, risk flags and strings.",
    ),
    op(
        "/api/operator-preflight",
        "What would happen if an address tried to run a data service. Read-only: no wallet, no gas.",
    ),
    op(
        "/api/service-census",
        "Every data service's registered providers, and whether their endpoints actually answer.",
    ),
    op(
        "/api/manifest",
        "How hard a subgraph is to sync, scored out of a hundred with the reasoning attached.",
    ),
    op(
        "/api/ens",
        "The primary ENS name for an address, forward-verified, or null where it has none. With ?name=, the address a .eth name resolves to.",
    ),
    op(
        "/api/subgraph-schema/{hash}",
        "The GraphQL schema a deployment was built from, as text.",
    ),
    op(
        "/api/indexer-node-health",
        "Whether an indexer's own endpoints answer.",
    ),
];

const fn op(path: &'static str, summary: &'static str) -> Operation {
    Operation { path, summary }
}

/// Every `/api/...` path the router registers.
pub fn served_paths() -> Vec<String> {
    routes_in(ROUTER)
}

fn routes_in(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices(".route(") {
        let rest = src[i + ".route(".len()..].trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            let p = &rest[..end];
            if p.starts_with("/api/") {
                out.push(p.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The handlers' source, read for the same reason as the router's: what a route accepts is
/// declared by its extractors, and a hand-kept copy of it would be wrong the first time one
/// changed.
const HANDLERS: &str = include_str!("handlers.rs");

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// One method on a served path, and the handler function bound to it.
pub struct Served {
    pub path: String,
    pub method: &'static str,
    handler: String,
}

/// The text up to the parenthesis that closes one already opened.
fn balanced(src: &str, open: char, close: char) -> &str {
    let mut depth = 1;
    for (i, c) in src.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return &src[..i];
            }
        }
    }
    src
}

/// Every `/api/...` path with the methods the router binds to it.
///
/// A document that said GET for a route bound only to POST sent every client that trusted it to a
/// 405.
pub fn served_routes() -> Vec<Served> {
    let mut out = Vec::new();
    for (i, _) in ROUTER.match_indices(".route(") {
        let span = balanced(&ROUTER[i + ".route(".len()..], '(', ')');
        let Some(rest) = span.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let path = &rest[..end];
        if !path.starts_with("/api/") {
            continue;
        }
        let bindings = &rest[end + 1..];
        for method in METHODS {
            let call = format!("{method}(");
            for (j, _) in bindings.match_indices(&call) {
                let before = bindings[..j].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let handler = balanced(&bindings[j + call.len()..], '(', ')')
                    .trim()
                    .rsplit("::")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                out.push(Served {
                    path: path.to_string(),
                    method,
                    handler,
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.path, a.method).cmp(&(&b.path, b.method)));
    out.dedup_by(|a, b| a.path == b.path && a.method == b.method);
    out
}

/// One field of a query string or a request body.
struct Field {
    name: String,
    json_type: &'static str,
    required: bool,
    doc: String,
}

/// The source of a function's parameter list, between its parentheses.
fn handler_params(handler: &str) -> Option<&'static str> {
    let needle = format!("fn {handler}(");
    [HANDLERS, ROUTER].into_iter().find_map(|src| {
        let at = src.find(&needle)? + needle.len();
        Some(balanced(&src[at..], '(', ')'))
    })
}

/// The type inside each `extractor<...>` among a handler's parameters.
fn extractor_types<'a>(params: &'a str, extractor: &str) -> Vec<&'a str> {
    let open = format!("{extractor}<");
    params
        .match_indices(&open)
        .filter(|(i, _)| {
            !params[..*i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
        .map(|(i, _)| balanced(&params[i + open.len()..], '<', '>').trim())
        .collect()
}

fn camel(snake: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in snake.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn json_type(ty: &str) -> &'static str {
    let ty = ty
        .strip_prefix("Option<")
        .map_or(ty, |t| t.trim_end_matches('>'));
    match ty {
        t if t.starts_with(['i', 'u']) && t[1..].chars().all(|c| c.is_ascii_digit()) => "integer",
        "f32" | "f64" => "number",
        "bool" => "boolean",
        t if t.starts_with("Vec<") => "array",
        t if t.contains("Map<") || t.ends_with("Value") => "object",
        _ => "string",
    }
}

/// The fields of the named struct, read from its declaration. `None` when the type is not declared
/// in this crate, which is the caller's cue to describe it as free-form.
fn struct_fields(ty: &str) -> Option<Vec<Field>> {
    let ty = ty.rsplit("::").next()?;
    let needle = format!("struct {ty} {{");
    let (src, at) = [HANDLERS, ROUTER]
        .into_iter()
        .find_map(|src| Some((src, src.find(&needle)?)))?;
    let camel_case = src[..at]
        .lines()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#[") || l.trim_start().starts_with("///"))
        .any(|l| l.contains("rename_all = \"camelCase\""));
    let body = src[at + needle.len()..].split("\n}").next()?;

    let (mut fields, mut doc, mut attrs) = (Vec::new(), Vec::new(), String::new());
    for line in body.lines().map(str::trim) {
        if let Some(d) = line.strip_prefix("///") {
            doc.push(d.trim().to_string());
        } else if line.starts_with("#[") {
            attrs.push_str(line);
        } else if let Some((name, ty)) = line.trim_start_matches("pub ").split_once(':') {
            let ty = ty.trim().trim_end_matches(',').trim();
            let name = name.trim().trim_start_matches("r#");
            let renamed = attrs
                .split("rename = \"")
                .nth(1)
                .and_then(|s| s.split('"').next());
            fields.push(Field {
                name: renamed.map_or_else(
                    || {
                        if camel_case {
                            camel(name)
                        } else {
                            name.to_string()
                        }
                    },
                    str::to_string,
                ),
                json_type: json_type(ty),
                required: !ty.starts_with("Option<") && !attrs.contains("default"),
                doc: doc.join(" "),
            });
            doc.clear();
            attrs.clear();
        }
    }
    Some(fields)
}

/// Query parameters a route declares, and whether it also takes ones nobody declared.
fn query_fields(handler: &str) -> (Vec<Field>, bool) {
    let Some(params) = handler_params(handler) else {
        return (Vec::new(), false);
    };
    let mut free_form = params.contains("RawQuery");
    let mut fields = Vec::new();
    for ty in extractor_types(params, "Query") {
        match struct_fields(ty) {
            Some(f) => fields.extend(f),
            None => free_form = true,
        }
    }
    (fields, free_form)
}

/// The JSON schema of a request body, free-form when the type is not one this crate declares.
fn body_schema(handler: &str) -> serde_json::Value {
    let fields = handler_params(handler)
        .and_then(|p| extractor_types(p, "Json").first().copied())
        .and_then(struct_fields);
    let Some(fields) = fields else {
        return serde_json::json!({ "type": "object" });
    };
    let properties: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .map(|f| {
            (
                f.name.clone(),
                serde_json::json!({ "type": f.json_type, "description": f.doc }),
            )
        })
        .collect();
    let required: Vec<&str> = fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name.as_str())
        .collect();
    serde_json::json!({ "type": "object", "properties": properties, "required": required })
}

/// The router's `{name}` parameters, as OpenAPI wants them declared.
fn parameters(path: &str) -> Vec<serde_json::Value> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| {
            serde_json::json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
            })
        })
        .collect()
}

/// The document.
///
/// Deliberately thin on response schemas. Describing every payload by hand would be the
/// hand-maintained artefact this exists to avoid, and it would be wrong the first time a shape
/// changed. What is described is what a caller cannot discover by trying: which paths exist, what
/// each one is for, and which parameters are part of the path.
pub fn document(version: &str) -> serde_json::Value {
    let mut paths = serde_json::Map::new();
    for served in served_routes() {
        let path = &served.path;
        if WITHHELD.iter().any(|(p, _)| p == path) {
            continue;
        }
        let Some(o) = SUMMARIES.iter().find(|o| o.path == *path) else {
            continue; // The test below is what stops this being silent.
        };
        let summary = METHOD_SUMMARIES
            .iter()
            .find(|(p, m, _)| p == path && *m == served.method)
            .map_or(o.summary, |(_, _, s)| s);

        let mut parameters = parameters(path);
        let (query, free_form) = query_fields(&served.handler);
        parameters.extend(query.iter().map(|f| {
            serde_json::json!({
                "name": f.name,
                "in": "query",
                "required": f.required,
                "description": f.doc,
                "schema": { "type": f.json_type },
            })
        }));
        let mut op = serde_json::json!({
            "summary": summary,
            "parameters": parameters,
            "responses": {
                "200": { "description": "The answer, with a provenance envelope." },
                "400": { "description": "The request was not understood; the message says why." },
                "429": { "description": "Rate limited." },
                "503": { "description": "A source this route depends on is not answering." },
            },
        });
        if free_form {
            op["x-query-free-form"] = true.into();
        }
        if served.method != "get" {
            op["requestBody"] = serde_json::json!({
                "content": { "application/json": { "schema": body_schema(&served.handler) } },
            });
        }
        paths
            .entry(path.clone())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("a path item is an object")
            .insert(served.method.into(), op);
    }

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Lodestar",
            "version": version,
            "description":
                "The Graph protocol's indexers, delegators, curators and subgraphs, read from \
                 nuthatch nests rather than from a subgraph. Every answer carries a provenance \
                 envelope saying which block it was sealed through.\n\n\
                 Paths are generated from the router, so this document cannot describe a route \
                 that does not exist or omit one that does. A few routes are served but not \
                 described, because documenting them would advertise something that is not a \
                 public protocol read; those are listed under `x-withheld` with the reason, so \
                 this document is still a complete inventory of what is served.",
        },
        "servers": [{ "url": "https://api.lodestar-dashboard.com" }],
        "paths": paths,
        // Served, deliberately undescribed, and named anyway.
        //
        // Without this the document answers "which routes exist?" with a subset, and a reader
        // cannot tell a withheld route from a deleted one. That distinction is the whole point for
        // a caller checking its own call sites: the frontend names seventeen of these, and a check
        // built on `paths` alone would report every one as a route that no longer exists.
        "x-withheld": WITHHELD
            .iter()
            .map(|(path, why)| serde_json::json!({ "path": path, "reason": why }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The check that keeps this honest.** A route added to the router is published or withheld
    /// in writing, and neither happening fails the build.
    ///
    /// Same shape as the parity harness's coverage test, and for the same reason: on 2026-09-07
    /// three routes went to production having never been in a list anybody checked. A document
    /// that silently omits a route is the same failure wearing different clothes - a consumer
    /// concludes the endpoint does not exist.
    #[test]
    fn every_served_path_is_published_or_withheld() {
        let doc = document("test");
        let published = doc["paths"].as_object().expect("paths is an object");

        let mut undescribed = Vec::new();
        for path in served_paths() {
            if published.contains_key(&path) || WITHHELD.iter().any(|(p, _)| *p == path) {
                continue;
            }
            undescribed.push(path);
        }
        assert!(
            undescribed.is_empty(),
            "these routes are served and the published document says nothing about them:\n  {}\n\n\
             Add a summary to `SUMMARIES`, or a reason to `WITHHELD`.",
            undescribed.join("\n  ")
        );
    }

    /// The other direction. A summary for a path the router no longer serves describes an endpoint
    /// that answers 404, which is worse than describing nothing.
    #[test]
    fn nothing_is_described_that_is_not_served() {
        let served = served_paths();
        for o in SUMMARIES {
            assert!(
                served.contains(&o.path.to_string()),
                "{} is described and not served",
                o.path
            );
        }
        for (p, _) in WITHHELD {
            assert!(
                served.contains(&p.to_string()),
                "{p} is withheld and not served; delete the entry"
            );
        }
    }

    /// The arithmetic, stated once so neither list can drift without the other noticing.
    #[test]
    fn published_plus_withheld_is_everything_served() {
        let doc = document("test");
        let published = doc["paths"].as_object().unwrap().len();
        assert_eq!(
            published + WITHHELD.len(),
            served_paths().len(),
            "published {published}, withheld {}, served {}",
            WITHHELD.len(),
            served_paths().len()
        );
        assert!(published > 25, "only {published} paths described");
    }

    /// If the extraction breaks, both tests above pass vacuously.
    #[test]
    fn the_router_is_readable_and_has_routes_in_it() {
        let served = served_paths();
        assert!(
            served.len() > 25,
            "only found {} routes; the extraction is broken, not the router",
            served.len()
        );
        assert!(served.contains(&"/api/network-stats".to_string()));
    }

    /// Withholding is a decision, so it carries a reason somebody can disagree with.
    /// **The document is an inventory, not only a description.**
    ///
    /// A caller checking its own call sites against `paths` alone sees every withheld route as
    /// one that does not exist. The frontend names seventeen of them, so that check would report
    /// seventeen phantom failures and be switched off, and the real signal it exists for - a call
    /// site naming a route that has actually been deleted - would go with it.
    #[test]
    fn x_withheld_makes_the_document_a_complete_inventory() {
        let doc = document("test");
        let withheld = doc["x-withheld"]
            .as_array()
            .expect("x-withheld is an array");
        assert_eq!(withheld.len(), WITHHELD.len());

        let named: std::collections::HashSet<&str> = withheld
            .iter()
            .map(|e| e["path"].as_str().expect("each entry names a path"))
            .collect();
        for (path, _) in WITHHELD {
            assert!(named.contains(path), "{path} is withheld but not named");
        }
        for e in withheld {
            assert!(
                !e["reason"].as_str().unwrap_or_default().is_empty(),
                "a withheld route must carry its reason into the document too"
            );
        }

        // Together they are everything the router serves, which is the property that makes this an
        // inventory rather than two partial lists.
        let published: std::collections::HashSet<&str> = doc["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        for path in served_paths() {
            assert!(
                published.contains(path.as_str()) || named.contains(path.as_str()),
                "{path} is served and appears in neither paths nor x-withheld"
            );
        }
    }

    #[test]
    fn every_withheld_route_says_why() {
        for (path, reason) in WITHHELD {
            assert!(reason.len() > 30, "{path} is withheld without a reason");
        }
    }

    #[test]
    fn a_path_parameter_is_declared_as_one() {
        let doc = document("test");
        let params = &doc["paths"]["/api/indexer/{address}"]["get"]["parameters"];
        assert_eq!(params[0]["name"], "address");
        assert_eq!(params[0]["in"], "path");
        assert_eq!(params[0]["required"], true);
        // And a path with none declares none, rather than an empty-named parameter.
        assert_eq!(
            doc["paths"]["/api/network-stats"]["get"]["parameters"]
                .as_array()
                .map(|a| a.len()),
            Some(0)
        );
    }

    /// The one thing a reader must not be able to find here.
    #[test]
    fn the_arbitrary_sql_route_is_not_published() {
        let doc = document("test");
        assert!(doc["paths"].get("/api/sql/query").is_none());
        assert!(
            doc["paths"].get("/api/sql/named").is_some(),
            "the named tier is"
        );
    }

    #[test]
    fn a_post_only_route_is_published_as_a_post_and_not_a_get() {
        let doc = document("test");
        let names = &doc["paths"]["/api/subgraph-names"];
        assert!(names.get("get").is_none(), "{names}");
        assert!(names["post"]["requestBody"].is_object());
        assert_eq!(
            names["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"]["hashes"]
                ["type"],
            "array"
        );
    }

    #[test]
    fn a_route_bound_to_two_methods_is_published_under_both() {
        let doc = document("test");
        let named = &doc["paths"]["/api/sql/named"];
        assert!(
            named["get"].is_object() && named["post"].is_object(),
            "{named}"
        );
        assert_ne!(named["get"]["summary"], named["post"]["summary"]);
        assert_eq!(
            named["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"]["name"]
                ["type"],
            "string"
        );
    }

    /// Every published operation is something the MCP server will send. A new mutating route is a
    /// decision to make here, not a thing that appears as a tool.
    #[test]
    fn only_the_two_read_posts_are_published_beyond_get() {
        let doc = document("test");
        let mut non_get = Vec::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys().filter(|m| *m != "get") {
                non_get.push(format!("{method} {path}"));
            }
        }
        non_get.sort();
        assert_eq!(non_get, ["post /api/sql/named", "post /api/subgraph-names"]);
    }

    #[test]
    fn query_parameters_come_from_the_handlers_extractors() {
        let doc = document("test");
        let params = doc["paths"]["/api/portfolio"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let address = params
            .iter()
            .find(|p| p["name"] == "address")
            .expect("address");
        assert_eq!(address["in"], "query");
        assert_eq!(address["required"], false);
        assert!(
            params.iter().any(|p| p["name"] == "type"),
            "r#type is spelt type"
        );

        let indexers = doc["paths"]["/api/indexers"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let first = indexers
            .iter()
            .find(|p| p["name"] == "first")
            .expect("first");
        assert_eq!(first["schema"]["type"], "integer");
        assert_eq!(first["required"], false, "it has a default");
        assert!(indexers.iter().any(|p| p["name"] == "order_by"));
    }
}
