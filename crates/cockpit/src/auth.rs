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
/// A nonce is the signer, the issue time and eight random bytes, then a tag over them, so issuing
/// one stores nothing and only a completed sign-in is remembered.
const NONCE_BODY: usize = 20 + 8 + 8;
const NONCE_TAG: usize = 16;

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
            AuthError::Chain(e) => write!(f, "could not read operators from the chain: {e}"),
        }
    }
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
    /// Nonces of completed sign-ins, until they expire, so each signature signs in once.
    spent: Mutex<HashMap<String, i64>>,
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
            spent: Mutex::new(HashMap::new()),
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
        let mut body = [0u8; NONCE_BODY];
        body[..20].copy_from_slice(signer.as_slice());
        body[20..28].copy_from_slice(&now.to_be_bytes());
        getrandom::fill(&mut body[28..]).map_err(|e| AuthError::Chain(e.to_string()))?;
        let mut sealed = body.to_vec();
        sealed.extend_from_slice(&self.nonce_mac(&body).finalize().into_bytes()[..NONCE_TAG]);
        let nonce = URL_SAFE_NO_PAD.encode(sealed);
        let expires_at = now + self.nonce_ttl;
        Ok(Issued {
            message: self.message(signer, &nonce, now, expires_at),
            nonce,
            expires_at,
        })
    }

    fn nonce_mac(&self, body: &[u8]) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac takes any key length");
        mac.update(b"cockpit-nonce|");
        mac.update(body);
        mac
    }

    fn message(&self, signer: Address, nonce: &str, issued_at: i64, expires_at: i64) -> String {
        format!(
            "Sign in to the Lodestar Cockpit for indexer {indexer}.\n\n\
             This signature only proves you control this address. It sends no transaction.\n\n\
             Signer: {signer}\nCockpit: {origin}\nNonce: {nonce}\nIssued At: {issued}\nExpires At: {expires}",
            indexer = self.indexer,
            origin = self.origin,
            issued = rfc3339(issued_at),
            expires = rfc3339(expires_at),
        )
    }

    /// A nonce signs in once: it is spent when a sign-in with it succeeds.
    pub async fn verify(
        &self,
        nonce: &str,
        signature: &str,
        now: i64,
    ) -> Result<Session, AuthError> {
        let sealed = URL_SAFE_NO_PAD
            .decode(nonce)
            .ok()
            .filter(|s| s.len() == NONCE_BODY + NONCE_TAG)
            .ok_or(AuthError::UnknownNonce)?;
        let (body, tag) = sealed.split_at(NONCE_BODY);
        self.nonce_mac(body)
            .verify_truncated_left(tag)
            .map_err(|_| AuthError::UnknownNonce)?;
        let signer = Address::from_slice(&body[..20]);
        let issued_at = i64::from_be_bytes(body[20..28].try_into().expect("eight bytes"));
        let expires_at = issued_at + self.nonce_ttl;
        if expires_at <= now {
            return Err(AuthError::Expired);
        }
        let recovered = recover(
            &self.message(signer, nonce, issued_at, expires_at),
            signature,
        )?;
        if recovered != signer {
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
        let mut spent = self.spent.lock().expect("spent lock");
        spent.retain(|_, until| *until > now);
        if spent.insert(nonce.to_string(), expires_at).is_some() {
            return Err(AuthError::UnknownNonce);
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
