CREATE TABLE IF NOT EXISTS dune_executions (
    execution_id text PRIMARY KEY,
    query_id text NOT NULL,
    executed_at timestamptz NOT NULL,
    row_count bigint NOT NULL CHECK (row_count >= 0)
);

CREATE TABLE IF NOT EXISTS dune_grt_price_daily (
    execution_id text NOT NULL REFERENCES dune_executions(execution_id),
    executed_at timestamptz NOT NULL,
    row_index integer NOT NULL,
    payload jsonb NOT NULL,
    PRIMARY KEY (execution_id, row_index)
);

CREATE TABLE IF NOT EXISTS dune_publications (
    query_id text PRIMARY KEY,
    execution_id text NOT NULL REFERENCES dune_executions(execution_id),
    executed_at timestamptz NOT NULL
);

CREATE VIEW v_grt_price_daily AS
SELECT (r.payload->>'day')::timestamptz AS day,
       (r.payload->>'open')::double precision AS open,
       (r.payload->>'high')::double precision AS high,
       (r.payload->>'low')::double precision AS low,
       (r.payload->>'close')::double precision AS close,
       (r.payload->>'samples')::bigint AS samples,
       r.execution_id, r.executed_at,
       (r.payload->>'day')::timestamptz + interval '1 day' <= r.executed_at AS complete_day
FROM dune_grt_price_daily r
JOIN dune_publications p ON p.execution_id = r.execution_id
WHERE p.query_id = 'grt_price_daily';
