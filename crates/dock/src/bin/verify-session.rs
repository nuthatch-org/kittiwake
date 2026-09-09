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

use kittiwake_dock::{Secret, SessionError, parse_session};

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
    let now = chrono_now();
    let mut failed = false;

    // 1. It parses.
    match parse_session(&secret, &token, now) {
        Ok(address) => println!("  ok    the token parses, and vouches for {address}"),
        Err(SessionError::Expired) => {
            // Worth its own line: an expired token proves the signature was accepted, which is the
            // compatibility question, but it cannot prove the address. Capture a fresher one.
            println!("  ??    the signature was accepted and the session has expired.");
            println!("        That answers the compatibility question and not the address one;");
            println!("        sign in again and re-run with a fresh cookie.");
        }
        Err(e) => {
            println!("  FAIL  the token does not parse: {e}");
            println!("        This is the failure #12 exists to catch. Every logged-in user");
            println!("        would be signed out at the cutover. Do not switch.");
            failed = true;
        }
    }

    // 2. One byte of the signature, changed.
    let sig_at = token.rfind(':').map(|i| i + 1);
    match sig_at.and_then(|i| flip_hex_at(&token, i)) {
        Some(forged) => match parse_session(&secret, &forged, now) {
            Err(SessionError::BadSignature) => {
                println!("  ok    one byte of the signature changed, and it is rejected")
            }
            other => {
                println!("  FAIL  a forged signature was not rejected: {other:?}");
                failed = true;
            }
        },
        None => {
            println!("  FAIL  the token has no hex signature to mutate; is it a session cookie?");
            failed = true;
        }
    }

    // 3. One byte of the payload, changed.
    match flip_hex_at(&token, 2) {
        Some(tampered) if tampered != token => match parse_session(&secret, &tampered, now) {
            Err(SessionError::BadSignature) => {
                println!("  ok    one byte of the address changed, and it is rejected")
            }
            other => {
                println!("  FAIL  a tampered payload was not rejected: {other:?}");
                failed = true;
            }
        },
        _ => println!("  --    the address could not be mutated in place; skipped"),
    }

    if failed {
        println!("\nnot compatible. The Dock must not be cut over.");
        std::process::ExitCode::FAILURE
    } else {
        println!("\ncompatible. #12 is answered for this token.");
        std::process::ExitCode::SUCCESS
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
    use super::flip_hex_at;
    use kittiwake_dock::{Secret, SessionError, create_session, parse_session};

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

    /// The three assertions the binary makes, against a token this process minted. Proves the
    /// procedure is sound, so a real token failing means the token disagrees with us rather than
    /// that the check is broken.
    #[test]
    fn the_procedure_passes_against_a_token_we_minted_ourselves() {
        let secret = Secret::new("a-secret-long-enough-to-be-accepted-here").expect("secret");
        let now = 1_800_000_000;
        let token = create_session(&secret, "0xAbC0000000000000000000000000000000000001", now);

        assert_eq!(
            parse_session(&secret, &token, now).as_deref(),
            Ok("0xabc0000000000000000000000000000000000001")
        );

        let sig_at = token.rfind(':').unwrap() + 1;
        let forged = flip_hex_at(&token, sig_at).expect("a hex signature to mutate");
        assert_eq!(
            parse_session(&secret, &forged, now),
            Err(SessionError::BadSignature)
        );

        let tampered = flip_hex_at(&token, 2).expect("a hex address to mutate");
        assert_ne!(tampered, token);
        assert_eq!(
            parse_session(&secret, &tampered, now),
            Err(SessionError::BadSignature)
        );
    }
}
