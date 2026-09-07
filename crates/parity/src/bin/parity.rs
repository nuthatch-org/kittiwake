//! Compare kittiwake against the API it replaces, route by route.
//!
//! Run it from somewhere that can reach both:
//!
//! ```text
//! kittiwake-parity --incumbent https://www.lodestar-dashboard.com --ours http://127.0.0.1:8080
//! ```
//!
//! It reports, per route: paths the incumbent has and we do not (which break consumers), paths
//! whose type changed (which break consumers silently, and are the dangerous ones), and paths we
//! add (which do not). It says nothing about values, deliberately - see the module docs in the
//! library half.

use kittiwake_parity::{Diff, diff, shape_of};

/// One route, with the parameters to compare it at.
///
/// Fixed parameters rather than random ones: two runs a week apart should differ only where the
/// data differs, and a route sampled differently each time cannot be compared to its own history.
struct Route {
    path: &'static str,
    /// Where the payload sits on each side, as a JSON pointer. Empty means the whole body.
    incumbent_at: &'static str,
    ours_at: &'static str,
}

/// A real indexer from the directory. Not our own: it is excluded from the list by design, and the
/// detail route answers 404 for it, which is a fact about the fixture rather than about parity.
const A: &str = "0x090f7382f9ea85c733cd501f4d87f16cb5b83ed3";

/// Route paths are built from a fixture address, so they outlive the call that made them.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn routes() -> Vec<Route> {
    let both = |path: &'static str| Route {
        path,
        incumbent_at: "",
        ours_at: "",
    };
    vec![
        both("/api/network-stats"),
        both("/api/epochs?count=5"),
        both("/api/indexers?first=10"),
        both("/api/curators?first=10"),
        both("/api/token-metrics?count=5"),
        Route {
            path: leak(format!("/api/provisions?indexer={A}")),
            incumbent_at: "",
            ours_at: "",
        },
        both("/api/payments"),
        both("/api/poi?first=50"),
        both("/api/delegation-events?first=20"),
        both("/api/delegation-flows?days=30"),
        both("/api/developer-activity"),
        both("/api/subgraph-deployments?first=10"),
        both("/api/subgraph-fees-30d"),
        both("/api/dips"),
        both("/api/horizon/activity"),
        both("/api/subgraph-search?q=uniswap"),
        both("/api/chain-lag"),
        both("/api/dropped-chains"),
        both("/api/price"),
        both("/api/tvl"),
        Route {
            path: leak(format!("/api/reo?address={A}")),
            incumbent_at: "",
            ours_at: "",
        },
        Route {
            path: leak(format!("/api/indexer/{A}")),
            incumbent_at: "",
            ours_at: "",
        },
        Route {
            path: leak(format!("/api/indexer-status/{A}")),
            incumbent_at: "",
            ours_at: "",
        },
        // The type is required; without it the incumbent answers 400 and nothing is compared.
        Route {
            path: leak(format!("/api/portfolio?address={A}&type=delegator")),
            incumbent_at: "",
            ours_at: "",
        },
    ]
}

/// Paths we add and they do not have. Not differences anyone needs told about.
const IGNORE: &[&str] = &[".provenance", ".source"];

#[tokio::main]
async fn main() {
    let incumbent_base =
        arg("--incumbent").unwrap_or_else(|| "https://www.lodestar-dashboard.com".into());
    let ours_base = arg("--ours").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let verbose = std::env::args().any(|a| a == "--verbose");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .user_agent("kittiwake-parity")
        .build()
        .expect("client");

    println!("parity: {ours_base} against {incumbent_base}\n");

    let mut clean = 0;
    let mut broken = 0;
    let mut unreachable = 0;

    for r in routes() {
        let (a, a_status) = fetch(&http, &format!("{incumbent_base}{}", r.path)).await;
        let (b, b_status) = fetch(&http, &format!("{ours_base}{}", r.path)).await;

        let name = r.path.replace(A, "<addr>");
        match (a, b) {
            (Some(a), Some(b)) => {
                let a = at(&a, r.incumbent_at);
                let b = at(&b, r.ours_at);
                let d = diff(&shape_of(&a), &shape_of(&b), IGNORE);
                if d.breaking() {
                    broken += 1;
                    println!("  BREAK  {name}");
                    report(&d, verbose);
                } else {
                    clean += 1;
                    println!("  ok     {name}{}", extra_note(&d));
                    if verbose {
                        report(&d, verbose);
                    }
                }
            }
            (None, _) => {
                unreachable += 1;
                println!("  ?      {name}  (incumbent answered {a_status})");
            }
            (_, None) => {
                unreachable += 1;
                println!("  ?      {name}  (ours answered {b_status})");
            }
        }
    }

    println!("\n  {clean} matching, {broken} breaking, {unreachable} not compared");
    if broken > 0 {
        std::process::exit(1);
    }
}

fn extra_note(d: &Diff) -> String {
    if d.extra.is_empty() {
        String::new()
    } else {
        format!(
            "  (+{} added path{})",
            d.extra.len(),
            if d.extra.len() == 1 { "" } else { "s" }
        )
    }
}

fn report(d: &Diff, verbose: bool) {
    for p in &d.missing {
        println!("           missing  {p}");
    }
    for (p, was, now) in &d.retyped {
        println!("           retyped  {p}: {was} -> {now}");
    }
    if verbose {
        for p in &d.extra {
            println!("           added    {p}");
        }
    }
}

/// Take the payload at a JSON pointer, or the whole body when the pointer is empty.
fn at(v: &serde_json::Value, pointer: &str) -> serde_json::Value {
    if pointer.is_empty() {
        v.clone()
    } else {
        v.pointer(pointer)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

async fn fetch(http: &reqwest::Client, url: &str) -> (Option<serde_json::Value>, String) {
    match http.get(url).send().await {
        Ok(r) => {
            let status = r.status();
            match r.json::<serde_json::Value>().await {
                Ok(v) if status.is_success() => (Some(v), status.to_string()),
                Ok(_) => (None, status.to_string()),
                Err(_) => (None, format!("{status}, unreadable body")),
            }
        }
        Err(e) => (
            None,
            if e.is_timeout() {
                "timeout".into()
            } else {
                "unreachable".into()
            },
        ),
    }
}
