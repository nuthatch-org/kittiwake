//! Shared vocabulary: where an answer came from, how current it is, and how a failure is told.
//!
//! The rule this crate exists to enforce is the one from the migration checklist: an answer a
//! caller cannot date is an answer they cannot cite. Every response body carries provenance, so
//! there is no route that can quietly forget to.

use serde::{Deserialize, Serialize};

/// Which nest a query is addressed to. The string is the path prefix under the shared origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NestId {
    /// `/alloc` — graph-allocations-nest, the protocol.
    Alloc,
    /// `/gns` — graph-gns-nest.
    Gns,
    /// `/dips` — dips-nest on Arbitrum One.
    Dips,
    /// `/dips-sepolia` — the same contracts where the lifecycle has rows.
    DipsSepolia,
    /// `/legacy-flows` — the frozen pre-Horizon archive. Stalled by design.
    LegacyFlows,
    /// `/qos` — qos-reo-nest on Gnosis, Edge & Node's QoS oracle read from its own postings.
    Qos,
}

impl NestId {
    /// The path under the shared origin. Not configurable per call: a route names a nest, and the
    /// deployment names the path, so a staged nest moves in config and nowhere else.
    pub fn default_base_path(self) -> &'static str {
        match self {
            NestId::Alloc => "/alloc",
            NestId::Gns => "/gns",
            NestId::Dips => "/dips",
            NestId::DipsSepolia => "/dips-sepolia",
            NestId::LegacyFlows => "/legacy-flows",
            NestId::Qos => "/qos",
        }
    }

    /// A frozen archive answers from sealed segments and never advances, so `/ready` reports
    /// `stalled` for ever and is right to. Health checks and readiness gates must not treat that
    /// as an outage, or they cry wolf from day one and the next real one is ignored with it.
    pub fn is_archival(self) -> bool {
        matches!(self, NestId::LegacyFlows)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NestId::Alloc => "alloc",
            NestId::Gns => "gns",
            NestId::Dips => "dips",
            NestId::DipsSepolia => "dips-sepolia",
            NestId::LegacyFlows => "legacy-flows",
            NestId::Qos => "qos",
        }
    }

    pub const ALL: [NestId; 6] = [
        NestId::Alloc,
        NestId::Gns,
        NestId::Dips,
        NestId::DipsSepolia,
        NestId::LegacyFlows,
        NestId::Qos,
    ];
}

/// Where the bytes in this response actually came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServedFrom {
    Nest,
    Cache,
    Postgres,
    Chain,
}

impl ServedFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            ServedFrom::Nest => "nest",
            ServedFrom::Cache => "cache",
            ServedFrom::Postgres => "postgres",
            ServedFrom::Chain => "chain",
        }
    }
}

/// The nest's own account of when its answer was true, carried verbatim.
///
/// Every field is optional because the nest's envelope makes them optional, and inventing a value
/// for a missing one would be worse than admitting we do not know.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Block the nest considered the tip when it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<i64>,
    /// Last block sealed into a segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed_through: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The nest cut the answer off at its row cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The nest served from a degraded set of tables.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub degraded: bool,
}

/// Provenance plus what this particular delivery did: served from where, how old, stale or not.
#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    #[serde(flatten)]
    pub provenance: Provenance,
    pub served_from: ServedFrom,
    /// True when the value is past its freshness budget. Never true and silent: a stale value is
    /// labelled here and in the `x-kittiwake-stale` header.
    pub stale: bool,
    pub age_seconds: u64,
}

/// The response shape every data route returns.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope<T> {
    pub data: T,
    pub provenance: Meta,
}

impl<T> Envelope<T> {
    pub fn new(data: T, provenance: Meta) -> Self {
        Self { data, provenance }
    }
}

/// Everything that can be told to a caller, and the status it is told with.
///
/// There is deliberately no variant that means "we substituted another source". A route reads one
/// source; when that source cannot answer, the caller is told so.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("nest busy")]
    NestBusy { retry_after: u64 },
    #[error("nest unready: {reason}")]
    NestUnready {
        reason: String,
        lag_blocks: Option<i64>,
        sealed_through: Option<i64>,
    },
    #[error("nest timeout")]
    NestTimeout,
    #[error("nest upstream error {0}")]
    NestUpstream(u16),
    #[error("nest response could not be decoded: {0}")]
    NestDecode(String),
    #[error("rate limited")]
    RateLimited { retry_after: u64 },
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("dependency unavailable: {0}")]
    Unavailable(String),
    #[error("internal error")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            // A busy or gate-timed-out nest is us declining to hammer it, not the caller's fault,
            // and it is retryable — so 503 with a Retry-After rather than 500.
            ApiError::NestBusy { .. } | ApiError::NestUnready { .. } => 503,
            ApiError::NestTimeout => 504,
            ApiError::NestUpstream(_) | ApiError::NestDecode(_) => 502,
            ApiError::RateLimited { .. } => 429,
            ApiError::BadRequest(_) => 400,
            ApiError::NotFound => 404,
            ApiError::Unauthorized => 401,
            ApiError::Unavailable(_) => 503,
            ApiError::Internal(_) => 500,
        }
    }

    /// A stable machine-readable code. The frontend switches on this, never on the message.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::NestBusy { .. } => "nest_busy",
            ApiError::NestUnready { .. } => "nest_unready",
            ApiError::NestTimeout => "nest_timeout",
            ApiError::NestUpstream(_) => "nest_upstream",
            ApiError::NestDecode(_) => "nest_decode",
            ApiError::RateLimited { .. } => "rate_limited",
            ApiError::BadRequest(_) => "bad_request",
            ApiError::NotFound => "not_found",
            ApiError::Unauthorized => "unauthorized",
            ApiError::Unavailable(_) => "unavailable",
            ApiError::Internal(_) => "internal",
        }
    }

    pub fn retry_after(&self) -> Option<u64> {
        match self {
            ApiError::NestBusy { retry_after } | ApiError::RateLimited { retry_after } => {
                Some(*retry_after)
            }
            ApiError::NestUnready { .. } => Some(30),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archival_nest_is_the_only_one_exempt_from_readiness() {
        for n in NestId::ALL {
            assert_eq!(n.is_archival(), n == NestId::LegacyFlows, "{n:?}");
        }
    }

    #[test]
    fn a_busy_nest_is_retryable_and_never_a_server_error() {
        let e = ApiError::NestBusy { retry_after: 2 };
        assert_eq!(e.status(), 503);
        assert_eq!(e.retry_after(), Some(2));
    }

    #[test]
    fn provenance_omits_what_the_nest_did_not_say() {
        let json = serde_json::to_string(&Provenance::default()).unwrap();
        assert_eq!(json, "{}", "absent fields must not serialize as nulls");
    }
}
