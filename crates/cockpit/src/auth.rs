//! Sign-in by wallet: an EIP-191 signature over a message carrying a single-use nonce, from the
//! indexer or one of its operators, exchanged for a short-lived HMAC-signed session cookie.

use std::collections::HashMap;
use std::sync::Mutex;

use alloy::primitives::{Address, Signature};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const COOKIE: &str = "lodestar_cockpit";
/// Enough for every operator on a box to be mid sign-in; beyond it, a flood.
const MAX_OUTSTANDING: usize = 1024;

/// Whether an address may act for the indexer on the SubgraphService.
pub trait Operators: Send + Sync {
    fn is_operator(&self, indexer: Address, signer: Address)
    -> BoxFuture<'_, Result<bool, String>>;
}

pub struct ChainOperators(pub kittiwake_chain::Chain);

impl Operators for ChainOperators {
    fn is_operator(
        &self,
        indexer: Address,
        signer: Address,
    ) -> BoxFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            self.0
                .is_subgraph_service_operator(indexer, signer)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    UnknownNonce,
    Expired,
    BadSignature,
    WrongSigner,
    NotAnOperator,
    Busy,
    Chain(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::UnknownNonce => write!(f, "unknown or already used nonce"),
            AuthError::Expired => write!(f, "the sign-in message has expired"),
            AuthError::BadSignature => write!(f, "not a valid signature"),
            AuthError::WrongSigner => write!(f, "signed by a different address than asked"),
            AuthError::NotAnOperator => {
                write!(
                    f,
                    "the signer is neither the indexer nor one of its operators"
                )
            }
            AuthError::Busy => write!(f, "too many sign-ins in flight"),
            AuthError::Chain(e) => write!(f, "could not read operators from the chain: {e}"),
        }
    }
}

struct Challenge {
    signer: Address,
    message: String,
    expires_at: i64,
}

pub struct Issued {
    pub nonce: String,
    pub message: String,
    pub expires_at: i64,
}

pub struct Auth {
    indexer: Address,
    origin: String,
    nonce_ttl: i64,
    session_ttl: i64,
    key: [u8; 32],
    challenges: Mutex<HashMap<String, Challenge>>,
    operators: Box<dyn Operators>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub indexer: Address,
    pub signer: Address,
    pub expires_at: i64,
}

impl Auth {
    pub fn new(
        indexer: Address,
        origin: String,
        nonce_ttl: u64,
        session_ttl: u64,
        key: [u8; 32],
        operators: Box<dyn Operators>,
    ) -> Self {
        Self {
            indexer,
            origin,
            nonce_ttl: nonce_ttl as i64,
            session_ttl: session_ttl as i64,
            key,
            challenges: Mutex::new(HashMap::new()),
            operators,
        }
    }

    pub fn indexer(&self) -> Address {
        self.indexer
    }

    pub fn session_ttl(&self) -> i64 {
        self.session_ttl
    }

    pub fn challenge(&self, signer: Address, now: i64) -> Result<Issued, AuthError> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| AuthError::Chain(e.to_string()))?;
        let nonce = hex::encode(nonce);
        let expires_at = now + self.nonce_ttl;
        let message = format!(
            "Sign in to the Lodestar Cockpit for indexer {indexer}.\n\n\
             This signature only proves you control this address. It sends no transaction.\n\n\
             Signer: {signer}\nCockpit: {origin}\nNonce: {nonce}\nIssued At: {issued}\nExpires At: {expires}",
            indexer = self.indexer,
            origin = self.origin,
            issued = rfc3339(now),
            expires = rfc3339(expires_at),
        );
        let mut map = self.challenges.lock().expect("challenge lock");
        map.retain(|_, c| c.expires_at > now);
        if map.len() >= MAX_OUTSTANDING {
            return Err(AuthError::Busy);
        }
        map.insert(
            nonce.clone(),
            Challenge {
                signer,
                message: message.clone(),
                expires_at,
            },
        );
        Ok(Issued {
            nonce,
            message,
            expires_at,
        })
    }

    /// Consumes the nonce whatever the outcome, so a signature is good for one attempt only.
    pub async fn verify(
        &self,
        nonce: &str,
        signature: &str,
        now: i64,
    ) -> Result<Session, AuthError> {
        let challenge = self
            .challenges
            .lock()
            .expect("challenge lock")
            .remove(nonce)
            .ok_or(AuthError::UnknownNonce)?;
        if challenge.expires_at <= now {
            return Err(AuthError::Expired);
        }
        let recovered = recover(&challenge.message, signature)?;
        if recovered != challenge.signer {
            return Err(AuthError::WrongSigner);
        }
        if recovered != self.indexer
            && !self
                .operators
                .is_operator(self.indexer, recovered)
                .await
                .map_err(AuthError::Chain)?
        {
            return Err(AuthError::NotAnOperator);
        }
        Ok(Session {
            indexer: self.indexer,
            signer: recovered,
            expires_at: now + self.session_ttl,
        })
    }

    pub fn seal(&self, s: &Session) -> String {
        let payload = format!("{}|{}", s.signer, s.expires_at);
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac takes any key length");
        mac.update(payload.as_bytes());
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    pub fn open(&self, token: &str, now: i64) -> Option<Session> {
        let (payload, tag) = token.split_once('.')?;
        let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac takes any key length");
        mac.update(&payload);
        mac.verify_slice(&tag).ok()?;
        let payload = String::from_utf8(payload).ok()?;
        let (signer, expires_at) = payload.split_once('|')?;
        let expires_at: i64 = expires_at.parse().ok()?;
        if expires_at <= now {
            return None;
        }
        Some(Session {
            indexer: self.indexer,
            signer: signer.parse().ok()?,
            expires_at,
        })
    }
}

/// The address behind an EIP-191 `personal_sign` signature over `message`.
pub fn recover(message: &str, signature: &str) -> Result<Address, AuthError> {
    let bytes =
        hex::decode(signature.trim_start_matches("0x")).map_err(|_| AuthError::BadSignature)?;
    let sig = Signature::try_from(bytes.as_slice()).map_err(|_| AuthError::BadSignature)?;
    sig.recover_address_from_msg(message.as_bytes())
        .map_err(|_| AuthError::BadSignature)
}

fn rfc3339(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
