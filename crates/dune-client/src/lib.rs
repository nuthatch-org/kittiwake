//! Cached Dune results, with optional shared-ledger-gated execution support.

use std::time::Duration;

use reqwest::{
    Url,
    header::{HeaderMap, HeaderValue},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Dune client configuration")]
    Config,
    #[cfg(feature = "ledger")]
    #[error("Dune ledger admission failed")]
    Ledger(#[from] dune_ledger::Error),
    #[cfg(feature = "executions")]
    #[error(
        "Dune execution {0} was accepted but ledger attachment failed; reconcile before retrying"
    )]
    LedgerAttachment(String),
    #[error("Dune transport failed")]
    Transport,
    #[error("Dune returned HTTP {0}")]
    Http(u16),
    #[error("invalid Dune response: {0}")]
    Response(&'static str),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    pub row_count: usize,
    pub total_row_count: usize,
    pub datapoint_count: usize,
    pub result_set_bytes: u64,
    pub total_result_set_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct Status {
    pub execution_id: String,
    pub query_id: u64,
    pub state: String,
    pub is_execution_finished: bool,
    pub execution_ended_at: Option<String>,
    pub execution_cost_credits: Option<f64>,
    pub result_metadata: Option<Metadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResultData {
    pub rows: Vec<serde_json::Value>,
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub execution_id: String,
    pub query_id: u64,
    pub state: String,
    pub is_execution_finished: bool,
    pub execution_ended_at: Option<String>,
    pub result: ResultData,
    pub next_offset: Option<usize>,
}

/// A complete snapshot. Metadata on `first_page` describes that page, not the accumulated rows.
#[derive(Debug)]
pub struct Snapshot {
    pub first_page: Page,
    pub rows: Vec<serde_json::Value>,
}

/// Four tries rather than three. Dune's rate limiter sends no `Retry-After`, and a 429 measured on
/// 2026-09-14 took up to 15 seconds to clear, so the old 250ms/500ms budget gave up roughly an
/// order of magnitude too early.
const RETRY_ATTEMPTS: u32 = 4;

/// Backoff before the next attempt. A 429 is a rate limit with a window measured in seconds, so it
/// waits seconds; a 5xx is usually transient and keeps the original sub-second schedule.
fn backoff(rate_limited: bool, attempt: u32) -> Duration {
    // Bounded jitter avoids synchronised retry bursts without another RNG dependency.
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
        % 200;
    if rate_limited {
        // 1s, 2s, 4s: covers the observed 15s worst case across four attempts.
        Duration::from_millis((1000 << attempt) + jitter)
    } else {
        Duration::from_millis((250 << attempt) + jitter)
    }
}

/// Account usage for one billing period, as `POST /api/v1/usage` reports it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Usage {
    pub billing_periods: Vec<BillingPeriod>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BillingPeriod {
    pub start_date: String,
    pub end_date: String,
    pub credits_used: f64,
    pub credits_included: f64,
}

impl Usage {
    /// The period Dune is currently billing. The endpoint has only ever returned one; if that ever
    /// changes, the most-used period is the conservative choice for a ceiling check.
    pub fn current(&self) -> Option<&BillingPeriod> {
        self.billing_periods
            .iter()
            .max_by(|a, b| a.credits_used.total_cmp(&b.credits_used))
    }
}

pub struct DuneClient {
    base: Url,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    #[serde(default)]
    pub table_name: Option<String>,
    #[serde(default)]
    pub bytes_written: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
struct UploadPayload<'a> {
    table_name: &'a str,
    description: &'a str,
    data: &'a str,
    is_private: bool,
}

#[cfg(feature = "executions")]
#[derive(Debug, Deserialize)]
pub struct Execution {
    pub execution_id: String,
    pub state: String,
}

impl DuneClient {
    /// No automatic retry: a timeout can mean Dune accepted and charged the execution.
    /// The reservation remains submitted on ambiguous failures until explicit reconciliation.
    #[cfg(feature = "executions")]
    pub async fn execute(
        &self,
        query: u64,
        parameters: &std::collections::BTreeMap<String, serde_json::Value>,
        reservation: dune_ledger::LedgerReservation,
    ) -> Result<Execution, Error> {
        let url = self
            .base
            .join(&format!("query/{query}/execute"))
            .map_err(|_| Error::Config)?;
        let submitted = reservation.claim(query).await?;
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({"performance": "medium", "query_parameters": parameters}))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        let execution: Execution = response
            .json()
            .await
            .map_err(|_| Error::Response("execution JSON"))?;
        validate_execution(&execution.execution_id)?;
        submitted
            .attach_execution(&execution.execution_id)
            .await
            .map_err(|_| Error::LedgerAttachment(execution.execution_id.clone()))?;
        Ok(execution)
    }

    pub fn new(key: SecretString) -> Result<Self, Error> {
        Self::with_base(key, "https://api.dune.com/api/v1/")
    }

    /// Account usage for the current billing period.
    ///
    /// `POST`, not `GET`: a `GET` on this path returns 405 with `allow: POST`. Measured
    /// 2026-09-14, this endpoint is not billed, which is what makes it usable as an admission
    /// input rather than only a monthly report.
    pub async fn usage(&self) -> Result<Usage, Error> {
        let url = self.base.join("usage").map_err(|_| Error::Config)?;
        for attempt in 0..RETRY_ATTEMPTS {
            let response = self
                .http
                .post(url.clone())
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            let status = response.status();
            let rate_limited = status.as_u16() == 429;
            if (rate_limited || status.is_server_error()) && attempt < RETRY_ATTEMPTS - 1 {
                tokio::time::sleep(backoff(rate_limited, attempt)).await;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http(status.as_u16()));
            }
            return response
                .json()
                .await
                .map_err(|_| Error::Response("JSON shape"));
        }
        unreachable!()
    }

    /// Custom origins support local fixture tests. Redirects never carry credentials elsewhere.
    pub fn with_base(key: SecretString, base: &str) -> Result<Self, Error> {
        let base = Url::parse(base).map_err(|_| Error::Config)?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
            || key.expose_secret().is_empty()
        {
            return Err(Error::Config);
        }
        let mut value = HeaderValue::from_str(key.expose_secret()).map_err(|_| Error::Config)?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-dune-api-key", value);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self { base, http })
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<T, Error> {
        let url = self.base.join(path).map_err(|_| Error::Config)?;
        for attempt in 0..RETRY_ATTEMPTS {
            let response = self
                .http
                .get(url.clone())
                .query(params)
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            let status = response.status();
            let rate_limited = status.as_u16() == 429;
            if (rate_limited || status.is_server_error()) && attempt < RETRY_ATTEMPTS - 1 {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                let delay = retry_after
                    .map(|s| Duration::from_secs(s.min(30)))
                    .unwrap_or_else(|| backoff(rate_limited, attempt));
                tokio::time::sleep(delay).await;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http(status.as_u16()));
            }
            return response
                .json()
                .await
                .map_err(|_| Error::Response("JSON shape"));
        }
        unreachable!()
    }

    pub async fn status(&self, execution: &str) -> Result<Status, Error> {
        validate_execution(execution)?;
        let status: Status = self
            .get(&format!("execution/{execution}/status"), &[])
            .await?;
        if status.execution_id != execution {
            return Err(Error::Response("execution identity"));
        }
        Ok(status)
    }

    /// Rounds a credit cost up to whole microcredits, per the ledger's own rounding rule:
    /// overstate a measured charge rather than understate it.
    #[cfg(feature = "ledger")]
    fn cost_to_microcredits(credits: f64) -> Option<i64> {
        if !credits.is_finite() || credits < 0.0 {
            return None;
        }
        Some((credits * 1_000_000.0).ceil() as i64)
    }

    pub async fn latest_results(&self, query: u64, limit: usize) -> Result<Page, Error> {
        if query == 0 || limit == 0 {
            return Err(Error::Config);
        }
        let page: Page = self
            .get(&format!("query/{query}/results"), &page_params(limit, 0))
            .await?;
        validate_page(&page, query)?;
        Ok(page)
    }

    pub async fn results(
        &self,
        execution: &str,
        query: u64,
        limit: usize,
        offset: usize,
    ) -> Result<Page, Error> {
        validate_execution(execution)?;
        if query == 0 || limit == 0 {
            return Err(Error::Config);
        }
        let page: Page = self
            .get(
                &format!("execution/{execution}/results"),
                &page_params(limit, offset),
            )
            .await?;
        validate_page(&page, query)?;
        if page.execution_id != execution {
            return Err(Error::Response("execution identity"));
        }
        Ok(page)
    }

    /// Pin all following pages to the first execution, even if the query refreshes while reading.
    pub async fn snapshot(
        &self,
        query: u64,
        page_size: usize,
        max_rows: usize,
    ) -> Result<Snapshot, Error> {
        if max_rows == 0 {
            return Err(Error::Config);
        }
        let first = self.latest_results(query, page_size.min(max_rows)).await?;
        let total = first.result.metadata.total_row_count;
        if total > max_rows {
            return Err(Error::Response("row budget exceeded"));
        }
        let mut rows = first.result.rows.clone();
        let mut next = first.next_offset;
        while let Some(offset) = next {
            if offset != rows.len() || rows.is_empty() || offset >= total {
                return Err(Error::Response("pagination offset"));
            }
            let page = self
                .results(
                    &first.execution_id,
                    query,
                    page_size.min(max_rows - offset),
                    offset,
                )
                .await?;
            if page.result.rows.is_empty()
                || page.result.metadata.total_row_count != total
                || page.result.metadata.column_names != first.result.metadata.column_names
                || page.result.metadata.column_types != first.result.metadata.column_types
                || page.execution_ended_at != first.execution_ended_at
            {
                return Err(Error::Response("snapshot changed between pages"));
            }
            rows.extend(page.result.rows);
            if rows.len() > total {
                return Err(Error::Response("too many rows"));
            }
            next = page.next_offset;
        }
        if rows.len() != total {
            return Err(Error::Response("incomplete snapshot"));
        }
        Ok(Snapshot {
            first_page: first,
            rows,
        })
    }

    pub async fn upload_csv(
        &self,
        table_name: &str,
        description: &str,
        csv_data: &str,
        is_private: bool,
    ) -> Result<UploadResponse, Error> {
        validate_table_name(table_name)?;
        if csv_data.trim().is_empty() {
            return Err(Error::Config);
        }
        let url = self.base.join("uploads/csv").map_err(|_| Error::Config)?;
        let payload = UploadPayload {
            table_name,
            description,
            data: csv_data,
            is_private,
        };
        let response = self
            .http
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        let upload: UploadResponse = response
            .json()
            .await
            .map_err(|_| Error::Response("upload response JSON"))?;
        Ok(upload)
    }
}

fn validate_table_name(value: &str) -> Result<(), Error> {
    if value.is_empty()
        || !value.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        || !value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Err(Error::Config);
    }
    Ok(())
}

fn validate_execution(value: &str) -> Result<(), Error> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::Config);
    }
    Ok(())
}

/// Lets `dune-ledger::reconcile` learn real costs through the same read endpoint the poller
/// uses, without the ledger crate ever depending on this one.
#[cfg(feature = "ledger")]
impl dune_ledger::ExecutionCostSource for DuneClient {
    async fn execution_cost(&self, execution_id: &str) -> dune_ledger::ExecutionCost {
        let Ok(status) = self.status(execution_id).await else {
            return dune_ledger::ExecutionCost::Unknown;
        };
        if !status.is_execution_finished {
            return dune_ledger::ExecutionCost::Pending;
        }
        match status
            .execution_cost_credits
            .and_then(Self::cost_to_microcredits)
        {
            Some(microcredits) => dune_ledger::ExecutionCost::Settled { microcredits },
            None => dune_ledger::ExecutionCost::Unknown,
        }
    }
}

#[cfg(feature = "ledger")]
impl dune_ledger::AccountUsageSource for DuneClient {
    async fn account_usage(&self) -> Option<dune_ledger::AccountUsage> {
        let usage = self.usage().await.ok()?;
        let period = usage.current()?;
        // Round used up and included down, so neither rounding direction can make the account look
        // like it has headroom it does not have.
        let used = credits_to_microcredits_ceil(period.credits_used)?;
        let included = credits_to_microcredits_floor(period.credits_included)?;
        Some(dune_ledger::AccountUsage {
            used_microcredits: used,
            included_microcredits: included,
            period_start: period.start_date.clone(),
            period_end: period.end_date.clone(),
        })
    }
}

#[cfg(feature = "ledger")]
fn credits_to_microcredits_ceil(credits: f64) -> Option<i64> {
    (credits.is_finite() && credits >= 0.0).then(|| (credits * 1_000_000.0).ceil() as i64)
}

#[cfg(feature = "ledger")]
fn credits_to_microcredits_floor(credits: f64) -> Option<i64> {
    (credits.is_finite() && credits >= 0.0).then(|| (credits * 1_000_000.0).floor() as i64)
}

fn page_params(limit: usize, offset: usize) -> Vec<(String, String)> {
    vec![
        ("limit".into(), limit.to_string()),
        ("offset".into(), offset.to_string()),
    ]
}

fn validate_page(page: &Page, query: u64) -> Result<(), Error> {
    validate_execution(&page.execution_id)?;
    if page.query_id != query
        || page.state != "QUERY_STATE_COMPLETED"
        || !page.is_execution_finished
        || page
            .execution_ended_at
            .as_ref()
            .is_none_or(|s| s.is_empty())
    {
        return Err(Error::Response("not a completed result for this query"));
    }
    let m = &page.result.metadata;
    if m.row_count != page.result.rows.len()
        || m.row_count > m.total_row_count
        || m.column_names.len() != m.column_types.len()
    {
        return Err(Error::Response("result metadata"));
    }
    Ok(())
}
