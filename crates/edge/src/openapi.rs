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
    (
        "/api/scuttlebutt/admin/login",
        "validates a moderation credential; it is not a public data operation",
    ),
    (
        "/api/scuttlebutt/admin/messages",
        "the moderation view of room history, protected by the administrator credential",
    ),
    (
        "/api/scuttlebutt/bans",
        "reads and writes moderation bans, protected by the administrator credential",
    ),
    (
        "/api/scuttlebutt/messages/{id}",
        "deletes a message and is protected by the administrator credential",
    ),
];

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
    op("/api/indexer/{address}", "One indexer in full."),
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
        "The nightswatchhq/graph-support archive, mirrored twice a day.",
    ),
    op(
        "/api/support/templates",
        "The graph-support issue forms, as the repository's own YAML.",
    ),
    op(
        "/api/support/{number}",
        "One graph-support thread: the opening post and every reply.",
    ),
    op("/api/provisions", "One indexer's Horizon provisions."),
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
        "Deployments with their signal, stake and fees.",
    ),
    op(
        "/api/subgraph-fees-30d",
        "Thirty days of query fees per deployment.",
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
        "One address's position as a delegator or an indexer.",
    ),
    op(
        "/api/sql/catalog",
        "The datasets the SQL surface exposes, and what is in them.",
    ),
    op(
        "/api/sql/named",
        "The named-query tier: what may be asked, and asking it by name.",
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
        "/api/indexer/{address}/revenue",
        "Query fees and indexing rewards over a rolling window, by day and by deployment.",
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
        "/api/qos/capture",
        "How allocated stake sits across quality bands. Unscored means not measured, which is most of it.",
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
        "The primary ENS name for an address, forward-verified, or null where it has none.",
    ),
    op(
        "/api/subgraph-schema/{hash}",
        "The GraphQL schema a deployment was built from, as text.",
    ),
    op(
        "/api/indexer-node-health",
        "Whether an indexer's own endpoints answer.",
    ),
    op(
        "/api/scuttlebutt/messages",
        "The anonymous room's durable message history, newest first.",
    ),
    op(
        "/api/scuttlebutt/stream",
        "The anonymous room's live Server-Sent Event stream. A gap event means refetch history.",
    ),
];

const fn op(path: &'static str, summary: &'static str) -> Operation {
    Operation { path, summary }
}

/// Every `/api/...` path the router registers.
pub fn served_paths() -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in ROUTER.match_indices(".route(") {
        let rest = ROUTER[i + ".route(".len()..].trim_start();
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
    for path in served_paths() {
        if WITHHELD.iter().any(|(p, _)| *p == path) {
            continue;
        }
        let Some(o) = SUMMARIES.iter().find(|o| o.path == path) else {
            continue; // The test below is what stops this being silent.
        };
        let mut item = serde_json::Map::new();
        item.insert(
            "get".into(),
            serde_json::json!({
                "summary": o.summary,
                "parameters": parameters(&path),
                "responses": {
                    "200": { "description": "The answer, with a provenance envelope." },
                    "400": { "description": "The request was not understood; the message says why." },
                    "429": { "description": "Rate limited." },
                    "503": { "description": "A source this route depends on is not answering." },
                },
            }),
        );
        paths.insert(path, serde_json::Value::Object(item));
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
                 that does not exist or omit one that does.",
        },
        "servers": [{ "url": "https://api.lodestar-dashboard.com" }],
        "paths": paths,
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
}
