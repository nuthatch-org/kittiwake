//! `/api/disassembly` and `/api/disassembly/diff`: fetching what the analysis needs, and keeping
//! the answer.
//!
//! The analysis itself is `kittiwake_disasm` and is pure. This is the part that talks to IPFS and
//! to the nest.

use kittiwake_disasm::report::{self as build, Fetched};
use kittiwake_disasm::types::DisassemblyReport;
use kittiwake_ext::Ipfs;
use kittiwake_nest::{NestGate, QuerySpec};
use kittiwake_types::{ApiError, NestId};
use serde::{Deserialize, Serialize};

/// Bumped whenever the analysis changes what it would say about the same bytes.
///
/// A deployment id names immutable content, so a stored report never goes stale on its own and
/// there is no expiry to choose. What does go stale is the analysis, and without this a fixed bug
/// would be invisible on every deployment anybody had already looked at.
pub const ANALYSIS_VERSION: u32 = 1;

/// Matches the incumbent's cap. A module larger than this is not a mapping.
const MAX_WASM_BYTES: usize = 32 * 1024 * 1024;

fn cache_key(deployment_id: &str) -> String {
    format!("disasm:v{ANALYSIS_VERSION}:{deployment_id}")
}

/// The report for one deployment, from the cache if it is there.
pub async fn report(
    pool: Option<&kittiwake_db::Pool>,
    ipfs: &Ipfs,
    deployment_id: &str,
) -> Result<DisassemblyReport, ApiError> {
    let key = cache_key(deployment_id);
    if let Some(pool) = pool
        && let Ok(Some((value, _))) = kittiwake_db::get_live_answer(pool, &key).await
        && let Ok(report) = serde_json::from_value::<DisassemblyReport>(value)
    {
        return Ok(report);
    }

    let yaml = match pool {
        Some(pool) => kittiwake_ext::read_through_text(pool, ipfs, deployment_id).await,
        None => ipfs.cat(deployment_id).await.ok(),
    }
    .ok_or_else(|| ApiError::Unavailable("that manifest could not be read from IPFS".into()))?;

    let plan = build::plan(&yaml).map_err(|e| ApiError::BadRequest(e.0))?;

    // Concurrently, because a subgraph with a dozen mappings would otherwise pay a dozen round
    // trips in series and the caller would give up first.
    let fetches = plan
        .modules
        .iter()
        .map(|hash| async move { (hash.clone(), ipfs.cat_bytes(hash, MAX_WASM_BYTES).await) });
    let fetched: Fetched = futures::future::join_all(fetches)
        .await
        .into_iter()
        .collect();

    // Whether every module the manifest names actually arrived. A report assembled during a
    // gateway outage is a real answer to serve and a terrible one to keep for ever.
    let complete = fetched.values().all(Result::is_ok);
    let report = build::assemble(deployment_id, plan, &fetched);

    if complete
        && let Some(pool) = pool
        && let Ok(value) = serde_json::to_value(&report)
        && let Err(e) = kittiwake_db::put_live_answer(pool, &key, &value).await
    {
        tracing::warn!(error = %e, deployment_id, "disassembly report not kept");
    }

    Ok(report)
}

/// How much GRT rides on a deployment. A risk flag on a subgraph with five GRT signalled is
/// trivia; the same flag with four hundred thousand behind it is news.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalContext {
    pub signalled_tokens: String,
    pub query_fees_amount: String,
    // `camelCase` renders these `signalledGrt` and `queryFeesGrt`, and the incumbent spells both
    // with `GRT`. The frontend destructures the incumbent's spelling: `formatGRT(signal.signalledGRT)`
    // would have printed a blank, and `signal.signalledGRT > 0` would have been false, so the
    // warning that "N GRT signalled is exposed to M risk flags" would have stopped appearing at
    // cutover with nothing else visibly wrong. Found by diffing the two services on the same
    // deployment, which the harness excuses this route from doing.
    #[serde(rename = "signalledGRT")]
    pub signalled_grt: f64,
    #[serde(rename = "queryFeesGRT")]
    pub query_fees_grt: f64,
    pub curator_count: usize,
    pub curator_count_capped: bool,
    pub exposure: &'static str,
}

const SIGNAL_FIRST: usize = 100;

/// Thresholds tuned to the signal distribution, not to round numbers.
pub fn exposure(signalled_grt: f64) -> &'static str {
    match signalled_grt {
        g if g <= 0.0 => "none",
        g if g < 1_000.0 => "low",
        g if g < 50_000.0 => "medium",
        _ => "high",
    }
}

fn deployment_signals_sql(deployment_id: &str) -> Option<String> {
    if !crate::sql::is_deployment_id(deployment_id) {
        return None;
    }
    Some(format!(
        "SELECT CAST(deployment_signalled_tokens AS VARCHAR) AS deployment_signalled_tokens, \
         CAST(deployment_query_fees_amount AS VARCHAR) AS deployment_query_fees_amount \
         FROM lodestar_curator_signals s WHERE LOWER(s.subgraph_deployment) = '{deployment_id}' \
         AND s.signal > 0 ORDER BY s.signalled_tokens DESC, s.curator LIMIT {SIGNAL_FIRST}"
    ))
}

fn wei_to_grt(wei: &str) -> f64 {
    wei.parse::<f64>().unwrap_or(0.0) / 1e18
}

/// Best effort, and never load-bearing: a nest that cannot answer leaves the overlay absent and
/// the static analysis standing on its own.
pub async fn signal(gate: &NestGate, deployment_id: &str) -> Option<SignalContext> {
    let hex = crate::shape::ipfs_hash_to_bytes32(deployment_id)?.to_lowercase();
    let sql = deployment_signals_sql(&hex)?;
    let answer = gate
        .query_coalesced(
            "alloc:deployment-signal",
            QuerySpec::read(NestId::Alloc, sql),
        )
        .await
        .ok()?;
    let first = answer.rows.first()?;
    let text = |key: &str| -> String {
        first
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0")
            .to_string()
    };
    let signalled_tokens = text("deployment_signalled_tokens");
    let query_fees_amount = text("deployment_query_fees_amount");
    let signalled_grt = wei_to_grt(&signalled_tokens);
    Some(SignalContext {
        query_fees_grt: wei_to_grt(&query_fees_amount),
        signalled_grt,
        signalled_tokens,
        query_fees_amount,
        curator_count: answer.rows.len(),
        curator_count_capped: answer.rows.len() >= SIGNAL_FIRST,
        exposure: exposure(signalled_grt),
    })
}

/// The report with its signal overlay attached, which is the shape the page reads.
pub fn with_signal(
    report: &DisassemblyReport,
    signal: Option<&SignalContext>,
) -> serde_json::Value {
    let mut value = serde_json::to_value(report).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "signal".into(),
            // Null rather than absent: the page distinguishes "no signal on this deployment" from
            // "the nest could not be asked", and renders the second as a line saying so.
            signal
                .and_then(|s| serde_json::to_value(s).ok())
                .unwrap_or(serde_json::Value::Null),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_key_carries_the_analysis_version() {
        assert_eq!(
            cache_key("QmAbc"),
            format!("disasm:v{ANALYSIS_VERSION}:QmAbc")
        );
    }

    /// The buckets, at their edges. A deployment with nothing signalled is `none` rather than
    /// `low`, because zero at stake is a different statement from a little at stake.
    #[test]
    fn exposure_buckets_at_their_boundaries() {
        assert_eq!(exposure(0.0), "none");
        assert_eq!(exposure(0.5), "low");
        assert_eq!(exposure(999.9), "low");
        assert_eq!(exposure(1_000.0), "medium");
        assert_eq!(exposure(49_999.0), "medium");
        assert_eq!(exposure(50_000.0), "high");
    }

    /// The nest takes a SQL string rather than parameters, so the id shape is the guard. Anything
    /// that is not thirty-two hex bytes gets no query at all.
    #[test]
    fn only_a_real_deployment_id_reaches_the_nest() {
        let good = "0x".to_string() + &"ab".repeat(32);
        assert!(deployment_signals_sql(&good).is_some());
        assert!(deployment_signals_sql("0xabc").is_none());
        assert!(deployment_signals_sql("' OR 1=1 --").is_none());
        assert!(deployment_signals_sql("QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG").is_none());
    }

    #[test]
    fn wei_becomes_grt() {
        assert_eq!(wei_to_grt("1000000000000000000"), 1.0);
        assert_eq!(wei_to_grt("0"), 0.0);
        assert_eq!(wei_to_grt("not a number"), 0.0);
    }
}

#[cfg(test)]
mod signal_context_names {
    //! The field names a consumer destructures, pinned.
    //!
    //! `rename_all = "camelCase"` is right for every other field on this struct and wrong for two:
    //! the incumbent spells them `signalledGRT` and `queryFeesGRT`, and the dashboard reads those.
    //! It went unnoticed because the parity harness excuses this route entirely, so nothing
    //! compared the two shapes. A rename here is silent at compile time and invisible in a test
    //! that checks values, so the names themselves are asserted.

    use super::SignalContext;

    #[test]
    fn grt_is_spelled_as_the_consumer_reads_it() {
        let json = serde_json::to_value(SignalContext {
            signalled_tokens: "1".into(),
            query_fees_amount: "2".into(),
            signalled_grt: 3.0,
            query_fees_grt: 4.0,
            curator_count: 1,
            curator_count_capped: false,
            exposure: "low",
        })
        .expect("a plain struct serialises");

        assert_eq!(json.get("signalledGRT").and_then(|v| v.as_f64()), Some(3.0));
        assert_eq!(json.get("queryFeesGRT").and_then(|v| v.as_f64()), Some(4.0));
        assert!(
            json.get("signalledGrt").is_none(),
            "the camelCase spelling must not appear"
        );
        assert!(
            json.get("queryFeesGrt").is_none(),
            "the camelCase spelling must not appear"
        );
    }
}
