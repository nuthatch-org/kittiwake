//! Reading a Horizon data service's provider registry and its price of entry.
//!
//! Every one of these contracts is a fork of the same `DataService` base and emits the same two
//! events, which is what lets one reader work across all of them. #67

use std::str::FromStr;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::SolEvent;
use kittiwake_types::ApiError;

sol! {
    #[sol(rpc)]
    interface IDataService {
        event ProviderRegistered(address indexed provider, string endpoint, string geoHash);
        event ProviderDeregistered(address indexed provider);

        function getProvisionTokensRange() external view returns (uint256, uint256);
        function getThawingPeriodRange() external view returns (uint64, uint64);
        function getVerifierCutRange() external view returns (uint32, uint32);
    }
}

/// One registration or deregistration, as the log carried it. The fold that turns these into a
/// current provider list lives in `kittiwake-read`, which depends on this crate rather than the
/// other way round.
pub struct RegistryLog {
    pub provider: String,
    pub endpoint: Option<String>,
    pub block_number: u64,
    pub log_index: u64,
    pub registered: bool,
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
    /// Every registration and deregistration this service has ever emitted.
    pub async fn provider_registry(&self, address: &str) -> Result<Vec<RegistryLog>, ApiError> {
        let address = Address::from_str(address)
            .map_err(|_| ApiError::Internal(format!("{address} is not an address")))?;
        let parsed = self
            .arbitrum
            .parse()
            .map_err(|_| ApiError::Unavailable("the Arbitrum endpoint is not a URL".into()))?;
        let provider = ProviderBuilder::new().connect_http(parsed);

        let mut out = Vec::new();
        for (topic, registered) in [
            (IDataService::ProviderRegistered::SIGNATURE_HASH, true),
            (IDataService::ProviderDeregistered::SIGNATURE_HASH, false),
        ] {
            let filter = Filter::new()
                .address(address)
                .event_signature(topic)
                .from_block(0);
            let logs = provider
                .get_logs(&filter)
                .await
                .map_err(|e| ApiError::Unavailable(e.to_string()))?;
            for log in logs {
                // The provider is the indexed parameter, so it is in the topics rather than the
                // data, whichever event this is.
                let Some(provider_topic) = log.topics().get(1) else {
                    continue;
                };
                let endpoint = registered
                    .then(|| {
                        IDataService::ProviderRegistered::decode_log_data(log.data())
                            .ok()
                            .map(|decoded| decoded.endpoint)
                    })
                    .flatten();
                out.push(RegistryLog {
                    provider: format!("0x{}", hex::encode(&provider_topic.0[12..])),
                    endpoint,
                    block_number: log.block_number.unwrap_or(0),
                    log_index: log.log_index.unwrap_or(0),
                    registered,
                });
            }
        }
        Ok(out)
    }

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
