CREATE TABLE dune_budget_periods (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account text NOT NULL,
    starts_at timestamptz NOT NULL,
    ends_at timestamptz NOT NULL CHECK (ends_at > starts_at),
    limit_microcredits bigint NOT NULL CHECK (limit_microcredits >= 0),
    CHECK (account <> ''),
    UNIQUE (account, starts_at)
);
CREATE TABLE dune_budget_allocations (
    period_id bigint NOT NULL REFERENCES dune_budget_periods(id),
    workstream text NOT NULL,
    limit_microcredits bigint NOT NULL CHECK (limit_microcredits >= 0),
    PRIMARY KEY (period_id, workstream)
);
CREATE TABLE dune_reservations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    period_id bigint NOT NULL,
    workstream text NOT NULL,
    query_id bigint NOT NULL CHECK (query_id > 0),
    api_key_name text NOT NULL,
    estimate_microcredits bigint NOT NULL CHECK (estimate_microcredits > 0),
    actual_microcredits bigint CHECK (actual_microcredits >= 0),
    state text NOT NULL DEFAULT 'reserved' CHECK (state IN ('reserved', 'submitted', 'settled')),
    created_at timestamptz NOT NULL DEFAULT now(),
    execution_id text UNIQUE,
    FOREIGN KEY (period_id, workstream) REFERENCES dune_budget_allocations(period_id, workstream)
);
CREATE INDEX dune_reservations_period ON dune_reservations(period_id, workstream);
CREATE VIEW dune_budget_usage AS
SELECT period_id, workstream,
       sum(coalesce(actual_microcredits, estimate_microcredits)) AS charged_microcredits,
       count(*) FILTER (WHERE state <> 'settled') AS unresolved
FROM dune_reservations GROUP BY period_id, workstream;
