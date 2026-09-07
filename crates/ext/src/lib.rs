//! Sources outside the protocol: the price of GRT and the network's TVL.
//!
//! These are the only figures on the dashboard that no nest can produce, and they are the reason
//! the "one source per route" rule needs stating carefully. A price route reads a price API; it
//! must not, on failure, quietly serve yesterday's price as today's. It reports the failure and the
//! panel shows a gap, exactly as a nest route would.

use std::time::Duration;

use kittiwake_types::ApiError;

const COINGECKO: &str = "https://api.coingecko.com/api/v3/simple/price?ids=the-graph&vs_currencies=usd&include_24hr_change=true";
const DEFILLAMA: &str = "https://api.llama.fi/v2/historicalChainTvl/Arbitrum";

#[derive(Clone)]
pub struct Ext {
    pub(crate) http: reqwest::Client,
    /// Overridable so the tests can point at a stand-in rather than the live API.
    price_url: String,
    tvl_url: String,
}

impl Default for Ext {
    fn default() -> Self {
        Self::new(COINGECKO, DEFILLAMA)
    }
}

impl Ext {
    pub fn new(price_url: impl Into<String>, tvl_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("kittiwake/", env!("CARGO_PKG_VERSION")))
                // Short: these are decorations on a page of protocol figures, and a slow price API
                // must never be why an indexer table is late.
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap_or_default(),
            price_url: price_url.into(),
            tvl_url: tvl_url.into(),
        }
    }

    /// The GRT price in USD, and the 24-hour change if the source gave one.
    pub async fn grt_price(&self) -> Result<(f64, Option<f64>), ApiError> {
        let body: serde_json::Value = self
            .http
            .get(&self.price_url)
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("price source: {e}")))?
            .error_for_status()
            .map_err(|e| ApiError::Unavailable(format!("price source: {e}")))?
            .json()
            .await
            .map_err(|e| ApiError::Unavailable(format!("price source: {e}")))?;

        let node = body.get("the-graph").ok_or_else(|| {
            ApiError::Unavailable("price source answered without a the-graph entry".into())
        })?;
        let usd = node.get("usd").and_then(|v| v.as_f64()).ok_or_else(|| {
            ApiError::Unavailable("price source answered without a usd price".into())
        })?;
        let change = node.get("usd_24h_change").and_then(|v| v.as_f64());
        Ok((usd, change))
    }

    /// The newest Arbitrum TVL point. The series is returned oldest-first, so the answer is the
    /// last element rather than the first, which is the sort of thing that reads as working when
    /// it is wrong.
    pub async fn arbitrum_tvl(&self) -> Result<f64, ApiError> {
        let body: Vec<serde_json::Value> = self
            .http
            .get(&self.tvl_url)
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("tvl source: {e}")))?
            .error_for_status()
            .map_err(|e| ApiError::Unavailable(format!("tvl source: {e}")))?
            .json()
            .await
            .map_err(|e| ApiError::Unavailable(format!("tvl source: {e}")))?;

        body.last()
            .and_then(|p| p.get("tvl"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ApiError::Unavailable("tvl source answered with an empty series".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_price_is_read_with_its_change() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/price"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "the-graph": {"usd": 0.0871, "usd_24h_change": -2.4}
            })))
            .mount(&server)
            .await;

        let ext = Ext::new(format!("{}/price", server.uri()), "unused");
        let (usd, change) = ext.grt_price().await.unwrap();
        assert_eq!(usd, 0.0871);
        assert_eq!(change, Some(-2.4));
    }

    #[tokio::test]
    async fn a_price_source_that_answers_without_a_price_is_a_failure_not_a_zero() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/price"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let ext = Ext::new(format!("{}/price", server.uri()), "unused");
        assert!(
            ext.grt_price().await.is_err(),
            "an empty answer must not become 0.0"
        );
    }

    /// The series is oldest-first. Taking the first element would quietly report a figure from
    /// 2021 and look entirely plausible.
    #[tokio::test]
    async fn the_newest_tvl_point_is_the_last_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tvl"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"date": 1, "tvl": 100.0},
                {"date": 2, "tvl": 250.0}
            ])))
            .mount(&server)
            .await;

        let ext = Ext::new("unused", format!("{}/tvl", server.uri()));
        assert_eq!(ext.arbitrum_tvl().await.unwrap(), 250.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Probing an indexer's own endpoint
// ---------------------------------------------------------------------------------------------

/// What an indexer's status endpoint says about one deployment.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexingStatus {
    pub deployment: String,
    /// The chain this deployment indexes. graph-node answers one chain per status, so the field is
    /// singular here where the GraphQL shape is a list.
    pub network: Option<String>,
    pub health: String,
    pub synced: bool,
    pub latest_block: Option<i64>,
    pub chain_head_block: Option<i64>,
    /// How far behind the chain head it is, when both figures are known.
    pub blocks_behind: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub url: String,
    pub reachable: bool,
    pub elapsed_ms: u64,
    pub statuses: Vec<IndexingStatus>,
    /// Why it failed, in words a reader can act on. Present only when `reachable` is false.
    pub error: Option<String>,
}

/// The standard status query. Asked of the indexer's own endpoint, which is unauthenticated by
/// design: an indexer that hides it cannot be checked by anyone, including its delegators.
const STATUS_QUERY: &str = r#"{"query":"{ indexingStatuses { subgraph health synced chains { network chainHeadBlock { number } latestBlock { number } } } }"}"#;

impl Ext {
    /// Ask an indexer what it is serving.
    ///
    /// The URL comes from the service registry, which anyone may write to, so it goes through the
    /// SSRF guard first: without that, an indexer can register `http://169.254.169.254/` and use
    /// our server to read our own cloud metadata.
    pub async fn probe_indexer(&self, base_url: &str) -> ProbeResult {
        let began = std::time::Instant::now();
        let url = format!("{}/status", base_url.trim_end_matches('/'));

        let checked = match kittiwake_ssrf::check_resolved(&url).await {
            Ok(u) => u,
            Err(refusal) => {
                return ProbeResult {
                    url,
                    reachable: false,
                    elapsed_ms: began.elapsed().as_millis() as u64,
                    statuses: Vec::new(),
                    error: Some(refusal.to_string()),
                };
            }
        };

        let sent = self
            .http
            .post(checked)
            .header("content-type", "application/json")
            .body(STATUS_QUERY)
            .send()
            .await;

        let body: serde_json::Value = match sent {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(v) => v,
                Err(e) => return failed(url, began, format!("unreadable answer: {e}")),
            },
            Ok(r) => return failed(url, began, format!("HTTP {}", r.status().as_u16())),
            Err(e) => {
                let why = if e.is_timeout() {
                    "timed out".to_string()
                } else {
                    e.to_string()
                };
                return failed(url, began, why);
            }
        };

        let statuses = body
            .pointer("/data/indexingStatuses")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(parse_status).collect())
            .unwrap_or_default();

        ProbeResult {
            url,
            reachable: true,
            elapsed_ms: began.elapsed().as_millis() as u64,
            statuses,
            error: None,
        }
    }
}

fn failed(url: String, began: std::time::Instant, why: String) -> ProbeResult {
    ProbeResult {
        url,
        reachable: false,
        elapsed_ms: began.elapsed().as_millis() as u64,
        statuses: Vec::new(),
        error: Some(why),
    }
}

fn parse_status(v: &serde_json::Value) -> Option<IndexingStatus> {
    let deployment = v.get("subgraph")?.as_str()?.to_string();
    let chain = v
        .get("chains")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());
    let num = |k: &str| -> Option<i64> {
        chain?
            .get(k)?
            .get("number")?
            .as_str()
            .and_then(|s| s.parse().ok())
    };
    let latest = num("latestBlock");
    let head = num("chainHeadBlock");
    Some(IndexingStatus {
        deployment,
        network: chain
            .and_then(|c| c.get("network"))
            .and_then(|n| n.as_str())
            .map(|n| n.to_string()),
        health: v
            .get("health")
            .and_then(|h| h.as_str())
            .unwrap_or("unknown")
            .to_string(),
        synced: v.get("synced").and_then(|s| s.as_bool()).unwrap_or(false),
        latest_block: latest,
        chain_head_block: head,
        // Only when both are known. A missing head would otherwise make every indexer look
        // perfectly caught up, which is the failure that looks like success.
        blocks_behind: match (head, latest) {
            (Some(h), Some(l)) => Some((h - l).max(0)),
            _ => None,
        },
    })
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_registered_url_pointing_inward_is_refused_before_any_request() {
        let ext = Ext::default();
        let r = ext.probe_indexer("http://169.254.169.254").await;
        assert!(!r.reachable);
        assert!(r.error.unwrap().contains("not an address we will fetch"));
    }

    #[tokio::test]
    async fn a_healthy_indexer_reports_how_far_behind_it_is() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"indexingStatuses": [{
                    "subgraph": "QmAbc", "health": "healthy", "synced": true,
                    "chains": [{"chainHeadBlock": {"number": "1000"}, "latestBlock": {"number": "990"}}]
                }]}
            })))
            .mount(&server)
            .await;

        // Point straight at the mock, bypassing the guard, which loopback would otherwise refuse:
        // the guard itself is tested above and in the ssrf crate.
        let ext = Ext::default();
        let body: serde_json::Value = ext
            .http
            .post(format!("{}/status", server.uri()))
            .body(STATUS_QUERY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let parsed: Vec<IndexingStatus> = body
            .pointer("/data/indexingStatuses")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_status)
            .collect();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].blocks_behind, Some(10));
        assert!(parsed[0].synced);
    }

    /// A missing chain head must not read as "perfectly caught up".
    #[test]
    fn an_unknown_chain_head_leaves_the_gap_unknown() {
        let v = serde_json::json!({
            "subgraph": "QmAbc", "health": "healthy", "synced": false,
            "chains": [{"latestBlock": {"number": "990"}}]
        });
        let s = parse_status(&v).unwrap();
        assert_eq!(s.latest_block, Some(990));
        assert_eq!(s.blocks_behind, None);
    }

    #[test]
    fn an_indexer_ahead_of_the_head_is_zero_behind_not_negative() {
        let v = serde_json::json!({
            "subgraph": "QmAbc", "health": "healthy", "synced": true,
            "chains": [{"chainHeadBlock": {"number": "990"}, "latestBlock": {"number": "1000"}}]
        });
        assert_eq!(parse_status(&v).unwrap().blocks_behind, Some(0));
    }
}

// ---------------------------------------------------------------------------------------------
// IPFS
// ---------------------------------------------------------------------------------------------

/// The Graph's public IPFS gateway. Overridable, because a self-hosted node is faster and does not
/// share a rate limit with everyone else on the network.
pub const THEGRAPH_IPFS: &str = "https://api.thegraph.com/ipfs";

/// Documents behind CIDs, cached in Postgres.
///
/// Two document kinds share one table and one CID space: subgraph metadata, which is JSON, and
/// deployment manifests, which are YAML and stored as text. A CID is one or the other, never both,
/// so `json` and `text` are separate columns rather than one polymorphic blob.
///
/// The cache is what makes search possible at all. Name search is a query against the cached
/// documents rather than against IPFS - nothing else would answer inside a request - which is why
/// the warm job exists and why a subgraph nobody has fetched is not findable until it has run.
#[derive(Clone)]
pub struct Ipfs {
    http: reqwest::Client,
    endpoint: String,
}

/// How long a failed fetch is honoured before it is tried again. An IPFS gateway that is down for
/// a minute must not cost us a re-fetch storm, and one that is down for a day must not permanently
/// poison a document.
const FAILURE_TTL: Duration = Duration::from_secs(3600);

impl Default for Ipfs {
    fn default() -> Self {
        Self::new(THEGRAPH_IPFS)
    }
}

impl Ipfs {
    pub fn new(endpoint: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                // IPFS is content-addressed and slow; eight seconds is the incumbent's figure and
                // is long enough for a cold block and short enough not to hold a warm job's slot.
                .timeout(Duration::from_secs(8))
                .user_agent(concat!("kittiwake/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// The IPFS API's `cat`, which The Graph's endpoint serves on POST rather than GET.
    pub async fn cat(&self, cid: &str) -> Result<String, String> {
        let url = format!("{}/api/v0/cat", self.endpoint);
        let res = self
            .http
            .post(&url)
            .query(&[("arg", cid)])
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "timed out".to_string()
                } else {
                    e.to_string()
                }
            })?;
        if !res.status().is_success() {
            return Err(format!("ipfs cat {cid}: HTTP {}", res.status().as_u16()));
        }
        res.text().await.map_err(|e| e.to_string())
    }
}

/// What the cache holds for one CID.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CachedDoc {
    pub cid: String,
    pub json: Option<serde_json::Value>,
    pub text: Option<String>,
    pub error: Option<String>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

impl CachedDoc {
    /// A failure recent enough that re-fetching it now would just fail again.
    pub fn failed_recently(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        // A negative age (a clock skew, a row written slightly in the future) converts to no
        // Duration at all, and is treated as recent - the conservative direction.
        self.error.is_some()
            && (now - self.fetched_at)
                .to_std()
                .map(|d| d < FAILURE_TTL)
                .unwrap_or(true)
    }
}

/// Undo a document that was stored as a JSON string containing JSON rather than as JSON.
///
/// The service this replaces writes `${JSON.stringify(doc)}::jsonb` through postgres.js, which sees
/// a JavaScript string bound to a `jsonb` column and encodes it as a JSON *string* - so the column
/// holds `"{\"displayName\":\"Lido Ethereum\"}"` rather than an object, and `json->>'displayName'`
/// finds nothing. Every one of the 15,972 cached documents is stored that way, which is why no
/// subgraph on the dashboard has a name.
///
/// We write objects. But we read a table that is mostly not ours and has years of these in it, so
/// this is tolerant on the way in and strict on the way out: a legacy row still yields a name, and
/// nothing we write adds another. It stays useful even after the table is repaired, because a
/// rollback to the old handler would start producing them again.
pub fn undouble(v: serde_json::Value) -> serde_json::Value {
    match &v {
        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(v),
        _ => v,
    }
}

/// Read whatever the cache holds for these CIDs. Absent CIDs are simply absent from the result.
pub async fn cached_docs(
    pool: &sqlx::PgPool,
    cids: &[String],
) -> Result<Vec<CachedDoc>, sqlx::Error> {
    if cids.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = sqlx::query_as::<_, CachedDoc>(
        "SELECT cid, json, text, error, fetched_at FROM ipfs_metadata WHERE cid = ANY($1)",
    )
    .bind(cids)
    .fetch_all(pool)
    .await?;
    for r in &mut rows {
        r.json = r.json.take().map(undouble);
    }
    Ok(rows)
}

/// Fetch one JSON document and record the outcome, returning what the cache now holds for it.
///
/// A document that fetched but is not JSON is recorded as `not json` and, because that is a
/// permanent property of the document rather than a transient failure, is not retried on the
/// hourly schedule the way a network error is.
pub async fn fetch_json(pool: &sqlx::PgPool, ipfs: &Ipfs, cid: &str) -> Option<serde_json::Value> {
    let (json, error) = match ipfs.cat(cid).await {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => (Some(v), None),
            Err(_) => (None, Some("not json".to_string())),
        },
        Err(e) => {
            tracing::debug!(cid, err = %e, "ipfs fetch failed");
            (None, Some(e))
        }
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO ipfs_metadata (cid, json, error, fetched_at) VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (cid) DO UPDATE SET json = EXCLUDED.json, error = EXCLUDED.error, \
         fetched_at = NOW()",
    )
    .bind(cid)
    .bind(&json)
    .bind(&error)
    .execute(pool)
    .await
    {
        tracing::warn!(cid, err = %e, "ipfs cache write failed");
    }
    json
}

/// The same for a text document - a deployment manifest.
pub async fn fetch_text(pool: &sqlx::PgPool, ipfs: &Ipfs, cid: &str) -> Option<String> {
    let (text, error) = match ipfs.cat(cid).await {
        Ok(t) => (Some(t), None),
        Err(e) => {
            tracing::debug!(cid, err = %e, "ipfs fetch failed");
            (None, Some(e))
        }
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO ipfs_metadata (cid, text, error, fetched_at) VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (cid) DO UPDATE SET text = EXCLUDED.text, error = EXCLUDED.error, \
         fetched_at = NOW()",
    )
    .bind(cid)
    .bind(&text)
    .bind(&error)
    .execute(pool)
    .await
    {
        tracing::warn!(cid, err = %e, "ipfs cache write failed");
    }
    text
}

#[cfg(test)]
mod ipfs_tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn cat_posts_the_cid_as_a_query_argument() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v0/cat"))
            .and(query_param("arg", "QmAbc"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("{\"displayName\":\"Uniswap\"}"),
            )
            .mount(&server)
            .await;

        let ipfs = Ipfs::new(&server.uri());
        assert_eq!(
            ipfs.cat("QmAbc").await.unwrap(),
            "{\"displayName\":\"Uniswap\"}"
        );
    }

    /// A 404 is an error, not an empty document. Returning `Ok("")` here would cache a subgraph
    /// with no name as if that were the truth.
    #[tokio::test]
    async fn a_missing_document_is_an_error_rather_than_an_empty_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v0/cat"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = Ipfs::new(&server.uri()).cat("QmAbc").await.unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    /// The bug that leaves every subgraph on the dashboard nameless.
    #[test]
    fn a_document_stored_as_a_string_of_json_is_read_as_the_document() {
        let wrapped = serde_json::json!("{\"displayName\":\"Lido Ethereum\"}");
        assert_eq!(
            undouble(wrapped)["displayName"],
            serde_json::json!("Lido Ethereum")
        );
    }

    #[test]
    fn a_document_stored_properly_is_left_alone() {
        let plain = serde_json::json!({"displayName": "Lido Ethereum"});
        assert_eq!(undouble(plain.clone()), plain);
    }

    /// A string that is not JSON is a value in its own right, not a broken document. Parsing it
    /// leniently would turn `"123"` into the number 123 and a name into something else entirely.
    #[test]
    fn a_string_that_is_not_json_stays_a_string() {
        let s = serde_json::json!("just a name");
        assert_eq!(undouble(s.clone()), s);
    }

    #[test]
    fn a_recent_failure_is_honoured_and_an_old_one_is_not() {
        let now = chrono::Utc::now();
        let doc = |err: Option<&str>, age_secs: i64| CachedDoc {
            cid: "QmAbc".into(),
            json: None,
            text: None,
            error: err.map(|e| e.to_string()),
            fetched_at: now - chrono::Duration::seconds(age_secs),
        };
        assert!(doc(Some("timed out"), 60).failed_recently(now));
        assert!(!doc(Some("timed out"), 7200).failed_recently(now));
        // A success is never "failed recently", however old it is.
        assert!(!doc(None, 60).failed_recently(now));
    }
}
