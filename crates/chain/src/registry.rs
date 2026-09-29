//! A Horizon data service's price of entry. Its provider registry is read from data-services-nest.
//!
//! Every one of these contracts is a fork of the same `DataService` base, which is what lets one
//! reader work across all of them. #67

use std::str::FromStr;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use kittiwake_types::ApiError;

sol! {
    #[sol(rpc)]
    interface IDataService {
        function getProvisionTokensRange() external view returns (uint256, uint256);
        function getThawingPeriodRange() external view returns (uint64, uint64);
        function getVerifierCutRange() external view returns (uint32, uint32);
    }
}

/// The ranges one service demands of an operator, or nothing.
///
/// `None` rather than zeroes when the calls fail, because a service that does not answer and a
/// service that asks for nothing must not look the same. Zero is a real and meaningful answer
/// here, which is exactly why it cannot double as "we could not tell".
pub struct ProvisionRanges {
    pub min_tokens: u128,
    /// `None` where the service sets no ceiling.
    pub max_tokens: Option<u128>,
    pub min_thawing: u64,
    /// `None` where the service sets no ceiling. The protocol's still applies.
    pub max_thawing: Option<u64>,
    pub min_cut_ppm: u32,
    pub max_cut_ppm: u32,
}

impl super::Chain {
    /// What one service demands of an operator, read from its own `ProvisionManager`.
    pub async fn provision_ranges(&self, address: &str) -> Option<ProvisionRanges> {
        let address = Address::from_str(address).ok()?;
        let parsed = self.arbitrum.parse().ok()?;
        let contract = IDataService::new(address, ProviderBuilder::new().connect_http(parsed));

        let (tokens_call, thawing_call, cut_call) = (
            contract.getProvisionTokensRange(),
            contract.getThawingPeriodRange(),
            contract.getVerifierCutRange(),
        );
        let (tokens, thawing, cut) =
            tokio::join!(tokens_call.call(), thawing_call.call(), cut_call.call());
        let (tokens, thawing, cut) = (tokens.ok()?, thawing.ok()?, cut.ok()?);

        // `type(uint256).max` and `type(uint64).max` are how a service says "no upper limit".
        let unbounded_tokens = tokens._1 == U256::MAX;
        Some(ProvisionRanges {
            min_tokens: u128::try_from(tokens._0).unwrap_or(u128::MAX),
            max_tokens: (!unbounded_tokens).then(|| u128::try_from(tokens._1).unwrap_or(u128::MAX)),
            min_thawing: thawing._0,
            max_thawing: (thawing._1 != u64::MAX).then_some(thawing._1),
            min_cut_ppm: cut._0,
            max_cut_ppm: cut._1,
        })
    }
}

sol! {
    #[sol(rpc)]
    interface IHorizonStaking {
        function getIdleStake(address serviceProvider) external view returns (uint256);
        function getStake(address serviceProvider) external view returns (uint256);
        function getProvision(address serviceProvider, address verifier) external view returns (
            uint256 tokens,
            uint256 tokensThawing,
            uint256 sharesThawing,
            uint32 maxVerifierCut,
            uint64 thawingPeriod,
            uint64 createdAt,
            uint32 maxVerifierCutPending,
            uint64 thawingPeriodPending,
            uint256 lastParametersStagedAt,
            uint256 thawingNonce
        );
    }
    #[sol(rpc)]
    interface IERC20Balance {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// GRT on Arbitrum One.
pub const GRT_ARBITRUM: &str = "0x9623063377AD1B27544C965cCd7342f7EA7e88C7";
/// HorizonStaking's proxy on Arbitrum One.
pub const HORIZON_STAKING: &str = "0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03";
/// `bytes32(uint256(keccak256('eip1967.proxy.implementation')) - 1)`.
///
/// Written as the hex everybody quotes rather than as limbs. Hand-decomposing this into four
/// little-endian `u64`s compiles whether or not it is right, and getting it wrong reads an empty
/// slot for every service - which the checklist then reports as "not a proxy, blocked" for all of
/// them. A plausible wrong answer, which is the exact failure the slot check exists to catch. The
/// first version of this constant had two limbs transposed and did precisely that.
///
/// Checked against Arbitrum One on 2026-09-10: Dispatch's slot holds
/// `0x3527a12af6256634df6aa9cc2896ed9588e12de3`, agreed by two independent endpoints.
const EIP1967_SLOT_HEX: &str = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

fn eip1967_slot() -> U256 {
    U256::from_str_radix(EIP1967_SLOT_HEX.trim_start_matches("0x"), 16).expect("constant")
}

/// What the checklist needs off the chain, in one round.
pub struct PreflightReads {
    pub grt_balance: u128,
    pub stake: u128,
    pub idle_stake: u128,
    pub provision_tokens: u128,
    pub provision_thawing_seconds: u64,
    /// The EIP-1967 implementation slot, as a 32-byte hex string.
    pub implementation_slot: String,
}

impl super::Chain {
    pub async fn preflight_reads(
        &self,
        address: &str,
        service: &str,
    ) -> Result<PreflightReads, ApiError> {
        let bad = |what: &str| ApiError::BadRequest(format!("{what} is not an address"));
        let who = Address::from_str(address).map_err(|_| bad("address"))?;
        let verifier = Address::from_str(service).map_err(|_| bad("service"))?;
        let parsed = self
            .arbitrum
            .parse()
            .map_err(|_| ApiError::Unavailable("the Arbitrum endpoint is not a URL".into()))?;
        let provider = ProviderBuilder::new().connect_http(parsed);

        let staking = IHorizonStaking::new(
            Address::from_str(HORIZON_STAKING).expect("constant"),
            provider.clone(),
        );
        let grt = IERC20Balance::new(
            Address::from_str(GRT_ARBITRUM).expect("constant"),
            provider.clone(),
        );
        let (balance_call, stake_call, idle_call, provision_call) = (
            grt.balanceOf(who),
            staking.getStake(who),
            staking.getIdleStake(who),
            staking.getProvision(who, verifier),
        );
        let (balance, stake, idle, provision, slot) = tokio::join!(
            balance_call.call(),
            stake_call.call(),
            idle_call.call(),
            provision_call.call(),
            provider.get_storage_at(verifier, eip1967_slot()),
        );

        let fail = |e: String| ApiError::Unavailable(e);
        let provision = provision.map_err(|e| fail(e.to_string()))?;
        Ok(PreflightReads {
            grt_balance: u128::try_from(balance.map_err(|e| fail(e.to_string()))?)
                .unwrap_or(u128::MAX),
            stake: u128::try_from(stake.map_err(|e| fail(e.to_string()))?).unwrap_or(u128::MAX),
            idle_stake: u128::try_from(idle.map_err(|e| fail(e.to_string()))?).unwrap_or(u128::MAX),
            provision_tokens: u128::try_from(provision.tokens).unwrap_or(u128::MAX),
            provision_thawing_seconds: provision.thawingPeriod,
            implementation_slot: format!("{:#066x}", slot.map_err(|e| fail(e.to_string()))?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot every proxy standard quotes, asserted rather than assumed. An off-by-one limb
    /// reads a different slot, finds nothing, and the checklist calls every service a non-proxy.
    #[test]
    fn the_implementation_slot_is_the_one_eip1967_specifies() {
        assert_eq!(format!("{:#066x}", eip1967_slot()), EIP1967_SLOT_HEX);
        // And it is keccak256("eip1967.proxy.implementation") - 1, which is where it comes from.
        let hashed = alloy::primitives::keccak256(b"eip1967.proxy.implementation");
        assert_eq!(
            U256::from_be_bytes(hashed.0) - U256::from(1),
            eip1967_slot()
        );
    }
}
