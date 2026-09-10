//! Check a real production session cookie against this implementation, before the cutover.
//!
//! ```sh
//! DOCK_SESSION_SECRET='…' DOCK_SESSION_TOKEN='0xabc…:1757…:9f3…' \
//!   cargo run -p kittiwake-dock --bin verify-session
//! ```
//!
//! **Why this is a binary and not a test.** The two values are production credentials, so they
//! cannot live in the repository and the test would have to skip when they are absent. A test that
//! passes when unconfigured is the exact failure this codebase keeps correcting: absent data
//! reading as health. A binary cannot pass quietly - somebody runs it and reads what it says.
//!
//! **Why it must happen before the switch and not after.** The session cookie is stateless and
//! signed. If this implementation differs from the incumbent's in any detail - the field order, the
//! separator, the encoding, how the secret is read - then every logged-in user is silently signed
//! out the moment the rewrite points here. Nothing errors and nobody is told. #12
//!
//! It checks three things, and the third is the one that makes the first mean anything:
//!
//! 1. the real token parses, and yields an address;
//! 2. the same token with **one byte of its signature changed** is rejected as a bad signature -
//!    so the check is testing the signature rather than the parse;
//! 3. the same token with one byte of its **payload** changed is rejected too - so the signature
//!    covers the payload rather than sitting beside it.

use kittiwake_dock::{SESSION_TTL_SECS, Secret, SessionError, parse_session};

/// Flip one hex digit at `at`, wrapping `f` to `0`. A mutation that changes exactly one byte and
/// keeps the token the same length and shape, so what is being tested is the signature and not the
/// parser's tolerance for a short string.
fn flip_hex_at(s: &str, at: usize) -> Option<String> {
    let mut bytes: Vec<char> = s.chars().collect();
    let c = *bytes.get(at)?;
    let next = match c {
        '0'..='8' | 'a'..='e' => char::from_u32(c as u32 + 1)?,
        '9' => 'a',
        'f' => '0',
        _ => return None,
    };
    bytes[at] = next;
    Some(bytes.into_iter().collect())
}

/// The three checks, and the verdict.
///
/// **Every check is judged at a clock taken from the token, not at the wall clock.** A cookie older
/// than [`SESSION_TTL_SECS`] makes `parse_session` return `Expired` before it looks at the
/// signature, so judging at `now` meant a stale cookie produced `FAIL … not compatible. The Dock
/// must not be cut over.` about an implementation that agrees perfectly - and produced exactly the
/// same output for a forged token, so the verdict carried no information at all. Whether the cookie
/// is still live is a fact about the cookie and is reported separately; it is not a compatibility
/// answer either way.
fn check(secret: &Secret, token: &str, now: i64) -> (bool, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut compatible = true;

    let Some(issued_at) = issued_at_of(token) else {
        out.push("  FAIL  the token is not `address:issued_at:signature`.".into());
        out.push("        This is not a session cookie, or it was truncated in transit.".into());
        return (false, out);
    };
    // One second after it was minted: inside the window by construction, so expiry can never
    // stand in for the answer.
    let at = issued_at.saturating_add(1);

    // 1. It parses, and the signature is ours.
    match parse_session(secret, token, at) {
        Ok(address) => out.push(format!(
            "  ok    the signature is ours, and it vouches for {address}"
        )),
        Err(e) => {
            out.push(format!("  FAIL  the signature is not ours: {e}"));
            out.push(
                "        This is the failure #12 exists to catch. Every logged-in user".into(),
            );
            out.push("        would be signed out at the cutover. Do not switch.".into());
            compatible = false;
        }
    }

    // 2. One byte of the signature, changed.
    let sig_at = token.rfind(':').map(|i| i + 1);
    match sig_at.and_then(|i| flip_hex_at(token, i)) {
        Some(forged) => match parse_session(secret, &forged, at) {
            Err(SessionError::BadSignature) => {
                out.push("  ok    one byte of the signature changed, and it is rejected".into())
            }
            other => {
                out.push(format!(
                    "  FAIL  a forged signature was not rejected: {other:?}"
                ));
                compatible = false;
            }
        },
        None => {
            out.push(
                "  FAIL  the token has no hex signature to mutate; is it a session cookie?".into(),
            );
            compatible = false;
        }
    }

    // 3. One byte of the payload, changed.
    match flip_hex_at(token, 2) {
        Some(tampered) if tampered != token => match parse_session(secret, &tampered, at) {
            Err(SessionError::BadSignature) => {
                out.push("  ok    one byte of the address changed, and it is rejected".into())
            }
            other => {
                out.push(format!(
                    "  FAIL  a tampered payload was not rejected: {other:?}"
                ));
                compatible = false;
            }
        },
        _ => out.push("  --    the address could not be mutated in place; skipped".into()),
    }

    // Reported, never judged. A cookie captured last month answers the compatibility question just
    // as well as one captured a minute ago.
    let age = now.saturating_sub(issued_at);
    if age > SESSION_TTL_SECS {
        let days = age / 86_400;
        out.push(format!(
            "  --    for information: this cookie is {days} days old and has expired."
        ));
        out.push("        That says nothing about compatibility and is not counted below.".into());
    }

    (compatible, out)
}

/// The `issued_at` in `address:issued_at:signature`.
fn issued_at_of(token: &str) -> Option<i64> {
    let (payload, _) = token.rsplit_once(':')?;
    let (_, issued) = payload.rsplit_once(':')?;
    issued.parse().ok()
}

fn main() -> std::process::ExitCode {
    let (Ok(secret), Ok(token)) = (
        std::env::var("DOCK_SESSION_SECRET"),
        std::env::var("DOCK_SESSION_TOKEN"),
    ) else {
        eprintln!(
            "set DOCK_SESSION_SECRET and DOCK_SESSION_TOKEN.\n\
             \n\
             The secret is the platform's `SESSION_SECRET`. The token is the value of a real\n\
             `studio_session` cookie, taken from a browser that is signed in - your own will do.\n\
             Neither is stored anywhere by this program and neither may be committed."
        );
        return std::process::ExitCode::from(2);
    };

    let secret = match Secret::new(&secret) {
        Ok(s) => s,
        Err(why) => {
            eprintln!("the secret is not usable: {why}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let (compatible, lines) = check(&secret, &token, chrono_now());
    for line in lines {
        println!("{line}");
    }

    if compatible {
        println!("\ncompatible. #12 is answered for this token.");
        std::process::ExitCode::SUCCESS
    } else {
        println!("\nnot compatible. The Dock must not be cut over.");
        std::process::ExitCode::FAILURE
    }
}

/// Seconds since the epoch. Its own function so the binary needs no clock dependency.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{check, flip_hex_at};
    use kittiwake_dock::{Secret, create_session};

    /// The checker checked. A mutation helper that quietly returned the original string would make
    /// every run of this binary print three `ok`s and mean nothing.
    #[test]
    fn flipping_a_hex_digit_changes_exactly_one_and_wraps_at_f() {
        assert_eq!(flip_hex_at("abc", 0).as_deref(), Some("bbc"));
        assert_eq!(flip_hex_at("f00", 0).as_deref(), Some("000"));
        assert_eq!(flip_hex_at("9ff", 0).as_deref(), Some("aff"));
        assert_eq!(flip_hex_at("abc", 9), None, "past the end changes nothing");
        assert_eq!(
            flip_hex_at(":ab", 0),
            None,
            "a non-hex character is left alone"
        );
    }

    fn secret() -> Secret {
        Secret::new("a-secret-long-enough-to-be-accepted-here").expect("secret")
    }

    const MINTED_AT: i64 = 1_800_000_000;
    /// Long enough after minting that the cookie is well outside the seven-day window.
    const MUCH_LATER: i64 = MINTED_AT + 400 * 86_400;

    fn token() -> String {
        create_session(
            &secret(),
            "0xAbC0000000000000000000000000000000000001",
            MINTED_AT,
        )
    }

    /// The regression, and the whole reason this file has tests.
    ///
    /// Judged at the wall clock, a cookie older than the TTL made `parse_session` answer `Expired`
    /// before it ever looked at the signature. All three checks then failed, and the binary told
    /// whoever ran it `not compatible. The Dock must not be cut over.` about an implementation
    /// that agrees exactly. The test that was here set `now` to the minting time, so it exercised
    /// a path the binary never takes.
    #[test]
    fn an_expired_but_genuine_cookie_is_compatible() {
        let (compatible, lines) = check(&secret(), &token(), MUCH_LATER);
        assert!(compatible, "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.contains("has expired")),
            "the age is worth saying, just not worth failing over: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("vouches for 0xabc0000000000000000000000000000000000001")),
            "{lines:#?}"
        );
    }

    /// The other half of it. A tool that says "not compatible" for a stale cookie *and* for a
    /// forged one is telling you nothing either time.
    #[test]
    fn a_forged_signature_is_not_compatible_even_when_expired() {
        let sig_at = token().rfind(':').unwrap() + 1;
        let forged = flip_hex_at(&token(), sig_at).expect("a hex signature to mutate");
        let (compatible, lines) = check(&secret(), &forged, MUCH_LATER);
        assert!(!compatible, "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("the signature is not ours")),
            "{lines:#?}"
        );
    }

    #[test]
    fn the_wrong_secret_is_not_compatible() {
        let other = Secret::new("a-different-secret-of-sufficient-length").expect("secret");
        let (compatible, _) = check(&other, &token(), MUCH_LATER);
        assert!(!compatible);
    }

    /// A fresh cookie passes and says nothing about age, so the ordinary run is three lines.
    #[test]
    fn a_fresh_cookie_passes_without_a_word_about_age() {
        let (compatible, lines) = check(&secret(), &token(), MINTED_AT + 60);
        assert!(compatible, "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("expired")), "{lines:#?}");
        assert_eq!(lines.len(), 3, "{lines:#?}");
    }

    /// Something that is not a session cookie is refused by name rather than by three confusing
    /// failures.
    #[test]
    fn something_that_is_not_a_token_is_named_as_such() {
        for bad in ["", "not-a-token", "0xabc:notanumber:deadbeef"] {
            let (compatible, lines) = check(&secret(), bad, MUCH_LATER);
            assert!(!compatible, "{bad:?}");
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("not `address:issued_at:signature`")
                        || l.contains("the signature is not ours")),
                "{bad:?}: {lines:#?}"
            );
        }
    }
}
