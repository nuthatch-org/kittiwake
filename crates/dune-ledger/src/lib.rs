//! Shared Postgres admission for all Dune execution callers. Uncertain submissions remain charged.

use sqlx::PgPool;

pub const MIGRATION: &str = include_str!("../migrations/001_ledger.sql");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Dune reservation refused: {0}")]
    Refused(&'static str),
    #[error("Dune ledger database error")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct Ledger {
    pool: PgPool,
}

/// Not cloneable or serialisable. Only a committed database reservation can create this value.
pub struct LedgerReservation {
    pool: PgPool,
    id: i64,
    query: u64,
}

/// A consumed reservation. A crash or ambiguous network response leaves it charged for reconciliation.
pub struct SubmittedReservation {
    pool: PgPool,
    id: i64,
}

impl Ledger {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn reserve(
        &self,
        period: i64,
        workstream: &str,
        query: u64,
        key_name: &str,
        estimate: i64,
    ) -> Result<LedgerReservation, Error> {
        let query_i64 = i64::try_from(query).map_err(|_| Error::Refused("query ID"))?;
        if query == 0 || estimate <= 0 || key_name.trim().is_empty() {
            return Err(Error::Refused("reservation fields"));
        }
        let mut tx = self.pool.begin().await?;
        // The shared account-period lock serialises admission across processes and workstreams.
        let limit: Option<i64> = sqlx::query_scalar("SELECT limit_microcredits FROM dune_budget_periods WHERE id = $1 AND now() >= starts_at AND now() < ends_at FOR UPDATE")
            .bind(period).fetch_optional(&mut *tx).await?;
        let limit = limit.ok_or(Error::Refused("inactive billing period"))?;
        let allocation: Option<i64> = sqlx::query_scalar("SELECT limit_microcredits FROM dune_budget_allocations WHERE period_id = $1 AND workstream = $2")
            .bind(period).bind(workstream).fetch_optional(&mut *tx).await?;
        let allocation = allocation.ok_or(Error::Refused("unknown workstream"))?;
        let fits: bool = sqlx::query_scalar("SELECT coalesce(sum(coalesce(actual_microcredits, estimate_microcredits)), 0) + $2::numeric <= $3::numeric AND coalesce(sum(coalesce(actual_microcredits, estimate_microcredits)) FILTER (WHERE workstream = $4), 0) + $2::numeric <= $5::numeric FROM dune_reservations WHERE period_id = $1")
            .bind(period).bind(estimate).bind(limit).bind(workstream).bind(allocation).fetch_one(&mut *tx).await?;
        if !fits {
            return Err(Error::Refused("credit allocation exceeded"));
        }
        let id: i64 = sqlx::query_scalar("INSERT INTO dune_reservations(period_id, workstream, query_id, api_key_name, estimate_microcredits) VALUES ($1,$2,$3,$4,$5) RETURNING id")
            .bind(period).bind(workstream).bind(query_i64).bind(key_name).bind(estimate).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(LedgerReservation {
            pool: self.pool.clone(),
            id,
            query,
        })
    }

    /// Settle only after a terminal execution status supplies its actual cost. Repeated settlement
    /// with identical data is harmless; conflicting data is refused for operator review.
    pub async fn settle(&self, execution: &str, actual: i64) -> Result<(), Error> {
        if actual < 0 {
            return Err(Error::Refused("negative actual cost"));
        }
        let mut tx = self.pool.begin().await?;
        // Settle under the same account lock as admission so a concurrent reservation sees
        // either the estimate or the complete actual-cost update, in a serial order.
        let period: Option<i64> = sqlx::query_scalar("SELECT p.id FROM dune_budget_periods p JOIN dune_reservations r ON r.period_id = p.id WHERE r.execution_id = $1 FOR UPDATE OF p")
            .bind(execution).fetch_optional(&mut *tx).await?;
        if period.is_none() {
            return Err(Error::Refused("unknown settlement"));
        }
        let result = sqlx::query("UPDATE dune_reservations SET actual_microcredits = $2, state = 'settled' WHERE execution_id = $1 AND (state = 'submitted' OR (state = 'settled' AND actual_microcredits = $2))")
            .bind(execution).bind(actual).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(Error::Refused("unknown or conflicting settlement"));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record an observed execution or export charge without prior interactive reservation.
    pub async fn record_observed(
        &self,
        account: &str,
        workstream: &str,
        query: u64,
        key_name: &str,
        execution_id: Option<&str>,
        microcredits: i64,
    ) -> Result<bool, Error> {
        let query_i64 = i64::try_from(query).map_err(|_| Error::Refused("query ID"))?;
        if query == 0 || microcredits <= 0 || key_name.trim().is_empty() {
            return Err(Error::Refused("reservation fields"));
        }
        let mut tx = self.pool.begin().await?;
        let period: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM dune_budget_periods WHERE account = $1 AND now() >= starts_at AND now() < ends_at FOR UPDATE",
        )
        .bind(account)
        .fetch_optional(&mut *tx)
        .await?;
        let period = period.ok_or(Error::Refused("inactive billing period"))?;

        if let Some(exec) = execution_id {
            let res = sqlx::query(
                "INSERT INTO dune_reservations(period_id, workstream, query_id, api_key_name, estimate_microcredits, actual_microcredits, state, execution_id) \
                 VALUES ($1, $2, $3, $4, $5, $5, 'settled', $6) \
                 ON CONFLICT (execution_id) DO NOTHING",
            )
            .bind(period)
            .bind(workstream)
            .bind(query_i64)
            .bind(key_name)
            .bind(microcredits)
            .bind(exec)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(res.rows_affected() > 0)
        } else {
            sqlx::query(
                "INSERT INTO dune_reservations(period_id, workstream, query_id, api_key_name, estimate_microcredits, actual_microcredits, state) \
                 VALUES ($1, $2, $3, $4, $5, $5, 'settled')",
            )
            .bind(period)
            .bind(workstream)
            .bind(query_i64)
            .bind(key_name)
            .bind(microcredits)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(true)
        }
    }

    pub async fn current_usage(&self, account: &str) -> Result<Option<BudgetUsageSummary>, Error> {
        let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT p.id, p.limit_microcredits, \
             coalesce(sum(coalesce(r.actual_microcredits, r.estimate_microcredits)), 0)::bigint AS charged, \
             count(r.id) FILTER (WHERE r.state <> 'settled')::bigint AS unresolved \
             FROM dune_budget_periods p \
             LEFT JOIN dune_reservations r ON r.period_id = p.id \
             WHERE p.account = $1 AND now() >= p.starts_at AND now() < p.ends_at \
             GROUP BY p.id, p.limit_microcredits",
        )
        .bind(account)
        .fetch_optional(&self.pool)
        .await?;

        let Some((period_id, total_limit, total_charged, unresolved)) = row else {
            return Ok(None);
        };

        let ws_rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT a.workstream, a.limit_microcredits, \
             coalesce(sum(coalesce(r.actual_microcredits, r.estimate_microcredits)), 0)::bigint AS charged \
             FROM dune_budget_allocations a \
             LEFT JOIN dune_reservations r ON r.period_id = a.period_id AND r.workstream = a.workstream \
             WHERE a.period_id = $1 \
             GROUP BY a.workstream, a.limit_microcredits",
        )
        .bind(period_id)
        .fetch_all(&self.pool)
        .await?;

        let mut workstream_usage = std::collections::BTreeMap::new();
        let mut workstream_limits = std::collections::BTreeMap::new();
        for (ws, limit, charged) in ws_rows {
            workstream_limits.insert(ws.clone(), limit);
            workstream_usage.insert(ws, charged);
        }

        Ok(Some(BudgetUsageSummary {
            period_id,
            total_limit_microcredits: total_limit,
            total_charged_microcredits: total_charged,
            workstream_usage,
            workstream_limits,
            unresolved_count: unresolved,
        }))
    }

    /// Settles every submitted reservation that has an attached execution ID, using `source`
    /// to learn what each one actually cost. A reservation whose HTTP call never got far enough
    /// to attach an execution ID is a different, harder problem this does not attempt: nothing
    /// here can tell whether Dune ever ran that request at all, only reconcile ones it did.
    pub async fn reconcile(
        &self,
        source: &impl ExecutionCostSource,
        limit: i64,
    ) -> Result<ReconciliationReport, Error> {
        let pending: Vec<String> = sqlx::query_scalar(
            "SELECT execution_id FROM dune_reservations \
             WHERE state = 'submitted' AND execution_id IS NOT NULL \
             ORDER BY created_at LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut report = ReconciliationReport::default();
        for execution_id in pending {
            match source.execution_cost(&execution_id).await {
                ExecutionCost::Pending => report.still_pending += 1,
                ExecutionCost::Unknown => report.unresolved += 1,
                ExecutionCost::Settled { microcredits } => {
                    match self.settle(&execution_id, microcredits).await {
                        Ok(()) => report.settled += 1,
                        Err(_) => report.unresolved += 1,
                    }
                }
            }
        }
        Ok(report)
    }
}

/// What an execution-cost source learned about one submitted reservation's execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionCost {
    /// Still running; check again on a later pass.
    Pending,
    /// Terminal, with a cost the ledger can settle.
    Settled { microcredits: i64 },
    /// Terminal with no attributable cost, or the lookup itself failed. Left charged at its
    /// estimate rather than guessed at zero, per the same overestimate-over-silence rule as
    /// an unresolved submission.
    Unknown,
}

/// Looks up what Dune actually charged for one execution. Implemented by the HTTP client;
/// kept as a trait here so the ledger never depends on it, avoiding a crate cycle (the
/// client crate already depends on this one to gate execution on a reservation).
pub trait ExecutionCostSource {
    fn execution_cost(
        &self,
        execution_id: &str,
    ) -> impl std::future::Future<Output = ExecutionCost> + Send;
}

/// Tally from one `reconcile` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconciliationReport {
    pub settled: i64,
    pub still_pending: i64,
    pub unresolved: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetUsageSummary {
    pub period_id: i64,
    pub total_limit_microcredits: i64,
    pub total_charged_microcredits: i64,
    pub workstream_usage: std::collections::BTreeMap<String, i64>,
    pub workstream_limits: std::collections::BTreeMap<String, i64>,
    pub unresolved_count: i64,
}

impl BudgetUsageSummary {
    pub fn percent_used(&self) -> f64 {
        if self.total_limit_microcredits <= 0 {
            return 0.0;
        }
        (self.total_charged_microcredits as f64 / self.total_limit_microcredits as f64) * 100.0
    }

    pub fn is_threshold_exceeded(&self, threshold_pct: f64) -> bool {
        self.percent_used() >= threshold_pct
    }
}

impl LedgerReservation {
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Must succeed before any HTTP request. Consuming self prevents local reuse; the database
    /// state transition also protects against replay across processes or process restarts.
    pub async fn claim(self, query: u64) -> Result<SubmittedReservation, Error> {
        if query != self.query {
            return Err(Error::Refused("reservation query mismatch"));
        }
        let result = sqlx::query("UPDATE dune_reservations r SET state = 'submitted' FROM dune_budget_periods p WHERE r.id = $1 AND r.period_id = p.id AND r.state = 'reserved' AND now() >= p.starts_at AND now() < p.ends_at")
            .bind(self.id).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(Error::Refused("reservation no longer valid"));
        }
        Ok(SubmittedReservation {
            pool: self.pool,
            id: self.id,
        })
    }
}

impl SubmittedReservation {
    pub async fn attach_execution(self, execution: &str) -> Result<(), Error> {
        if execution.is_empty() || !execution.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::Refused("execution ID"));
        }
        let result = sqlx::query("UPDATE dune_reservations SET execution_id = $2 WHERE id = $1 AND state = 'submitted' AND execution_id IS NULL")
            .bind(self.id).bind(execution).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(Error::Refused("execution already attached"));
        }
        Ok(())
    }
}
