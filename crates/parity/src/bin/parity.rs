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
    let mut rows: Vec<Row> = Vec::new();

    for r in routes() {
        let (a, a_status, a_ms) = timed(&http, &format!("{incumbent_base}{}", r.path)).await;
        let (b, b_status, b_ms) = timed(&http, &format!("{ours_base}{}", r.path)).await;

        let name = r.path.replace(A, "<addr>");
        let (theirs, ours) = (median(a_ms), median(b_ms));
        match (a, b) {
            (Some(a), Some(b)) => {
                let a = at(&a, r.incumbent_at);
                let b = at(&b, r.ours_at);
                let d = diff(&shape_of(&a), &shape_of(&b), IGNORE);
                if d.breaking() {
                    broken += 1;
                    println!("  BREAK  {name}");
                    report(&d, verbose);
                    rows.push(Row::new(&name, "breaking", theirs, ours));
                } else {
                    clean += 1;
                    println!("  ok     {name}{}", extra_note(&d));
                    if verbose {
                        report(&d, verbose);
                    }
                    rows.push(Row::new(&name, "matching", theirs, ours));
                }
            }
            (None, _) => {
                unreachable += 1;
                println!("  ?      {name}  (incumbent answered {a_status})");
                rows.push(Row::new(&name, "incumbent down", theirs, ours));
            }
            (_, None) => {
                unreachable += 1;
                println!("  ?      {name}  (ours answered {b_status})");
                rows.push(Row::new(&name, "ours down", theirs, ours));
            }
        }
    }

    println!("\n  {clean} matching, {broken} breaking, {unreachable} not compared");

    if let Some(path) = arg("--markdown") {
        match write_markdown(
            &path,
            &incumbent_base,
            &ours_base,
            &rows,
            clean,
            broken,
            unreachable,
        ) {
            Ok(()) => println!("  wrote {path}"),
            Err(e) => eprintln!("  could not write {path}: {e}"),
        }
    }

    if broken > 0 {
        std::process::exit(1);
    }
}

struct Row {
    name: String,
    verdict: &'static str,
    theirs_ms: Option<u128>,
    ours_ms: Option<u128>,
}

impl Row {
    fn new(
        name: &str,
        verdict: &'static str,
        theirs_ms: Option<u128>,
        ours_ms: Option<u128>,
    ) -> Self {
        Self {
            name: name.to_string(),
            verdict,
            theirs_ms,
            ours_ms,
        }
    }

    /// How many times faster we are. `None` when either side did not answer, because a route the
    /// incumbent could not serve has no ratio - reporting one would turn their outage into our win.
    fn speedup(&self) -> Option<f64> {
        match (self.theirs_ms, self.ours_ms) {
            (Some(t), Some(o)) if o > 0 => Some(t as f64 / o as f64),
            _ => None,
        }
    }

    fn ms(v: Option<u128>) -> String {
        v.map(|us| format!("{:.1} ms", us as f64 / 1000.0))
            .unwrap_or_else(|| "-".into())
    }
}

/// Write the tracker.
///
/// Generated on every run rather than maintained by hand, because a hand-kept table of what is
/// migrated is a table that is right on the day it is written and slowly stops being true. This one
/// cannot drift: it is whatever the last run actually observed.
fn write_markdown(
    path: &str,
    incumbent: &str,
    ours: &str,
    rows: &[Row],
    clean: usize,
    broken: usize,
    unreachable: usize,
) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = String::new();
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");

    out.push_str(
        "# Migration status

",
    );
    let _ = writeln!(
        out,
        "**Generated by `kittiwake-parity --markdown`. Do not edit by hand** - the next run \
         overwrites it. A tracker somebody maintains is a tracker that is true on the day it is \
         written; this one is whatever the last run actually observed.
"
    );
    let _ = writeln!(out, "- Run: {now}");
    let _ = writeln!(out, "- Ours: `{ours}`");
    let _ = writeln!(out, "- Incumbent: `{incumbent}`");
    let _ = writeln!(
        out,
        "- Result: **{clean} matching, {broken} breaking, {unreachable} not compared**
"
    );
    out.push_str(
        "Route coverage across the whole API - which of Lodestar's 90 routes exist here at all - \
         is in [migration.md](migration.md). This file is about the ones that do.

",
    );
    out.push_str(
        "## Per route

",
    );
    out.push_str(
        "Timings are the **median of three warm requests** from wherever this ran, so they include \
         network distance to the incumbent and none to us. They are a comparison, not an absolute. \
         The cold-path figures that motivated the migration are in [benchmarks.md](benchmarks.md); \
         a cold path can only be measured once per cache lifetime and so cannot be sampled here.

",
    );
    out.push_str(
        "| Route | Parity | Incumbent | Ours | Faster by |
",
    );
    out.push_str(
        "|---|---|---:|---:|---:|
",
    );
    for r in rows {
        let speed = r
            .speedup()
            .map(|s| format!("{s:.1}x"))
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} |",
            r.name,
            r.verdict,
            Row::ms(r.theirs_ms),
            Row::ms(r.ours_ms),
            speed
        );
    }

    let ratios: Vec<f64> = rows.iter().filter_map(|r| r.speedup()).collect();
    if !ratios.is_empty() {
        let mut sorted = ratios.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = sorted[sorted.len() / 2];
        let worst = sorted[0];
        let best = sorted[sorted.len() - 1];
        let _ = writeln!(
            out,
            "
**Median {med:.1}x faster** across {} comparable routes, worst {worst:.1}x, best \
             {best:.1}x.
",
            ratios.len()
        );
        if worst < 1.0 {
            let _ = writeln!(
                out,
                "At least one route is **slower** here than on the incumbent. That is a finding, \
                 not a rounding error: find it in the table above before claiming the migration is \
                 a win.
"
            );
        }
    }
    std::fs::write(path, out)
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

/// How many times each side is sampled. Three is enough to throw out one unlucky request and few
/// enough not to be a load test against somebody else's production.
const SAMPLES: usize = 3;

/// Fetch a route several times and keep the body plus how long each attempt took.
///
/// **These are warm-path figures**, and the distinction matters for anyone reading the table. Both
/// services cache, so by the second sample both are answering from something warm. The cold-path
/// numbers - the 23.5, 17.9 and 7.7 second routes that motivated this whole exercise - are in
/// `docs/benchmarks.md` and are measured differently, because a cold path can only be measured
/// once per cache lifetime and not on demand.
async fn timed(
    http: &reqwest::Client,
    url: &str,
) -> (Option<serde_json::Value>, String, Vec<u128>) {
    let mut body = None;
    let mut status = String::new();
    let mut times = Vec::new();
    for i in 0..SAMPLES {
        // A unique parameter per request, so the incumbent's CDN cannot answer. Without it the
        // "incumbent" column is a Vercel edge hit from a nearby PoP rather than the application
        // doing the work, and the comparison flatters it enormously - every route reads about 35 ms
        // whatever it actually costs to compute. Unknown query parameters are ignored on both
        // sides. Ours sends `no-store` anyway; this keeps the two symmetrical.
        let sep = if url.contains('?') { '&' } else { '?' };
        let bust = format!(
            "{url}{sep}_cb={}-{i}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let began = std::time::Instant::now();
        let (b, st) = fetch(http, &bust).await;
        // Microseconds, not milliseconds. At millisecond resolution every fast route reads `0`,
        // which is then indistinguishable from "did not answer" and drops out of the ratio - so the
        // summary is computed only from the slow routes and reports a migration that made things
        // worse. Measured that, believed it for about a minute.
        let elapsed = began.elapsed().as_micros();
        status = st;
        if b.is_some() {
            times.push(elapsed);
            if body.is_none() {
                body = b;
            }
        }
    }
    (body, status, times)
}

fn median(mut v: Vec<u128>) -> Option<u128> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
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
