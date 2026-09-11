//! A TAP v2 receipt, signed the way GraphTallyCollector expects one.
//!
//! Used by the data-service playground to make one real paid-shaped request against a provider.
//! The providers involved accept receipts from any signer, so a fresh key per request is enough to
//! read: nothing is settled here, and no escrow is touched. A real payment is a different act.
//!
//! **The signed value and the reported value are the same number.** The incumbent built the
//! message from a bigint and then wrote `Number(timestamp_ns)` into the JSON it sent, and three
//! timestamps in four do not survive that: `Date.now() * 1e6` is around 1.79e18, where an `f64`
//! can only land on multiples of 256. The signature covered the exact nanosecond and the receipt
//! declared one up to 128ns away. It goes unnoticed because these providers do not verify, and it
//! would break the day one did. #67

use alloy::primitives::{Address, Bytes};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolStruct, eip712_domain};
use serde::Serialize;

/// The collector the domain is bound to, on Arbitrum One.
pub const GRAPH_TALLY_COLLECTOR: Address =
    alloy::primitives::address!("8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e");

sol! {
    struct Receipt {
        address data_service;
        address service_provider;
        uint64 timestamp_ns;
        uint64 nonce;
        uint128 value;
        bytes metadata;
    }
}

fn domain() -> alloy::sol_types::Eip712Domain {
    eip712_domain! {
        name: "GraphTallyCollector",
        version: "1",
        chain_id: 42161,
        verifying_contract: GRAPH_TALLY_COLLECTOR,
    }
}

/// One collection unit, declared rather than settled.
pub const ONE_CU_WEI: u128 = 4_000_000_000_000;

/// The JSON body a provider reads out of the `TAP-Receipt` header.
#[derive(Debug, Serialize)]
pub struct SignedReceipt {
    pub receipt: ReceiptJson,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct ReceiptJson {
    pub data_service: String,
    pub service_provider: String,
    /// Exact, not rounded through a float. See the module note.
    pub timestamp_ns: u64,
    pub nonce: u64,
    pub value: u128,
    pub metadata: String,
}

/// Sign a receipt with a fresh key.
///
/// `metadata` carries the payer, which is this ephemeral address: the provider reads the first
/// twenty bytes and finds the same account that signed.
pub fn sign_receipt(
    data_service: Address,
    service_provider: Address,
    timestamp_ns: u64,
    nonce: u64,
) -> Result<SignedReceipt, String> {
    let signer = PrivateKeySigner::random();
    sign_receipt_with(&signer, data_service, service_provider, timestamp_ns, nonce)
}

/// The same, with the key supplied, so the signing itself can be pinned to a vector.
pub fn sign_receipt_with(
    signer: &PrivateKeySigner,
    data_service: Address,
    service_provider: Address,
    timestamp_ns: u64,
    nonce: u64,
) -> Result<SignedReceipt, String> {
    let metadata = Bytes::from(signer.address().to_vec());
    let receipt = Receipt {
        data_service,
        service_provider,
        timestamp_ns,
        nonce,
        value: ONE_CU_WEI,
        metadata: metadata.clone(),
    };
    let signature = signer
        .sign_hash_sync(&receipt.eip712_signing_hash(&domain()))
        .map_err(|e| e.to_string())?;
    Ok(SignedReceipt {
        receipt: ReceiptJson {
            data_service: data_service.to_string().to_lowercase(),
            service_provider: service_provider.to_string().to_lowercase(),
            timestamp_ns,
            nonce,
            value: ONE_CU_WEI,
            metadata: format!("0x{}", hex_of(&metadata)),
        },
        signature: format!("0x{}", hex_of(&signature.as_bytes())),
    })
}

/// The SubgraphService, which is the `data_service` a subgraph query is paid through.
pub const SUBGRAPH_SERVICE: Address =
    alloy::primitives::address!("b2Bb92d0DE618878E438b55D5846cfecD9301105");

/// One wei, which is what the incumbent declares for a serving probe.
///
/// Deliberately not [`ONE_CU_WEI`]. The playground is buying a real answer from a provider that
/// reads the value; a probe is asking an indexer whether its serving path is up, and the receipt
/// exists so the question can be asked through the paid door rather than to pay for anything. An
/// indexer accumulates these and redeems the total, so a probe that declared a full collection
/// unit would be spending escrow to find out whether a subgraph syncs.
pub const PROBE_VALUE_WEI: u128 = 1;

/// A payer with a persistent, funded key.
///
/// Distinct from [`sign_receipt`], which mints a throwaway key per call. That is right for the
/// playground, whose providers accept any signer and settle nothing. It is wrong here: an indexer
/// that actually verifies looks the payer up in escrow, and an address it has never seen has no
/// balance. Same receipt format, different question being asked, so the two do not share a path.
pub struct Payer {
    signer: PrivateKeySigner,
}

/// Written by hand rather than derived: a derived one prints the key, and the whole point of a
/// type holding a signing key is that it cannot be printed by accident.
impl std::fmt::Debug for Payer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payer")
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

impl Payer {
    /// `key` is a 32-byte hex private key, with or without `0x`.
    pub fn from_hex(key: &str) -> Result<Self, String> {
        let trimmed = key.trim().trim_start_matches("0x");
        let signer: PrivateKeySigner = trimmed
            .parse()
            // Never the parse error itself: on a malformed key alloy's message can carry the
            // material it was given, and this one ends up in a log.
            .map_err(|_| "not a 32-byte hex private key".to_string())?;
        Ok(Self { signer })
    }

    /// The address an indexer will look for in escrow.
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// The `TAP-Receipt` header value for a query to `service_provider`.
    ///
    /// A receipt names one service provider, so a round that probes six indexers signs six: one
    /// receipt reused across them is addressed to whichever indexer it named and invalid at the
    /// other five.
    pub fn receipt_for(&self, service_provider: Address) -> Result<String, String> {
        let signed = self.sign_probe_receipt(service_provider, now_ns(), random_nonce())?;
        serde_json::to_string(&signed).map_err(|e| e.to_string())
    }

    /// The same with the clock and the nonce supplied, so it can be pinned to a vector.
    pub fn sign_probe_receipt(
        &self,
        service_provider: Address,
        timestamp_ns: u64,
        nonce: u64,
    ) -> Result<SignedReceipt, String> {
        // Empty, not the payer address. The playground's providers read a payer out of the
        // metadata; an indexer recovers it from the signature, and the incumbent has always sent
        // `0x` here. A receipt these indexers already accept is not the place to improve on that.
        let metadata = Bytes::new();
        let receipt = Receipt {
            data_service: SUBGRAPH_SERVICE,
            service_provider,
            timestamp_ns,
            nonce,
            value: PROBE_VALUE_WEI,
            metadata: metadata.clone(),
        };
        let signature = self
            .signer
            .sign_hash_sync(&receipt.eip712_signing_hash(&domain()))
            .map_err(|e| e.to_string())?;
        Ok(SignedReceipt {
            receipt: ReceiptJson {
                data_service: SUBGRAPH_SERVICE.to_string().to_lowercase(),
                service_provider: service_provider.to_string().to_lowercase(),
                timestamp_ns,
                nonce,
                value: PROBE_VALUE_WEI,
                metadata: "0x".into(),
            },
            signature: format!("0x{}", hex_of(&signature.as_bytes())),
        })
    }
}

/// Nanoseconds since the epoch, exact. See the module note on why this is not rounded.
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// An indexer rejects a receipt whose nonce it has already seen from this payer, so this only has
/// to not repeat. Below 2^53 because the incumbent's nonces are, and an indexer that stored one in
/// a double would round a larger one into a collision with its neighbour.
fn random_nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
        & ((1u64 << 53) - 1)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The vector that stops the domain or the type hash drifting.** Computed with viem's
    /// `hashTypedData` on 2026-09-10 over exactly these values, which is the library the incumbent
    /// signs with. A different domain separator or field order produces a plausible signature that
    /// no provider will accept.
    #[test]
    fn the_signing_hash_is_the_one_viem_computes() {
        let receipt = Receipt {
            data_service: Address::ZERO,
            service_provider: Address::ZERO,
            timestamp_ns: 1_789_038_882_260_000_000,
            nonce: 42,
            value: 4_000_000_000_000,
            metadata: Bytes::from(
                alloy::primitives::address!("1111111111111111111111111111111111111111").to_vec(),
            ),
        };
        assert_eq!(
            receipt.eip712_signing_hash(&domain()).to_string(),
            "0x5c04af3dc900755bda822d12ebbfc6fab727239e9b3bb351398b4c9490725ffa"
        );
    }

    /// The whole point of the module note: what is signed and what is reported must be the same
    /// number. This timestamp is one of the three in four that a JavaScript `Number` rounds.
    #[test]
    fn the_reported_timestamp_is_the_signed_one_to_the_nanosecond() {
        let ts = 1_789_038_882_260_000_001u64;
        assert_ne!(
            ts as f64 as u64, ts,
            "this fixture must be a timestamp a float cannot hold, or it proves nothing"
        );
        let signed = sign_receipt(Address::ZERO, Address::ZERO, ts, 7).unwrap();
        assert_eq!(signed.receipt.timestamp_ns, ts);
        let json = serde_json::to_string(&signed).unwrap();
        assert!(
            json.contains("1789038882260000001"),
            "the wire value was rounded: {json}"
        );
    }

    /// The well-known Hardhat account #1. A test key, and only ever a test key.
    const TEST_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    /// **The payer vector, from viem.** Computed on 2026-09-11 with the library the incumbent
    /// signs with, over the payer shape rather than the playground's: SubgraphService as the data
    /// service, one wei, and empty metadata. The signature is pinned as well as the hash, because
    /// a matching hash with a different recovery byte is still a receipt no indexer accepts.
    #[test]
    fn the_payer_receipt_is_the_one_viem_signs() {
        let payer = Payer::from_hex(TEST_KEY).unwrap();
        assert_eq!(
            payer.address().to_string(),
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
        );

        let provider = alloy::primitives::address!("0000000000000000000000000000000000000001");
        let receipt = Receipt {
            data_service: SUBGRAPH_SERVICE,
            service_provider: provider,
            timestamp_ns: 1_789_038_882_260_000_001,
            nonce: 42,
            value: PROBE_VALUE_WEI,
            metadata: Bytes::new(),
        };
        assert_eq!(
            receipt.eip712_signing_hash(&domain()).to_string(),
            "0x17ff78b5929a75592a5c045aeef20a669c750bacfe4cb6d8efa95260fc2ed8ed"
        );

        let signed = payer
            .sign_probe_receipt(provider, 1_789_038_882_260_000_001, 42)
            .unwrap();
        assert_eq!(
            signed.signature,
            "0xaf0440ab7c4821ca873d9c04e63708f1796f89108df95ed44a91e69f1ae083cc50d1130c4403219055429b892f37e0c4c127b8f08de2d80fc3d8060f7209e85e1b"
        );
        assert_eq!(signed.receipt.metadata, "0x");
        assert_eq!(signed.receipt.value, 1);
    }

    /// A receipt names one indexer. Reusing a round's first receipt across the rest would address
    /// them all to whichever indexer it named, and the other five would reject it - which the
    /// classifier would read as "asked to be paid" rather than "we sent the wrong receipt".
    #[test]
    fn each_indexer_gets_a_receipt_addressed_to_it() {
        let payer = Payer::from_hex(TEST_KEY).unwrap();
        let a = payer
            .receipt_for(alloy::primitives::address!(
                "0000000000000000000000000000000000000001"
            ))
            .unwrap();
        let b = payer
            .receipt_for(alloy::primitives::address!(
                "0000000000000000000000000000000000000002"
            ))
            .unwrap();
        assert!(a.contains("0x0000000000000000000000000000000000000001"));
        assert!(b.contains("0x0000000000000000000000000000000000000002"));
        assert_ne!(a, b);
    }

    /// The same payer every time, because an indexer settles against a balance held by one
    /// address. This is the difference from the playground signer, and it is the whole point.
    #[test]
    fn the_payer_key_is_persistent_unlike_the_playgrounds() {
        let a = Payer::from_hex(TEST_KEY).unwrap();
        let b = Payer::from_hex(TEST_KEY).unwrap();
        assert_eq!(a.address(), b.address());
        assert_ne!(
            sign_receipt(Address::ZERO, Address::ZERO, 1, 1)
                .unwrap()
                .signature,
            sign_receipt(Address::ZERO, Address::ZERO, 1, 1)
                .unwrap()
                .signature,
        );
    }

    /// A key it cannot use must be a refusal at start-up, not a receipt nobody can verify. And the
    /// message must not quote the material it was handed.
    #[test]
    fn a_bad_key_is_refused_without_repeating_it() {
        let err = Payer::from_hex("0xdeadbeef").unwrap_err();
        assert!(!err.contains("deadbeef"), "the key leaked into: {err}");
        assert!(Payer::from_hex("").is_err());
        // With and without the prefix, because the environment carries both spellings.
        assert!(Payer::from_hex(TEST_KEY).is_ok());
        assert!(Payer::from_hex(TEST_KEY.trim_start_matches("0x")).is_ok());
    }

    /// A fresh key per request, and the payer in the metadata is that same key.
    #[test]
    fn every_receipt_is_signed_by_a_new_key_that_names_itself_as_payer() {
        let a = sign_receipt(Address::ZERO, Address::ZERO, 1, 1).unwrap();
        let b = sign_receipt(Address::ZERO, Address::ZERO, 1, 1).unwrap();
        assert_ne!(
            a.receipt.metadata, b.receipt.metadata,
            "a new key each time"
        );
        assert_ne!(a.signature, b.signature);
        assert_eq!(a.receipt.metadata.len(), 42, "0x plus twenty bytes");
        assert_eq!(a.signature.len(), 132, "0x plus sixty-five bytes");
        assert_eq!(a.receipt.value, ONE_CU_WEI);
    }
}
