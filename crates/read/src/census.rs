//! How many providers has each data service actually got, and do their endpoints answer?
//!
//! Every service in this stack has a provider list that reads "us", and a data service with one
//! provider is a contract address rather than a market. The counts used to be kept by hand and
//! hand-written counts drift: the tracker carried Seahorn at zero while its registry held two.
//!
//! The distinction running through the module: **registered** is a promise and **serving** is
//! evidence. Dispatch had two registered providers and no answering endpoint for 39 days, and
//! nothing we monitored noticed, because everything we monitored was on chain and on-chain state
//! stayed green throughout. #67

use std::collections::BTreeMap;

use serde::Serialize;

/// What a healthy answer looks like, which is not the same question for every service.
///
/// `Paywall` exists because the Nuthatch Data Service's entire product is a paywall: it answers
/// `402 TAP-Receipt header required` when it is working perfectly. A checker treating 2xx as up
/// would mark the one service here that *is* serving as down, and a dead container's cheerful 404
/// as fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Http,
    Paywall,
    JsonRpc,
}

/// One service whose registry follows the register/deregister shape.
pub struct Service {
    /// Matches the id the data-services page joins on.
    pub id: &'static str,
    pub name: &'static str,
    pub address: &'static str,
    /// Table prefix in data-services-nest: `{table}__provider_registered`.
    pub table: &'static str,
    pub probe: Probe,
}

/// Only services whose contract is deployed and which follow that shape. A service missing from
/// here has no registry to read, and reporting "0 providers" about it would be a measurement
/// nobody took.
pub const SERVICES: &[Service] = &[
    Service {
        id: "dispatch",
        name: "Dispatch",
        address: "0x7101d5c1a5c89c3647f5118da118e56c023ba0b9",
        table: "dispatch",
        probe: Probe::JsonRpc,
    },
    Service {
        id: "seahorn",
        name: "Seahorn",
        address: "0xdDE3F913cb6D1332Bc018Eb63647020a87dD7B37",
        table: "seahorn",
        probe: Probe::Http,
    },
    Service {
        id: "sdsce",
        name: "SDSCE",
        address: "0x1c3e9cca124ad19b9ed3c202d2e6cd106944640c",
        table: "sdsce",
        probe: Probe::Http,
    },
    Service {
        id: "nuthatch-data-service",
        name: "Nuthatch Data Service",
        address: "0x647D1Fd14AF2DE3947522B74F1de5B99d317c10A",
        table: "nuthatch_data_service",
        probe: Probe::Paywall,
    },
    Service {
        id: "mainline-firehose",
        name: "Mainline",
        address: "0x12C722149804a8C1Bb5924374e675956315B4456",
        table: "mainline",
        probe: Probe::Http,
    },
];

/// The Subgraph Service, carried as a benchmark rather than a census row: it has no registry of
/// this shape, and it is not ours. It is here because it is the bar everybody assumes applies, and
/// the point of showing what these services cost is that it does not.
pub const SUBGRAPH_SERVICE: &str = "0xb2Bb92d0DE618878E438b55D5846cfecD9301105";

#[derive(Debug, Clone)]
pub struct RegistryEvent {
    pub provider: String,
    pub endpoint: Option<String>,
    pub block_number: u64,
    pub log_index: u64,
    pub registered: bool,
}

/// Every registration and deregistration `service` has emitted, from data-services-nest.
///
/// Read from a nest because the chain read was one `getLogs` from block 0, which only Alchemy served.
pub fn registry_sql(service: &Service) -> String {
    let t = service.table;
    format!(
        "SELECT provider, endpoint, block_number, log_index, true AS registered \
         FROM {t}__provider_registered \
         UNION ALL \
         SELECT provider, NULL AS endpoint, block_number, log_index, false AS registered \
         FROM {t}__provider_deregistered"
    )
}

/// The rows [`registry_sql`] returns. A row missing a field fails the whole read rather than being
/// skipped, because a dropped deregistration would report a departed provider as present.
pub fn registry_events(rows: &[serde_json::Value]) -> Result<Vec<RegistryEvent>, String> {
    let int = |row: &serde_json::Value, key: &str| {
        let v = &row[key];
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| format!("registry row has no integer {key}: {row}"))
    };
    rows.iter()
        .map(|row| {
            Ok(RegistryEvent {
                provider: row["provider"]
                    .as_str()
                    .ok_or_else(|| format!("registry row has no provider: {row}"))?
                    .to_lowercase(),
                endpoint: row["endpoint"].as_str().map(str::to_string),
                block_number: int(row, "block_number")?,
                log_index: int(row, "log_index")?,
                registered: row["registered"]
                    .as_bool()
                    .ok_or_else(|| format!("registry row has no registered flag: {row}"))?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Provider {
    pub address: String,
    pub endpoint: String,
}

/// Which providers are registered right now, from the event log.
///
/// **Last event wins; this is not set subtraction.** On Dispatch, one provider deregistered at
/// block 456,950,409 and registered again ten blocks later. Removing every address that appears in
/// a deregistration would report it gone and the service as having one provider rather than two,
/// which is precisely the wrong answer to the question this exists to settle.
///
/// Checked against Arbitrum One on 2026-09-10: Dispatch has four events and two current providers,
/// which is the case above, and the count agrees with what the incumbent serves.
pub fn current_providers(events: &[RegistryEvent]) -> Vec<Provider> {
    let mut ordered: Vec<&RegistryEvent> = events.iter().collect();
    ordered.sort_by_key(|e| (e.block_number, e.log_index));

    let mut latest: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for event in ordered {
        let key = event.provider.to_lowercase();
        // A deregistration carries no endpoint, so the last one advertised is kept rather than
        // blanked: if the provider registers again without changing it, that is still the address
        // to call.
        let endpoint = event
            .endpoint
            .clone()
            .or_else(|| latest.get(&key).map(|(_, e)| e.clone()))
            .unwrap_or_default();
        latest.insert(key, (event.registered, endpoint));
    }
    latest
        .into_iter()
        .filter(|(_, (registered, _))| *registered)
        .map(|(address, (_, endpoint))| Provider { address, endpoint })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Answered the way this service is supposed to answer.
    Serving,
    /// Answered 402, which for a TAP-gated service is the door working rather than shut.
    Paywalled,
    /// Answered, but not with anything a consumer could use. A dead container still returns 404.
    HttpError,
    Unreachable,
    Timeout,
    /// Registered without advertising anywhere to call.
    NoEndpoint,
    /// Points somewhere we will not fetch. The endpoint is a string somebody put on chain, so a
    /// census that followed it would be an SSRF with a status code for an oracle.
    Refused,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProbe {
    pub address: String,
    pub endpoint: String,
    pub verdict: Verdict,
    pub http_status: Option<u16>,
    pub latency_ms: Option<u64>,
}

/// True when a provider is telling consumers to call something that does not answer.
///
/// Not measuring something is not catching it in a lie, so `NoEndpoint` and `Refused` are both
/// outside this.
pub fn is_lying(p: &ProviderProbe) -> bool {
    !matches!(
        p.verdict,
        Verdict::Serving | Verdict::Paywalled | Verdict::NoEndpoint | Verdict::Refused
    )
}

/// How an HTTP answer reads, given what this service is meant to answer.
///
/// 402 is checked first and only counts as healthy where the service charges. Reading it as a
/// payment demand elsewhere would be reading one from a service with nothing to sell.
pub fn verdict_of(status: u16, kind: Probe) -> Verdict {
    match status {
        402 if kind == Probe::Paywall => Verdict::Paywalled,
        402 => Verdict::HttpError,
        s if (200..400).contains(&s) => Verdict::Serving,
        _ => Verdict::HttpError,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCensus {
    pub id: &'static str,
    pub name: &'static str,
    pub address: &'static str,
    pub registered: usize,
    /// Answering the way this service is supposed to.
    pub serving: usize,
    /// Registered, advertising an endpoint, and that endpoint does not answer.
    pub lying: usize,
    pub providers: Vec<ProviderProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requirements: Option<RequirementsJson>,
    /// Set when the registry itself could not be read, so zero is never mistaken for "we checked".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn summarise(
    service: &Service,
    providers: Vec<ProviderProbe>,
    requirements: Option<RequirementsJson>,
) -> ServiceCensus {
    ServiceCensus {
        id: service.id,
        name: service.name,
        address: service.address,
        registered: providers.len(),
        serving: providers
            .iter()
            .filter(|p| matches!(p.verdict, Verdict::Serving | Verdict::Paywalled))
            .count(),
        lying: providers.iter().filter(|p| is_lying(p)).count(),
        providers,
        requirements,
        error: None,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Headline {
    pub services: usize,
    pub with_any_provider: usize,
    pub with_any_serving: usize,
    pub registered: usize,
    pub serving: usize,
}

/// The one figure this exists for: services with at least one provider that actually answers.
///
/// A service whose registry could not be read is left out of every count rather than folded in as
/// a zero, because "we could not look" is not "there is nobody there".
pub fn headline(all: &[ServiceCensus]) -> Headline {
    let measured: Vec<&ServiceCensus> = all.iter().filter(|s| s.error.is_none()).collect();
    Headline {
        services: measured.len(),
        with_any_provider: measured.iter().filter(|s| s.registered > 0).count(),
        with_any_serving: measured.iter().filter(|s| s.serving > 0).count(),
        registered: measured.iter().map(|s| s.registered).sum(),
        serving: measured.iter().map(|s| s.serving).sum(),
    }
}

// ── What it costs to run one ─────────────────────────────────────────────────

/// The protocol's own ceiling, from `getMaxThawingPeriod()` on Arbitrum One: exactly 28 days.
pub const PROTOCOL_MAX_THAWING_SECONDS: u64 = 2_419_200;

const WEI_PER_GRT: u128 = 1_000_000_000_000_000_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementsJson {
    pub min_tokens_grt: u128,
    pub max_tokens_grt: Option<u128>,
    pub min_thawing_days: f64,
    pub max_thawing_days: f64,
    pub min_verifier_cut_ppm: u32,
    pub max_verifier_cut_ppm: u32,
    pub no_minimum: bool,
    pub summary: String,
}

/// Turn the raw ranges a `ProvisionManager` reports into what an operator needs to know.
///
/// Two sentinels matter. `type(uint256).max` for the token ceiling means the service sets none;
/// `type(uint64).max` for the thawing ceiling means the same, but **not** that there is no limit -
/// the protocol's 28 days still applies, and an operator reading only the service's range would
/// pick a period the protocol refuses.
pub fn interpret(
    min_tokens: u128,
    max_tokens: Option<u128>,
    min_thawing: u64,
    max_thawing: Option<u64>,
    min_cut: u32,
    max_cut: u32,
) -> RequirementsJson {
    let service_max = max_thawing.unwrap_or(PROTOCOL_MAX_THAWING_SECONDS);
    let max_thawing_seconds = service_max.min(PROTOCOL_MAX_THAWING_SECONDS);
    let min_tokens_grt = min_tokens / WEI_PER_GRT;
    let days = |s: u64| s as f64 / 86_400.0;
    RequirementsJson {
        min_tokens_grt,
        max_tokens_grt: max_tokens.map(|t| t / WEI_PER_GRT),
        min_thawing_days: days(min_thawing),
        max_thawing_days: days(max_thawing_seconds),
        min_verifier_cut_ppm: min_cut,
        max_verifier_cut_ppm: max_cut,
        // Reported rather than advertised as free. On SDSCE both floors are zero, which reads more
        // like parameters nobody set than a deliberate offer, and telling somebody a service is
        // free to run when the truth is that it is unconfigured is the wrong encouragement.
        no_minimum: min_tokens == 0 && min_thawing == 0,
        summary: summary(min_tokens_grt, min_thawing, max_thawing_seconds),
    }
}

/// The sentence to put next to "could be yours to run".
///
/// Says the thawing *window* rather than one number. Our own operator write-up recommends 14 days
/// as "comfortably inside", which is right for Dispatch for the wrong reason: 14 is that service's
/// floor and 13 would be refused. Somebody reasoning downward from "comfortably inside" walks into
/// a revert.
fn summary(min_tokens_grt: u128, min_thawing: u64, max_thawing: u64) -> String {
    let stake = if min_tokens_grt == 0 {
        "No minimum provision".to_string()
    } else {
        format!("{} GRT", grouped(min_tokens_grt))
    };
    let lo = min_thawing as f64 / 86_400.0;
    let hi = max_thawing as f64 / 86_400.0;
    let window = if lo == 0.0 {
        format!("up to {} days", trim(hi))
    } else {
        format!("{} to {} days", trim(lo), trim(hi))
    };
    format!("{stake}, thawing period {window}.")
}

/// `toLocaleString('en-GB')`, which groups in threes with commas exactly as en-US does.
fn grouped(n: u128) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// JavaScript prints a whole float without a trailing `.0`, and these read as "14 days".
fn trim(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(
        provider: &str,
        registered: bool,
        block: u64,
        index: u64,
        endpoint: Option<&str>,
    ) -> RegistryEvent {
        RegistryEvent {
            provider: provider.into(),
            endpoint: endpoint.map(str::to_string),
            block_number: block,
            log_index: index,
            registered,
        }
    }

    /// The one this fold exists for. A provider that deregistered and registered again ten blocks
    /// later is registered, and set subtraction would report it gone.
    #[test]
    fn a_provider_that_changed_its_mind_is_counted_by_its_last_event() {
        let providers = current_providers(&[
            ev("0xAA", true, 456_950_400, 0, Some("https://a")),
            ev("0xAA", false, 456_950_409, 0, None),
            ev("0xAA", true, 456_950_419, 0, Some("https://a")),
        ]);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].address, "0xaa");
    }

    #[test]
    fn a_deregistration_is_the_last_word_when_it_is_last() {
        assert!(
            current_providers(&[
                ev("0xAA", true, 1, 0, Some("https://a")),
                ev("0xAA", false, 2, 0, None),
            ])
            .is_empty()
        );
    }

    /// Two events in one block are ordered by log index, or the answer depends on which the node
    /// happened to return first.
    #[test]
    fn two_events_in_one_block_are_ordered_within_it() {
        let out = current_providers(&[
            ev("0xAA", true, 100, 5, Some("https://a")),
            ev("0xAA", false, 100, 2, None),
        ]);
        assert_eq!(out.len(), 1, "the registration at index 5 is the later one");
    }

    /// A deregistration carries no endpoint. Blanking it would lose the address to call when the
    /// provider registers again without changing it.
    #[test]
    fn a_deregistration_does_not_blank_the_last_endpoint() {
        let out = current_providers(&[
            ev("0xAA", true, 1, 0, Some("https://a")),
            ev("0xAA", false, 2, 0, None),
            ev("0xAA", true, 3, 0, None),
        ]);
        assert_eq!(out[0].endpoint, "https://a");
    }

    /// 402 is the Nuthatch Data Service working. Reading it as an error would mark the one service
    /// that is serving as down.
    #[test]
    fn a_payment_demand_is_health_only_where_there_is_something_to_sell() {
        assert_eq!(verdict_of(402, Probe::Paywall), Verdict::Paywalled);
        assert_eq!(verdict_of(402, Probe::Http), Verdict::HttpError);
        assert_eq!(verdict_of(200, Probe::Http), Verdict::Serving);
        assert_eq!(verdict_of(301, Probe::Http), Verdict::Serving);
        assert_eq!(verdict_of(404, Probe::Http), Verdict::HttpError);
        assert_eq!(verdict_of(500, Probe::Paywall), Verdict::HttpError);
    }

    fn probe(verdict: Verdict) -> ProviderProbe {
        ProviderProbe {
            address: "0xaa".into(),
            endpoint: "https://a".into(),
            verdict,
            http_status: None,
            latency_ms: None,
        }
    }

    /// Declining to look is not catching somebody out.
    #[test]
    fn only_a_measured_failure_counts_as_a_lie() {
        assert!(is_lying(&probe(Verdict::HttpError)));
        assert!(is_lying(&probe(Verdict::Unreachable)));
        assert!(is_lying(&probe(Verdict::Timeout)));
        assert!(!is_lying(&probe(Verdict::Serving)));
        assert!(!is_lying(&probe(Verdict::Paywalled)));
        assert!(!is_lying(&probe(Verdict::NoEndpoint)));
        assert!(!is_lying(&probe(Verdict::Refused)), "we did not measure it");
    }

    /// A registry we could not read is left out of every count. "We could not look" is not "there
    /// is nobody there", and a zero would say the second.
    #[test]
    fn a_service_we_could_not_read_is_absent_from_the_headline() {
        let good = summarise(&SERVICES[0], vec![probe(Verdict::Serving)], None);
        let mut broken = summarise(&SERVICES[1], vec![], None);
        broken.error = Some("rpc timed out".into());

        let h = headline(&[good, broken]);
        assert_eq!(h.services, 1, "only the one we actually measured");
        assert_eq!(h.with_any_provider, 1);
        assert_eq!(h.with_any_serving, 1);
        assert_eq!(h.registered, 1);
        assert_eq!(h.serving, 1);
    }

    /// `uint64::MAX` means the service sets no ceiling, which does not remove the protocol's.
    #[test]
    fn an_unbounded_service_still_meets_the_protocols_twenty_eight_days() {
        let r = interpret(555 * WEI_PER_GRT, None, 14 * 86_400, None, 0, 1_000_000);
        assert_eq!(r.max_thawing_days, 28.0);
        assert_eq!(r.min_thawing_days, 14.0);
        assert_eq!(r.max_tokens_grt, None);
        assert_eq!(r.min_tokens_grt, 555);
        assert!(!r.no_minimum);
        assert_eq!(r.summary, "555 GRT, thawing period 14 to 28 days.");
    }

    /// A service stricter than the protocol keeps its own ceiling.
    #[test]
    fn a_service_below_the_protocol_ceiling_keeps_its_own() {
        let r = interpret(0, Some(1000 * WEI_PER_GRT), 0, Some(7 * 86_400), 0, 0);
        assert_eq!(r.max_thawing_days, 7.0);
        assert_eq!(r.max_tokens_grt, Some(1000));
        assert!(r.no_minimum, "both floors are zero");
        assert_eq!(
            r.summary,
            "No minimum provision, thawing period up to 7 days."
        );
    }

    #[test]
    fn a_large_minimum_is_grouped_the_way_the_page_prints_it() {
        let r = interpret(100_000 * WEI_PER_GRT, None, 28 * 86_400, None, 0, 0);
        assert_eq!(r.summary, "100,000 GRT, thawing period 28 to 28 days.");
    }

    #[test]
    fn every_service_has_a_distinct_id_and_address() {
        let ids: std::collections::BTreeSet<&str> = SERVICES.iter().map(|s| s.id).collect();
        let addresses: std::collections::BTreeSet<String> =
            SERVICES.iter().map(|s| s.address.to_lowercase()).collect();
        let tables: std::collections::BTreeSet<&str> = SERVICES.iter().map(|s| s.table).collect();
        assert_eq!(ids.len(), SERVICES.len());
        assert_eq!(addresses.len(), SERVICES.len());
        assert_eq!(tables.len(), SERVICES.len());
    }

    #[test]
    fn the_registry_query_reads_both_of_the_services_own_tables() {
        let sql = registry_sql(&SERVICES[0]);
        assert!(sql.contains("FROM dispatch__provider_registered"), "{sql}");
        assert!(
            sql.contains("FROM dispatch__provider_deregistered"),
            "{sql}"
        );
    }

    /// Dispatch's real history as data-services-nest returns it: deregistered at 456,950,409 and
    /// registered again ten blocks later, with the nest's integers arriving as strings.
    #[test]
    fn nest_rows_fold_into_the_same_providers_the_chain_read_gave() {
        let rows = vec![
            serde_json::json!({"provider": "0xAA", "endpoint": "https://a", "block_number": "456917519", "log_index": "3", "registered": true}),
            serde_json::json!({"provider": "0xbb", "endpoint": "https://b", "block_number": 456940000, "log_index": 1, "registered": true}),
            serde_json::json!({"provider": "0xaa", "endpoint": null, "block_number": 456950409, "log_index": 0, "registered": false}),
            serde_json::json!({"provider": "0xaa", "endpoint": "https://a2", "block_number": 456950419, "log_index": 0, "registered": true}),
        ];
        let providers = current_providers(&registry_events(&rows).unwrap());
        assert_eq!(
            providers,
            vec![
                Provider {
                    address: "0xaa".into(),
                    endpoint: "https://a2".into()
                },
                Provider {
                    address: "0xbb".into(),
                    endpoint: "https://b".into()
                },
            ]
        );
    }

    #[test]
    fn a_registry_row_missing_a_field_fails_the_read() {
        for row in [
            serde_json::json!({"endpoint": null, "block_number": 1, "log_index": 0, "registered": false}),
            serde_json::json!({"provider": "0xaa", "block_number": 1, "registered": false}),
            serde_json::json!({"provider": "0xaa", "block_number": 1, "log_index": 0}),
        ] {
            assert!(
                registry_events(std::slice::from_ref(&row)).is_err(),
                "accepted {row}"
            );
        }
    }
}
