-- The ledger's own sum only covers work it admitted. An execution started from the Dune web UI, a
-- Dune-side schedule, or any other holder of the same API key spends from the same account and is
-- invisible to `dune_reservations`. Admission that trusts only its own sum will therefore authorise
-- spending that the account cannot afford.
--
-- `observed_used_microcredits` carries Dune's own usage figure for the period, so admission can be
-- checked against what the account has actually been billed. It is monotonic within a period:
-- Dune's counter only rises, and a lower reading means a stale or failed fetch, never a refund.
ALTER TABLE dune_budget_periods
    ADD COLUMN observed_used_microcredits bigint NOT NULL DEFAULT 0
        CHECK (observed_used_microcredits >= 0),
    ADD COLUMN observed_included_microcredits bigint
        CHECK (observed_included_microcredits IS NULL OR observed_included_microcredits >= 0),
    ADD COLUMN observed_at timestamptz;

COMMENT ON COLUMN dune_budget_periods.observed_used_microcredits IS
    'Dune-reported credits used for this period, in microcredits. Ground truth for spend that did not pass through this ledger.';
COMMENT ON COLUMN dune_budget_periods.observed_included_microcredits IS
    'Dune-reported included allowance, in microcredits. Recorded to detect a plan change under the configured ceiling.';
COMMENT ON COLUMN dune_budget_periods.observed_at IS
    'When observed_used_microcredits was last refreshed. NULL means never; admission may be configured to refuse on staleness.';
