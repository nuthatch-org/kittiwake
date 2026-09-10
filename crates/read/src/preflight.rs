//! Rehearsing the operator sequence for somebody, before they spend anything.
//!
//! The written guidance says to rehearse on a fork, which is right and asks a prospective operator
//! to install Foundry and write a Solidity test before they have decided whether they care. Most of
//! what a fork rehearsal tells you is readable straight off Arbitrum with `eth_call`, for free,
//! given only an address. No wallet, no signature, no gas: the address need not be one you control
//! or have funded.
//!
//! **The first check is the one that earns its place.** Several Horizon addresses in circulation
//! are implementations rather than proxies, and calling an implementation does not revert. The
//! usual telling is that uninitialised storage makes views return zero, which is the benign half:
//! a zero is obviously wrong and gets caught. The stray `RPCDataService` implementation is
//! initialised, and returns the same thawing range, verifier cut and owner as the live proxy with
//! a minimum provision of 10,000 GRT where the proxy says 555. Four answers right, one eighteenfold
//! wrong, and nothing says so. The EIP-1967 slot is the definitive test. #67

use serde::Serialize;

use crate::census::RequirementsJson;

/// `bytes32(uint256(keccak256('eip1967.proxy.implementation')) - 1)`.
pub const EIP1967_IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

const WEI_PER_GRT: u128 = 1_000_000_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Already true. Nothing to do.
    Done,
    /// Not true yet, and doing it is the ordinary next move.
    Todo,
    /// Not true, and something has to change before it can be.
    Blocked,
    /// Could not be established. Never conflated with fine.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub key: &'static str,
    pub title: String,
    pub status: Status,
    pub detail: String,
}

/// What the chain says about one address against one service.
#[derive(Debug, Clone)]
pub struct Reads {
    pub grt_balance: u128,
    pub stake: u128,
    pub idle_stake: u128,
    pub provision_tokens: u128,
    pub provision_thawing_seconds: u64,
    /// Whether the registry currently lists this address as a provider.
    pub registered: bool,
    /// `None` when the service's implementation slot is empty, which is the silent trap.
    pub service_implementation: Option<String>,
    pub requirements: Option<RequirementsJson>,
}

/// The last twenty bytes of the slot, or nothing when it holds none.
///
/// Its own function because it is the whole silent-trap detector, and a detector that cannot be
/// tested without a chain is a detector nobody checks.
pub fn implementation_from_slot(slot: Option<&str>) -> Option<String> {
    let slot = slot?.strip_prefix("0x")?;
    if slot.len() != 64 || !slot.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let address = format!("0x{}", slot[24..].to_lowercase());
    (address != "0x0000000000000000000000000000000000000000").then_some(address)
}

/// GRT for display, to two decimals, grouped the way the page prints it.
fn grt(wei: u128) -> String {
    let whole = wei / WEI_PER_GRT;
    let hundredths = (wei % WEI_PER_GRT) / (WEI_PER_GRT / 100);
    let grouped = {
        let digits = whole.to_string();
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        out
    };
    if hundredths == 0 {
        format!("{grouped} GRT")
    } else {
        format!("{grouped}.{:02} GRT", hundredths)
            .replace("0 GRT", " GRT")
            .replace("  ", " ")
    }
}

fn step(key: &'static str, title: &str, status: Status, detail: String) -> Step {
    Step {
        key,
        title: title.to_string(),
        status,
        detail,
    }
}

/// Turn the reads into an ordered checklist.
///
/// Pure, so the whole thing is testable without a chain and the wording can be argued over without
/// anybody reproducing an on-chain state to see it.
pub fn preflight(i: &Reads) -> Vec<Step> {
    let mut steps = Vec::new();
    let minimum = i
        .requirements
        .as_ref()
        .map(|r| r.min_tokens_grt * WEI_PER_GRT);

    // 1. The silent trap, checked first because everything after it is meaningless if it fires.
    steps.push(match &i.service_implementation {
        None => step(
            "proxy",
            "The service address is not a proxy",
            Status::Blocked,
            "Its EIP-1967 implementation slot is empty, so this is either an implementation \
             contract or not an upgradeable proxy at all, and calling one does not revert. If it \
             was never initialised its views return zero forever. If it was initialised, and one \
             of these is, they return stale values that look entirely plausible: the stray \
             RPCDataService implementation reports a minimum provision of 10,000 GRT where the \
             live proxy says 555, and agrees with it on everything else. Resolve the address from \
             the Controller before going further."
                .into(),
        ),
        Some(implementation) => step(
            "proxy",
            "The service address is a proxy",
            Status::Done,
            format!("Implementation {implementation}. Calls will reach initialised storage."),
        ),
    });

    // 2. Can they afford it at all? Stake already down counts, so this is not a wallet balance.
    let total = i.grt_balance + i.stake;
    steps.push(match minimum {
        None => step(
            "funds",
            "Minimum provision",
            Status::Unknown,
            "The service did not answer its own requirements, so there is nothing to compare to."
                .into(),
        ),
        Some(0) => step(
            "funds",
            "No minimum provision",
            Status::Done,
            "This service sets no token floor. That is worth reading twice: a floor of zero may be \
             a deliberate choice or a parameter nobody set, and a provision is stake at risk under \
             the service's slashing terms rather than a fee."
                .into(),
        ),
        Some(min) if total >= min => step(
            "funds",
            "Enough GRT for the minimum provision",
            Status::Done,
            format!(
                "Needs {}; this address holds {} and has {} staked.",
                grt(min),
                grt(i.grt_balance),
                grt(i.stake)
            ),
        ),
        Some(min) => step(
            "funds",
            "Not enough GRT yet",
            Status::Blocked,
            format!(
                "Needs {} and this address has {} between wallet and stake.",
                grt(min),
                grt(total)
            ),
        ),
    });

    // 3. Stake, which is the step people skip because provisioning sounds like it includes it.
    steps.push(if i.stake > 0 {
        step(
            "stake",
            "Staked",
            Status::Done,
            format!(
                "{} staked, of which {} is idle and available to provision.",
                grt(i.stake),
                grt(i.idle_stake)
            ),
        )
    } else {
        step(
            "stake",
            "Stake first",
            Status::Todo,
            "Approve GRT to HorizonStaking, then stakeTo(you, tokens). Provisioning moves idle \
             stake to a service; it does not stake for you."
                .into(),
        )
    });

    // 4. The provision itself, and whether its terms actually satisfy the service.
    if i.provision_tokens == 0 {
        steps.push(step(
            "provision",
            "Provision to this service",
            if i.stake > 0 && i.idle_stake > 0 {
                Status::Todo
            } else {
                Status::Blocked
            },
            match &i.requirements {
                Some(r) => format!(
                    "provision(you, service, tokens, maxVerifierCut, thawingPeriod) with at least \
                     {} GRT and a thawing period between {} and {} days. The floor is the \
                     service's and the ceiling is the protocol's, so neither is negotiable.",
                    r.min_tokens_grt, r.min_thawing_days, r.max_thawing_days
                ),
                None => "provision(you, service, tokens, maxVerifierCut, thawingPeriod).".into(),
            },
        ));
    } else {
        let thaw_days = i.provision_thawing_seconds as f64 / 86_400.0;
        let below_floor = i.requirements.as_ref().is_some_and(|r| {
            i.provision_tokens < r.min_tokens_grt * WEI_PER_GRT || thaw_days < r.min_thawing_days
        });
        let mut detail = format!(
            "{} at a {thaw_days}-day thawing period.",
            grt(i.provision_tokens)
        );
        if let (true, Some(r)) = (below_floor, i.requirements.as_ref()) {
            detail.push_str(&format!(
                " The service requires at least {} GRT and {} days, so register will be refused \
                 until this is raised.",
                r.min_tokens_grt, r.min_thawing_days
            ));
        }
        steps.push(step(
            "provision",
            if below_floor {
                "Provisioned, below what the service accepts"
            } else {
                "Provisioned"
            },
            if below_floor {
                Status::Blocked
            } else {
                Status::Done
            },
            detail,
        ));
    }

    // 5. Registration, last, because it is the step that fails for reasons set up earlier.
    steps.push(if i.registered {
        step(
            "register",
            "Registered with the service",
            Status::Done,
            "The registry lists this address. Whether the endpoint it advertises answers is a \
             separate question, and the one that actually matters."
                .into(),
        )
    } else {
        step(
            "register",
            "Register, then start the service",
            if i.provision_tokens > 0 {
                Status::Todo
            } else {
                Status::Blocked
            },
            "register(you, abi.encode(endpoint, geoHash, paymentsDestination)), then startService \
             for whatever the service is scoped to. The collector checks your provision is \
             non-zero before paying anybody, so registering without one leaves you unpayable."
                .into(),
        )
    });

    steps
}

/// The one-line answer, for somebody who will not read five rows.
///
/// **Reads the checklist in order and reports the first step that is not done**, rather than
/// hunting for the first blocker anywhere in it. The steps are a sequence, so a later one is
/// routinely blocked *by* an earlier one that is merely outstanding: an address holding GRT and no
/// stake has a blocked provision step, and telling that person "blocked: provision to this
/// service" points them at the wrong thing entirely. The next move is to stake.
pub fn verdict(steps: &[Step]) -> String {
    let Some(next) = steps.iter().find(|s| s.status != Status::Done) else {
        return "Everything on chain is in place.".into();
    };
    let title = next.title.to_lowercase();
    match next.status {
        Status::Unknown => format!("Could not check: {title}."),
        Status::Blocked => format!("Blocked: {title}."),
        _ => format!("Next: {title}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirements(min_grt: u128, min_days: f64) -> RequirementsJson {
        crate::census::interpret(
            min_grt * WEI_PER_GRT,
            None,
            (min_days * 86_400.0) as u64,
            None,
            0,
            1_000_000,
        )
    }

    fn reads() -> Reads {
        Reads {
            grt_balance: 0,
            stake: 0,
            idle_stake: 0,
            provision_tokens: 0,
            provision_thawing_seconds: 0,
            registered: false,
            service_implementation: Some("0xabc".into()),
            requirements: Some(requirements(555, 14.0)),
        }
    }

    /// The silent trap. An empty slot means calls reach an implementation, which does not revert
    /// and may answer plausibly and wrongly.
    #[test]
    fn an_empty_implementation_slot_is_the_first_and_blocking_answer() {
        let mut r = reads();
        r.service_implementation = None;
        let steps = preflight(&r);
        assert_eq!(steps[0].key, "proxy");
        assert_eq!(steps[0].status, Status::Blocked);
        assert!(
            steps[0].detail.contains("10,000 GRT"),
            "the measured figure is named"
        );
        assert_eq!(
            verdict(&steps),
            "Blocked: the service address is not a proxy."
        );
    }

    #[test]
    fn the_slot_reader_takes_the_last_twenty_bytes_or_nothing() {
        assert_eq!(
            implementation_from_slot(Some(
                "0x000000000000000000000000a983b1234567890123456789012345678901bcde"
            ))
            .as_deref(),
            Some("0xa983b1234567890123456789012345678901bcde")
        );
        // All zeroes is an empty slot, not the zero address as an implementation.
        assert_eq!(
            implementation_from_slot(Some(&format!("0x{}", "0".repeat(64)))),
            None
        );
        assert_eq!(implementation_from_slot(None), None);
        assert_eq!(implementation_from_slot(Some("0x")), None);
        assert_eq!(implementation_from_slot(Some("not hex")), None);
        assert_eq!(implementation_from_slot(Some("0xabc")), None);
    }

    /// The reason the verdict walks in order. An address with GRT and no stake has a blocked
    /// provision step, and naming that points them at the wrong thing: the next move is to stake.
    #[test]
    fn the_verdict_names_the_next_step_rather_than_the_first_blocker() {
        let mut r = reads();
        r.grt_balance = 1000 * WEI_PER_GRT;
        let steps = preflight(&r);

        assert_eq!(steps[2].key, "stake");
        assert_eq!(steps[2].status, Status::Todo);
        assert_eq!(steps[3].key, "provision");
        assert_eq!(
            steps[3].status,
            Status::Blocked,
            "the provision is genuinely blocked by having no stake"
        );
        assert_eq!(
            verdict(&steps),
            "Next: stake first.",
            "and the answer is the outstanding step, not the downstream blocker"
        );
    }

    #[test]
    fn stake_already_down_counts_towards_the_minimum() {
        let mut r = reads();
        r.grt_balance = 300 * WEI_PER_GRT;
        r.stake = 300 * WEI_PER_GRT;
        let steps = preflight(&r);
        assert_eq!(
            steps[1].status,
            Status::Done,
            "600 between wallet and stake clears 555"
        );
        assert!(steps[1].detail.contains("555 GRT"));
    }

    #[test]
    fn not_enough_between_wallet_and_stake_is_blocked_and_says_the_total() {
        let mut r = reads();
        r.grt_balance = 100 * WEI_PER_GRT;
        let steps = preflight(&r);
        assert_eq!(steps[1].status, Status::Blocked);
        assert!(steps[1].detail.contains("100 GRT"), "{}", steps[1].detail);
        assert_eq!(verdict(&steps), "Blocked: not enough grt yet.");
    }

    /// A provision that exists but sits under the service's floor is blocked, not done: register
    /// will be refused, and saying "provisioned" would be the misdirection this module avoids.
    #[test]
    fn a_provision_below_the_floor_is_blocked_rather_than_done() {
        let mut r = reads();
        r.stake = 1000 * WEI_PER_GRT;
        r.idle_stake = 500 * WEI_PER_GRT;
        r.provision_tokens = 100 * WEI_PER_GRT;
        r.provision_thawing_seconds = 14 * 86_400;
        let steps = preflight(&r);
        assert_eq!(steps[3].status, Status::Blocked);
        assert!(steps[3].title.contains("below what the service accepts"));

        // A thawing period under the floor is equally refused, whatever the tokens.
        r.provision_tokens = 1000 * WEI_PER_GRT;
        r.provision_thawing_seconds = 13 * 86_400;
        assert_eq!(
            preflight(&r)[3].status,
            Status::Blocked,
            "13 days is under the 14-day floor"
        );
    }

    /// A service that did not answer is unknown, never fine.
    #[test]
    fn requirements_we_could_not_read_are_unknown_and_not_satisfied() {
        let mut r = reads();
        r.requirements = None;
        r.grt_balance = 10_000 * WEI_PER_GRT;
        let steps = preflight(&r);
        assert_eq!(steps[1].status, Status::Unknown);
        assert_eq!(verdict(&steps), "Could not check: minimum provision.");
    }

    #[test]
    fn everything_in_place_says_so() {
        let r = Reads {
            grt_balance: 0,
            stake: 1000 * WEI_PER_GRT,
            idle_stake: 100 * WEI_PER_GRT,
            provision_tokens: 555 * WEI_PER_GRT,
            provision_thawing_seconds: 14 * 86_400,
            registered: true,
            service_implementation: Some("0xabc".into()),
            requirements: Some(requirements(555, 14.0)),
        };
        let steps = preflight(&r);
        assert!(steps.iter().all(|s| s.status == Status::Done), "{steps:#?}");
        assert_eq!(verdict(&steps), "Everything on chain is in place.");
    }

    #[test]
    fn a_zero_floor_is_reported_rather_than_advertised_as_free() {
        let mut r = reads();
        r.requirements = Some(requirements(0, 0.0));
        let steps = preflight(&r);
        assert_eq!(steps[1].status, Status::Done);
        assert!(steps[1].detail.contains("worth reading twice"));
    }
}
