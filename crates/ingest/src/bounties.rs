//! Bounty reconciliation: the chain is the truth, `sync_bounties` is only a cache.
//!
//! A bounty's state can change without anything touching the Dock. An indexer claims through
//! Arbiscan, a developer cancels directly, anyone refunds one that has expired. The UI's own PATCH
//! is best-effort and needs a studio session, which indexers generally do not have, so a claim
//! frequently never reaches the database at all. This job reads the chain for every non-terminal
//! bounty and brings the cache back into line.
//!
//! The seventeenth cron, and the last one outside the product layer. It reads the chain and writes
//! one table; there is no signing key and nothing is spent, which is the whole difference between
//! this and `tap-provision` (#11).

use kittiwake_chain::{Chain, ChainBounty};
use kittiwake_db::Pool;

use crate::{IngestError, Ingested};

/// A cached bounty's status. The four the Dock writes and the UI renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Claimed,
    Cancelled,
    Expired,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Claimed => "claimed",
            Status::Cancelled => "cancelled",
            Status::Expired => "expired",
        }
    }
}

const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// What a bounty's cached status *should* be, given what the chain says.
///
/// Always returns the canonical answer; the caller diffs it against the stored row. Deriving the
/// desired state without reference to the current one is what makes this exhaustively testable,
/// and it is how the implementation being replaced was arranged too.
///
/// The one subtlety is that `cancel()` and `refundExpired()` are indistinguishable on-chain: both
/// settle the bounty and leave the winner as the zero address. Whether it had passed its expiry is
/// the only thing separating them, so that is what the cache records.
pub fn desired_status(chain: &ChainBounty, now: i64) -> (Status, Option<String>) {
    let past_expiry = chain.expires_at > 0 && now > chain.expires_at as i64;
    let winner = chain.winner.to_lowercase();
    let has_winner = winner != ZERO_ADDRESS;

    if chain.settled {
        if has_winner {
            return (Status::Claimed, Some(winner));
        }
        return (
            if past_expiry {
                Status::Expired
            } else {
                Status::Cancelled
            },
            None,
        );
    }

    // Unsettled but past its expiry: `claim()` would revert, so it is no longer claimable even
    // though nobody has settled it. The developer can still call `refundExpired()`.
    if past_expiry {
        return (Status::Expired, None);
    }

    (Status::Open, None)
}

/// Whether the stored row disagrees with the chain.
///
/// The winner is compared as well as the status, and not only for tidiness: the UI's optimistic
/// PATCH records whichever address held the studio session, which is the developer's rather than
/// the winning indexer's. A row can therefore be `claimed` and still be wrong about by whom.
pub fn needs_update(
    stored_status: &str,
    stored_claimed_by: Option<&str>,
    desired: (Status, Option<String>),
) -> bool {
    let (status, claimed_by) = desired;
    if stored_status != status.as_str() {
        return true;
    }
    if status == Status::Claimed {
        let stored = stored_claimed_by.unwrap_or_default().to_lowercase();
        return stored != claimed_by.unwrap_or_default();
    }
    false
}

/// Read every reconcilable bounty from the chain and write back the ones that have moved.
///
/// A bounty whose `getBounty` call fails is skipped rather than failing the run. The board holds
/// many bounties and one unreadable id must not stop the other twenty being corrected; the next
/// pass tries it again in ten minutes.
pub async fn reconcile(
    pool: &Pool,
    chain: &Chain,
    board: &str,
    now: i64,
) -> Result<Ingested, IngestError> {
    let rows = kittiwake_db::reconcilable_bounties(pool).await?;
    let scanned = rows.len();
    let mut changed = 0u64;
    let mut unreadable = 0u64;

    for (id, chain_bounty_id, status, claimed_by) in rows {
        let Some(on_chain_id) = chain_bounty_id.trim().parse::<u64>().ok() else {
            // A row that claims a chain id we cannot parse is a data fault, not a transient one,
            // so it is worth a line each pass rather than a silent skip.
            tracing::warn!(
                bounty = id,
                chain_bounty_id = %chain_bounty_id,
                "bounty has an unparseable chain id; skipping"
            );
            unreadable += 1;
            continue;
        };
        let state = match chain.bounty(board, on_chain_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(bounty = id, chain_bounty_id = on_chain_id, error = %e, "getBounty failed");
                unreadable += 1;
                continue;
            }
        };
        let desired = desired_status(&state, now);
        if needs_update(&status, claimed_by.as_deref(), desired.clone()) {
            let (want, winner) = desired;
            kittiwake_db::update_bounty_status(pool, id, want.as_str(), winner.as_deref()).await?;
            tracing::info!(bounty = id, from = %status, to = want.as_str(), "bounty reconciled");
            changed += 1;
        }
    }

    // Registered at zero, so "nothing was unreadable" and "this job has never run" are different
    // lines on the scrape rather than the same absence. The lesson from #40 and #47.
    metrics::counter!("bounty_unreadable_total").increment(0);
    metrics::counter!("bounty_unreadable_total").increment(unreadable);
    metrics::gauge!("bounty_reconcilable").set(scanned as f64);

    Ok(Ingested { rows: changed })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINNER: &str = "0x090F7382F9EA85C733Cd501f4D87F16CB5B83ed3";
    const NOW: i64 = 1_800_000_000;

    fn state(settled: bool, winner: &str, expires_at: u64) -> ChainBounty {
        ChainBounty {
            settled,
            winner: winner.to_string(),
            expires_at,
        }
    }

    #[test]
    fn an_unsettled_bounty_inside_its_window_is_open() {
        let (s, w) = desired_status(&state(false, ZERO_ADDRESS, (NOW + 3600) as u64), NOW);
        assert_eq!(s, Status::Open);
        assert!(w.is_none());
    }

    /// The contract's sentinel for "never expires". Reading it as a timestamp in 1970 would make
    /// every open bounty expired the moment this job first ran.
    #[test]
    fn a_zero_expiry_means_no_expiry_and_not_the_epoch() {
        assert_eq!(
            desired_status(&state(false, ZERO_ADDRESS, 0), NOW).0,
            Status::Open
        );
        assert_eq!(
            desired_status(&state(true, ZERO_ADDRESS, 0), NOW).0,
            Status::Cancelled
        );
    }

    #[test]
    fn a_settled_bounty_with_a_winner_is_claimed_by_them_in_lowercase() {
        let (s, w) = desired_status(&state(true, WINNER, (NOW - 10) as u64), NOW);
        assert_eq!(s, Status::Claimed);
        assert_eq!(w.as_deref(), Some(WINNER.to_lowercase().as_str()));
    }

    /// `cancel()` and `refundExpired()` leave identical on-chain state: settled, no winner. The
    /// expiry is the only thing that tells them apart, and the cache records the distinction
    /// because the UI shows it.
    #[test]
    fn cancelled_and_expired_differ_only_by_whether_the_window_had_passed() {
        assert_eq!(
            desired_status(&state(true, ZERO_ADDRESS, (NOW + 3600) as u64), NOW).0,
            Status::Cancelled
        );
        assert_eq!(
            desired_status(&state(true, ZERO_ADDRESS, (NOW - 3600) as u64), NOW).0,
            Status::Expired
        );
    }

    /// Unsettled and past expiry: `claim()` would revert, so it is not open even though nobody has
    /// settled it.
    #[test]
    fn an_unsettled_bounty_past_its_expiry_is_expired_rather_than_open() {
        assert_eq!(
            desired_status(&state(false, ZERO_ADDRESS, (NOW - 1) as u64), NOW).0,
            Status::Expired
        );
    }

    /// Exactly on the boundary is not yet past it. `>` and not `>=`, matching the contract, which
    /// reverts only once `block.timestamp` is strictly beyond `expiresAt`.
    #[test]
    fn the_expiry_boundary_is_exclusive() {
        assert_eq!(
            desired_status(&state(false, ZERO_ADDRESS, NOW as u64), NOW).0,
            Status::Open
        );
        assert_eq!(
            desired_status(&state(false, ZERO_ADDRESS, (NOW - 1) as u64), NOW).0,
            Status::Expired
        );
    }

    #[test]
    fn a_row_that_already_agrees_is_not_rewritten() {
        assert!(!needs_update("open", None, (Status::Open, None)));
        assert!(!needs_update(
            "claimed",
            Some(WINNER),
            (Status::Claimed, Some(WINNER.to_lowercase()))
        ));
    }

    /// The row the optimistic PATCH leaves behind: the right status, the wrong address. It is
    /// `claimed` either way, so a status-only comparison would call it settled and leave the
    /// developer recorded as the indexer who did the work.
    #[test]
    fn a_claim_recorded_against_the_wrong_address_still_needs_correcting() {
        assert!(needs_update(
            "claimed",
            Some("0x1111111111111111111111111111111111111111"),
            (Status::Claimed, Some(WINNER.to_lowercase()))
        ));
        assert!(needs_update(
            "claimed",
            None,
            (Status::Claimed, Some(WINNER.to_lowercase()))
        ));
    }

    /// A stored winner in checksum case against a desired one in lowercase is the same address,
    /// and rewriting the row every ten minutes for ever would be the visible symptom.
    #[test]
    fn the_stored_winner_is_compared_without_regard_to_case() {
        assert!(!needs_update(
            "claimed",
            Some(&WINNER.to_uppercase().replace("0X", "0x")),
            (Status::Claimed, Some(WINNER.to_lowercase()))
        ));
    }

    #[test]
    fn a_status_that_moved_needs_writing() {
        assert!(needs_update("open", None, (Status::Expired, None)));
        assert!(needs_update("open", None, (Status::Cancelled, None)));
        assert!(needs_update(
            "open",
            None,
            (Status::Claimed, Some(WINNER.to_lowercase()))
        ));
    }
}
