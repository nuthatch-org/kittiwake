//! Jobs whose output is read straight back out of the process, with no database in between.
//!
//! The service this replaces ran these as stateless invocations, so their results had to travel
//! through Redis to reach the route that served them. One long-lived process needs no courier: the
//! job writes into the same store the route reads. That removes a network round trip from every
//! request and a whole class of "the cron wrote it but the route cannot see it" failure.

use kittiwake_cache::{Entry, Store};
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_read::{shape, sql};
use kittiwake_types::NestId;

use crate::{IngestError, Ingested};

/// Refresh the Horizon activity feed.
pub async fn horizon_activity(
    store: &Store,
    gate: &NestGate,
    lane: Lane,
) -> Result<Ingested, IngestError> {
    let mut delegations = QuerySpec::cron(
        NestId::Alloc,
        sql::delegation_events_sql(None, 20, 0)
            .ok_or_else(|| IngestError::Refused("delegation query refused its own input".into()))?,
    );
    let mut provisions = QuerySpec::cron(NestId::Alloc, sql::newest_provisions_sql(10));
    delegations.lane = lane;
    provisions.lane = lane;

    let (d, p) = tokio::join!(gate.query(&delegations), gate.query(&provisions));
    let d = d?;
    let p = p?;

    let feed = shape::horizon_activity(&d.rows, &p.rows);
    let count = feed.as_array().map(|a| a.len()).unwrap_or(0);
    store
        .insert(
            shape::HORIZON_ACTIVITY_KEY.to_string(),
            Entry::new(feed, d.provenance.clone()),
        )
        .await;

    Ok(Ingested { rows: count as u64 })
}
