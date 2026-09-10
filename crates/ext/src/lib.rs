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

/// The whole graph-support archive in one request.
///
/// `state=all` because the archive is mostly closed threads: the write-ups are the product, and a
/// page of open issues would be the least useful two thirds of it. `per_page=100` fits the
/// repository several times over, and the caller pages anyway rather than trusting that.
///
/// No credential. The repository is public and the job that calls this runs twice a day, which is
/// two of the sixty requests an hour GitHub allows unauthenticated. A token here would be a thing
/// that can expire in exchange for headroom nothing needs.
const GRAPH_SUPPORT: &str = "https://api.github.com/repos/nightswatchhq/graph-support/issues?state=all&sort=updated&direction=desc&per_page=100";

/// Every comment in the repository, in one request rather than one per issue.
///
/// The obvious shape, walking `/issues/{n}/comments` for each issue, is thirty-four requests a run
/// today and one more for every issue ever filed. Unauthenticated GitHub allows sixty an hour, so
/// that design works until the archive is about sixty issues and then quietly starts failing
/// halfway through a run. This endpoint is one request for all of them, with `issue_url` to map
/// each back, and stays one request however large the repository gets.
const GRAPH_SUPPORT_COMMENTS: &str =
    "https://api.github.com/repos/nightswatchhq/graph-support/issues/comments?per_page=100";

/// The issue-form directory, listed rather than enumerated.
///
/// Naming the six files here would mean a seventh template, or a rename, silently never reaching
/// the dashboard's compose form. Listing the directory costs one request and cannot go stale.
const GRAPH_SUPPORT_TEMPLATES: &str =
    "https://api.github.com/repos/nightswatchhq/graph-support/contents/.github/ISSUE_TEMPLATE";

#[derive(Clone)]
pub struct Ext {
    pub(crate) http: reqwest::Client,
    /// Overridable so the tests can point at a stand-in rather than the live API.
    price_url: String,
    tvl_url: String,
    graph_support_url: String,
    graph_support_comments_url: String,
    graph_support_templates_url: String,
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
            graph_support_url: GRAPH_SUPPORT.to_string(),
            graph_support_comments_url: GRAPH_SUPPORT_COMMENTS.to_string(),
            graph_support_templates_url: GRAPH_SUPPORT_TEMPLATES.to_string(),
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

    /// Point the archive fetch at a stand-in. Tests only; the default is the live repository.
    pub fn with_graph_support_url(mut self, url: impl Into<String>) -> Self {
        self.graph_support_url = url.into();
        self
    }

    /// Point the comment fetch at a stand-in. Tests only.
    pub fn with_graph_support_comments_url(mut self, url: impl Into<String>) -> Self {
        self.graph_support_comments_url = url.into();
        self
    }

    /// Point the template fetch at a stand-in. Tests only.
    pub fn with_graph_support_templates_url(mut self, url: impl Into<String>) -> Self {
        self.graph_support_templates_url = url.into();
        self
    }

    /// The issue forms, as the YAML the repository holds.
    ///
    /// `config.yml` is skipped: it configures the chooser rather than describing a form, and
    /// rendering it would produce a template nobody can file.
    ///
    /// A directory listing plus one request per file, so seven today. That is a lot next to the two
    /// the rest of the mirror spends, and it is still twice a day against a sixty-an-hour
    /// allowance. Enumerating the filenames instead would cost one request and silently miss a
    /// seventh template for ever, which is the more expensive mistake.
    pub async fn graph_support_templates(&self) -> Result<Vec<GraphSupportTemplate>, ApiError> {
        let listing: Vec<GhContent> = self
            .http
            .get(&self.graph_support_templates_url)
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("graph-support templates: {e}")))?
            .error_for_status()
            .map_err(|e| ApiError::Unavailable(format!("graph-support templates: {e}")))?
            .json()
            .await
            .map_err(|e| ApiError::Unavailable(format!("graph-support templates: {e}")))?;

        let mut out = Vec::new();
        for entry in listing {
            if entry.name == "config.yml" || !entry.name.ends_with(".yml") {
                continue;
            }
            let Some(url) = entry.download_url else {
                continue;
            };
            let yaml = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support template: {e}")))?
                .error_for_status()
                .map_err(|e| ApiError::Unavailable(format!("graph-support template: {e}")))?
                .text()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support template: {e}")))?;
            out.push(GraphSupportTemplate {
                file: entry.name,
                yaml,
            });
        }

        if out.is_empty() {
            return Err(ApiError::Unavailable(
                "graph-support has no issue templates, which it has never been true of".into(),
            ));
        }
        Ok(out)
    }

    /// Every comment in the repository, with the issue number each belongs to.
    ///
    /// An empty answer is allowed here, unlike the issue list: a repository can genuinely have
    /// issues nobody has replied to yet, and refusing that would be inventing a fault.
    pub async fn graph_support_thread_comments(
        &self,
    ) -> Result<Vec<GraphSupportComment>, ApiError> {
        let mut collected: Vec<GraphSupportComment> = Vec::new();

        for page in 1..=20u32 {
            let batch: Vec<GhComment> = self
                .http
                .get(&self.graph_support_comments_url)
                .query(&[("page", page.to_string())])
                .send()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support comments: {e}")))?
                .error_for_status()
                .map_err(|e| ApiError::Unavailable(format!("graph-support comments: {e}")))?
                .json()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support comments: {e}")))?;

            let full_page = batch.len() == 100;
            for c in batch {
                // `issue_url` ends in the number. A comment whose parent cannot be read is dropped
                // rather than filed under a guess.
                let Some(number) = c
                    .issue_url
                    .rsplit('/')
                    .next()
                    .and_then(|n| n.parse::<i32>().ok())
                else {
                    continue;
                };
                collected.push(GraphSupportComment {
                    id: c.id,
                    issue_number: number,
                    author: c.user.and_then(|u| u.login).unwrap_or_default(),
                    body: c.body.unwrap_or_default(),
                    url: c.html_url,
                    created_at: c.created_at,
                    updated_at: c.updated_at,
                });
            }
            if !full_page {
                break;
            }
        }

        Ok(collected)
    }

    /// Every issue in nightswatchhq/graph-support, pull requests removed.
    ///
    /// An empty answer is a failure rather than an empty archive. That repository has never had
    /// zero issues, so a 200 with nothing in it means the request was wrong, not the repository -
    /// and writing it would replace a mirror of thirty-three worked answers with a mirror of none.
    pub async fn graph_support_issues(&self) -> Result<Vec<GraphSupportIssue>, ApiError> {
        let mut collected: Vec<GraphSupportIssue> = Vec::new();

        // Enough for the repository to grow thirty-fold before the cap is what truncates it.
        for page in 1..=10u32 {
            let batch: Vec<GhIssue> = self
                .http
                .get(&self.graph_support_url)
                .query(&[("page", page.to_string())])
                .send()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support: {e}")))?
                .error_for_status()
                .map_err(|e| ApiError::Unavailable(format!("graph-support: {e}")))?
                .json()
                .await
                .map_err(|e| ApiError::Unavailable(format!("graph-support: {e}")))?;

            let full_page = batch.len() == 100;
            for gh in batch {
                // The issues endpoint returns pull requests too, and they are not support threads.
                if gh.pull_request.is_some() {
                    continue;
                }
                collected.push(GraphSupportIssue {
                    number: gh.number,
                    title: gh.title,
                    body: gh.body.unwrap_or_default(),
                    url: gh.html_url,
                    state: if gh.state == "closed" {
                        "closed"
                    } else {
                        "open"
                    }
                    .to_string(),
                    labels: gh.labels.into_iter().filter_map(|l| l.name).collect(),
                    comments: gh.comments,
                    created_at: gh.created_at,
                    updated_at: gh.updated_at,
                });
            }
            if !full_page {
                break;
            }
        }

        if collected.is_empty() {
            return Err(ApiError::Unavailable(
                "graph-support answered with no issues at all".into(),
            ));
        }
        Ok(collected)
    }
}

/// One issue, reduced to what the dashboard renders.
#[derive(Debug, Clone)]
pub struct GraphSupportIssue {
    pub number: i32,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub labels: Vec<String>,
    pub comments: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Only the fields read above.
#[derive(serde::Deserialize)]
struct GhIssue {
    number: i32,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    state: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    comments: i32,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    /// Present on pull requests, which is how they are told apart from issues.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct GhLabel {
    name: Option<String>,
}

/// One comment, reduced to what the page renders.
#[derive(Debug, Clone)]
pub struct GraphSupportComment {
    pub id: i64,
    pub issue_number: i32,
    pub author: String,
    pub body: String,
    pub url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
struct GhComment {
    id: i64,
    /// `.../repos/owner/repo/issues/33`, which is the only link back to the parent.
    issue_url: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
struct GhUser {
    login: Option<String>,
}

/// One issue template, as bytes. Parsed by whoever renders it, not here.
#[derive(Debug, Clone)]
pub struct GraphSupportTemplate {
    pub file: String,
    pub yaml: String,
}

/// One entry from the contents API's directory listing.
#[derive(serde::Deserialize)]
struct GhContent {
    name: String,
    download_url: Option<String>,
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

/// How long an indexer's own `/status` endpoint gets to answer.
///
/// **Eight seconds, deliberately, against the incumbent's three.** Measured on 2026-09-09: of the
/// 44 chains this probe sees, **thirteen are served by two indexers or fewer**. A chain with two
/// indexers loses its entire sample if one of them is slow, so a shorter budget does not sharpen
/// the reading, it deletes the chain - and a chain served only by nodes that take four seconds to
/// answer is still a chain.
///
/// The cost is bounded and is not on any request path: the probe runs from a background job, ten
/// at a time, and nothing waits on it. See #7 and `docs/decisions.md`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

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
            // Explicit rather than inherited. The client's own eight seconds is set for the price
            // and TVL APIs, where the argument is the opposite one - a slow third party must never
            // be why an indexer table is late. The two numbers coinciding today is a coincidence,
            // and one of them changing should not silently change the other. #7
            .timeout(PROBE_TIMEOUT)
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

/// One indexer's answer about one deployment, and what it cost to get it.
///
/// Raw on purpose. Deciding what a 403 of HTML means is [`kittiwake_read::indexing_status`]'s job,
/// and this crate cannot ask it: `read` depends on `ext` and not the other way round. So the
/// evidence comes back here and the judgement is made where the rules and their tests live.
#[derive(Debug, Clone, Default)]
pub struct HttpOutcome {
    pub status: Option<u16>,
    pub content_type: String,
    pub body: String,
    /// Set when no response arrived at all: `timeout`, or the transport's own words. A response
    /// that arrived and was disliked leaves this `None` and fills in the three above.
    pub error: Option<String>,
    pub attempts: u32,
    pub elapsed_ms: u64,
}

/// A retry budget for the serving probe, and only for the absence of a response.
///
/// `broken` is load-bearing: against a deployment with one allocated indexer it is the whole
/// "effectively dead" verdict. So a transport failure gets exactly one more go after a short pause,
/// while a response of any kind is classified once - there is nothing to learn by asking an
/// endpoint to repeat an answer it has already given.
const PROBE_RETRY_PAUSE: Duration = Duration::from_millis(300);

impl Ext {
    /// Ask one indexer about one deployment, rather than about everything it serves.
    ///
    /// `probe_indexer` above asks for the whole list, which is right for the chain-health sweep and
    /// wrong here: an indexer serving two thousand deployments would send two thousand statuses to
    /// answer a question about one of them.
    /// Returns the raw `indexingStatuses[0]` rather than a parsed struct. `IndexingStatus` above
    /// is the chain-health sweep's shape and is serialised straight onto `/api/indexer-status`;
    /// widening it to carry `fatalError` and the rest would change that route's payload to suit a
    /// different one. The caller parses what it needs.
    pub async fn deployment_status(
        &self,
        base_url: &str,
        ipfs_hash: &str,
    ) -> Option<serde_json::Value> {
        let url = format!("{}/status", base_url.trim_end_matches('/'));
        // The registry is writable by anyone, so the URL is guarded before it is fetched. Without
        // this an indexer can register `http://169.254.169.254/` and have us read our own cloud
        // metadata for them.
        let checked = kittiwake_ssrf::check_resolved(&url).await.ok()?;
        // Interpolated rather than bound, so the hash is checked first. A deployment id is base58
        // or hex and nothing else; anything with a quote in it never reaches the query.
        if !ipfs_hash
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'x' || b == b'X')
        {
            return None;
        }
        let body = format!(
            r#"{{"query":"{{ indexingStatuses(subgraphs: [\"{ipfs_hash}\"]) {{ subgraph health synced entityCount fatalError {{ message }} nonFatalErrors {{ message }} chains {{ network chainHeadBlock {{ number }} latestBlock {{ number }} }} }} }}"}}"#
        );
        let res = self
            .http
            .post(checked)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
        }
        let json: serde_json::Value = res.json().await.ok()?;
        json.pointer("/data/indexingStatuses")?
            .as_array()?
            .first()
            .cloned()
    }

    /// Ask the paid query path whether it answers, and bring back what it said.
    ///
    /// `receipt` is a signed TAP receipt when one is available. With it, the probe gets past the
    /// payment gate and can see deployment-level failures - the only way to tell "the gate is up"
    /// from "the gate is up and the thing behind it is broken". Without it the reading is weaker
    /// and still worth having, which is what the handler falls back to when no key is configured.
    pub async fn probe_serving(
        &self,
        base_url: &str,
        ipfs_hash: &str,
        receipt: Option<&str>,
        timeout: Duration,
    ) -> HttpOutcome {
        let began = std::time::Instant::now();
        let elapsed = |b: std::time::Instant| b.elapsed().as_millis() as u64;
        let url = format!(
            "{}/subgraphs/id/{}",
            base_url.trim_end_matches('/'),
            ipfs_hash
        );

        let checked = match kittiwake_ssrf::check_resolved(&url).await {
            Ok(u) => u,
            Err(refusal) => {
                return HttpOutcome {
                    error: Some(refusal.to_string()),
                    attempts: 0,
                    elapsed_ms: elapsed(began),
                    ..Default::default()
                };
            }
        };

        let mut attempts = 0;
        let mut last_error = String::from("no attempt was made");
        for round in 0..2u32 {
            if round > 0 {
                tokio::time::sleep(PROBE_RETRY_PAUSE).await;
            }
            attempts += 1;
            let mut req = self
                .http
                .post(checked.clone())
                .header("content-type", "application/json")
                .timeout(timeout)
                .body(r#"{"query":"{ _meta { block { number } } }"}"#);
            if let Some(r) = receipt {
                req = req.header("TAP-Receipt", r);
            }
            match req.send().await {
                Ok(res) => {
                    let status = res.status().as_u16();
                    let content_type = res
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let body = res.text().await.unwrap_or_default();
                    // An answer, of whatever kind. Asking again would only get the same one.
                    return HttpOutcome {
                        status: Some(status),
                        content_type,
                        body,
                        error: None,
                        attempts,
                        elapsed_ms: elapsed(began),
                    };
                }
                Err(e) => {
                    last_error = if e.is_timeout() {
                        "timeout".to_string()
                    } else {
                        e.to_string()
                    };
                }
            }
        }
        HttpOutcome {
            error: Some(last_error),
            attempts,
            elapsed_ms: elapsed(began),
            ..Default::default()
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

    /// The same `cat`, for an artefact that is not text.
    ///
    /// A compiled mapping module is binary and can be tens of megabytes, so this takes its own
    /// longer timeout and refuses anything over the cap rather than holding it in memory to find
    /// out how big it was. The client's eight seconds is right for a manifest and short for a
    /// module on a cold gateway block.
    pub async fn cat_bytes(&self, cid: &str, cap: usize) -> Result<Vec<u8>, String> {
        let url = format!("{}/api/v0/cat", self.endpoint);
        let res = self
            .http
            .post(&url)
            .query(&[("arg", cid)])
            .timeout(Duration::from_secs(20))
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
        // The declared length first, so an oversized artefact is refused before it is downloaded.
        if let Some(len) = res.content_length()
            && len > cap as u64
        {
            return Err(format!("Artifact {cid} is {len} bytes (>{cap} cap)"));
        }
        let bytes = res.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > cap {
            return Err(format!(
                "Artifact {cid} is {} bytes (>{cap} cap)",
                bytes.len()
            ));
        }
        Ok(bytes.to_vec())
    }

    /// Put one JSON document through the IPFS API and return the CID it actually assigned.
    /// Metadata is content-addressed, so accepting an upstream response without a CID would be
    /// reporting a publication that nobody can subsequently resolve.
    pub async fn add_json(&self, value: &serde_json::Value) -> Result<String, String> {
        let part = reqwest::multipart::Part::text(value.to_string())
            .mime_str("application/json")
            .map_err(|e| e.to_string())?
            .file_name("metadata.json");
        let form = reqwest::multipart::Form::new().part("file", part);
        let response: serde_json::Value = self
            .http
            .post(format!("{}/api/v0/add", self.endpoint))
            .query(&[("pin", "true")])
            .multipart(form)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        response
            .get("Hash")
            .and_then(|v| v.as_str())
            .filter(|s| s.starts_with("Qm"))
            .map(str::to_string)
            .ok_or_else(|| "IPFS add answered without a CIDv0 Hash".to_string())
    }

    /// Forward a graph-cli IPFS API request after the edge has authenticated its deploy key.
    /// Authentication is intentionally not forwarded: the upstream is our configured IPFS node,
    /// not a second authority for the caller's Studio key.
    pub async fn proxy(
        &self,
        method: reqwest::Method,
        path: &str,
        query: Option<&str>,
        body: bytes::Bytes,
        content_type: Option<&str>,
    ) -> Result<(u16, bytes::Bytes, Option<String>), String> {
        let mut url = format!("{}/{}", self.endpoint, path.trim_start_matches('/'));
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            url.push('?');
            url.push_str(q);
        }
        let mut request = self.http.request(method, url).body(body);
        if let Some(ct) = content_type {
            request = request.header("content-type", ct);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        Ok((status, bytes, ct))
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

/// One document, from the cache if it is there and from IPFS if it is not.
///
/// A CID names its own content, so a hit is correct for ever and there is no freshness question to
/// get wrong. What needs a rule is the *miss* that failed: a gateway down for a minute must not
/// cost a re-fetch storm, so a recorded failure is honoured for [`FAILURE_TTL`] and retried after.
/// The warm job writes the same rows, so a document it has already pulled costs a route nothing.
pub async fn read_through_text(pool: &sqlx::PgPool, ipfs: &Ipfs, cid: &str) -> Option<String> {
    if let Ok(docs) = cached_docs(pool, std::slice::from_ref(&cid.to_string())).await
        && let Some(doc) = docs.into_iter().next()
    {
        if doc.text.is_some() {
            return doc.text;
        }
        let recent = chrono::Utc::now()
            .signed_duration_since(doc.fetched_at)
            .to_std()
            .map(|age| age < FAILURE_TTL)
            .unwrap_or(false);
        if doc.error.is_some() && recent {
            return None;
        }
    }
    fetch_text(pool, ipfs, cid).await
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

    fn gh(number: i32, state: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": format!("bad indexers: BadResponse(400) #{number}"),
            "html_url": format!("https://github.com/nightswatchhq/graph-support/issues/{number}"),
            "state": state,
            "labels": [{"name": "area/gateway"}, {"name": "owner/edge-and-node"}],
            "comments": 4,
            "created_at": "2026-09-01T00:00:00Z",
            "updated_at": "2026-09-08T00:00:00Z",
        })
    }

    #[tokio::test]
    async fn the_archive_is_read_with_its_labels_flattened() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh(31, "open")]))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        let issues = ext.graph_support_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 31);
        assert_eq!(issues[0].state, "open");
        assert_eq!(
            issues[0].labels,
            vec!["area/gateway", "owner/edge-and-node"]
        );
    }

    #[tokio::test]
    async fn pull_requests_are_not_support_threads() {
        // The issues endpoint returns both, told apart only by a `pull_request` key.
        let mut pr = gh(32, "open");
        pr["pull_request"] = serde_json::json!({"url": "https://example.invalid"});
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh(31, "open"), pr]))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        let issues = ext.graph_support_issues().await.unwrap();
        assert_eq!(
            issues.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![31]
        );
    }

    #[tokio::test]
    async fn an_empty_archive_is_a_failure_not_an_empty_archive() {
        // That repository has never had zero issues. Writing this to the mirror would replace
        // thirty-three worked answers with none, and the page would render it as "no issues".
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        assert!(ext.graph_support_issues().await.is_err());
    }

    #[tokio::test]
    async fn a_rejection_is_a_failure_rather_than_a_short_archive() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        assert!(ext.graph_support_issues().await.is_err());
    }

    #[tokio::test]
    async fn a_short_page_ends_the_walk() {
        // One request, not ten: the stop condition is a page that did not come back full.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh(31, "closed")]))
            .expect(1)
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        ext.graph_support_issues().await.unwrap();
    }

    fn gh_comment(id: i64, issue: i32) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "issue_url": format!("https://api.github.com/repos/nightswatchhq/graph-support/issues/{issue}"),
            "html_url": format!("https://github.com/nightswatchhq/graph-support/issues/{issue}#issuecomment-{id}"),
            "body": "the gateway is routing to an indexer whose store is down",
            "user": {"login": "cargopete"},
            "created_at": "2026-09-02T00:00:00Z",
            "updated_at": "2026-09-02T00:00:00Z",
        })
    }

    #[tokio::test]
    async fn a_comment_is_filed_under_the_issue_its_url_names() {
        // `issue_url` is the only link back to the parent, and getting it wrong files the whole
        // thread under the wrong issue or drops it. Neither errors.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh_comment(9001, 31)]))
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_comments_url(format!("{}/comments", server.uri()));
        let comments = ext.graph_support_thread_comments().await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].issue_number, 31);
        assert_eq!(comments[0].id, 9001);
        assert_eq!(comments[0].author, "cargopete");
    }

    #[tokio::test]
    async fn a_comment_whose_parent_cannot_be_read_is_dropped_not_guessed() {
        let mut orphan = gh_comment(9002, 31);
        orphan["issue_url"] = serde_json::json!("https://api.github.com/nonsense");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/comments"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(vec![gh_comment(9001, 31), orphan]),
            )
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_comments_url(format!("{}/comments", server.uri()));
        let comments = ext.graph_support_thread_comments().await.unwrap();
        assert_eq!(
            comments.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![9001]
        );
    }

    #[tokio::test]
    async fn no_comments_is_a_legitimate_answer_here() {
        // Unlike the issue list. A repository where nobody has replied yet is a real state, and
        // refusing it would be inventing a fault.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_comments_url(format!("{}/comments", server.uri()));
        assert!(
            ext.graph_support_thread_comments()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_rejected_comment_fetch_is_a_failure_not_a_silent_empty_thread() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/comments"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_comments_url(format!("{}/comments", server.uri()));
        assert!(ext.graph_support_thread_comments().await.is_err());
    }

    #[tokio::test]
    async fn an_issue_body_comes_across_with_the_issue() {
        let mut issue = gh(31, "open");
        issue["body"] = serde_json::json!("## What happened\n\nEvery allocated indexer 400s.");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue]))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        let issues = ext.graph_support_issues().await.unwrap();
        assert!(issues[0].body.contains("Every allocated indexer 400s"));
    }

    #[tokio::test]
    async fn an_issue_filed_with_no_body_reads_as_empty_rather_than_failing() {
        let mut issue = gh(31, "open");
        issue["body"] = serde_json::Value::Null;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue]))
            .mount(&server)
            .await;

        let ext = Ext::default().with_graph_support_url(format!("{}/issues", server.uri()));
        assert_eq!(ext.graph_support_issues().await.unwrap()[0].body, "");
    }

    #[tokio::test]
    async fn the_templates_are_listed_and_fetched_verbatim() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/templates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "01-query-error.yml", "download_url": format!("{}/raw/01", server.uri())},
                {"name": "config.yml", "download_url": format!("{}/raw/cfg", server.uri())},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/raw/01"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("name: Query fails\nbody: []\n"),
            )
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_templates_url(format!("{}/templates", server.uri()));
        let out = ext.graph_support_templates().await.unwrap();

        // config.yml configures the chooser rather than describing a form, so it is not offered.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "01-query-error.yml");
        assert!(out[0].yaml.contains("name: Query fails"));
    }

    #[tokio::test]
    async fn a_repository_with_no_templates_is_a_failure_not_an_empty_chooser() {
        // It has never been true of that repository, so nothing here means the request was wrong.
        // A compose page with no options would read as a decision rather than a fault.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/templates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_templates_url(format!("{}/templates", server.uri()));
        assert!(ext.graph_support_templates().await.is_err());
    }

    #[tokio::test]
    async fn a_seventh_template_arrives_without_anyone_being_told() {
        // The reason the directory is listed rather than the six filenames enumerated.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/templates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "07-brand-new.yml", "download_url": format!("{}/raw/07", server.uri())},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/raw/07"))
            .respond_with(ResponseTemplate::new(200).set_body_string("name: Brand new\n"))
            .mount(&server)
            .await;

        let ext =
            Ext::default().with_graph_support_templates_url(format!("{}/templates", server.uri()));
        let out = ext.graph_support_templates().await.unwrap();
        assert_eq!(out[0].file, "07-brand-new.yml");
    }
}
