//! The few figures only the chain has.
//!
//! Everything else on the dashboard comes from a nest, which is a fold over history and can be
//! asked "as of when". A contract read cannot: it is true at one block and says nothing about any
//! other. So these values carry no nest provenance and are cached briefly rather than folded, and
//! a failed read is reported rather than replaced with the last known answer.

use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use kittiwake_types::ApiError;

/// The Rewards Eligibility Oracle (GIP-0079) on Arbitrum One.
pub const REO_ADDRESS: &str = "0x8ec2767a9d9ba02b4e09e8ff4fac2e14a340f304";
/// GRT on Arbitrum One.
pub const GRT_ARBITRUM: &str = "0x9623063377ad1b27544c965ccd7342f7ea7e88c7";
/// GRT on Ethereum mainnet, where the token was minted.
pub const GRT_MAINNET: &str = "0xc944e90c64b2c07662a292be6244bdf05cda44a7";

sol! {
    #[sol(rpc)]
    interface IRewardsEligibilityOracle {
        function isEligible(address indexer) external view returns (bool);
        function getEligibilityRenewalTime(address indexer) external view returns (uint256);
        function getEligibilityPeriod() external view returns (uint256);
        /// Whether the oracle's verdict is being enforced at all. When this is false, an
        /// "ineligible" reading is advisory and must not be reported as a live penalty.
        function getEligibilityValidation() external view returns (bool);
        function getLastOracleUpdateTime() external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC20 {
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// What the oracle says about one indexer, with the context needed to read it correctly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Eligibility {
    pub eligible: bool,
    /// When this indexer's eligibility was last renewed, as a unix timestamp.
    pub renewed_at: Option<u64>,
    /// How long a renewal lasts.
    pub period_seconds: Option<u64>,
    /// Seconds of runway left, or `None` when either figure above is missing.
    pub expires_in: Option<i64>,
    /// Whether the protocol is enforcing the oracle. False means an ineligible reading costs the
    /// indexer nothing today, which changes what the dashboard should say about it.
    pub enforced: bool,
    /// When the oracle itself last ran. A stale oracle makes every verdict stale.
    pub oracle_updated_at: Option<u64>,
}

#[derive(Clone)]
pub struct Chain {
    arbitrum: String,
    mainnet: Vec<String>,
}

/// Public endpoints, used only for ENS and supply reads. Ordered: the first that answers wins.
const PUBLIC_MAINNET: [&str; 3] = [
    "https://ethereum-rpc.publicnode.com",
    "https://eth.llamarpc.com",
    "https://cloudflare-eth.com",
];

impl Chain {
    /// `arbitrum_rpc` is required: every protocol contract read goes there and there is no public
    /// endpoint that will carry the load politely.
    pub fn new(arbitrum_rpc: impl Into<String>, mainnet_rpc: Option<String>) -> Self {
        Self {
            arbitrum: arbitrum_rpc.into(),
            mainnet: match mainnet_rpc {
                Some(url) => vec![url],
                None => PUBLIC_MAINNET.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    fn arb(&self) -> Result<impl Provider + Clone, ApiError> {
        let url = self
            .arbitrum
            .parse()
            .map_err(|_| ApiError::Internal(format!("bad arbitrum rpc url: {}", self.arbitrum)))?;
        Ok(ProviderBuilder::new().connect_http(url))
    }

    /// Read the oracle for one indexer.
    ///
    /// Five calls, run concurrently: they are independent views and serialising them would make a
    /// profile page wait five round trips instead of one.
    pub async fn eligibility(&self, indexer: &str) -> Result<Eligibility, ApiError> {
        let addr = Address::from_str(indexer)
            .map_err(|_| ApiError::BadRequest(format!("not an address: {indexer}")))?;
        let provider = self.arb()?;
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            provider,
        );

        // Bound to locals first: a call builder is a temporary, and joining them inline would drop
        // each one at the end of the statement that created it.
        let c_eligible = oracle.isEligible(addr);
        let c_renewed = oracle.getEligibilityRenewalTime(addr);
        let c_period = oracle.getEligibilityPeriod();
        let c_enforced = oracle.getEligibilityValidation();
        let c_updated = oracle.getLastOracleUpdateTime();

        let (eligible, renewed, period, enforced, updated) = tokio::join!(
            c_eligible.call(),
            c_renewed.call(),
            c_period.call(),
            c_enforced.call(),
            c_updated.call(),
        );

        // The eligibility flag is the answer; the rest is context. A missing context value is
        // reported as absent rather than as a zero, which would read as "expired".
        let eligible = eligible.map_err(|e| ApiError::Unavailable(format!("oracle: {e}")))?;
        let renewed_at = renewed.ok().map(u256_to_u64);
        let period_seconds = period.ok().map(u256_to_u64);
        let expires_in = match (renewed_at, period_seconds) {
            (Some(r), Some(p)) if r > 0 => Some((r + p) as i64 - now_secs()),
            _ => None,
        };

        Ok(Eligibility {
            eligible,
            renewed_at,
            period_seconds,
            expires_in,
            enforced: enforced.unwrap_or(false),
            oracle_updated_at: updated.ok().map(u256_to_u64),
        })
    }

    /// Total GRT supply on a chain, in wei as a decimal string.
    ///
    /// A string rather than a number on purpose: the supply exceeds what an f64 holds exactly, and
    /// a total supply that is subtly wrong is worse than one that is missing.
    pub async fn grt_total_supply(&self, chain: SupplyChain) -> Result<String, ApiError> {
        let (token, urls) = match chain {
            SupplyChain::Arbitrum => (GRT_ARBITRUM, vec![self.arbitrum.clone()]),
            SupplyChain::Mainnet => (GRT_MAINNET, self.mainnet.clone()),
        };
        let token = Address::from_str(token).expect("token address is a constant");

        let mut last: Option<String> = None;
        for url in urls {
            let Ok(parsed) = url.parse() else { continue };
            let provider = ProviderBuilder::new().connect_http(parsed);
            let erc20 = IERC20::new(token, provider);
            match tokio::time::timeout(Duration::from_secs(10), erc20.totalSupply().call()).await {
                Ok(Ok(v)) => return Ok(v.to_string()),
                Ok(Err(e)) => last = Some(e.to_string()),
                Err(_) => last = Some("timed out".into()),
            }
        }
        Err(ApiError::Unavailable(format!(
            "no endpoint answered for the GRT supply: {}",
            last.unwrap_or_else(|| "none configured".into())
        )))
    }

    /// The current Arbitrum block, used by the health endpoint to say whether the RPC is answering.
    pub async fn block_number(&self) -> Result<u64, ApiError> {
        let provider = self.arb()?;
        tokio::time::timeout(Duration::from_secs(8), provider.get_block_number())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("arbitrum rpc: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupplyChain {
    Mainnet,
    Arbitrum,
}

fn u256_to_u64(v: U256) -> u64 {
    // Timestamps and periods fit in u64; saturating rather than wrapping, so a nonsense value from
    // a misbehaving contract reads as "very far away" instead of as "in 1970".
    v.try_into().unwrap_or(u64::MAX)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_addresses_parse() {
        for a in [REO_ADDRESS, GRT_ARBITRUM, GRT_MAINNET] {
            assert!(Address::from_str(a).is_ok(), "{a}");
        }
    }

    #[test]
    fn a_bad_address_is_a_bad_request_not_a_panic() {
        let chain = Chain::new("http://127.0.0.1:1", None);
        let err = futures_lite_block(chain.eligibility("nope")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    #[test]
    fn a_nonsense_timestamp_reads_as_far_away_rather_than_1970() {
        assert_eq!(u256_to_u64(U256::MAX), u64::MAX);
        assert_eq!(u256_to_u64(U256::from(1_788_000_000u64)), 1_788_000_000);
    }

    #[test]
    fn mainnet_falls_back_across_public_endpoints_only_when_none_is_configured() {
        assert_eq!(
            Chain::new("x", Some("https://mine".into())).mainnet.len(),
            1
        );
        assert_eq!(Chain::new("x", None).mainnet.len(), PUBLIC_MAINNET.len());
    }

    /// A tiny runtime so the address-validation test does not need the tokio macro.
    fn futures_lite_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }
}
