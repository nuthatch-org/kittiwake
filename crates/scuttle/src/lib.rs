//! Scuttlebutt: an anonymous room, rewritten rather than ported.
//!
//! The Node implementation is shaped around Node's concurrency - a stream per connection, with
//! moderation and fan-out arranged to fit, and Redis carrying state between serverless invocations
//! that could not otherwise see each other. Translating that line by line would carry the shape
//! across without carrying any of the reason for it. #17
//!
//! Here it is one process. The fan-out is a [`tokio::sync::broadcast`] channel; the flood guard is
//! a map behind a mutex rather than a round trip to Redis, because there is no second instance for
//! it to coordinate with. What genuinely outlives a restart - messages and bans - is in Postgres,
//! and what does not needs no store at all.
//!
//! This is the half with no I/O in it: identity, moderation and fan-out. The routes and the SSE
//! stream sit on top.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The longest display name kept. Longer is truncated rather than refused: a name is decoration
/// and refusing the whole message over one is a poor trade.
pub const MAX_NAME: usize = 40;

/// The longest message body accepted.
pub const MAX_BODY: usize = 2000;

/// How many messages the history endpoint returns by default.
pub const HISTORY_LIMIT: i64 = 50;

fn hmac_hex(key: &str, data: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(key.as_bytes()).expect("hmac takes a key of any length");
    mac.update(data.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ---------------------------------------------------------------------------------------------
// Identity, without accounts
// ---------------------------------------------------------------------------------------------

/// A tripcode: proof that two messages came from the same person, without either of them saying
/// who that is.
///
/// A poster types `Name#secret`. The part after the first `#` is theirs alone and becomes a short
/// deterministic code. The same secret always yields the same trip, so continuity is provable; the
/// secret cannot be recovered from the trip.
///
/// The salt is a server secret and without it the codes are guessable - anyone could compute the
/// trip for a name they wanted to impersonate. An empty salt is tolerated for local work, where
/// determinism still holds and secrecy does not, and [`Salt::is_secret`] says which you have.
#[derive(Clone)]
pub struct Salt(String);

impl Salt {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// Whether this salt is actually secret. An empty one makes every tripcode forgeable, which is
    /// tolerable locally and worth refusing to run with in production.
    pub fn is_secret(&self) -> bool {
        !self.0.is_empty()
    }
}

/// Never print the salt, not even by accident in a debug log of the config.
impl std::fmt::Debug for Salt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Salt(<{} bytes>)", self.0.len())
    }
}

pub fn make_tripcode(salt: &Salt, secret: &str) -> String {
    use base64::Engine;
    let mut mac =
        HmacSha256::new_from_slice(salt.0.as_bytes()).expect("hmac takes a key of any length");
    mac.update(secret.as_bytes());
    let digest = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    // Alphanumerics only and ten of them, so a trip is short enough to recognise at a glance and
    // cannot contain a character that means something in the markup it is rendered into.
    let code: String = digest
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect();
    format!("!{code}")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedName {
    /// The display name with the secret stripped. `None` means Anonymous.
    pub name: Option<String>,
    /// `!a8Df2xQ1z`, or `None` when no secret was given.
    pub tripcode: Option<String>,
}

/// Split a raw name field into a display name and an optional tripcode.
///
/// Splits on the **first** `#`, so a secret may contain one. `"#secret"` is anonymous with a trip,
/// which is the interesting case: it lets somebody be nobody in particular and still consistently
/// the same nobody.
pub fn parse_name(salt: &Salt, raw: Option<&str>) -> ParsedName {
    let Some(raw) = raw else {
        return ParsedName::default();
    };
    let trimmed = raw.trim();
    let Some(hash_at) = trimmed.find('#') else {
        let name = truncate(trimmed, MAX_NAME).trim().to_string();
        return ParsedName {
            name: (!name.is_empty()).then_some(name),
            tripcode: None,
        };
    };
    let name = truncate(trimmed[..hash_at].trim(), MAX_NAME)
        .trim()
        .to_string();
    let secret = &trimmed[hash_at + 1..];
    ParsedName {
        name: (!name.is_empty()).then_some(name),
        tripcode: (!secret.is_empty()).then(|| make_tripcode(salt, secret)),
    }
}

/// Truncate on a character boundary. Slicing bytes would panic on the first name with an emoji in
/// it, and people put emoji in names.
fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// An HMAC of the caller's address under a server pepper.
///
/// The raw address is never stored. This is stable enough to ban a repeat offender and useless as
/// personal data if the table ever leaks, which is the whole of the trade.
pub fn hash_ip(pepper: &str, ip: &str) -> String {
    hmac_hex(pepper, ip).chars().take(32).collect()
}

// ---------------------------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------------------------

/// The words masked before a message is stored.
///
/// Small and conservative on purpose. It masks rather than rejects, because rejecting a whole
/// message over one word teaches people to work around the filter, and a speed bump that is
/// obviously a speed bump annoys nobody who was not going to be annoying anyway. Admin delete and
/// ban are the backstop.
const CORE_BLOCKLIST: &[&str] = &[
    "fuck", "shit", "bitch", "cunt", "asshole", "bastard", "dick", "piss",
];

/// Mask blocked words, whole-word and case-insensitively.
///
/// Whole-word matters and is the difference between a filter and a nuisance: without it `piss`
/// masks the middle of a perfectly ordinary word and somebody has to explain why the room cannot
/// discuss a dickensian bastardisation of anything.
///
/// Returns `None` when there is nothing left after trimming.
pub fn clean(body: &str, extra: &[String]) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut word = String::new();

    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let lower = word.to_lowercase();
        let blocked = CORE_BLOCKLIST.iter().any(|w| *w == lower)
            || extra.iter().any(|w| w.eq_ignore_ascii_case(&lower));
        if blocked {
            out.extend(std::iter::repeat_n('*', word.chars().count()));
        } else {
            out.push_str(word);
        }
        word.clear();
    };

    for c in trimmed.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// The flood guard
// ---------------------------------------------------------------------------------------------

/// Minimum spacing between two posts from one address.
const MIN_GAP: std::time::Duration = std::time::Duration::from_secs(2);
/// The burst window, and how many posts it allows.
const WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_IN_WINDOW: u32 = 10;
/// Above this many tracked addresses, the oldest are dropped on the next insert. A chat room is
/// not a reason to grow a map without a ceiling.
const MAX_TRACKED: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flood {
    Ok,
    /// Two posts closer together than a person types.
    TooFast,
    /// Inside the limit per post and over it across the window.
    TooMany,
}

impl Flood {
    /// What the poster is told. Coarser than the edge's rate limit, and tuned for "a human typing
    /// in a chat" rather than "an API being leant on".
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Flood::Ok => None,
            Flood::TooFast => Some("Posting too fast, slow down a touch."),
            Flood::TooMany => Some("Too many messages, take a breather."),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    last: std::time::Instant,
    window_start: std::time::Instant,
    in_window: u32,
}

/// Per-address flood control, in this process.
///
/// The implementation being replaced keeps this in Redis, because on Vercel two requests may land
/// in two isolates that share nothing. Here they land in one process, so the map *is* the shared
/// state and the round trip buys nothing. It also cannot fail open, which the Redis version had to
/// be careful to do.
#[derive(Default)]
pub struct FloodGuard {
    seen: std::sync::Mutex<std::collections::HashMap<String, Bucket>>,
}

impl FloodGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&self, ip_hash: &str) -> Flood {
        self.check_at(ip_hash, std::time::Instant::now())
    }

    /// The clock is a parameter so the thresholds can be tested without sleeping through them.
    pub fn check_at(&self, ip_hash: &str, now: std::time::Instant) -> Flood {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(b) = seen.get_mut(ip_hash) {
            if now.duration_since(b.last) < MIN_GAP {
                // Deliberately not counted towards the window. A refused post is not a post, and
                // counting it would let one impatient person lock themselves out for half a minute.
                return Flood::TooFast;
            }
            if now.duration_since(b.window_start) >= WINDOW {
                b.window_start = now;
                b.in_window = 0;
            }
            if b.in_window >= MAX_IN_WINDOW {
                return Flood::TooMany;
            }
            b.last = now;
            b.in_window += 1;
            return Flood::Ok;
        }

        if seen.len() >= MAX_TRACKED {
            // Drop everything whose window has closed. If that frees nothing the room is under
            // something a flood guard is not the right answer to, and the edge's rate limit is.
            seen.retain(|_, b| now.duration_since(b.window_start) < WINDOW);
        }
        seen.insert(
            ip_hash.to_string(),
            Bucket {
                last: now,
                window_start: now,
                in_window: 1,
            },
        );
        Flood::Ok
    }
}

// ---------------------------------------------------------------------------------------------
// The fan-out
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: i64,
    pub room: String,
    pub name: Option<String>,
    pub tripcode: Option<String>,
    pub body: String,
    pub created_at: String,
}

/// What a subscriber receives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Message {
        message: Box<Message>,
    },
    Delete {
        id: i64,
    },
    /// **The reader fell behind and messages were dropped.**
    ///
    /// This is the design question #17 raises, answered: a slow reader is *told*. `broadcast` drops
    /// the oldest for a subscriber that cannot keep up and reports how many, and silently swallowing
    /// that is the easy option and the wrong one - the reader would show a conversation with a hole
    /// in it and no indication there was ever a hole. Telling them lets the client refetch history
    /// and know it needs to.
    Gap {
        missed: u64,
    },
}

/// How many messages a slow reader may fall behind before it starts missing them.
///
/// The channel is the only buffer: there is no per-connection queue growing behind somebody's
/// suspended laptop, which is the failure the Node shape invited. 256 is roughly five minutes of a
/// busy room, and past it the reader is told rather than served stale.
pub const CHANNEL_CAPACITY: usize = 256;

/// The room's fan-out. Cheap to clone; every clone feeds the same subscribers.
#[derive(Clone)]
pub struct Room {
    tx: tokio::sync::broadcast::Sender<Event>,
}

impl Default for Room {
    fn default() -> Self {
        Self::new()
    }
}

impl Room {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Publish. A send with no subscribers is not an error: an empty room is the normal state of a
    /// chat room, and treating it as a failure would fill the log with it.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> Subscriber {
        Subscriber {
            rx: self.tx.subscribe(),
        }
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

pub struct Subscriber {
    rx: tokio::sync::broadcast::Receiver<Event>,
}

impl Subscriber {
    /// The next event, or `None` when the room is gone.
    ///
    /// A reader that has fallen behind gets an [`Event::Gap`] carrying the count and then continues
    /// from where the channel now starts, rather than being disconnected. Falling behind is a
    /// normal thing for a browser tab to do.
    pub async fn next(&mut self) -> Option<Event> {
        use tokio::sync::broadcast::error::RecvError;
        match self.rx.recv().await {
            Ok(e) => Some(e),
            Err(RecvError::Lagged(missed)) => Some(Event::Gap { missed }),
            Err(RecvError::Closed) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt() -> Salt {
        Salt::new("a-server-side-salt")
    }

    // -- identity -----------------------------------------------------------------------------

    #[test]
    fn the_same_secret_always_gives_the_same_trip_and_a_different_one_does_not() {
        let a = make_tripcode(&salt(), "hunter2");
        assert_eq!(a, make_tripcode(&salt(), "hunter2"));
        assert_ne!(a, make_tripcode(&salt(), "hunter3"));
        // And the salt is what makes it unguessable: the same secret under another salt is another
        // trip, so nobody can compute the code for a name they want to wear.
        assert_ne!(a, make_tripcode(&Salt::new("another"), "hunter2"));
    }

    /// Ten alphanumerics after a `!`. Short enough to recognise at a glance, and containing nothing
    /// that means something in the markup it is rendered into.
    /// **Byte-identical to the implementation being replaced.** Computed with Node against
    /// `createHmac('sha256', salt).update(secret).digest('base64')`, stripped of non-alphanumerics
    /// and cut to ten, on 2026-09-09.
    ///
    /// This is the one thing in the module that must not drift. A tripcode is somebody's identity
    /// in a room with no accounts, and a port that quietly reissues every regular a new one has
    /// taken their name away without erroring. Same shape of risk as #12's session cookie, against
    /// a credential nobody can reset.
    #[test]
    fn a_tripcode_matches_the_implementation_it_replaces() {
        for (salt, secret, expected) in [
            ("a-server-side-salt", "hunter2", "!4lRzx2AfT6"),
            ("", "hunter2", "!SNF2O4ef2o"),
            // The secret may contain a hash; only the first one splits the name.
            ("a-server-side-salt", "a#b", "!53X7dGBcOa"),
            // Non-ASCII goes in as UTF-8 bytes on both sides.
            ("pepper", "\u{1f426}", "!T5b2oHSmX7"),
        ] {
            assert_eq!(
                make_tripcode(&Salt::new(salt), secret),
                expected,
                "salt {salt:?} secret {secret:?}"
            );
        }
    }

    /// The same, for the address pepper. A ban follows this hash, so a change to it silently
    /// unbans everybody who was banned before the deploy.
    #[test]
    fn an_address_hash_matches_the_implementation_it_replaces() {
        assert_eq!(
            hash_ip("pepper", "1.2.3.4"),
            "f34d6ba7bc16379f13f4d116234e9f62"
        );
    }

    #[test]
    fn a_trip_is_ten_alphanumerics_behind_a_bang() {
        let t = make_tripcode(&salt(), "hunter2");
        assert!(t.starts_with('!'), "{t}");
        assert_eq!(t.len(), 11, "{t}");
        assert!(t[1..].chars().all(|c| c.is_ascii_alphanumeric()), "{t}");
    }

    #[test]
    fn a_name_splits_on_the_first_hash_and_a_secret_may_contain_one() {
        let p = parse_name(&salt(), Some("Pete#hunter2"));
        assert_eq!(p.name.as_deref(), Some("Pete"));
        assert_eq!(p.tripcode, Some(make_tripcode(&salt(), "hunter2")));

        // The secret is everything after the first `#`, including further hashes.
        let q = parse_name(&salt(), Some("Pete#a#b"));
        assert_eq!(q.tripcode, Some(make_tripcode(&salt(), "a#b")));
    }

    /// Anonymous and consistently the same anonymous, which is the point of the whole mechanism.
    #[test]
    fn a_bare_hash_is_anonymous_with_a_trip() {
        let p = parse_name(&salt(), Some("#hunter2"));
        assert!(p.name.is_none());
        assert_eq!(p.tripcode, Some(make_tripcode(&salt(), "hunter2")));
    }

    #[test]
    fn no_name_and_a_trailing_hash_both_mean_anonymous() {
        assert_eq!(parse_name(&salt(), None), ParsedName::default());
        assert_eq!(parse_name(&salt(), Some("   ")), ParsedName::default());
        // A `#` with nothing after it is not a secret.
        assert_eq!(parse_name(&salt(), Some("Pete#")).tripcode, None);
    }

    /// A byte slice would panic here, and people put emoji in names.
    #[test]
    fn a_long_name_is_truncated_on_a_character_boundary() {
        let long = "🐦".repeat(60);
        let p = parse_name(&salt(), Some(&long));
        assert_eq!(p.name.as_deref().map(|n| n.chars().count()), Some(MAX_NAME));
    }

    #[test]
    fn the_salt_knows_whether_it_is_secret_and_never_prints_itself() {
        assert!(salt().is_secret());
        assert!(!Salt::new("").is_secret());
        assert_eq!(format!("{:?}", salt()), "Salt(<18 bytes>)");
        assert!(!format!("{:?}", salt()).contains("server-side"));
    }

    #[test]
    fn an_address_is_hashed_to_a_stable_thirty_two_characters_and_never_kept() {
        let a = hash_ip("pepper", "1.2.3.4");
        assert_eq!(a.len(), 32);
        assert_eq!(a, hash_ip("pepper", "1.2.3.4"));
        assert_ne!(a, hash_ip("pepper", "1.2.3.5"));
        assert_ne!(a, hash_ip("other", "1.2.3.4"));
        assert!(!a.contains("1.2.3.4"));
    }

    // -- moderation ---------------------------------------------------------------------------

    #[test]
    fn a_blocked_word_is_masked_to_its_own_length_and_the_rest_survives() {
        assert_eq!(clean("oh shit", &[]).as_deref(), Some("oh ****"));
        assert_eq!(clean("SHIT", &[]).as_deref(), Some("****"));
    }

    /// Whole-word, and the difference between a filter and a nuisance. Without it the room cannot
    /// discuss Scunthorpe, dickensian anything, or a bastardisation of a word.
    #[test]
    fn a_blocked_word_inside_a_longer_one_is_left_alone() {
        for innocent in ["dickensian", "bastardisation", "Scunthorpe", "assholes"] {
            assert_eq!(
                clean(innocent, &[]).as_deref(),
                Some(innocent),
                "{innocent} was masked"
            );
        }
    }

    #[test]
    fn punctuation_still_bounds_a_word() {
        assert_eq!(clean("well, shit.", &[]).as_deref(), Some("well, ****."));
        assert_eq!(clean("(shit)", &[]).as_deref(), Some("(****)"));
    }

    #[test]
    fn extra_words_can_be_added_without_a_code_change() {
        let extra = vec!["banana".to_string()];
        assert_eq!(clean("a banana", &extra).as_deref(), Some("a ******"));
        assert_eq!(clean("a banana", &[]).as_deref(), Some("a banana"));
    }

    #[test]
    fn an_empty_message_is_refused_rather_than_stored_blank() {
        assert!(clean("   \n ", &[]).is_none());
        assert!(clean("", &[]).is_none());
    }

    // -- the flood guard ----------------------------------------------------------------------

    #[test]
    fn two_posts_closer_than_the_gap_are_too_fast() {
        let g = FloodGuard::new();
        let t = std::time::Instant::now();
        assert_eq!(g.check_at("ip", t), Flood::Ok);
        assert_eq!(
            g.check_at("ip", t + std::time::Duration::from_millis(500)),
            Flood::TooFast
        );
        assert_eq!(g.check_at("ip", t + MIN_GAP), Flood::Ok);
    }

    /// A refused post is not a post. Counting it towards the burst window would let one impatient
    /// person lock themselves out for half a minute by double-clicking.
    #[test]
    fn a_refused_post_does_not_count_towards_the_window() {
        let g = FloodGuard::new();
        let t = std::time::Instant::now();
        assert_eq!(g.check_at("ip", t), Flood::Ok);
        for _ in 0..50 {
            assert_eq!(g.check_at("ip", t), Flood::TooFast);
        }
        // Still nine left in the window, so the next properly spaced post is fine.
        assert_eq!(g.check_at("ip", t + MIN_GAP), Flood::Ok);
    }

    #[test]
    fn the_window_allows_its_count_and_then_refuses_until_it_rolls() {
        let g = FloodGuard::new();
        let t = std::time::Instant::now();
        for i in 0..MAX_IN_WINDOW {
            assert_eq!(g.check_at("ip", t + MIN_GAP * i), Flood::Ok, "post {i}");
        }
        assert_eq!(
            g.check_at("ip", t + MIN_GAP * MAX_IN_WINDOW),
            Flood::TooMany
        );
        // A fresh window, and it is allowed again.
        assert_eq!(g.check_at("ip", t + WINDOW + MIN_GAP), Flood::Ok);
    }

    #[test]
    fn one_persons_flooding_does_not_refuse_anybody_else() {
        let g = FloodGuard::new();
        let t = std::time::Instant::now();
        for i in 0..MAX_IN_WINDOW {
            g.check_at("loud", t + MIN_GAP * i);
        }
        assert_eq!(
            g.check_at("loud", t + MIN_GAP * MAX_IN_WINDOW),
            Flood::TooMany
        );
        assert_eq!(g.check_at("quiet", t + MIN_GAP * MAX_IN_WINDOW), Flood::Ok);
    }

    #[test]
    fn every_refusal_has_something_to_say_and_success_has_nothing() {
        assert!(Flood::Ok.reason().is_none());
        assert!(Flood::TooFast.reason().is_some());
        assert!(Flood::TooMany.reason().is_some());
    }

    // -- the fan-out --------------------------------------------------------------------------

    fn msg(id: i64) -> Event {
        Event::Message {
            message: Box::new(Message {
                id,
                room: "main".into(),
                name: None,
                tripcode: None,
                body: format!("m{id}"),
                created_at: "2026-09-09T00:00:00Z".into(),
            }),
        }
    }

    #[tokio::test]
    async fn every_subscriber_gets_every_message() {
        let room = Room::new();
        let mut a = room.subscribe();
        let mut b = room.subscribe();
        assert_eq!(room.subscribers(), 2);

        room.publish(msg(1));
        assert_eq!(a.next().await, Some(msg(1)));
        assert_eq!(b.next().await, Some(msg(1)));
    }

    /// An empty room is the normal state of a chat room, not a failure to be logged.
    #[test]
    fn publishing_to_nobody_is_not_an_error() {
        Room::new().publish(msg(1));
    }

    /// **The design question, answered.** A reader that falls behind is told how much it missed
    /// rather than shown a conversation with a silent hole in it.
    #[tokio::test]
    async fn a_reader_that_falls_behind_is_told_how_much_it_missed() {
        let room = Room::new();
        let mut slow = room.subscribe();
        for i in 0..(CHANNEL_CAPACITY as i64 + 10) {
            room.publish(msg(i));
        }
        match slow.next().await {
            Some(Event::Gap { missed }) => assert!(missed >= 10, "missed {missed}"),
            other => panic!("expected a gap, got {other:?}"),
        }
        // And it carries on from where the channel now begins rather than being disconnected.
        assert!(matches!(slow.next().await, Some(Event::Message { .. })));
    }

    #[tokio::test]
    async fn a_deletion_reaches_the_room_as_its_own_event() {
        let room = Room::new();
        let mut s = room.subscribe();
        room.publish(Event::Delete { id: 7 });
        assert_eq!(s.next().await, Some(Event::Delete { id: 7 }));
    }

    #[tokio::test]
    async fn a_closed_room_ends_the_stream_rather_than_hanging() {
        let room = Room::new();
        let mut s = room.subscribe();
        drop(room);
        assert_eq!(s.next().await, None);
    }

    /// The wire tag, pinned. The client switches on it, and renaming a variant would be a silent
    /// break rather than a compile error.
    #[test]
    fn the_events_serialise_as_the_client_reads_them() {
        let j = |e: &Event| serde_json::to_value(e).unwrap();
        assert_eq!(j(&Event::Delete { id: 3 })["type"], "delete");
        assert_eq!(j(&Event::Gap { missed: 4 })["type"], "gap");
        assert_eq!(j(&Event::Gap { missed: 4 })["missed"], 4);
        assert_eq!(j(&msg(1))["type"], "message");
        assert_eq!(j(&msg(1))["message"]["id"], 1);
    }
}
