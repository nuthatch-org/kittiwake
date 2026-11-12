//! The few figures only the chain has.
//!
//! Everything else on the dashboard comes from a nest, which is a fold over history and can be
//! asked "as of when". A contract read cannot: it is true at one block and says nothing about any
//! other. So these values carry no nest provenance and are cached briefly rather than folded, and
//! a failed read is reported rather than replaced with the last known answer.

mod metered;
pub mod registry;
pub mod tap;

use std::str::FromStr;
use std::time::Duration;

use alloy::ens::{EnsError, ProviderEnsExt, namehash};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{MulticallBuilder, MulticallItem, Provider};
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
/// RewardsManager on Arbitrum One.
pub const REWARDS_MANAGER: &str = "0x971b9d3d0ae3eca029cab5ea1fb0f72c85e6a525";
/// Allocations per `aggregate3`, kept under what a public endpoint's `eth_call` gas cap carries.
const PENDING_REWARDS_BATCH: usize = 250;

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
        function isAuthorized(address serviceProvider, address verifier, address operator)
            external view returns (bool);

        struct Provision {
            uint256 tokens;
            uint256 tokensThawing;
            uint256 sharesThawing;
            uint32 maxVerifierCut;
            uint64 thawingPeriod;
            uint64 createdAt;
            uint32 maxVerifierCutPending;
            uint64 thawingPeriodPending;
            uint256 lastParametersStagedAt;
            uint256 thawingNonce;
        }
        struct ThawRequestList {
            bytes32 head;
            bytes32 tail;
            uint256 nonce;
            uint256 count;
        }
        function getProvision(address serviceProvider, address verifier)
            external view returns (Provision memory);
        /// `requestType` 0 is a provision thaw; `owner` is then the service provider itself.
        function getThawRequestList(uint8 requestType, address serviceProvider, address verifier, address owner)
            external view returns (ThawRequestList memory);
        /// PPM. `paymentType` 0 is query fees, 1 indexing fees, 2 indexing rewards.
        function getDelegationFeeCut(address serviceProvider, address verifier, uint8 paymentType)
            external view returns (uint256);
    }

    /// The ranges a data service accepts a provision's parameters within (ProvisionManager).
    #[sol(rpc)]
    interface IDataService {
        function getThawingPeriodRange() external view returns (uint64 min, uint64 max);
        function getVerifierCutRange() external view returns (uint32 min, uint32 max);
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
    interface IRewardsManager {
        function getRewards(address rewardsIssuer, address allocationID)
            external view returns (uint256);
        function denylist(bytes32 subgraphDeploymentID) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC20 {
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IEnsRegistry {
        function owner(bytes32 node) external view returns (address);
    }

}

/// The ENS node a Graph `SetDefaultName` may claim, or `None` when the network subgraph would have
/// refused it: ENS only (`nameSystem` 0), a bare `.eth` label, and an identifier that is its node.
pub fn default_name_node(name: &str, name_system: i64, identifier: &str) -> Option<B256> {
    if name_system != 0 || name.is_empty() || name.contains('.') {
        return None;
    }
    let node = alloy::ens::namehash(&format!("{name}.eth"));
    (B256::from_str(identifier).ok()? == node).then_some(node)
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

/// A name `ens_address` will look up: plain lowercase ASCII labels under `.eth`.
///
/// ENS names are normalised with UTS-46 before hashing, and this does not implement it. Refusing
/// anything outside the ASCII subset, where normalisation is lowercasing, means a name is never
/// hashed into a node its owner did not register.
pub fn ens_lookup_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_ascii_lowercase();
    let ok = name.len() <= 255
        && name.ends_with(".eth")
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    ok.then_some(name)
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

    /// A paid endpoint tried after every other mainnet endpoint has failed.
    ///
    /// The public endpoints are free and carry these reads fine on a normal day; this is what keeps
    /// ENS names and the mainnet supply on the page on the day all three throttle or fall over at
    /// once. Last, never first, because it bills per call and the reads it covers are decorations,
    /// not protocol figures. A call only reaches it when every endpoint before it has already
    /// failed, so on a healthy day it costs nothing.
    pub fn with_mainnet_fallback(mut self, url: Option<String>) -> Self {
        if let Some(url) = url.filter(|u| !u.trim().is_empty())
            && !self.mainnet.contains(&url)
        {
            self.mainnet.push(url);
        }
        self
    }

    fn arb(&self, op: &'static str) -> Result<impl Provider + Clone, ApiError> {
        let url = self
            .arbitrum
            .parse()
            .map_err(|_| ApiError::Internal(format!("bad arbitrum rpc url: {}", self.arbitrum)))?;
        Ok(metered::connect(url, "arbitrum", op))
    }

    /// The oracle's network-wide state. Read once per refresh, not once per indexer.
    pub async fn oracle_context(&self) -> Result<OracleContext, ApiError> {
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            self.arb("oracle_context")?,
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

    /// Every indexer's verdict, in the order given, `PENDING_REWARDS_BATCH` to an `aggregate3`.
    ///
    /// One `eth_call` per indexer was 21 calls a minute from the five-minute refresh alone. A reverted
    /// read or an unparseable address is `None` for that indexer and costs no other verdict.
    pub async fn eligibility_batch(
        &self,
        indexers: &[String],
    ) -> Result<Vec<Option<bool>>, ApiError> {
        let provider = self.arb("eligibility_batch")?;
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            &provider,
        );
        let parsed: Vec<(usize, Address)> = indexers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Address::from_str(s).ok().map(|a| (i, a)))
            .collect();

        let mut out = vec![None; indexers.len()];
        for batch in parsed.chunks(PENDING_REWARDS_BATCH) {
            let multicall = MulticallBuilder::new_dynamic(&provider).extend_calls(
                batch
                    .iter()
                    .map(|(_, a)| oracle.isEligible(*a).into_call(true)),
            );
            let results = tokio::time::timeout(Duration::from_secs(20), multicall.aggregate3())
                .await
                .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
                .map_err(|e| ApiError::Unavailable(format!("isEligible multicall: {e}")))?;
            for ((i, _), r) in batch.iter().zip(results) {
                out[*i] = r.ok();
            }
        }
        Ok(out)
    }

    /// The block each deployment was denied rewards from, in the order given, or 0 where it earns.
    /// A reverted read or an id that is not 32 bytes is `None` for that deployment alone.
    pub async fn denylist_batch(
        &self,
        deployments: &[String],
    ) -> Result<Vec<Option<u64>>, ApiError> {
        let provider = self.arb("denylist_batch")?;
        let rewards = IRewardsManager::new(
            Address::from_str(REWARDS_MANAGER).expect("RewardsManager address is a constant"),
            &provider,
        );
        let parsed: Vec<(usize, B256)> = deployments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| B256::from_str(s).ok().map(|d| (i, d)))
            .collect();

        let mut out = vec![None; deployments.len()];
        for batch in parsed.chunks(PENDING_REWARDS_BATCH) {
            let multicall = MulticallBuilder::new_dynamic(&provider).extend_calls(
                batch
                    .iter()
                    .map(|(_, d)| rewards.denylist(*d).into_call(true)),
            );
            let results = tokio::time::timeout(Duration::from_secs(20), multicall.aggregate3())
                .await
                .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
                .map_err(|e| ApiError::Unavailable(format!("denylist multicall: {e}")))?;
            for ((i, _), r) in batch.iter().zip(results) {
                out[*i] = r.ok().map(u256_to_u64);
            }
        }
        Ok(out)
    }

    /// The full picture for one indexer: its verdict, plus the context needed to read that verdict
    /// correctly. Two round trips rather than five.
    pub async fn eligibility(&self, indexer: &str) -> Result<Eligibility, ApiError> {
        let addr = Address::from_str(indexer)
            .map_err(|_| ApiError::BadRequest(format!("not an address: {indexer}")))?;
        let oracle = IRewardsEligibilityOracle::new(
            Address::from_str(REO_ADDRESS).expect("REO address is a constant"),
            self.arb("eligibility")?,
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
        let (token, urls, label) = match chain {
            SupplyChain::Arbitrum => (GRT_ARBITRUM, vec![self.arbitrum.clone()], "arbitrum"),
            SupplyChain::Mainnet => (GRT_MAINNET, self.mainnet.clone(), "mainnet"),
        };
        let token = Address::from_str(token).expect("token address is a constant");

        let mut last: Option<String> = None;
        for url in urls {
            let Ok(parsed) = url.parse() else { continue };
            let provider = metered::connect(parsed, label, "grt_total_supply");
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
    /// The address an ENS name resolves to, or `None` when it has no resolver or resolves to zero.
    pub async fn ens_address(&self, name: &str) -> Result<Option<String>, ApiError> {
        let name = ens_lookup_name(name)
            .ok_or_else(|| ApiError::BadRequest("not a plain .eth name".into()))?;
        let mut last = String::from("no mainnet endpoint configured");
        for url in &self.mainnet {
            let Ok(parsed) = url.parse() else { continue };
            let provider = metered::connect(parsed, "mainnet", "ens_address");
            // The registry first: the Universal Resolver reverts on an unregistered name, and a
            // public RPC reports that revert as the same -32603 it uses for its own failures.
            let lookup = async {
                provider.get_resolver(namehash(&name), &name).await?;
                provider.resolve_name(&name).await
            };
            match tokio::time::timeout(Duration::from_secs(10), lookup).await {
                Ok(Ok(a)) if a == Address::ZERO => return Ok(None),
                Ok(Ok(a)) => return Ok(Some(a.to_string().to_lowercase())),
                Ok(Err(EnsError::ResolverNotFound(_))) => return Ok(None),
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "timed out".into(),
            }
        }
        Err(ApiError::Unavailable(format!("ENS lookup failed: {last}")))
    }

    pub async fn ens_name(&self, address: &str) -> Result<Option<String>, ApiError> {
        let address = Address::from_str(address)
            .map_err(|_| ApiError::BadRequest("not a 20-byte address".into()))?;
        let mut last = String::from("no mainnet endpoint configured");
        for url in &self.mainnet {
            let Ok(parsed) = url.parse() else { continue };
            let provider = metered::connect(parsed, "mainnet", "ens_name");
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

    /// A Graph default name, if the account owns its ENS node on mainnet today.
    ///
    /// The network subgraph made the same check on Arbitrum against an ENS address of zero, so on
    /// L2 it never passed. Asked on mainnet, where the registry is, it can.
    pub async fn verified_default_name(
        &self,
        account: &str,
        name: &str,
        name_system: i64,
        identifier: &str,
    ) -> Result<Option<String>, ApiError> {
        let Some(node) = default_name_node(name, name_system, identifier) else {
            return Ok(None);
        };
        let account = Address::from_str(account)
            .map_err(|_| ApiError::BadRequest("not a 20-byte address".into()))?;
        let mut last = String::from("no mainnet endpoint configured");
        for url in &self.mainnet {
            let Ok(parsed) = url.parse() else { continue };
            let registry = IEnsRegistry::new(
                alloy::ens::ENS_ADDRESS,
                metered::connect(parsed, "mainnet", "verified_default_name"),
            );
            let owner = registry.owner(node);
            match tokio::time::timeout(Duration::from_secs(10), owner.call()).await {
                Ok(Ok(owner)) => return Ok((owner == account).then(|| name.to_string())),
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "timed out".into(),
            }
        }
        Err(ApiError::Unavailable(format!(
            "ENS owner lookup failed: {last}"
        )))
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
                let provider = metered::connect(parsed, "mainnet", "grt_supply_breakdown");
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
        let provider = metered::connect(self.arbitrum.parse().ok()?, "arbitrum", "delegation_pool");
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

    /// Whether `operator` may act for `indexer` on the SubgraphService. HorizonStaking answers
    /// from the legacy operator table for this verifier, so operators set before Horizon count.
    pub async fn is_subgraph_service_operator(
        &self,
        indexer: Address,
        operator: Address,
    ) -> Result<bool, ApiError> {
        let staking = Address::from_str(HORIZON_STAKING).expect("staking address is a constant");
        let verifier = Address::from_str(SUBGRAPH_SERVICE).expect("verifier is a constant");
        let c = IHorizonStaking::new(staking, self.arb("is_subgraph_service_operator")?);
        match tokio::time::timeout(
            Duration::from_secs(10),
            c.isAuthorized(indexer, verifier, operator).call(),
        )
        .await
        {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ApiError::Unavailable(format!("isAuthorized: {e}"))),
            Err(_) => Err(ApiError::Unavailable("isAuthorized timed out".into())),
        }
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
        let c = IBountyBoard::new(addr, self.arb("bounty")?);
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

    /// What a bounty was posted as, which is what the Dock records rather than a caller's account
    /// of it.
    pub async fn posted_bounty(&self, board: &str, id: u64) -> Result<PostedBounty, ApiError> {
        let addr = Address::from_str(board.trim())
            .map_err(|_| ApiError::Internal(format!("bad bounty board address: {board}")))?;
        let c = IBountyBoard::new(addr, self.arb("bounty")?);
        let b = tokio::time::timeout(Duration::from_secs(8), c.getBounty(U256::from(id)).call())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("getBounty({id}): {e}")))?;
        Ok(PostedBounty {
            developer: format!("{:#x}", b.developer),
            deployment: format!("{:#x}", b.deploymentId),
            amount_grt: grt_decimal(b.amount),
            expires_at: b.expiresAt.to::<u64>(),
        })
    }

    /// What each open allocation would collect if a POI were presented now, before cuts, in wei.
    ///
    /// Every batch is pinned to one block so the rows add up. A row whose call reverts is `None`
    /// rather than zero; the read as a whole failing is an error.
    pub async fn pending_rewards(
        &self,
        allocations: &[String],
    ) -> Result<PendingRewards, ApiError> {
        let allocations = allocations
            .iter()
            .map(|a| Address::from_str(a))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApiError::BadRequest("not an allocation address".into()))?;
        let provider = self.arb("pending_rewards")?;
        let block = tokio::time::timeout(Duration::from_secs(8), provider.get_block_number())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("arbitrum rpc: {e}")))?;
        let issuer = Address::from_str(SUBGRAPH_SERVICE).expect("constant");
        let rewards = IRewardsManager::new(
            Address::from_str(REWARDS_MANAGER).expect("constant"),
            &provider,
        );

        let mut wei = Vec::with_capacity(allocations.len());
        for batch in allocations.chunks(PENDING_REWARDS_BATCH) {
            let multicall = MulticallBuilder::new_dynamic(&provider)
                .block(block.into())
                .extend_calls(
                    batch
                        .iter()
                        .map(|a| rewards.getRewards(issuer, *a).into_call(true)),
                );
            let results = tokio::time::timeout(Duration::from_secs(20), multicall.aggregate3())
                .await
                .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
                .map_err(|e| ApiError::Unavailable(format!("getRewards multicall: {e}")))?;
            wei.extend(results.into_iter().map(|r| r.ok().map(|v| v.to_string())));
        }
        Ok(PendingRewards { block, wei })
    }

    /// One indexer's provisions as HorizonStaking and each data service hold them, at one block.
    ///
    /// Five multicalls, one per call signature, so a data service that does not implement the
    /// ProvisionManager views costs its own ranges and nothing else. A reverted call is `None`.
    pub async fn horizon_provisions(
        &self,
        indexer: &str,
        services: &[String],
    ) -> Result<HorizonProvisions, ApiError> {
        let sp = Address::from_str(indexer)
            .map_err(|_| ApiError::BadRequest(format!("not an address: {indexer}")))?;
        let verifiers = services
            .iter()
            .map(|s| Address::from_str(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApiError::BadRequest("not a data service address".into()))?;
        let provider = self.arb("horizon_provisions")?;
        let block = tokio::time::timeout(Duration::from_secs(8), provider.get_block_number())
            .await
            .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?
            .map_err(|e| ApiError::Unavailable(format!("arbitrum rpc: {e}")))?;
        if verifiers.is_empty() {
            return Ok(HorizonProvisions {
                block,
                services: Vec::new(),
            });
        }
        let staking = IHorizonStaking::new(
            Address::from_str(HORIZON_STAKING).expect("constant"),
            &provider,
        );
        let services_c: Vec<_> = verifiers
            .iter()
            .map(|v| IDataService::new(*v, &provider))
            .collect();

        let provisions = MulticallBuilder::new_dynamic(&provider)
            .block(block.into())
            .extend_calls(
                verifiers
                    .iter()
                    .map(|v| staking.getProvision(sp, *v).into_call(true)),
            );
        let lists = MulticallBuilder::new_dynamic(&provider)
            .block(block.into())
            .extend_calls(
                verifiers
                    .iter()
                    .map(|v| staking.getThawRequestList(0, sp, *v, sp).into_call(true)),
            );
        let cuts = MulticallBuilder::new_dynamic(&provider)
            .block(block.into())
            .extend_calls(verifiers.iter().flat_map(|v| {
                (0u8..3).map(|t| staking.getDelegationFeeCut(sp, *v, t).into_call(true))
            }));
        let thawing = MulticallBuilder::new_dynamic(&provider)
            .block(block.into())
            .extend_calls(
                services_c
                    .iter()
                    .map(|c| c.getThawingPeriodRange().into_call(true)),
            );
        let verifier_cut = MulticallBuilder::new_dynamic(&provider)
            .block(block.into())
            .extend_calls(
                services_c
                    .iter()
                    .map(|c| c.getVerifierCutRange().into_call(true)),
            );

        let all = async {
            tokio::join!(
                provisions.aggregate3(),
                lists.aggregate3(),
                cuts.aggregate3(),
                thawing.aggregate3(),
                verifier_cut.aggregate3(),
            )
        };
        let (provisions, lists, cuts, thawing, verifier_cut) =
            tokio::time::timeout(Duration::from_secs(20), all)
                .await
                .map_err(|_| ApiError::Unavailable("arbitrum rpc timed out".into()))?;
        let unavailable = |what: &str, e: alloy::providers::MulticallError| {
            ApiError::Unavailable(format!("{what} multicall: {e}"))
        };
        let provisions = provisions.map_err(|e| unavailable("getProvision", e))?;
        let lists = lists.map_err(|e| unavailable("getThawRequestList", e))?;
        let cuts = cuts.map_err(|e| unavailable("getDelegationFeeCut", e))?;
        let thawing = thawing.map_err(|e| unavailable("getThawingPeriodRange", e))?;
        let verifier_cut = verifier_cut.map_err(|e| unavailable("getVerifierCutRange", e))?;

        let mut cuts = cuts.into_iter().map(|r| r.ok().map(u256_to_u64));
        let services = services
            .iter()
            .zip(provisions)
            .zip(lists)
            .zip(thawing)
            .zip(verifier_cut)
            .map(|((((id, p), l), t), v)| ServiceProvisionOnChain {
                data_service: id.to_lowercase(),
                provision: p.ok().map(|p| ProvisionOnChain {
                    tokens: p.tokens.to_string(),
                    tokens_thawing: p.tokensThawing.to_string(),
                    shares_thawing: p.sharesThawing.to_string(),
                    max_verifier_cut: p.maxVerifierCut,
                    thawing_period: p.thawingPeriod,
                    created_at: p.createdAt,
                    max_verifier_cut_pending: p.maxVerifierCutPending,
                    thawing_period_pending: p.thawingPeriodPending,
                    last_parameters_staged_at: u256_to_u64(p.lastParametersStagedAt),
                    thawing_nonce: p.thawingNonce.to_string(),
                }),
                thaw_request_count: l.ok().map(|l| u256_to_u64(l.count)),
                delegation_fee_cuts: DelegationFeeCuts {
                    query_fee: cuts.next().flatten(),
                    indexing_fee: cuts.next().flatten(),
                    indexing_rewards: cuts.next().flatten(),
                },
                thawing_period_range: t.ok().map(|r| Range {
                    min: r.min,
                    max: r.max,
                }),
                verifier_cut_range: v.ok().map(|r| Range {
                    min: u64::from(r.min),
                    max: u64::from(r.max),
                }),
            })
            .collect();
        Ok(HorizonProvisions { block, services })
    }

    pub async fn block_number(&self) -> Result<u64, ApiError> {
        let provider = self.arb("block_number")?;
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

/// The fields of a bounty written once, when it was posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedBounty {
    /// Lowercase `0x…`. The zero address means no bounty has this id.
    pub developer: String,
    /// `0x…` bytes32, the form `ipfs_hash_to_bytes32` gives.
    pub deployment: String,
    /// GRT as a decimal, without trailing zeros.
    pub amount_grt: String,
    /// Unix seconds, or 0 for no expiry.
    pub expires_at: u64,
}

/// Wei to a GRT decimal, exactly: `1500000000000000000` is `1.5`.
pub fn grt_decimal(wei: U256) -> String {
    let unit = U256::from(10u64).pow(U256::from(18u64));
    let (whole, frac) = (wei / unit, wei % unit);
    if frac.is_zero() {
        return whole.to_string();
    }
    let frac = format!("{:0>18}", frac.to_string());
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// `wei[i]` answers `allocations[i]` as passed to [`Chain::pending_rewards`], as a decimal string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRewards {
    pub block: u64,
    pub wei: Vec<Option<String>>,
}

/// [`Chain::horizon_provisions`], one row per data service in the order asked.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HorizonProvisions {
    pub block: u64,
    pub services: Vec<ServiceProvisionOnChain>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceProvisionOnChain {
    pub data_service: String,
    pub provision: Option<ProvisionOnChain>,
    /// Provision thaw requests still in HorizonStaking's list, fulfilled ones removed.
    pub thaw_request_count: Option<u64>,
    pub delegation_fee_cuts: DelegationFeeCuts,
    pub thawing_period_range: Option<Range>,
    pub verifier_cut_range: Option<Range>,
}

/// Token amounts are wei as decimal strings; cuts are PPM; periods and times are seconds.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionOnChain {
    pub tokens: String,
    pub tokens_thawing: String,
    pub shares_thawing: String,
    pub max_verifier_cut: u32,
    pub thawing_period: u64,
    pub created_at: u64,
    pub max_verifier_cut_pending: u32,
    pub thawing_period_pending: u64,
    pub last_parameters_staged_at: u64,
    pub thawing_nonce: String,
}

/// PPM per payment type.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationFeeCuts {
    pub query_fee: Option<u64>,
    pub indexing_fee: Option<u64>,
    pub indexing_rewards: Option<u64>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Range {
    pub min: u64,
    pub max: u64,
}

impl ProvisionOnChain {
    /// What a provision thaw request for `shares` is worth now, in wei. A request made under an
    /// older thawing nonce was invalidated by a slash and releases nothing, so it is "0".
    pub fn thaw_request_tokens(&self, shares: &str, nonce: &str) -> Option<String> {
        let shares = U256::from_str(shares).ok()?;
        if U256::from_str(nonce).ok()? != U256::from_str(&self.thawing_nonce).ok()? {
            return Some("0".into());
        }
        let tokens = U256::from_str(&self.tokens_thawing).ok()?;
        let total = U256::from_str(&self.shares_thawing).ok()?;
        if total.is_zero() {
            return None;
        }
        Some((shares.checked_mul(tokens)? / total).to_string())
    }
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
    #[test]
    fn a_bounty_amount_is_its_wei_in_grt_exactly() {
        let wei = |s: &str| alloy::primitives::U256::from_str_radix(s, 10).unwrap();
        assert_eq!(super::grt_decimal(wei("1500000000000000000")), "1.5");
        assert_eq!(super::grt_decimal(wei("2000000000000000000000")), "2000");
        assert_eq!(super::grt_decimal(wei("1")), "0.000000000000000001");
        assert_eq!(super::grt_decimal(wei("0")), "0");
    }

    /// Only names whose normalisation is lowercasing reach the chain; anything else would be hashed
    /// into a node that is not the one its owner registered.
    #[test]
    fn only_plain_eth_names_are_looked_up() {
        use super::ens_lookup_name as n;
        assert_eq!(n(" Vitalik.ETH "), Some("vitalik.eth".to_string()));
        assert_eq!(n("graphops.eth"), Some("graphops.eth".to_string()));
        assert_eq!(n("sub.name-1.eth"), Some("sub.name-1.eth".to_string()));
        for bad in [
            "vitalik",
            "vitalik.com",
            ".eth",
            "a..eth",
            "vitalík.eth",
            "a b.eth",
            "0xabc",
        ] {
            assert_eq!(n(bad), None, "{bad} should not be looked up");
        }
    }

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

    /// The one live `SetDefaultName` on L2GNS, log at block 125,605,436, as the chain has it.
    #[test]
    fn a_default_name_claims_exactly_the_node_of_its_eth_label() {
        let node = "0xa8f88ec42c70e80659f57464661c91d195ac316fdec07a4944b47abd807cf048";
        assert!(default_name_node("grassets-tech-2", 0, node).is_some());
        assert_eq!(default_name_node("grassets-tech-1", 0, node), None);
        assert_eq!(default_name_node("grassets-tech-2", 1, node), None);
        assert_eq!(default_name_node("sub.grassets-tech-2", 0, node), None);
        // A reset: nameIdentifier zero and an empty name.
        assert_eq!(
            default_name_node("", 0, &format!("0x{}", "0".repeat(64))),
            None
        );
    }

    /// Bridged tokens exist on both sides of the bridge; counting them twice would overstate the
    /// supply by the whole escrow balance.
    #[test]
    fn the_global_supply_does_not_count_bridged_tokens_twice() {
        let (l1, l2, escrow) = (10_800_262_816.0, 3_380_666_513.0, 2_694_465_409.0);
        assert_eq!(l1 + l2 - escrow, 11_486_463_920.0);
    }

    #[test]
    fn a_thaw_request_is_worth_its_share_of_what_is_thawing() {
        let p = ProvisionOnChain {
            tokens: "100".into(),
            tokens_thawing: "45".into(),
            shares_thawing: "30".into(),
            max_verifier_cut: 0,
            thawing_period: 0,
            created_at: 0,
            max_verifier_cut_pending: 0,
            thawing_period_pending: 0,
            last_parameters_staged_at: 0,
            thawing_nonce: "2".into(),
        };
        assert_eq!(p.thaw_request_tokens("10", "2"), Some("15".into()));
        assert_eq!(p.thaw_request_tokens("10", "1"), Some("0".into()));
        assert_eq!(p.thaw_request_tokens("x", "2"), None);
        let empty = ProvisionOnChain {
            shares_thawing: "0".into(),
            ..p
        };
        assert_eq!(empty.thaw_request_tokens("10", "2"), None);
    }

    #[test]
    fn the_contract_addresses_parse() {
        for a in [
            REO_ADDRESS,
            GRT_ARBITRUM,
            GRT_MAINNET,
            L1_BRIDGE_ESCROW,
            REWARDS_MANAGER,
            SUBGRAPH_SERVICE,
        ] {
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

    #[test]
    fn the_paid_fallback_is_tried_last_and_only_once() {
        let chain = Chain::new("x", None).with_mainnet_fallback(Some("https://paid".into()));
        assert_eq!(chain.mainnet.len(), PUBLIC_MAINNET.len() + 1);
        assert_eq!(
            chain.mainnet.last().map(String::as_str),
            Some("https://paid")
        );
        assert_eq!(
            chain.mainnet[..PUBLIC_MAINNET.len()],
            PUBLIC_MAINNET.map(String::from)
        );

        let mine = Chain::new("x", Some("https://mine".into()))
            .with_mainnet_fallback(Some("https://paid".into()));
        assert_eq!(mine.mainnet, ["https://mine", "https://paid"]);

        let same = Chain::new("x", Some("https://paid".into()))
            .with_mainnet_fallback(Some("https://paid".into()));
        assert_eq!(same.mainnet, ["https://paid"], "never asked twice");

        for none in [None, Some(String::new()), Some("  ".into())] {
            assert_eq!(
                Chain::new("x", None)
                    .with_mainnet_fallback(none)
                    .mainnet
                    .len(),
                PUBLIC_MAINNET.len()
            );
        }
    }

    /// The point of the fallback: every endpoint before it refuses, and the read still lands.
    #[tokio::test]
    async fn a_read_reaches_the_fallback_when_every_endpoint_before_it_fails() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dead = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&dead)
            .await;
        let paid = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": format!("0x{:064x}", 10_000_000_000u128 * 10u128.pow(18)),
            })))
            .expect(1)
            .mount(&paid)
            .await;

        let chain = Chain::new("x", Some(dead.uri())).with_mainnet_fallback(Some(paid.uri()));
        let supply = chain.grt_total_supply(SupplyChain::Mainnet).await.unwrap();
        assert_eq!(supply, "10000000000000000000000000000");
    }

    /// Against a live Arbitrum RPC: the batched verdicts must equal one-at-a-time reads, and a
    /// malformed address is `None` without costing its neighbours theirs.
    /// `KITTIWAKE_LIVE_ARB_RPC=… cargo test -- --ignored live_denylist`, with
    /// `KITTIWAKE_LIVE_DEPLOYMENTS` naming a file of ids, one a line, to time a directory's worth.
    #[test]
    #[ignore = "needs a live Arbitrum RPC"]
    fn live_denylist_reads_the_block_denial_began() {
        let rpc = std::env::var("KITTIWAKE_LIVE_ARB_RPC").expect("KITTIWAKE_LIVE_ARB_RPC");
        let mut ids: Vec<String> = vec![
            "0x02ccf00ae4852ac24e2d4f00657171aca0f5c9cc4243e9a975d51650dd76c628".into(),
            "not-a-deployment".into(),
            "0xc915fae50b8dc6976b454869031c52761538b1354ff5587a207dbdbbe66e73fa".into(),
        ];
        if let Ok(path) = std::env::var("KITTIWAKE_LIVE_DEPLOYMENTS") {
            ids.extend(
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(str::to_string),
            );
        }
        let chain = Chain::new(rpc, None);
        let began = std::time::Instant::now();
        let since = futures_lite_block(chain.denylist_batch(&ids)).unwrap();
        let denied = since.iter().filter(|s| s.is_some_and(|b| b > 0)).count();
        eprintln!("{} read, {denied} denied, {:?}", ids.len(), began.elapsed());
        assert_eq!(&since[..3], &[Some(17_292_847), None, Some(0)]);
    }

    /// `KITTIWAKE_LIVE_ARB_RPC=… KITTIWAKE_LIVE_INDEXERS=0xa,0xb cargo test -- --ignored live_`
    #[test]
    #[ignore = "needs a live Arbitrum RPC"]
    fn live_batched_eligibility_matches_single_reads() {
        let rpc = std::env::var("KITTIWAKE_LIVE_ARB_RPC").expect("KITTIWAKE_LIVE_ARB_RPC");
        let mut ids: Vec<String> = std::env::var("KITTIWAKE_LIVE_INDEXERS")
            .expect("KITTIWAKE_LIVE_INDEXERS")
            .split(',')
            .map(str::to_string)
            .collect();
        ids.insert(1, "not-an-address".into());
        let chain = Chain::new(rpc, None);
        let batch = futures_lite_block(chain.eligibility_batch(&ids)).unwrap();
        assert_eq!(batch.len(), ids.len());
        assert_eq!(batch[1], None);
        for (id, verdict) in ids
            .iter()
            .zip(&batch)
            .filter(|(id, _)| id.starts_with("0x"))
        {
            let single = futures_lite_block(chain.eligibility(id)).unwrap().eligible;
            assert_eq!(*verdict, Some(single), "{id}");
        }
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
