//! The directory's QoS Quality score: every indexer scored over thirty days of qos-reo-nest, once
//! an hour, rather than folded on every directory load.

use kittiwake_db::{Pool, QosQualityRow};
use kittiwake_nest::{Lane, NestGate};
use kittiwake_qos::aggregate::QosDailyRow;
use kittiwake_qos::nest::GRAPH_EPOCH_DAYS;
use kittiwake_read::qos::{
    Cohort, SCORE_WINDOW_DAYS, allocation_daily_from, allocation_row_from, deployment_daily_from,
    score_network,
};
use kittiwake_read::sql;
use kittiwake_types::NestId;

use crate::{IngestError, Ingested, page};

/// Days per totals statement. About 1,200 deployments a day, so ten days stays inside the row cap.
const TOTALS_DAYS_PER_PAGE: i64 = 10;

pub async fn score_qos(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
    now: i64,
) -> Result<Ingested, IngestError> {
    let today = now.div_euclid(86_400);
    let from = today - SCORE_WINDOW_DAYS;
    let mut cohort = Cohort::default();

    // One day per statement: a day of every indexer's allocations is a few thousand rows.
    for day in from..=today {
        let answer = page(
            gate,
            NestId::Qos,
            sql::qos_network_rows_sql(day),
            lane,
            "QoS rows",
        )
        .await?;
        cohort.rows.extend(
            answer
                .rows
                .iter()
                .filter_map(allocation_daily_from)
                .map(|r| QosDailyRow::from(&r)),
        );
    }
    let mut start = from;
    while start <= today {
        let end = (start + TOTALS_DAYS_PER_PAGE - 1).min(today);
        let answer = page(
            gate,
            NestId::Qos,
            sql::qos_network_totals_sql(start, end),
            lane,
            "QoS deployment totals",
        )
        .await?;
        cohort
            .totals
            .extend(answer.rows.iter().filter_map(deployment_daily_from));
        start = end + 1;
    }
    let answer = page(
        gate,
        NestId::Alloc,
        sql::ALLOCATION_SHARES_ALL_SQL.into(),
        lane,
        "allocation shares",
    )
    .await?;
    cohort.allocations = answer.rows.iter().filter_map(allocation_row_from).collect();

    let day = chrono::DateTime::from_timestamp(today * 86_400, 0)
        .map(|d| d.date_naive())
        .unwrap_or_default();
    let rows: Vec<QosQualityRow> = score_network(&cohort, today - GRAPH_EPOCH_DAYS)
        .into_iter()
        .map(|(indexer, detail, phase2)| QosQualityRow {
            indexer_address: indexer,
            day,
            q_score: detail.result.q_score,
            reliability: detail.result.reliability,
            lat_util: detail.result.lat_util,
            fresh_util: detail.result.fresh_util,
            coverage: detail.result.coverage,
            served_gap: phase2.map(|m| m.served_gap),
            efficiency: phase2.map(|m| m.efficiency),
        })
        .collect();
    let written = kittiwake_db::write_qos_quality(pool, &rows).await?;
    Ok(Ingested { rows: written })
}
