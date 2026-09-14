//! Shared Postgres admission for all Dune execution callers. Uncertain submissions remain charged.

use sqlx::PgPool;

pub const MIGRATION: &str = include_str!("../migrations/001_ledger.sql");
pub const MIGRATION_002: &str = include_str!("../migrations/002_account_ceiling.sql");

/// Every migration in order. Apply all of them; `MIGRATION` alone leaves the account ceiling
/// columns absent and `sync_account_usage` will fail against such a database.
pub const MIGRATIONS: &[&str] = &[MIGRATION, MIGRATION_002];

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
    /// When set, `reserve` refuses unless the account's Dune-reported usage was refreshed within
    /// this long. Admitting spend against an unverified account balance is exactly what a hard
    /// subscription ceiling forbids, so the safe failure is to refuse work, not to guess.
    max_observation_age: Option<std::time::Duration>,
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
        Self {
            pool,
            max_observation_age: None,
        }
    }

    /// Require the account's Dune-reported usage to be no older than `age` before admitting any
    /// reservation. Without this the ceiling still applies, but only to the last observation that
    /// happened to be taken, which may be arbitrarily stale. Production should set it.
    pub fn requiring_fresh_observation(mut self, age: std::time::Duration) -> Self {
        self.max_observation_age = Some(age);
        self
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
        // Freshness is compared in SQL against the same clock that wrote `observed_at`, which
        // avoids both a sqlx date-time feature and any disagreement between two machines' clocks.
        let max_age_secs = self
            .max_observation_age
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let period_row: Option<(i64, i64, bool)> = sqlx::query_as(
            "SELECT limit_microcredits, observed_used_microcredits, \
             (observed_at IS NOT NULL AND observed_at >= now() - make_interval(secs => $2)) \
             FROM dune_budget_periods \
             WHERE id = $1 AND now() >= starts_at AND now() < ends_at FOR UPDATE",
        )
        .bind(period)
        .bind(max_age_secs)
        .fetch_optional(&mut *tx)
        .await?;
        let (limit, observed_used, observation_fresh) =
            period_row.ok_or(Error::Refused("inactive billing period"))?;

        // Refuse to spend against an account balance we have not checked recently enough. A stale
        // reading is indistinguishable from a low one, and the ceiling is not negotiable.
        if self.max_observation_age.is_some() && !observation_fresh {
            return Err(Error::Refused(
                "account usage observation is missing or stale",
            ));
        }

        let allocation: Option<i64> = sqlx::query_scalar("SELECT limit_microcredits FROM dune_budget_allocations WHERE period_id = $1 AND workstream = $2")
            .bind(period).bind(workstream).fetch_optional(&mut *tx).await?;
        let allocation = allocation.ok_or(Error::Refused("unknown workstream"))?;

        // Two independent lower bounds on what this period has committed, neither of which may be
        // trusted alone:
        //
        //   ledger_charged  - everything this ledger admitted. Blind to any execution started
        //                     outside it (Dune web UI, a Dune-side schedule, another key holder).
        //   observed + in-flight - Dune's own billed figure, plus the reservations we authorised
        //                     that Dune may not have billed yet. Blind to nothing Dune charged,
        //                     but lags by however old the observation is.
        //
        // Taking the greater of the two means neither blind spot can let spending through.
        let fits: bool = sqlx::query_scalar(
            "SELECT greatest( \
                 coalesce(sum(coalesce(actual_microcredits, estimate_microcredits)), 0), \
                 $6::bigint + coalesce(sum(estimate_microcredits) \
                     FILTER (WHERE state IN ('reserved', 'submitted')), 0) \
             ) + $2::numeric <= $3::numeric \
             AND coalesce(sum(coalesce(actual_microcredits, estimate_microcredits)) \
                 FILTER (WHERE workstream = $4), 0) + $2::numeric <= $5::numeric \
             FROM dune_reservations WHERE period_id = $1",
        )
        .bind(period)
        .bind(estimate)
        .bind(limit)
        .bind(workstream)
        .bind(allocation)
        .bind(observed_used)
        .fetch_one(&mut *tx)
        .await?;
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

    /// Refreshes the period's record of what Dune says the account has actually spent, and returns
    /// the figure now stored.
    ///
    /// Monotonic within a period: a reading lower than the one already stored is kept out, because
    /// Dune's counter only rises and a fall means a stale or partial response, never a refund.
    /// `observed_at` is still advanced in that case, since the fetch did succeed and the stored
    /// figure remains a valid lower bound.
    ///
    /// A failed lookup leaves both the figure and its timestamp untouched, so a configured
    /// staleness limit will eventually refuse admission rather than let work through on a number
    /// nobody has confirmed.
    pub async fn sync_account_usage(
        &self,
        period: i64,
        source: &impl AccountUsageSource,
    ) -> Result<i64, Error> {
        let Some(usage) = source.account_usage().await else {
            return Err(Error::Refused("account usage lookup failed"));
        };
        if usage.used_microcredits < 0 || usage.included_microcredits < 0 {
            return Err(Error::Refused("negative account usage"));
        }
        let stored: Option<i64> = sqlx::query_scalar(
            "UPDATE dune_budget_periods \
             SET observed_used_microcredits = greatest(observed_used_microcredits, $2), \
                 observed_included_microcredits = $3, \
                 observed_at = now() \
             WHERE id = $1 \
             RETURNING observed_used_microcredits",
        )
        .bind(period)
        .bind(usage.used_microcredits)
        .bind(usage.included_microcredits)
        .fetch_optional(&self.pool)
        .await?;
        stored.ok_or(Error::Refused("unknown billing period"))
    }

    /// `sync_account_usage` for whichever period is active for `account` right now. The scheduler
    /// knows an account name, not a period ID, so this is what a background refresh job calls.
    pub async fn sync_active_period(
        &self,
        account: &str,
        source: &impl AccountUsageSource,
    ) -> Result<i64, Error> {
        let period: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM dune_budget_periods \
             WHERE account = $1 AND now() >= starts_at AND now() < ends_at",
        )
        .bind(account)
        .fetch_optional(&self.pool)
        .await?;
        let period = period.ok_or(Error::Refused("no active billing period"))?;
        self.sync_account_usage(period, source).await
    }

    /// What the account has left before the configured ceiling, in microcredits, taking the same
    /// two lower bounds `reserve` uses. Negative means the ceiling is already breached.
    pub async fn remaining_headroom(&self, period: i64) -> Result<i64, Error> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT p.limit_microcredits, \
             greatest( \
                 coalesce(sum(coalesce(r.actual_microcredits, r.estimate_microcredits)), 0), \
                 p.observed_used_microcredits + coalesce(sum(r.estimate_microcredits) \
                     FILTER (WHERE r.state IN ('reserved', 'submitted')), 0) \
             )::bigint \
             FROM dune_budget_periods p \
             LEFT JOIN dune_reservations r ON r.period_id = p.id \
             WHERE p.id = $1 \
             GROUP BY p.id, p.limit_microcredits, p.observed_used_microcredits",
        )
        .bind(period)
        .fetch_optional(&self.pool)
        .await?;
        let (limit, committed) = row.ok_or(Error::Refused("unknown billing period"))?;
        Ok(limit - committed)
    }
}

/// The account's own billing figures, as Dune reports them for the current period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountUsage {
    /// Credits Dune has billed this period, in microcredits, rounded up.
    pub used_microcredits: i64,
    /// The plan's included allowance this period, in microcredits. Recorded so a plan change
    /// under a configured ceiling is visible rather than silent.
    pub included_microcredits: i64,
}

/// Reads the account's real usage from Dune. Implemented by the HTTP client; the trait lives here
/// so the ledger keeps no dependency on it, for the same reason as `ExecutionCostSource`.
///
/// The underlying endpoint is not billed, so calling it often is free. That is what makes it
/// usable as an admission input rather than only a monthly reconciliation report.
pub trait AccountUsageSource {
    fn account_usage(&self) -> impl std::future::Future<Output = Option<AccountUsage>> + Send;
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
