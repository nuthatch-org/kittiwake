//! The Dock's sessions: the same HMAC scheme, byte for byte.
//!
//! This is the one part of the product-layer port with a deadline attached. The session cookie is
//! stateless and signed, so if this implementation differs from the incumbent's in any detail - the
//! field order, the separator, the encoding, how the secret is read - every logged-in user is
//! silently signed out the moment the rewrite points at this service. Nothing errors. They simply
//! find themselves logged out and have no idea why.
//!
//! So it is not written from a reading of the original and hoped over. `session_tests` pins tokens
//! produced by the incumbent's own `createSession` under a known test secret, and asserts this
//! parses them. A change that breaks compatibility fails the build rather than the users.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Seven days, matching `SESSION_TTL`. The token carries its own issue time and is never refreshed,
/// so this is a hard cap rather than an idle timeout: a session ends seven days after sign-in
/// whatever the user was doing.
pub const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 7;

/// How old a sign-in challenge may be, which is what stops a captured signature being replayed
/// tomorrow.
pub const AUTH_WINDOW_SECS: i64 = 300;

/// Arbitrum One, where the wallets this signs in hold their stake.
pub const SIGN_IN_CHAIN_ID: u64 = 42161;

const SIGN_IN_STATEMENT: &str =
    "Sign in to Lodestar. This proves you own this wallet. It is free and sends no transaction.";

pub const COOKIE_NAME: &str = "studio_session";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("the token is not in `address:issuedAt:signature` form")]
    Malformed,
    #[error("the issue time is not a number")]
    BadTimestamp,
    #[error("the session is older than its seven days")]
    Expired,
    #[error("the signature does not match")]
    BadSignature,
}

/// The secret, checked the way the incumbent checks it.
///
/// The length floor is not decoration: an HMAC key shorter than the hash it feeds adds no strength
/// beyond its own length, and a short `SESSION_SECRET` is the kind of thing that gets set once in a
/// hurry and never revisited.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(s: &str) -> Result<Self, String> {
        if s.len() < 32 {
            return Err(
                "SESSION_SECRET must be at least 32 characters (use a random 32+ byte value)"
                    .into(),
            );
        }
        Ok(Self(s.as_bytes().to_vec()))
    }
}

/// Never print the key, not even by accident in a debug log of the config.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<{} bytes>)", self.0.len())
    }
}

fn sign(secret: &Secret, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(&secret.0).expect("hmac takes a key of any length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a session token for an address.
///
/// The address is lowercased, as the incumbent lowercases it, because the same wallet arrives
/// checksummed from one client and lowercase from another and two spellings of one address would
/// be two sessions.
pub fn create_session(secret: &Secret, address: &str, issued_at: i64) -> String {
    let payload = format!("{}:{}", address.to_lowercase(), issued_at);
    let sig = sign(secret, &payload);
    format!("{payload}:{sig}")
}

/// The address a token vouches for, or why it does not.
///
/// Split on the **last** colon rather than the first, exactly as the incumbent does. An address
/// contains no colon so today the two agree, but the original chose `lastIndexOf` and a port that
/// quietly chose differently would diverge the moment anything else did.
pub fn parse_session(secret: &Secret, token: &str, now: i64) -> Result<String, SessionError> {
    let idx = token.rfind(':').ok_or(SessionError::Malformed)?;
    let (payload, sig) = (&token[..idx], &token[idx + 1..]);

    let mut parts = payload.split(':');
    let address = parts.next().ok_or(SessionError::Malformed)?;
    let issued_at = parts.next().ok_or(SessionError::Malformed)?;
    if parts.next().is_some() || address.is_empty() {
        return Err(SessionError::Malformed);
    }
    let issued_at: i64 = issued_at.parse().map_err(|_| SessionError::BadTimestamp)?;
    if now - issued_at > SESSION_TTL_SECS {
        return Err(SessionError::Expired);
    }

    // Constant time, because a comparison that returns early tells an attacker how much of a forged
    // signature was right, and a few thousand attempts turn that into the whole of it.
    let expected = sign(secret, payload);
    let ok: bool = <subtle::Choice as Into<bool>>::into(subtle::ConstantTimeEq::ct_eq(
        expected.as_bytes(),
        sig.as_bytes(),
    ));
    if expected.len() != sig.len() || !ok {
        return Err(SessionError::BadSignature);
    }
    Ok(address.to_string())
}

/// The origin a sign-in message may name, as its URI, or `None` for any other host. Local
/// development signs in against the production API, so `localhost` is accepted on any port.
fn sign_in_uri(domain: &str) -> Option<String> {
    if domain == "www.lodestar-dashboard.com" {
        return Some(format!("https://{domain}"));
    }
    let port = domain.strip_prefix("localhost")?;
    let local = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    local.then(|| format!("http://{domain}"))
}

/// A nonce this service can recognise as its own without remembering it: eight random bytes and
/// an HMAC over them and the issue time.
pub fn issue_sign_in_nonce(secret: &Secret, issued_at: i64) -> String {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).expect("the OS must be able to produce randomness");
    nonce_from(secret, &hex::encode(r), issued_at)
}

fn nonce_from(secret: &Secret, random: &str, issued_at: i64) -> String {
    let mac = sign(secret, &format!("sign-in-nonce:{random}:{issued_at}"));
    format!("{random}{}", &mac[..32])
}

/// The EIP-4361 message a wallet is asked to sign, byte for byte what viem's `createSiweMessage`
/// builds from the same fields, so a wallet can check the domain against the page asking.
pub fn sign_in_message(domain: &str, address: &str, nonce: &str, issued_at: i64) -> Option<String> {
    let uri = sign_in_uri(domain)?;
    let issued = chrono::DateTime::from_timestamp(issued_at, 0)?.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    Some(format!(
        "{domain} wants you to sign in with your Ethereum account:\n{address}\n\n\
         {SIGN_IN_STATEMENT}\n\nURI: {uri}\nVersion: 1\nChain ID: {SIGN_IN_CHAIN_ID}\n\
         Nonce: {nonce}\nIssued At: {issued}"
    ))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignInError {
    #[error("the message is not a sign-in message this service issued")]
    NotOurMessage,
    #[error("the nonce was not issued here")]
    BadNonce,
    #[error("the message is outside the five-minute window")]
    StaleMessage,
    #[error("the signature is not readable")]
    BadSignature,
    #[error("the signature is by a different address")]
    WrongSigner,
    #[error("that is not an address")]
    BadAddress,
}

/// A verified sign-in's nonce, for the caller to spend.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedIn {
    pub nonce: String,
    pub issued_at: i64,
}

/// Verify that `address` signed exactly the message this service would have asked it to sign:
/// an allowed domain, a nonce issued here, and fresh.
///
/// The signature is recovered last: recovering a public key is the expensive half, and a message
/// that is not ours is refused whether or not it was signed properly.
pub fn verify_sign_in(
    secret: &Secret,
    address: &str,
    message: &str,
    signature: &str,
    now: i64,
) -> Result<SignedIn, SignInError> {
    let want: alloy::primitives::Address = address.parse().map_err(|_| SignInError::BadAddress)?;
    let (domain, _) = message
        .split_once(" wants you to sign in with your Ethereum account:\n")
        .ok_or(SignInError::NotOurMessage)?;
    let field = |name: &str| {
        message
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .ok_or(SignInError::NotOurMessage)
    };
    let nonce = field("Nonce: ")?;
    let issued_at = chrono::DateTime::parse_from_rfc3339(field("Issued At: ")?)
        .map_err(|_| SignInError::NotOurMessage)?
        .timestamp();
    if (now - issued_at).abs() > AUTH_WINDOW_SECS {
        return Err(SignInError::StaleMessage);
    }
    let random = nonce.get(..16).ok_or(SignInError::BadNonce)?;
    let expected = nonce_from(secret, random, issued_at);
    if expected.len() != nonce.len()
        || !bool::from(subtle::ConstantTimeEq::ct_eq(
            expected.as_bytes(),
            nonce.as_bytes(),
        ))
    {
        return Err(SignInError::BadNonce);
    }
    if sign_in_message(domain, &want.to_checksum(None), nonce, issued_at).as_deref()
        != Some(message)
    {
        return Err(SignInError::NotOurMessage);
    }

    let bytes =
        hex::decode(signature.trim_start_matches("0x")).map_err(|_| SignInError::BadSignature)?;
    let sig = alloy::primitives::Signature::try_from(bytes.as_slice())
        .map_err(|_| SignInError::BadSignature)?;
    let recovered = sig
        .recover_address_from_msg(message.as_bytes())
        .map_err(|_| SignInError::BadSignature)?;
    if recovered != want {
        return Err(SignInError::WrongSigner);
    }
    Ok(SignedIn {
        nonce: nonce.to_string(),
        issued_at,
    })
}

/// Nonces already exchanged for a session, kept only while they could still verify.
#[derive(Clone, Default)]
pub struct SpentNonces(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>);

impl SpentNonces {
    /// True the first time a sign-in is spent; false for a replay of it.
    pub fn spend(&self, signed: &SignedIn, now: i64) -> bool {
        let mut spent = self.0.lock().expect("spent nonces lock");
        spent.retain(|_, issued_at| now - *issued_at <= AUTH_WINDOW_SECS);
        spent
            .insert(signed.nonce.clone(), signed.issued_at)
            .is_none()
    }
}

/// A new deploy key, and the hash we store instead of it.
///
/// The key is shown to the user once and never again; only the hash is kept, so a database that
/// leaks does not hand out working keys.
pub fn generate_deploy_key() -> String {
    // From the operating system's own source, not a seeded generator. A deploy key predicted from
    // a previous one is a deploy key that was never secret.
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the OS must be able to produce randomness");
    hex::encode(b)
}

pub fn hash_key(key: &str) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
mod session_tests {
    use super::*;

    /// Tokens minted by the incumbent's own `createSession` under this secret. Generated with
    /// node's `createHmac('sha256', SECRET).update(payload).digest('hex')` - the same call the live
    /// service makes - and pinned here so a change to this implementation that breaks compatibility
    /// fails the build rather than signing every user out at cutover.
    const SECRET: &str = "test-secret-that-is-at-least-32-characters-long";
    const TOKEN_A: &str = "0x1234567890abcdef1234567890abcdef12345678:1757000000:1df128fb0548710d9dcf1d0581d9669b35cde59dc1ffedaeeb43411d7b0e6984";
    const TOKEN_B: &str = "0xabcdef1234567890abcdef1234567890abcdef12:1700000000:1ea6a9726475ad7005944f46a6fd1168fbccfc9453b339c0f3d1a4e92a84b87c";

    fn secret() -> Secret {
        Secret::new(SECRET).unwrap()
    }

    #[test]
    fn a_token_the_incumbent_minted_is_accepted_here() {
        assert_eq!(
            parse_session(&secret(), TOKEN_A, 1_757_000_100).unwrap(),
            "0x1234567890abcdef1234567890abcdef12345678"
        );
        assert_eq!(
            parse_session(&secret(), TOKEN_B, 1_700_000_100).unwrap(),
            "0xabcdef1234567890abcdef1234567890abcdef12"
        );
    }

    /// And we mint what it would have minted. Without this the compatibility is one-way: we could
    /// read their cookies while handing out ones they cannot read, which breaks a rollback.
    #[test]
    fn we_mint_the_token_the_incumbent_would_have() {
        assert_eq!(
            create_session(
                &secret(),
                "0x1234567890abcdef1234567890abcdef12345678",
                1_757_000_000
            ),
            TOKEN_A
        );
        // A checksummed address must produce the lowercase token, or the same wallet gets two
        // different sessions depending on which client signed it in.
        assert_eq!(
            create_session(
                &secret(),
                "0xABCDEF1234567890ABCDEF1234567890ABCDEF12",
                1_700_000_000
            ),
            TOKEN_B
        );
    }

    /// The test that makes the one above mean something: it must be checking the signature, not
    /// merely the parse.
    #[test]
    fn one_changed_byte_in_the_signature_is_refused() {
        let mut t = TOKEN_A.to_string();
        let last = t.pop().unwrap();
        t.push(if last == 'a' { 'b' } else { 'a' });
        assert_eq!(
            parse_session(&secret(), &t, 1_757_000_100),
            Err(SessionError::BadSignature)
        );
    }

    #[test]
    fn a_different_secret_does_not_open_it() {
        let other = Secret::new("another-secret-that-is-also-at-least-32-chars").unwrap();
        assert_eq!(
            parse_session(&other, TOKEN_A, 1_757_000_100),
            Err(SessionError::BadSignature)
        );
    }

    /// The fixtures above are synthetic, so nothing here has yet met the live secret. This one
    /// does, and is skipped unless both variables are set: a production cookie must not enter the
    /// repository.
    ///
    ///     DOCK_SESSION_SECRET=... DOCK_SESSION_TOKEN=... \
    ///       cargo test -p kittiwake-dock a_production_cookie -- --nocapture
    ///
    /// **A skip here is not a pass**, and in CI it is always a skip. The check #12 actually asks
    /// for is `cargo run -p kittiwake-dock --bin verify-session`, which exits non-zero when it is
    /// unconfigured and so cannot be mistaken for a green run. It also makes the third assertion
    /// this one does not: that the signature covers the *payload* and not only itself.
    #[test]
    fn a_production_cookie_validates_against_the_production_secret() {
        let (Ok(raw), Ok(token)) = (
            std::env::var("DOCK_SESSION_SECRET"),
            std::env::var("DOCK_SESSION_TOKEN"),
        ) else {
            eprintln!("skipped: set DOCK_SESSION_SECRET and DOCK_SESSION_TOKEN");
            return;
        };

        let secret = Secret::new(&raw).expect("the production secret is shorter than 32 bytes");
        let (payload, _) = token
            .rsplit_once(':')
            .expect("token is not address:issued_at:sig");
        let (address, issued) = payload
            .rsplit_once(':')
            .expect("token is not address:issued_at:sig");
        let issued_at: i64 = issued
            .parse()
            .expect("the token's timestamp is not a number");

        // The clock is taken from the token so this tests the signature rather than how long ago
        // the cookie happened to be captured.
        assert_eq!(
            parse_session(&secret, &token, issued_at + 1),
            Ok(address.to_string())
        );

        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert_eq!(
            parse_session(&secret, &tampered, issued_at + 1),
            Err(SessionError::BadSignature)
        );
    }

    #[test]
    fn a_session_expires_after_seven_days_and_not_before() {
        let just_inside = 1_757_000_000 + SESSION_TTL_SECS;
        assert!(parse_session(&secret(), TOKEN_A, just_inside).is_ok());
        assert_eq!(
            parse_session(&secret(), TOKEN_A, just_inside + 1),
            Err(SessionError::Expired)
        );
    }

    #[test]
    fn nonsense_is_malformed_rather_than_a_panic() {
        for t in ["", "nocolons", "a:b:c:d:e"] {
            assert!(parse_session(&secret(), t, 1_757_000_100).is_err(), "{t}");
        }
        assert_eq!(
            parse_session(&secret(), "0xabc:notanumber:ff", 1_757_000_100),
            Err(SessionError::BadTimestamp)
        );
    }

    #[test]
    fn a_short_secret_is_refused_at_construction() {
        assert!(Secret::new("too short").is_err());
        assert!(Secret::new(&"x".repeat(32)).is_ok());
    }

    /// The secret must not be printable, or one debug log of the config hands out every session.
    #[test]
    fn the_secret_does_not_print_itself() {
        let s = format!("{:?}", secret());
        assert!(!s.contains("test-secret"), "{s}");
        assert!(s.contains("47 bytes"), "{s}");
    }

    #[test]
    fn a_deploy_key_is_stored_as_its_hash_and_not_as_itself() {
        let key = generate_deploy_key();
        assert_eq!(key.len(), 64);
        let h = hash_key(&key);
        assert_eq!(h.len(), 64);
        assert_ne!(h, key);
        // sha256("") pinned, so the digest is the one the incumbent computes.
        assert_eq!(
            hash_key(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

#[cfg(test)]
mod sign_in_tests {
    use super::*;

    /// Real EIP-191 signatures by viem over the message lodestar's `buildSignInMessage` builds. The
    /// key is a well-known throwaway (Hardhat's second account); it holds nothing and never will.
    const ADDRESS: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    const TS: i64 = 1_757_000_000;
    const NONCE: &str = "0123456789abcdef2716f2a93b5b752b1e1fbccb1bb1de1b";
    const MESSAGE: &str = "www.lodestar-dashboard.com wants you to sign in with your Ethereum account:\n0x70997970C51812dc3A010C7d01b50e0d17dc79C8\n\nSign in to Lodestar. This proves you own this wallet. It is free and sends no transaction.\n\nURI: https://www.lodestar-dashboard.com\nVersion: 1\nChain ID: 42161\nNonce: 0123456789abcdef2716f2a93b5b752b1e1fbccb1bb1de1b\nIssued At: 2025-09-04T15:33:20.000Z";
    const SIGNATURE: &str = "0x70e06b0ab32292442fe2deefa2f644112d9653aa71597adc5597028c03af16b04287a99a77a0ed3fe0deb43e8b2ea0c72f1750c0d889ff16df8419de035c37931c";
    const LOCAL_MESSAGE: &str = "localhost:3019 wants you to sign in with your Ethereum account:\n0x70997970C51812dc3A010C7d01b50e0d17dc79C8\n\nSign in to Lodestar. This proves you own this wallet. It is free and sends no transaction.\n\nURI: http://localhost:3019\nVersion: 1\nChain ID: 42161\nNonce: 0123456789abcdef2716f2a93b5b752b1e1fbccb1bb1de1b\nIssued At: 2025-09-04T15:33:20.000Z";
    const LOCAL_SIGNATURE: &str = "0xb433a07e5d996fc45659509f962e32f4b17fb6575a51e55f3d71406a66b022b96254889f8ed8219eab82c99dc179829378467bf581b4fff17db00567c40fc3781b";

    fn secret() -> Secret {
        Secret::new("test-secret-that-is-at-least-32-characters-long").unwrap()
    }

    fn verify(address: &str, message: &str, signature: &str, now: i64) -> Result<(), SignInError> {
        verify_sign_in(&secret(), address, message, signature, now).map(|_| ())
    }

    #[test]
    fn we_build_the_bytes_viem_signed() {
        assert_eq!(nonce_from(&secret(), "0123456789abcdef", TS), NONCE);
        assert_eq!(
            sign_in_message("www.lodestar-dashboard.com", ADDRESS, NONCE, TS).as_deref(),
            Some(MESSAGE)
        );
        assert_eq!(
            sign_in_message("localhost:3019", ADDRESS, NONCE, TS).as_deref(),
            Some(LOCAL_MESSAGE)
        );
    }

    #[test]
    fn a_signature_a_wallet_really_produced_is_accepted() {
        assert_eq!(
            verify_sign_in(&secret(), ADDRESS, MESSAGE, SIGNATURE, TS + 10),
            Ok(SignedIn {
                nonce: NONCE.into(),
                issued_at: TS
            })
        );
        assert_eq!(
            verify(ADDRESS, LOCAL_MESSAGE, LOCAL_SIGNATURE, TS + 10),
            Ok(())
        );
        // The body may carry the address in any case; the message is rebuilt checksummed.
        assert_eq!(
            verify(&ADDRESS.to_lowercase(), MESSAGE, SIGNATURE, TS + 10),
            Ok(())
        );
    }

    /// kittiwake#210: any message the wallet signed that mentioned a recent timestamp used to
    /// open a session. This one is a real signature over the old, unbound text.
    #[test]
    fn a_signature_over_any_other_message_with_a_timestamp_is_refused() {
        let other = "Sign in to Lodestar Studio\n\nAddress: 0x70997970C51812dc3A010C7d01b50e0d17dc79C8\nTimestamp: 1757000000";
        let sig = "0x875cfb39b4182be71e295ed0377180113046213647e43c8af9838274054169d26b12488eb948f310054c052470c07679e5c2cf755abf76faf59989ffee7853471c";
        assert_eq!(
            verify(ADDRESS, other, sig, TS + 10),
            Err(SignInError::NotOurMessage)
        );
    }

    #[test]
    fn another_domain_is_refused() {
        let elsewhere = MESSAGE.replace("www.lodestar-dashboard.com", "lodestar-dashboard.evil");
        assert_eq!(
            verify(ADDRESS, &elsewhere, SIGNATURE, TS + 10),
            Err(SignInError::NotOurMessage)
        );
        assert_eq!(sign_in_uri("localhost.evil"), None);
        assert_eq!(sign_in_uri("localhost:123456"), None);
        assert_eq!(sign_in_uri("lodestar-dashboard.com"), None);
    }

    #[test]
    fn a_nonce_this_service_did_not_issue_is_refused() {
        let other = Secret::new("another-secret-that-is-also-at-least-32-chars").unwrap();
        assert_eq!(
            verify_sign_in(&other, ADDRESS, MESSAGE, SIGNATURE, TS + 10),
            Err(SignInError::BadNonce)
        );
        let forged = MESSAGE.replace(NONCE, "0123456789abcdef00000000000000000000000000000000");
        assert_eq!(
            verify(ADDRESS, &forged, SIGNATURE, TS + 10),
            Err(SignInError::BadNonce)
        );
    }

    #[test]
    fn a_signature_by_someone_else_is_refused() {
        let other = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
        let message = sign_in_message("www.lodestar-dashboard.com", other, NONCE, TS).unwrap();
        assert_eq!(
            verify(other, &message, SIGNATURE, TS + 10),
            Err(SignInError::WrongSigner)
        );
        // Nor can the address be swapped under somebody else's message.
        assert_eq!(
            verify(other, MESSAGE, SIGNATURE, TS + 10),
            Err(SignInError::NotOurMessage)
        );
    }

    #[test]
    fn a_signature_outside_the_five_minute_window_is_refused() {
        assert_eq!(
            verify(ADDRESS, MESSAGE, SIGNATURE, TS + AUTH_WINDOW_SECS + 1),
            Err(SignInError::StaleMessage)
        );
        assert_eq!(
            verify(ADDRESS, MESSAGE, SIGNATURE, TS - AUTH_WINDOW_SECS - 1),
            Err(SignInError::StaleMessage)
        );
        assert!(verify(ADDRESS, MESSAGE, SIGNATURE, TS + AUTH_WINDOW_SECS).is_ok());
    }

    #[test]
    fn a_sign_in_is_spent_once() {
        let spent = SpentNonces::default();
        let signed = verify_sign_in(&secret(), ADDRESS, MESSAGE, SIGNATURE, TS + 10).unwrap();
        assert!(spent.spend(&signed, TS + 10));
        assert!(!spent.spend(&signed, TS + 20), "a replay inside the window");
    }

    #[test]
    fn a_fresh_nonce_verifies_and_is_different_each_time() {
        let a = issue_sign_in_nonce(&secret(), TS);
        assert_ne!(a, issue_sign_in_nonce(&secret(), TS));
        assert_eq!(a.len(), 48);
        assert_eq!(nonce_from(&secret(), &a[..16], TS), a);
    }

    #[test]
    fn rubbish_is_refused_rather_than_panicking() {
        for m in [
            "",
            "no timestamp here",
            "x wants you to sign in with your Ethereum account:\n",
        ] {
            assert_eq!(
                verify(ADDRESS, m, SIGNATURE, TS),
                Err(SignInError::NotOurMessage),
                "{m}"
            );
        }
        let short = MESSAGE.replace(NONCE, "abc");
        assert_eq!(
            verify(ADDRESS, &short, SIGNATURE, TS + 10),
            Err(SignInError::BadNonce)
        );
        assert_eq!(
            verify(ADDRESS, MESSAGE, "0xnothex", TS + 10),
            Err(SignInError::BadSignature)
        );
        assert_eq!(
            verify("not-an-address", MESSAGE, SIGNATURE, TS + 10),
            Err(SignInError::BadAddress)
        );
    }
}
