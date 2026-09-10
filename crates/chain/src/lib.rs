//! The few figures only the chain has.
//!
//! Everything else on the dashboard comes from a nest, which is a fold over history and can be
//! asked "as of when". A contract read cannot: it is true at one block and says nothing about any
//! other. So these values carry no nest provenance and are cached briefly rather than folded, and
//! a failed read is reported rather than replaced with the last known answer.

use std::str::FromStr;
use std::time::Duration;

use alloy::ens::{EnsError, ProviderEnsExt};
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
/// The L1 escrow holding GRT that has been bridged to L2. Counting both sides without subtracting
/// it would double-count every bridged token.
pub const L1_BRIDGE_ESCROW: &str = "0x36aff7001294dae4c2ed4fdefc478a00de77f090";
/// HorizonStaking, the proxy that owns every delegation pool.
pub const HORIZON_STAKING: &str = "0x00669a4cf01450b64e8a2a20e9b1fcb71e61ef03";
/// SubgraphService, which is the verifier a delegation pool is keyed by.
pub const SUBGRAPH_SERVICE: &str = "0xb2bb92d0de618878e438b55d5846cfecd9301105";

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
    interface IHorizonStaking {
        struct DelegationPool {
            uint256 tokens;
            uint256 shares;
            uint256 tokensThawing;
            uint256 sharesThawing;
            uint256 thawingNonce;
        }
        function getDelegationPool(address serviceProvider, address verifier)
            external view returns (DelegationPool memory);
    }

    /// The Dock's bounty board. Read-only here: this service reconciles the cache against the
    /// chain and never posts, claims or refunds, so nothing below needs a key.
    #[sol(rpc)]
    interface IBountyBoard {
        struct Bounty {
            address developer;
            bytes32 deploymentId;
            uint256 amount;
            uint256 postedAt;
            uint256 expiresAt;
            address winner;
            bool settled;
        }
        function getBounty(uint256 bountyId) external view returns (Bounty memory);
    }

    #[sol(rpc)]
    interface IERC20 {
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }

}

/// The oracle's network-wide state. None of this varies by indexer.
///
/// Split out because three of the oracle's five views take no arguments. Reading them per indexer,
/// as the first version of this did, made the refresh job cost 5N calls where N+3 will do - and on
/// a rate-limited endpoint that is the difference between finishing and being throttled.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct OracleContext {
    pub period_seconds: Option<u64>,
    /// Whether the protocol is enforcing the oracle's verdicts at all. False means an ineligible
    /// reading costs the indexer nothing today, which changes what the dashboard should say.
    pub enforced: bool,
    pub updated_at: Option<u64>,
    pub age_seconds: Option<i64>,
    /// The oracle has not run within one eligibility period, so every verdict it is giving is
    /// older than the period those verdicts are meant to cover.
    ///
    /// Measured on 2026-09-06: last run 2026-07-01, sixty-six days earlier, against a fourteen-day
    /// period, with enforcement on.
    pub stale: bool,
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
    pub oracle_age_seconds: Option<i64>,
    pub oracle_stale: bool,
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

/// The name an address is entitled to display, or nothing.
///
/// A reverse record is a claim the address makes about itself; the forward record is the name
/// owner's answer to it. Only when they agree has anybody actually granted the name, so a
/// mismatch is `None` rather than a name we would go on to print next to somebody's stake.
fn ens_verified(name: String, forward: Address, asked: Address) -> Option<String> {
    (forward == asked).then_some(name)
}

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

    /// The oracle's network-wide state. Read once per refresh, not once per indexer.
    pub async fn oracle_context(&self) -> Result<OracleContext, ApiError> {
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            self.arb()?,
        );
        let c_period = oracle.getEligibilityPeriod();
        let c_enforced = oracle.getEligibilityValidation();
        let c_updated = oracle.getLastOracleUpdateTime();
        let (period, enforced, updated) =
            tokio::join!(c_period.call(), c_enforced.call(), c_updated.call());

        let period_seconds = period.ok().map(u256_to_u64);
        let updated_at = updated.ok().map(u256_to_u64).filter(|v| *v > 0);
        let age_seconds = updated_at.map(|v| now_secs() - v as i64);
        let stale = match (age_seconds, period_seconds) {
            (Some(age), Some(p)) if p > 0 => age > p as i64,
            _ => false,
        };

        Ok(OracleContext {
            period_seconds,
            enforced: enforced.unwrap_or(false),
            updated_at,
            age_seconds,
            stale,
        })
    }

    /// Is this one indexer eligible? One call, which is what makes a whole-directory pass affordable.
    pub async fn is_eligible(&self, indexer: &str) -> Result<bool, ApiError> {
        let addr = Address::from_str(indexer)
            .map_err(|_| ApiError::BadRequest(format!("not an address: {indexer}")))?;
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            self.arb()?,
        );
        oracle
            .isEligible(addr)
            .call()
            .await
            .map_err(|e| ApiError::Unavailable(format!("oracle: {e}")))
    }

    /// The full picture for one indexer: its verdict, plus the context needed to read that verdict
    /// correctly. Two round trips rather than five.
    pub async fn eligibility(&self, indexer: &str) -> Result<Eligibility, ApiError> {
        let addr = Address::from_str(indexer)
            .map_err(|_| ApiError::BadRequest(format!("not an address: {indexer}")))?;
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            self.arb()?,
        );

        let c_eligible = oracle.isEligible(addr);
        let c_renewed = oracle.getEligibilityRenewalTime(addr);
        let (eligible, renewed) = tokio::join!(c_eligible.call(), c_renewed.call());

        let eligible = eligible.map_err(|e| ApiError::Unavailable(format!("oracle: {e}")))?;
        let renewed_at = renewed.ok().map(u256_to_u64);
        let ctx = self.oracle_context().await?;

        let expires_in = match (renewed_at, ctx.period_seconds) {
            (Some(r), Some(p)) if r > 0 => Some((r + p) as i64 - now_secs()),
            _ => None,
        };

        Ok(Eligibility {
            eligible,
            renewed_at,
            period_seconds: ctx.period_seconds,
            expires_in,
            enforced: ctx.enforced,
            oracle_updated_at: ctx.updated_at,
            oracle_age_seconds: ctx.age_seconds,
            oracle_stale: ctx.stale,
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

    /// The primary ENS name for an address, or `None` when it has none.
    ///
    /// **Reverse, then forward, and the second half is the point.** `lookup_address` reads the
    /// `<address>.addr.reverse` record, and alloy says plainly that what comes back is neither
    /// normalised nor forward-verified. Anyone may set a reverse record naming anything they like,
    /// so displaying it unchecked would let one address wear another's name on a page of indexer
    /// identities. Resolving the name back and comparing is what makes it a claim its owner made.
    ///
    /// A name that fails that check is `None` rather than an error: the address genuinely has no
    /// name it is entitled to display. A lookup that *fails* is an error, and callers must never
    /// render that as "no name" - the two are different answers and #36 is what happens when they
    /// are conflated.
    ///
    /// Checked against mainnet by hand on 2026-09-10, because the tests here take no network:
    /// `0xd8da…6045` returned `Some("vitalik.eth")` and `0x0000…dead` returned `None`.
    pub async fn ens_name(&self, address: &str) -> Result<Option<String>, ApiError> {
        let address = Address::from_str(address)
            .map_err(|_| ApiError::BadRequest("not a 20-byte address".into()))?;
        let mut last = String::from("no mainnet endpoint configured");
        for url in &self.mainnet {
            let Ok(parsed) = url.parse() else { continue };
            let provider = ProviderBuilder::new().connect_http(parsed);
            let lookup = async {
                let name = provider.lookup_address(&address).await?;
                let forward = provider.resolve_name(&name).await?;
                Ok::<_, EnsError>((name, forward))
            };
            match tokio::time::timeout(Duration::from_secs(10), lookup).await {
                Ok(Ok((name, forward))) => return Ok(ens_verified(name, forward, address)),
                // No reverse record, or none that resolves: the address has no name, and that is
                // an answer rather than a failure to reach for the next endpoint over.
                Ok(Err(EnsError::ResolverNotFound(_) | EnsError::ReverseRegistrarNotFound)) => {
                    return Ok(None);
                }
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "timed out".into(),
            }
        }
        Err(ApiError::Unavailable(format!("ENS lookup failed: {last}")))
    }

    /// Supply across both chains. Three reads; a failure in any of them makes the whole breakdown
    /// absent rather than partial, because a global supply computed from two of three figures is a
    /// plausible wrong number.
    pub async fn grt_supply_breakdown(&self) -> Result<SupplyBreakdown, ApiError> {
        let token_l1 = Address::from_str(GRT_MAINNET).expect("constant");
        let escrow = Address::from_str(L1_BRIDGE_ESCROW).expect("constant");

        let l2 = self.grt_total_supply(SupplyChain::Arbitrum);
        let l1_and_escrow = async {
            let mut last = String::from("none configured");
            for url in &self.mainnet {
                let Ok(parsed) = url.parse() else { continue };
                let provider = ProviderBuilder::new().connect_http(parsed);
                let erc20 = IERC20::new(token_l1, provider);
                let c_supply = erc20.totalSupply();
                let c_balance = erc20.balanceOf(escrow);
                let both = async { tokio::join!(c_supply.call(), c_balance.call()) };
                match tokio::time::timeout(Duration::from_secs(12), both).await {
                    Ok((Ok(supply), Ok(balance))) => return Ok((supply, balance)),
                    Ok((a, b)) => {
                        last = a
                            .err()
                            .map(|e| e.to_string())
                            .or_else(|| b.err().map(|e| e.to_string()))
                            .unwrap_or(last);
                    }
                    Err(_) => last = "timed out".into(),
                }
            }
            Err(ApiError::Unavailable(format!(
                "no mainnet endpoint answered: {last}"
            )))
        };

        let (l2, l1_pair) = tokio::join!(l2, l1_and_escrow);
        let (l1_supply, escrow_balance) = l1_pair?;
        let to_grt = |v: U256| -> f64 {
            // Through a string rather than a cast: the supply exceeds what u128 holds comfortably
            // and the figure is only ever displayed, so a parse to f64 at the end is honest.
            v.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
        };

        let l1_total_supply = to_grt(l1_supply);
        let l2_total_supply = l2?.parse::<f64>().unwrap_or(0.0) / 1e18;
        let bridge_escrow = to_grt(escrow_balance);

        Ok(SupplyBreakdown {
            l1_total_supply,
            l2_total_supply,
            bridge_escrow,
            global_supply: l1_total_supply + l2_total_supply - bridge_escrow,
        })
    }

    /// The current Arbitrum block, used by the health endpoint to say whether the RPC is answering.
    /// The live delegation pool for an indexer's SubgraphService provision.
    ///
    /// `tokens - tokensThawing` is the active, reward-earning capital, and the subtraction is exact
    /// rather than approximate: the contract keeps fully-thawed-but-not-yet-withdrawn tokens inside
    /// **both** figures until `withdraw()` fires, because there is no time-triggered transition at
    /// `thawingUntil`. So the difference isolates exactly what earns, which is the on-chain analogue
    /// of the nest's `delegated_tokens - delegated_thawing_tokens`, and the two should agree
    /// block for block.
    ///
    /// `None` when the call reverts, which is what an indexer with no provision looks like.
    pub async fn delegation_pool(&self, indexer: &str) -> Option<(f64, f64)> {
        let provider =
            alloy::providers::ProviderBuilder::new().connect_http(self.arbitrum.parse().ok()?);
        let staking: alloy::primitives::Address = HORIZON_STAKING.parse().ok()?;
        let verifier: alloy::primitives::Address = SUBGRAPH_SERVICE.parse().ok()?;
        let who: alloy::primitives::Address = indexer.parse().ok()?;
        let c = IHorizonStaking::new(staking, provider);
        // Bounded, because this sits in a request path. An RPC endpoint that stops answering must
        // cost the reader a missing cross-check, not a hung page: the trail beside it is still
        // worth showing, and `None` is how this route says "the chain did not answer" as distinct
        // from "the chain says zero".
        let pool = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            c.getDelegationPool(who, verifier).call(),
        )
        .await
        .ok()?
        .ok()?;
        let wei = |v: alloy::primitives::U256| -> f64 {
            v.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
        };
        Some((wei(pool.tokens), wei(pool.tokensThawing)))
    }

    /// One bounty's on-chain state, from the board at `board`.
    ///
    /// The board's address is configuration rather than a constant: it is the Dock's own contract
    /// and it is deployed per environment, unlike the protocol addresses above.
    ///
    /// The three fields returned are the whole of what decides a cached bounty's status. The rest
    /// of the struct - developer, amount, deployment - is written once when the bounty is posted
    /// and reconciliation has no business rewriting it.
    pub async fn bounty(&self, board: &str, id: u64) -> Result<ChainBounty, ApiError> {
        let addr = Address::from_str(board.trim())
            .map_err(|_| ApiError::Internal(format!("bad bounty board address: {board}")))?;
        let c = IBountyBoard::new(addr, self.arb()?);
        let b = tokio::time::timeout(Duration::from_secs(8), c.getBounty(U256::from(id)).call())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("getBounty({id}): {e}")))?;
        Ok(ChainBounty {
            settled: b.settled,
            winner: format!("{:#x}", b.winner),
            expires_at: b.expiresAt.to::<u64>(),
        })
    }

    pub async fn block_number(&self) -> Result<u64, ApiError> {
        let provider = self.arb()?;
        tokio::time::timeout(Duration::from_secs(8), provider.get_block_number())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("arbitrum rpc: {e}")))
    }
}

/// The part of a bounty's on-chain state that decides its cached status.
///
/// `expires_at` is zero when the bounty never expires, which the contract uses as its sentinel and
/// which is why this is a `u64` rather than an `Option`: zero is the contract's own spelling of
/// "no expiry" and translating it here would put the same fact in two shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBounty {
    pub settled: bool,
    /// Lowercase `0x…`. The zero address means no winner.
    pub winner: String,
    /// Unix seconds, or 0 for no expiry.
    pub expires_at: u64,
}

/// GRT in circulation across both chains, in whole GRT.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyBreakdown {
    pub l1_total_supply: f64,
    pub l2_total_supply: f64,
    pub bridge_escrow: f64,
    /// L1 plus L2 minus what sits in escrow: the tokens on L2 are the same tokens the escrow
    /// holds, so adding the two supplies without subtracting it counts them twice.
    pub global_supply: f64,
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
    /// A reverse record is a claim; the forward record is the name owner answering it. Printing an
    /// unverified reverse name next to somebody's stake is how one address wears another's name.
    #[test]
    fn a_name_that_does_not_resolve_back_is_not_a_name() {
        let asked = Address::from_str("0xd8da6bf26964af9d7eed9e03e53415d37aa96045").unwrap();
        let someone_else = Address::from_str("0x090f7382f9ea85c733cd501f4d87f16cb5b83ed3").unwrap();

        assert_eq!(
            super::ens_verified("vitalik.eth".into(), asked, asked),
            Some("vitalik.eth".to_string()),
            "the name owner agrees, so it may be displayed"
        );
        assert_eq!(
            super::ens_verified("vitalik.eth".into(), someone_else, asked),
            None,
            "the reverse record claims a name that resolves elsewhere"
        );
        assert_eq!(
            super::ens_verified("vitalik.eth".into(), Address::ZERO, asked),
            None,
            "and an unresolvable name is not one either"
        );
    }
    use super::*;

    /// Bridged tokens exist on both sides of the bridge; counting them twice would overstate the
    /// supply by the whole escrow balance.
    #[test]
    fn the_global_supply_does_not_count_bridged_tokens_twice() {
        let (l1, l2, escrow) = (10_800_262_816.0, 3_380_666_513.0, 2_694_465_409.0);
        assert_eq!(l1 + l2 - escrow, 11_486_463_920.0);
    }

    #[test]
    fn the_contract_addresses_parse() {
        for a in [REO_ADDRESS, GRT_ARBITRUM, GRT_MAINNET, L1_BRIDGE_ESCROW] {
            assert!(Address::from_str(a).is_ok(), "{a}");
        }
    }

    #[test]
    fn a_bad_address_is_a_bad_request_not_a_panic() {
        let chain = Chain::new("http://127.0.0.1:1", None);
        let err = futures_lite_block(chain.eligibility("nope")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    /// A verdict older than the period it covers is stale, and the flag must say so rather than
    /// leaving the reader to do the subtraction.
    #[test]
    fn an_oracle_that_has_not_run_within_a_period_is_stale() {
        let period = 14 * 86_400i64;
        let is_stale = |age: i64| age > period;
        assert!(is_stale(66 * 86_400), "the state observed on 2026-09-06");
        assert!(!is_stale(13 * 86_400));
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
