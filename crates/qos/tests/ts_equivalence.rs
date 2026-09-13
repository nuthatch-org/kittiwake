//! The port held to the TypeScript it replaces.
//!
//! `fixtures/ts-equivalence.json` is Lodestar's `qos-score.ts` and `qos-aggregate.ts` at `cb61cea~1`
//! run over seeded random inputs (seed `0x5eed1160`), thinned to every sixteenth case. Point
//! `KITTIWAKE_QOS_FIXTURE` at an unthinned file to check the lot.
//!
//! Two figures differ on purpose. The TypeScript divided an indexer's attempts by the gateway's query
//! count, so a query hedged across indexers pushed served share past 1; the port divides by every
//! indexer's attempts. `servedShare` and the `servedGap` built on it are left out of the comparison,
//! and served share is checked against its own definition instead. Everything else must still match.

use std::collections::HashMap;

use kittiwake_qos::aggregate::{
    AggregateOpts, AllocationRow, QosDailyRow, aggregate_indexer_metrics, compute_phase2_metrics,
    score_indexers,
};
use kittiwake_qos::score::{
    DEFAULTS, DeploymentMetrics, QualityOpts, block_time_sec, cohort_adjusted_reliability,
    cohort_adjusted_time_behind, compute_quality_detail, decay_util, ewma_weight, freshness_util,
    latency_util, median, qos_grade, wilson_lower_bound,
};
use serde::Deserialize;
use serde_json::{Value, json};

const TOLERANCE: f64 = 1e-9;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    kind: String,
    input: Value,
    output: Value,
}

fn field<T: for<'de> Deserialize<'de>>(input: &Value, key: &str) -> T {
    serde_json::from_value(input.get(key).cloned().unwrap_or(Value::Null))
        .unwrap_or_else(|e| panic!("{key}: {e}"))
}

fn run(kind: &str, input: &Value) -> Value {
    match kind {
        "wilson" => {
            let z: Option<f64> = field(input, "z");
            json!(wilson_lower_bound(
                field(input, "successes"),
                field(input, "n"),
                z.unwrap_or(DEFAULTS.z)
            ))
        }
        "decay" => json!(decay_util(field(input, "value"), field(input, "tau"))),
        "latency" => json!(latency_util(
            field(input, "latencyMs"),
            field(input, "tauMs")
        )),
        "freshness" => {
            let tau: Option<f64> = field(input, "tauSec");
            json!(freshness_util(
                field(input, "timeBehindSec"),
                tau.unwrap_or(DEFAULTS.tau_fresh_sec)
            ))
        }
        "ewma" => {
            let half_life: Option<f64> = field(input, "halfLifeDays");
            json!(ewma_weight(
                field(input, "ageDays"),
                half_life.unwrap_or(DEFAULTS.half_life_days)
            ))
        }
        "median" => json!(median(&field::<Vec<f64>>(input, "xs"))),
        "blockTime" => json!(block_time_sec(
            field::<Option<String>>(input, "chainId").as_deref()
        )),
        "grade" => json!(qos_grade(field(input, "q"))),
        "cohortReliability" => json!(cohort_adjusted_reliability(
            field(input, "r"),
            field(input, "cohortBest")
        )),
        "cohortTimeBehind" => {
            json!(cohort_adjusted_time_behind(
                field(input, "timeBehindSec"),
                field(input, "cohortFloorSec")
            ))
        }
        "quality" => {
            let rows: Vec<DeploymentMetrics> = field(input, "rows");
            let opts: Option<QualityOpts> = field(input, "opts");
            json!(compute_quality_detail(&rows, &opts.unwrap_or_default()))
        }
        "aggregate" | "scoreIndexers" => {
            let rows: Vec<QosDailyRow> = field(input, "qosRows");
            let opts: AggregateOpts = field(input, "opts");
            if kind == "aggregate" {
                json!(aggregate_indexer_metrics(&rows, &opts))
            } else {
                json!(score_indexers(&rows, &opts))
            }
        }
        "phase2" => {
            let allocations: Vec<AllocationRow> = field(input, "allocations");
            let rows: Vec<QosDailyRow> = field(input, "qosRows");
            json!(compute_phase2_metrics(&allocations, &rows))
        }
        other => panic!("unknown case kind {other}"),
    }
}

/// `v` with `key` removed from every object in it.
fn without(v: &Value, key: &str) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| k.as_str() != key)
                .map(|(k, x)| (k.clone(), without(x, key)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| without(x, key)).collect()),
        other => other.clone(),
    }
}

/// Every served share in an aggregate answer, against attempts over every indexer's attempts
/// recomputed from the case's rows. Returns how many shares the TypeScript put somewhere else.
fn check_served_share(input: &Value, ours: &Value, theirs: &Value) -> usize {
    let rows: Vec<QosDailyRow> = field(input, "qosRows");
    let mut pair: HashMap<(&str, &str), f64> = HashMap::new();
    let mut deployment: HashMap<&str, f64> = HashMap::new();
    for r in &rows {
        *pair
            .entry((&r.indexer_address, &r.deployment_id))
            .or_default() += r.query_count;
        *deployment.entry(&r.deployment_id).or_default() += r.query_count;
    }
    let shares = |v: &Value| -> Vec<(String, String, f64)> {
        v.as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| {
                let ix = entry[0].as_str().unwrap().to_string();
                entry[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |d| {
                        (
                            ix.clone(),
                            d["deployment"].as_str().unwrap().to_string(),
                            d["servedShare"].as_f64().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    for (ix, dep, share) in shares(ours) {
        let total = deployment[dep.as_str()];
        let expected = if total > 0.0 {
            pair[&(ix.as_str(), dep.as_str())] / total
        } else {
            0.0
        };
        assert!(
            (share - expected).abs() <= TOLERANCE && (0.0..=1.0).contains(&share),
            "{ix} on {dep}: share {share}, attempts over attempts {expected}"
        );
    }
    shares(ours)
        .iter()
        .zip(shares(theirs))
        .filter(|(a, b)| (a.2 - b.2).abs() > TOLERANCE)
        .count()
}

/// Structural equality with numbers compared to a tolerance scaled by magnitude, returning the
/// largest absolute deviation seen.
fn compare(path: &str, ours: &Value, theirs: &Value) -> f64 {
    match (ours, theirs) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            let d = (a - b).abs();
            assert!(
                d <= TOLERANCE * b.abs().max(1.0),
                "{path}: ours {a}, typescript {b}"
            );
            d
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: length");
            a.iter()
                .zip(b)
                .enumerate()
                .map(|(i, (x, y))| compare(&format!("{path}[{i}]"), x, y))
                .fold(0.0, f64::max)
        }
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<_> = a.keys().collect();
            keys.sort();
            let mut theirs_keys: Vec<_> = b.keys().collect();
            theirs_keys.sort();
            assert_eq!(keys, theirs_keys, "{path}: keys");
            a.iter()
                .map(|(k, v)| compare(&format!("{path}.{k}"), v, &b[k]))
                .fold(0.0, f64::max)
        }
        _ => {
            assert_eq!(ours, theirs, "{path}");
            0.0
        }
    }
}

#[test]
fn the_port_matches_the_typescript() {
    let text = match std::env::var("KITTIWAKE_QOS_FIXTURE") {
        Ok(path) => std::fs::read_to_string(path).expect("fixture path readable"),
        Err(_) => include_str!("fixtures/ts-equivalence.json").to_string(),
    };
    let fixture: Fixture = serde_json::from_str(&text).expect("fixture parses");
    assert!(!fixture.cases.is_empty());
    let mut worst = 0.0f64;
    let mut shares_moved = 0;
    for (i, case) in fixture.cases.iter().enumerate() {
        let ours = run(&case.kind, &case.input);
        let (ours, theirs) = match case.kind.as_str() {
            "aggregate" => {
                shares_moved += check_served_share(&case.input, &ours, &case.output);
                (
                    without(&ours, "servedShare"),
                    without(&case.output, "servedShare"),
                )
            }
            "phase2" => (
                without(&ours, "servedGap"),
                without(&case.output, "servedGap"),
            ),
            _ => (ours, case.output.clone()),
        };
        worst = worst.max(compare(
            &format!("case {i} ({})", case.kind),
            &ours,
            &theirs,
        ));
    }
    eprintln!(
        "{} cases, largest absolute deviation {worst:e}; {shares_moved} served shares moved by the \
         change of denominator",
        fixture.cases.len()
    );
}
