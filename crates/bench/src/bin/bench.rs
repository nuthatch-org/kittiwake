//! Drives the service and reports what a reader would actually experience.
//!
//! Three things are measured, because they answer three different questions:
//!
//! 1. **Cold**, one reader arriving at a key nobody has asked for. This is the fold, and it is the
//!    number the warmer exists to keep off the request path.
//! 2. **Coalescing**, many readers arriving at one cold key at once. The question is how many of
//!    them the nest hears about; the answer should be one.
//! 3. **Warm**, the ordinary case. This is what almost every real request should be.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use kittiwake_bench::{Sample, ms};

#[tokio::main]
async fn main() {
    let base = arg("--base").unwrap_or_else(|| "http://127.0.0.1:8123".into());
    let nest = arg("--nest").unwrap_or_else(|| "http://127.0.0.1:9911".into());
    let route = arg("--route").unwrap_or_else(|| "/api/indexers".into());
    let warm_n: usize = arg("--warm").and_then(|v| v.parse().ok()).unwrap_or(2000);
    let burst: usize = arg("--burst").and_then(|v| v.parse().ok()).unwrap_or(40);
    let conc: usize = arg("--concurrency")
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);

    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(conc * 2)
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("client");

    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("kittiwake bench");
    println!("  target {base}{route}");
    println!();

    // --- 1. Cold: one reader, a key nobody has asked for.
    reset(&http, &nest).await;
    let cold_url = format!("{base}{route}{}first=11&_c={run}", sep(&route));
    let began = Instant::now();
    let cold_status = get(&http, &cold_url).await;
    let cold_ms = began.elapsed().as_secs_f64() * 1000.0;
    let cold_nest = hits(&http, &nest).await;

    // --- 2. Coalescing: many readers, one cold key, all at once.
    reset(&http, &nest).await;
    let burst_url = format!("{base}{route}{}first=12&_c={run}", sep(&route));
    let began = Instant::now();
    let mut set = Vec::new();
    for _ in 0..burst {
        let http = http.clone();
        let url = burst_url.clone();
        set.push(tokio::spawn(async move {
            let t = Instant::now();
            let s = get(&http, &url).await;
            (s, t.elapsed().as_secs_f64() * 1000.0)
        }));
    }
    let mut burst_lat = Vec::new();
    let mut burst_ok = 0;
    for h in set {
        let (status, took) = h.await.expect("join");
        if status == 200 {
            burst_ok += 1;
        }
        burst_lat.push(took);
    }
    let burst_wall = began.elapsed().as_secs_f64();
    let burst_nest = hits(&http, &nest).await;
    let burst_lat = Sample::new(burst_lat);

    // --- 3. Warm: the ordinary case, at concurrency.
    reset(&http, &nest).await;
    let warm_url = format!("{base}{route}{}first=13&_c={run}", sep(&route));
    let _ = get(&http, &warm_url).await; // fill the entry
    let counter = Arc::new(AtomicUsize::new(0));
    let began = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..conc {
        let http = http.clone();
        let url = warm_url.clone();
        let counter = counter.clone();
        workers.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            loop {
                if counter.fetch_add(1, Ordering::SeqCst) >= warm_n {
                    break;
                }
                let t = Instant::now();
                let _ = get(&http, &url).await;
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            lat
        }));
    }
    let mut warm_lat = Vec::new();
    for w in workers {
        warm_lat.extend(w.await.expect("join"));
    }
    let warm_wall = began.elapsed().as_secs_f64();
    let warm_nest = hits(&http, &nest).await;
    let warm = Sample::new(warm_lat);
    let rps = warm.len() as f64 / warm_wall;

    println!("cold, one reader at an uncached key");
    println!("  status              {cold_status}");
    println!("  latency             {}", ms(cold_ms));
    println!("  nest queries        {cold_nest}");
    println!();
    println!("coalescing, {burst} readers arriving together at one uncached key");
    println!("  answered 200        {burst_ok} of {burst}");
    println!("  nest queries        {burst_nest}   <- the number that matters");
    println!("  wall clock          {:.2} s", burst_wall);
    println!(
        "  p50 / p99 / max     {} / {} / {}",
        ms(burst_lat.p(50.0)),
        ms(burst_lat.p(99.0)),
        ms(burst_lat.max())
    );
    println!();
    println!("warm, {} requests at concurrency {conc}", warm.len());
    println!("  p50                 {}", ms(warm.p(50.0)));
    println!("  p95                 {}", ms(warm.p(95.0)));
    println!("  p99                 {}", ms(warm.p(99.0)));
    println!("  max                 {}", ms(warm.max()));
    println!("  throughput          {rps:.0} req/s");
    println!("  nest queries        {warm_nest}");
    println!(
        "  nest per request    {:.5}",
        warm_nest as f64 / warm.len().max(1) as f64
    );
}

fn sep(route: &str) -> &'static str {
    if route.contains('?') { "&" } else { "?" }
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

async fn get(http: &reqwest::Client, url: &str) -> u16 {
    match http.get(url).send().await {
        Ok(r) => {
            let s = r.status().as_u16();
            let _ = r.bytes().await;
            s
        }
        Err(_) => 0,
    }
}

async fn reset(http: &reqwest::Client, nest: &str) {
    let _ = http.get(format!("{nest}/reset")).send().await;
}

async fn hits(http: &reqwest::Client, nest: &str) -> u64 {
    match http.get(format!("{nest}/hits")).send().await {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("sql").and_then(|s| s.as_u64()))
            .unwrap_or(0),
        Err(_) => 0,
    }
}
