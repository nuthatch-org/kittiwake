//! `indexer_snapshots`, the daily exchange rates the rewards-history chart folds a delegator's
//! shares through. Lodestar's refresh cron wrote it until lodestar#171 and nothing did after. #236
//!
//! Each run writes every indexer as it stands now, then fills any closed day in the window that has
//! no snapshot at all from `lodestar_indexer_ledger` summed to that day's last second. The ledger's
//! pool is tokens less thawing, which matched `getDelegationPool` to the wei at two past blocks.

use std::collections::BTreeSet;

use futures::future::BoxFuture;
use kittiwake_db::Pool;
use kittiwake_nest::{Lane, NestGate};
use kittiwake_read::sql;
use kittiwake_rewards as rw;
use kittiwake_types::NestId;

use crate::{IngestError, Ingested, i, page, s};

/// What the rewards-history route reads back.
pub const WINDOW_DAYS: i64 = 90;
const DAY: i64 = 86_400;

/// One indexer at one instant. Wei stays a decimal string; a backfilled day carries only the rate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub indexer: String,
    pub self_stake: Option<String>,
    pub delegated: Option<String>,
    pub delegated_thawing: Option<String>,
    pub allocated: Option<String>,
    pub reward_cut: Option<i64>,
    pub query_fee_cut: Option<i64>,
    pub query_fees: Option<String>,
    pub rate: Option<f64>,
}

pub trait SnapshotStore: Send + Sync {
    /// UTC days since `from` with at least one snapshot.
    fn days(&self, from: i64) -> BoxFuture<'_, Result<BTreeSet<i64>, IngestError>>;
    fn write<'a>(
        &'a self,
        at: i64,
        rows: &'a [Snapshot],
    ) -> BoxFuture<'a, Result<u64, IngestError>>;
}

impl SnapshotStore for Pool {
    fn days(&self, from: i64) -> BoxFuture<'_, Result<BTreeSet<i64>, IngestError>> {
        Box::pin(async move {
            // `::date` as `delegation_exchange_rates` reads it; sqlx connects with `TimeZone=UTC`.
            let days: Vec<i32> = sqlx::query_scalar(
                "SELECT DISTINCT snapshot_at::date - DATE '1970-01-01' FROM indexer_snapshots \
                 WHERE snapshot_at >= to_timestamp($1)",
            )
            .bind(from * DAY)
            .fetch_all(self)
            .await?;
            Ok(days.into_iter().map(i64::from).collect())
        })
    }

    fn write<'a>(
        &'a self,
        at: i64,
        rows: &'a [Snapshot],
    ) -> BoxFuture<'a, Result<u64, IngestError>> {
        Box::pin(async move {
            if rows.is_empty() {
                return Ok(0);
            }
            let col = |f: fn(&Snapshot) -> Option<String>| rows.iter().map(f).collect::<Vec<_>>();
            let cut = |f: fn(&Snapshot) -> Option<i64>| rows.iter().map(f).collect::<Vec<_>>();
            let indexers: Vec<String> = rows.iter().map(|r| r.indexer.clone()).collect();
            let rates: Vec<Option<f64>> = rows.iter().map(|r| r.rate).collect();
            // Deliberately not `query!`: the wei columns go in as text and become `numeric` here. #2.
            let done = sqlx::query(
                "INSERT INTO indexer_snapshots (indexer_address, self_stake_grt, delegated_grt, \
                 delegated_thawing_grt, allocated_grt, reward_cut, query_fee_cut, \
                 query_fees_collected_grt, delegation_exchange_rate, snapshot_at) \
                 SELECT indexer, stake / 1e18, delegated / 1e18, thawing / 1e18, allocated / 1e18, \
                 reward_cut, query_fee_cut, fees / 1e18, rate, to_timestamp($10) \
                 FROM UNNEST($1::text[], $2::numeric[], $3::numeric[], $4::numeric[], $5::numeric[], \
                 $6::bigint[], $7::bigint[], $8::numeric[], $9::float8[]) \
                 AS t(indexer, stake, delegated, thawing, allocated, reward_cut, query_fee_cut, fees, rate)",
            )
            .bind(&indexers)
            .bind(col(|r| r.self_stake.clone()))
            .bind(col(|r| r.delegated.clone()))
            .bind(col(|r| r.delegated_thawing.clone()))
            .bind(col(|r| r.allocated.clone()))
            .bind(cut(|r| r.reward_cut))
            .bind(cut(|r| r.query_fee_cut))
            .bind(col(|r| r.query_fees.clone()))
            .bind(&rates)
            .bind(at)
            .execute(self)
            .await?;
            Ok(done.rows_affected())
        })
    }
}

pub async fn snapshot_indexers<S: SnapshotStore>(
    store: &S,
    gate: &NestGate,
    lane: Lane,
    now: i64,
) -> Result<Ingested, IngestError> {
    let live = page(
        gate,
        NestId::Alloc,
        sql::INDEXERS_ALL_SQL.into(),
        lane,
        "indexers",
    )
    .await?;
    let mut written = store.write(now, &live_rows(&live.rows)).await?;

    let today = now.div_euclid(DAY);
    let present = store.days(today - WINDOW_DAYS).await?;
    for day in (today - WINDOW_DAYS..today).filter(|d| !present.contains(d)) {
        let close = (day + 1) * DAY - 1;
        let ledger = page(
            gate,
            NestId::Alloc,
            sql::exchange_rates_as_of_sql(close),
            lane,
            "exchange rates",
        )
        .await?;
        written += store.write(close, &ledger_rows(&ledger.rows)).await?;
    }
    Ok(Ingested { rows: written })
}

fn wei(r: &serde_json::Value, k: &str) -> f64 {
    s(r, k).and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

/// The pool priced as HorizonStaking pays it, `(tokens - tokensThawing) / shares`, as refresh does.
fn live_rows(rows: &[serde_json::Value]) -> Vec<Snapshot> {
    rows.iter()
        .filter_map(|r| {
            let shares = wei(r, "delegator_shares");
            Some(Snapshot {
                indexer: s(r, "id")?.to_lowercase(),
                self_stake: s(r, "staked_tokens"),
                delegated: s(r, "delegated_tokens"),
                delegated_thawing: s(r, "delegated_thawing_tokens"),
                allocated: s(r, "allocated_tokens"),
                reward_cut: i(r, "indexing_reward_cut"),
                query_fee_cut: i(r, "query_fee_cut"),
                query_fees: s(r, "query_fees_collected"),
                rate: (shares > 0.0).then(|| {
                    rw::pool_exchange_rate(
                        wei(r, "delegated_tokens"),
                        wei(r, "delegated_thawing_tokens"),
                        shares,
                    )
                }),
            })
        })
        .collect()
}

/// The ledger's pool already excludes thawing, so nothing is subtracted here.
fn ledger_rows(rows: &[serde_json::Value]) -> Vec<Snapshot> {
    rows.iter()
        .filter_map(|r| {
            Some(Snapshot {
                indexer: s(r, "indexer")?.to_lowercase(),
                rate: Some(rw::pool_exchange_rate(
                    wei(r, "pool_tokens"),
                    0.0,
                    wei(r, "pool_shares"),
                )),
                ..Snapshot::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

    const TODAY: i64 = 20_745;
    const NOW: i64 = TODAY * DAY + 3_600;
    const ELLIPFRA: &str = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";

    #[derive(Default)]
    struct MemStore {
        present: BTreeSet<i64>,
        written: Mutex<BTreeMap<i64, Vec<Snapshot>>>,
    }

    impl SnapshotStore for MemStore {
        fn days(&self, from: i64) -> BoxFuture<'_, Result<BTreeSet<i64>, IngestError>> {
            let out = self.present.range(from..).copied().collect();
            Box::pin(async move { Ok(out) })
        }
        fn write<'a>(
            &'a self,
            at: i64,
            rows: &'a [Snapshot],
        ) -> BoxFuture<'a, Result<u64, IngestError>> {
            self.written.lock().unwrap().insert(at, rows.to_vec());
            Box::pin(async move { Ok(rows.len() as u64) })
        }
    }

    /// ellipfra's pool per `getDelegationPool` on 2026-10-19, and its ledger at block 505,705,796.
    struct Nest {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Respond for Nest {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            self.seen.lock().unwrap().push(q.clone());
            let rows = if q.contains("lodestar_indexer_ledger") {
                serde_json::json!([{"indexer": ELLIPFRA,
                    "pool_tokens": "144659891031187289395611538",
                    "pool_shares": "69467007168476390705972741"}])
            } else {
                serde_json::json!([{"id": ELLIPFRA,
                    "delegated_tokens": "156308737360443203892155785",
                    "delegated_thawing_tokens": "4929831969931604453709303",
                    "delegator_shares": "71115565442162459850219192",
                    "staked_tokens": "13168862233516110598314665",
                    "indexing_reward_cut": 430000, "query_fee_cut": 950000},
                    {"id": "0xA", "delegated_tokens": "0", "delegator_shares": "0"}])
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": rows.as_array().map(Vec::len).unwrap_or(0), "rows": rows,
                "provenance": {}
            }))
        }
    }

    async fn run<S: SnapshotStore>(store: &S) -> Vec<String> {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Nest { seen: seen.clone() })
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let gate = NestGate::new(cfg).unwrap();
        snapshot_indexers(store, &gate, Lane::Cron, NOW)
            .await
            .expect("the run failed");
        seen.lock().unwrap().clone()
    }

    /// `KITTIWAKE_TEST_DATABASE_URL=postgres://… cargo test -p kittiwake-ingest -- --ignored snapshots_`
    /// against a database with `db/schema.sql` applied.
    #[tokio::test]
    #[ignore = "needs a Postgres"]
    async fn snapshots_fill_the_window_once_and_read_back_one_rate_a_day() {
        let url =
            std::env::var("KITTIWAKE_TEST_DATABASE_URL").expect("KITTIWAKE_TEST_DATABASE_URL");
        let pool = kittiwake_db::connect(&url, 2).await.unwrap();
        run(&pool).await;
        assert_eq!(
            pool.days(TODAY - WINDOW_DAYS).await.unwrap(),
            (TODAY - WINDOW_DAYS..=TODAY).collect::<BTreeSet<_>>()
        );
        let again = run(&pool).await;
        assert!(
            !again.iter().any(|q| q.contains("lodestar_indexer_ledger")),
            "a filled window was filled again: {again:#?}"
        );
        let since = chrono::DateTime::from_timestamp((TODAY - WINDOW_DAYS) * DAY, 0).unwrap();
        let rates = kittiwake_db::delegation_exchange_rates(&pool, &[ELLIPFRA.into()], since)
            .await
            .unwrap();
        assert_eq!(rates.len(), WINDOW_DAYS as usize + 1);
        assert!((rates[0].2 - 2.082_425_843_974_358).abs() < 1e-12);
        assert!((rates.last().unwrap().2 - 2.128_632_521_576_8).abs() < 1e-12);
    }

    #[tokio::test]
    async fn a_share_is_priced_without_the_pools_thawing_tokens() {
        let store = MemStore {
            present: (TODAY - WINDOW_DAYS..TODAY).collect(),
            ..MemStore::default()
        };
        run(&store).await;
        let written = store.written.lock().unwrap();
        let now = &written[&NOW];
        assert_eq!(now[0].indexer, ELLIPFRA);
        // 2.1980 with the thawing left in.
        assert!((now[0].rate.unwrap() - 2.128_632_521_576_8).abs() < 1e-12);
        assert_eq!(now[1].rate, None, "no shares, no price");
    }

    #[tokio::test]
    async fn only_a_closed_day_with_no_snapshot_is_filled_and_at_its_last_second() {
        let gap = [TODAY - 40, TODAY - 39, TODAY - 1];
        let store = MemStore {
            present: (TODAY - WINDOW_DAYS..TODAY)
                .filter(|d| !gap.contains(d))
                .collect(),
            ..MemStore::default()
        };
        let seen = run(&store).await;

        let closes: Vec<i64> = gap.iter().map(|d| (d + 1) * DAY - 1).collect();
        let ledger: Vec<&String> = seen
            .iter()
            .filter(|q| q.contains("lodestar_indexer_ledger"))
            .collect();
        assert_eq!(ledger.len(), gap.len(), "{seen:#?}");
        for (q, close) in ledger.iter().zip(&closes) {
            assert!(q.contains(&format!("ts <= {close} ")), "{q}");
        }

        let written = store.written.lock().unwrap();
        assert_eq!(
            written.keys().copied().collect::<Vec<_>>(),
            [closes.as_slice(), &[NOW]].concat()
        );
        let backfilled = &written[&closes[0]][0];
        assert_eq!(backfilled.indexer, ELLIPFRA);
        assert!((backfilled.rate.unwrap() - 2.082_425_843_974_358).abs() < 1e-12);
        assert_eq!(
            backfilled.delegated, None,
            "the ledger has no tokens-with-thawing figure"
        );
    }
}
