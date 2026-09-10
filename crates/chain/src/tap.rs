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
