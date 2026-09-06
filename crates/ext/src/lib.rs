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
    http: reqwest::Client,
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
