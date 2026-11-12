-- The database this service reads and writes.
--
-- **This file is authoritative.** It moved here from `nuthatch-org/lodestar`'s
-- `scripts/schema.sql` on 2026-09-09, and a new column is added here and nowhere else. #31
--
-- WHY IT MOVED
--
-- The tables below are read on nearly every route and, since the crons were ported, written by
-- this service and no other. A repo that is to hold no server code (#29) cannot go on owning the
-- schema of a database it does not write to.
--
-- That is not only tidiness. #24 was a live 200-with-an-empty-array, and diagnosing it meant
-- reading the authoritative column types out of *another repository*. Doing so turned up four
-- wrongly-typed columns across two queries where the issue had recorded two, and none of them was
-- visible from inside this repository because the truth about the columns was a file somewhere
-- else. #2 then made the queries check themselves against this schema at compile time, which is
-- the cheaper half of the fix; this file is the structural half behind it.
--
-- HOW TO CHANGE IT
--
--   1. Edit here.
--   2. `bash deploy/sqlx-prepare.sh`, which builds a throwaway Postgres from this file and
--      re-checks every `query!` against it. A column that does not exist fails to compile.
--   3. Apply it to the box. Additive changes go in a `deploy/migrate-*.sh`; anything that alters
--      an existing table is a one-way door and wants its own decision - see `docs/decisions.md`
--      on the rollback being an environment variable.
--
-- Safe to re-run: every statement is `IF NOT EXISTS`.
--
-- WHAT IS NOT HERE
--
-- The Dock's tables are here because Kittiwake now owns their routes. `sync_bounties` is declared
-- below with the other durable write-side state, where the reconciliation job can keep its chain
-- cache honest.
--
-- `scuttlebutt_messages` and `scuttlebutt_bans` are declared below with their rewrite (#17).
--
-- `deployment_daily`, `indexer_qos_score` and `qos_daily` are the QoS oracle's, which
-- `migrations/020_drop_qos_oracle.sql` retired. Nothing here has ever read them and they are not
-- worth carrying into a file that describes what this service needs.

CREATE TABLE IF NOT EXISTS ingestion_state (
  key        TEXT PRIMARY KEY,
  last_epoch INTEGER,
  last_block INTEGER,
  last_id    TEXT,
  -- A cursor that is a time rather than a block, in unix seconds. `last_block` is an int4 and the
  -- RAV ingest kept a unix timestamp in it, which works until 19 January 2038 and then wraps
  -- negative rather than stalling, turning every delta run into a full backfill. #4
  last_timestamp BIGINT,
  updated_at TIMESTAMPTZ DEFAULT NOW()
);
ALTER TABLE ingestion_state ADD COLUMN IF NOT EXISTS last_timestamp BIGINT;

-- Seed required cursor rows (ingest scripts expect these to exist)
INSERT INTO ingestion_state (key) VALUES
  ('epochs'),
  ('allocations'),
  ('delegation_events'),
  ('disputes'),
  ('rav'),
  ('qos')
ON CONFLICT (key) DO NOTHING;

-- ── On-chain data ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS epochs (
  id                      INTEGER PRIMARY KEY,
  start_block             INTEGER,
  end_block               INTEGER,
  stake_deposited         NUMERIC,
  signalled_tokens        NUMERIC,
  total_rewards           NUMERIC,
  total_indexer_rewards   NUMERIC,
  total_delegator_rewards NUMERIC,
  total_query_fees        NUMERIC,
  query_fees_collected    NUMERIC,
  curator_query_fees      NUMERIC,
  query_fee_rebates       NUMERIC,
  taxed_query_fees        NUMERIC,
  -- EpochManager's exact L1 range (nuthatch-org/nuthatch#1882). Null until the allocations nest
  -- carries it; start_block/end_block are L2 blocks, and approximate once it does.
  start_l1_block          INTEGER,
  end_l1_block            INTEGER
);
ALTER TABLE epochs ADD COLUMN IF NOT EXISTS start_l1_block INTEGER;
ALTER TABLE epochs ADD COLUMN IF NOT EXISTS end_l1_block INTEGER;

CREATE TABLE IF NOT EXISTS indexers (
  address                          TEXT PRIMARY KEY,
  name                             TEXT,
  ens_name                         TEXT,
  url                              TEXT,
  geo_hash                         TEXT,
  created_at_epoch                 INTEGER,
  self_stake_grt                   NUMERIC,
  delegated_grt                    NUMERIC,
  allocated_grt                    NUMERIC,
  provisioned_grt                  NUMERIC,
  reward_cut                       NUMERIC,
  query_fee_cut                    NUMERIC,
  effective_cut                    NUMERIC,
  delegator_apr                    NUMERIC,
  delegation_capacity_pct          NUMERIC,
  over_delegation_dilution         NUMERIC,
  own_stake_ratio                  NUMERIC,
  indexer_rewards_own_ratio        NUMERIC,
  delegator_parameter_cooldown     INTEGER,
  last_delegation_param_update     INTEGER,
  reo_status                       TEXT,
  reo_source                       TEXT,
  reo_renewal_timestamp            BIGINT,
  reo_expires_at                   TIMESTAMPTZ,
  reo_days_remaining               INTEGER,
  score                            NUMERIC,
  score_grade                      TEXT,
  delegations_in_7d                INTEGER,
  undelegations_in_7d              INTEGER,
  net_flow_grt_7d                  NUMERIC,
  rewards_earned_grt               NUMERIC,
  query_fees_collected_grt         NUMERIC,
  allocation_count                 INTEGER,
  distinct_data_services           INTEGER DEFAULT 0,
  delegation_exchange_rate         NUMERIC,
  last_updated                     TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS indexer_snapshots (
  id                       SERIAL PRIMARY KEY,
  indexer_address          TEXT NOT NULL,
  epoch                    INTEGER,
  self_stake_grt           NUMERIC,
  delegated_grt            NUMERIC,
  delegated_thawing_grt    NUMERIC,
  allocated_grt            NUMERIC,
  reward_cut               NUMERIC,
  query_fee_cut            NUMERIC,
  delegator_apr            NUMERIC,
  delegation_capacity_pct  NUMERIC,
  query_fees_collected_grt NUMERIC,
  delegation_exchange_rate NUMERIC,
  score                    NUMERIC,
  score_grade              TEXT,
  snapshot_at              TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_indexer_snapshots_address_epoch
  ON indexer_snapshots (indexer_address, epoch DESC);

CREATE TABLE IF NOT EXISTS allocations (
  id                    TEXT PRIMARY KEY,
  indexer_address       TEXT NOT NULL,
  deployment_id         TEXT,
  allocated_tokens_grt  NUMERIC,
  created_epoch         INTEGER,
  closed_epoch          INTEGER,
  created_at            TIMESTAMPTZ,
  closed_at             TIMESTAMPTZ,
  signal_at_open        NUMERIC,
  poi                   TEXT,
  indexing_rewards_grt  NUMERIC,
  query_fees_grt        NUMERIC,
  status                TEXT
);

CREATE INDEX IF NOT EXISTS idx_allocations_indexer ON allocations (indexer_address);
CREATE INDEX IF NOT EXISTS idx_allocations_status  ON allocations (status);

-- RAV redemptions — historical query-fee revenue (GraphTallyCollector collections
-- + legacy rebates). Time-series behind indexer P&L. Stores COLLECTED tokens only.
CREATE TABLE IF NOT EXISTS rav_redemptions (
  id              TEXT PRIMARY KEY,
  indexer_address TEXT        NOT NULL,
  payer           TEXT,
  allocation_id   TEXT,
  deployment_id   TEXT,
  tokens_grt      NUMERIC     NOT NULL,
  source          TEXT        NOT NULL DEFAULT 'graphtally',  -- 'graphtally' | 'legacy_rebate'
  collected_at    TIMESTAMPTZ,
  block           INTEGER,
  chain_id        INTEGER     NOT NULL DEFAULT 42161,
  ingested_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_rav_indexer_collected
  ON rav_redemptions (indexer_address, collected_at DESC);
CREATE INDEX IF NOT EXISTS idx_rav_deployment_collected
  ON rav_redemptions (deployment_id, collected_at DESC);

CREATE TABLE IF NOT EXISTS delegation_events (
  id          TEXT PRIMARY KEY,
  event_type  TEXT,
  delegator   TEXT NOT NULL,
  indexer     TEXT NOT NULL,
  tokens_grt  NUMERIC,
  timestamp   TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_delegation_events_indexer   ON delegation_events (indexer);
CREATE INDEX IF NOT EXISTS idx_delegation_events_delegator ON delegation_events (delegator);
CREATE INDEX IF NOT EXISTS idx_delegation_events_timestamp ON delegation_events (timestamp DESC);

CREATE TABLE IF NOT EXISTS disputes (
  id                  TEXT PRIMARY KEY,
  dispute_type        TEXT,
  indexer_address     TEXT NOT NULL,
  fisherman           TEXT,
  allocation_id       TEXT,
  deployment_id       TEXT,
  status              TEXT,
  tokens_slashed_grt  NUMERIC,
  tokens_burned_grt   NUMERIC,
  created_at          TIMESTAMPTZ,
  closed_at           TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_disputes_indexer ON disputes (indexer_address);

CREATE TABLE IF NOT EXISTS parameter_changes (
  id               SERIAL PRIMARY KEY,
  indexer_address  TEXT NOT NULL,
  param_name       TEXT NOT NULL,
  old_value        NUMERIC,
  new_value        NUMERIC,
  epoch            INTEGER,
  created_at       TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_parameter_changes_indexer ON parameter_changes (indexer_address, created_at DESC);

CREATE TABLE IF NOT EXISTS network_snapshots (
  id                    SERIAL PRIMARY KEY,
  total_staked          NUMERIC,
  total_delegated       NUMERIC,
  total_signalled       NUMERIC,
  total_allocated       NUMERIC,
  total_supply_grt      NUMERIC,
  indexer_count         INTEGER,
  active_indexer_count  INTEGER,
  delegator_count       INTEGER,
  active_delegator_count INTEGER,
  curator_count         INTEGER,
  active_curator_count  INTEGER,
  subgraph_count        INTEGER,
  active_subgraph_count INTEGER,
  current_epoch         INTEGER,
  grt_price_usd         NUMERIC,
  network_tvl_usd       NUMERIC,
  snapshot_at           TIMESTAMPTZ DEFAULT NOW()
);

-- ── Scoring ───────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS indexer_scores (
  indexer_address           TEXT NOT NULL,
  period_type               TEXT NOT NULL,
  period_start              DATE NOT NULL,
  period_end                DATE NOT NULL,
  query_fee_score           NUMERIC,
  allocation_efficiency_score NUMERIC,
  delegator_apr_score       NUMERIC,
  effective_cut_score       NUMERIC,
  capacity_score            NUMERIC,
  cut_stability_score       NUMERIC,
  tenure_bonus              NUMERIC,
  retention_score           NUMERIC,
  reo_score                 NUMERIC,
  poi_consensus_score       NUMERIC,
  allocation_breadth_score  NUMERIC,
  community_vote_score      NUMERIC,
  data_service_score        NUMERIC,
  subtotal                  NUMERIC,
  penalty_multiplier        NUMERIC,
  final_score               NUMERIC,
  months_active             NUMERIC,
  is_eligible_for_badge     BOOLEAN DEFAULT FALSE,
  rank                      INTEGER,
  PRIMARY KEY (indexer_address, period_type, period_start)
);

CREATE INDEX IF NOT EXISTS idx_indexer_scores_period ON indexer_scores (period_type, period_start DESC);

-- ── Community features ────────────────────────────────────────────────────────

-- ── Operations ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS cron_runs (
  id            SERIAL PRIMARY KEY,
  step          TEXT NOT NULL,
  started_at    TIMESTAMPTZ NOT NULL,
  duration_ms   INTEGER NOT NULL,
  rows_affected INTEGER,
  success       BOOLEAN NOT NULL,
  error_message TEXT,
  created_at    TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_cron_runs_step_started ON cron_runs (step, started_at DESC);
-- `failing_jobs` windows on started_at alone, which the index above cannot serve.
CREATE INDEX IF NOT EXISTS idx_cron_runs_started ON cron_runs (started_at);

-- One row per probe round of /api/indexing-status/[hash] (RFC-006 D5). "Effectively dead" is
-- derived from the last K rows per deployment, never from one round (lodestar#59).
CREATE TABLE IF NOT EXISTS servability_rounds (
  id                     BIGSERIAL PRIMARY KEY,
  deployment_hash        TEXT NOT NULL,
  probed_at              TIMESTAMPTZ NOT NULL,
  serving_operator_count INTEGER NOT NULL,
  serving_indexer_count  INTEGER NOT NULL,
  gateway_verdict        TEXT,
  verdict_json           JSONB NOT NULL,
  created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_servability_rounds_hash_probed
  ON servability_rounds (deployment_hash, probed_at DESC);

-- Ownership. The application connects as `lodestar`; a migration run as the superuser creates
-- tables owned by `postgres`, and every statement from the app then fails with 42501 - silently, in

-- Trade clicks and protocol visits from the dashboard, written by /api/analytics/clickthrough.
--
-- The declaration moved here from lodestar's `scripts/migrate-clickthroughs.sql` when the route
-- did (#31 put the schema in the repository that writes it). The conversion columns are filled by
-- nothing at present: they were for a cron that checked whether a wallet that clicked through
-- actually swapped, and that cron does not exist. They are kept because the rows do.
CREATE TABLE IF NOT EXISTS clickthrough_events (
  id             BIGSERIAL PRIMARY KEY,
  event_type     TEXT NOT NULL CHECK (event_type IN ('trade_click', 'protocol_visit')),
  token_address  TEXT,
  token_symbol   TEXT,
  protocol_slug  TEXT,
  venue          TEXT NOT NULL,
  pool_address   TEXT,
  chain          TEXT NOT NULL DEFAULT 'mainnet',
  destination_url TEXT,
  wallet         TEXT,
  session_id     TEXT NOT NULL,
  clicked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  converted      BOOLEAN,
  converted_at   TIMESTAMPTZ,
  converted_tx   TEXT,
  converted_usd  NUMERIC(20, 4)
);

CREATE INDEX IF NOT EXISTS idx_ct_wallet  ON clickthrough_events (wallet) WHERE wallet IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ct_pending ON clickthrough_events (clicked_at) WHERE converted IS NULL AND wallet IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ct_venue   ON clickthrough_events (venue);

-- the case of a fire-and-forget writer (#113). Idempotent, and a no-op when already correct.
ALTER TABLE ingestion_state OWNER TO lodestar;
ALTER TABLE epochs OWNER TO lodestar;
ALTER TABLE indexers OWNER TO lodestar;
ALTER TABLE indexer_snapshots OWNER TO lodestar;
ALTER TABLE allocations OWNER TO lodestar;
ALTER TABLE rav_redemptions OWNER TO lodestar;
ALTER TABLE delegation_events OWNER TO lodestar;
ALTER TABLE disputes OWNER TO lodestar;
ALTER TABLE parameter_changes OWNER TO lodestar;
ALTER TABLE network_snapshots OWNER TO lodestar;
ALTER TABLE indexer_scores OWNER TO lodestar;
ALTER TABLE cron_runs OWNER TO lodestar;
ALTER TABLE servability_rounds OWNER TO lodestar;

-- ── The IPFS document cache ───────────────────────────────────────────────────
--
-- This table was missed when the schema moved (#31, #57), and it matters: `warm-ipfs` writes it and
-- name and address search read it, so a database built from this file alone came up without the one
-- table `/api/subgraph-search` cannot answer a single question without.
--
-- It was missed because it was never in `scripts/schema.sql`. It lives in lodestar's `migrations/`
-- directory, which the move did not look at - and the compile-time check did not catch the gap
-- because every statement touching this table is a runtime `sqlx::query` in `crates/ext`, so
-- nothing was checking it against anything. Two absences agreeing is not a confirmation.
--
-- A CID is its content, so a row never goes stale. A row with `error` set and `json` null is a
-- fetch that failed and is retried an hour later. `text` is for documents that are not JSON: a
-- deployment's manifest is YAML at the deployment's own CID.

CREATE TABLE IF NOT EXISTS ipfs_metadata (
  cid        TEXT PRIMARY KEY,
  json       JSONB,
  error      TEXT,
  fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE ipfs_metadata ADD COLUMN IF NOT EXISTS text TEXT;

-- Name search over what has been fetched. #43 measured the sequential scan behind it at 20 ms over
-- 26,775 rows, so this index is not what makes that route slow; do not go looking for a trigram
-- index on the strength of the route's latency.
CREATE INDEX IF NOT EXISTS idx_ipfs_metadata_display_name
  ON ipfs_metadata (LOWER(json->>'displayName'))
  WHERE json ? 'displayName';

-- ── The Dock ──────────────────────────────────────────────────────────────────
--
-- These are the current Studio tables, including the three later publication columns. Kittiwake
-- owns their routes now, so this is the one schema a fresh deployment must receive.

CREATE TABLE IF NOT EXISTS studio_subgraphs (
  id                             SERIAL PRIMARY KEY,
  owner_address                  TEXT NOT NULL,
  slug                           TEXT NOT NULL UNIQUE,
  display_name                   TEXT,
  description                    TEXT,
  deployment_id                  TEXT,
  network                        TEXT,
  published_subgraph_id          TEXT,
  version_label                  TEXT,
  last_published_deployment_id   TEXT,
  created_at                     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at                     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_studio_subgraphs_owner ON studio_subgraphs(owner_address);

CREATE TABLE IF NOT EXISTS studio_deploy_keys (
  id             SERIAL PRIMARY KEY,
  owner_address  TEXT NOT NULL UNIQUE,
  key_hash       TEXT NOT NULL,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_used_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS sync_bounties (
  id                 SERIAL PRIMARY KEY,
  deployment_id      TEXT NOT NULL,
  subgraph_id        INTEGER,
  developer_address  TEXT NOT NULL,
  amount_grt         TEXT NOT NULL,
  message            TEXT,
  status             TEXT NOT NULL DEFAULT 'open',  -- open | claimed | cancelled | expired
  claimed_by         TEXT,
  claimed_at         TIMESTAMPTZ,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  expires_at         TIMESTAMPTZ,
  chain_bounty_id    TEXT,
  post_tx_hash       TEXT
);

CREATE INDEX IF NOT EXISTS idx_sync_bounties_deployment ON sync_bounties(deployment_id);
CREATE INDEX IF NOT EXISTS idx_sync_bounties_status     ON sync_bounties(status);

-- ── Scuttlebutt ───────────────────────────────────────────────────────────────
--
-- The anonymous room. From lodestar's `migrations/009_scuttlebutt.sql`, brought across with the
-- rewrite (#17) - the messages and the bans are the part of that feature which has to outlive a
-- restart, and everything else about it does not need a store at all.
--
-- **Raw addresses are never stored.** `ip_hash` is an HMAC of the address under a server pepper:
-- enough to ban a repeat offender, useless as personal data if the table ever leaks. A change to
-- the pepper silently unbans everybody, which is why `kittiwake_scuttle` pins the hash against the
-- implementation it replaces.

CREATE TABLE IF NOT EXISTS scuttlebutt_messages (
  id            BIGSERIAL PRIMARY KEY,
  room          TEXT        NOT NULL DEFAULT 'main',
  name          TEXT,                       -- display name without the tripcode; NULL is Anonymous
  tripcode      TEXT,                       -- "!a8Df2xQ1z"; NULL when no #secret was given
  body          TEXT        NOT NULL,
  ip_hash       TEXT        NOT NULL,       -- HMAC(address, pepper); never the address
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  deleted       BOOLEAN     NOT NULL DEFAULT false,
  deleted_at    TIMESTAMPTZ,
  delete_reason TEXT
);

-- History is read newest-first within a room, with keyset pagination on the id.
CREATE INDEX IF NOT EXISTS idx_sb_msg_room_id ON scuttlebutt_messages (room, id DESC);

CREATE TABLE IF NOT EXISTS scuttlebutt_bans (
  id         BIGSERIAL PRIMARY KEY,
  ip_hash    TEXT,                          -- ban by address hash...
  tripcode   TEXT,                          -- ...and/or by tripcode
  reason     TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at TIMESTAMPTZ,                   -- NULL is permanent
  -- A ban with neither target bans nobody, and would sit in the list looking as though it did.
  CONSTRAINT sb_ban_target CHECK (ip_hash IS NOT NULL OR tripcode IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_sb_ban_iphash   ON scuttlebutt_bans (ip_hash);
CREATE INDEX IF NOT EXISTS idx_sb_ban_tripcode ON scuttlebutt_bans (tripcode);

-- ── Ours, and only ours ───────────────────────────────────────────────────────
--
-- The six tables this service created for itself. Nothing else reads or writes them, which is
-- what keeps the migration reversible: the old handlers do not know they exist and do not care.
-- `crates/db` also creates them at start-up, so a box that has not had this file applied still
-- comes up; the two must agree and this file is the one to read.

CREATE TABLE IF NOT EXISTS chain_head_records (
  network             TEXT PRIMARY KEY,
  head                BIGINT NOT NULL,
  head_first_seen_at  TIMESTAMPTZ NOT NULL,
  last_checked_at     TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS indexer_chains (
  address      TEXT PRIMARY KEY,
  chains       TEXT[] NOT NULL,
  captured_at  TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS live_answers (
  key          TEXT PRIMARY KEY,
  value        JSONB NOT NULL,
  computed_at  TIMESTAMPTZ NOT NULL
);

-- How fast indexers sync each deployment (lodestar#319). The chain-health job keeps each indexer's
-- last look at a deployment it is behind on, and a second look is a measured speed.
CREATE TABLE IF NOT EXISTS sync_observations (
  indexer       TEXT NOT NULL,
  deployment    TEXT NOT NULL,
  network       TEXT NOT NULL,
  latest_block  BIGINT NOT NULL,
  head_block    BIGINT NOT NULL,
  observed_at   TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (indexer, deployment)
);

CREATE TABLE IF NOT EXISTS sync_speeds (
  deployment             TEXT PRIMARY KEY,
  network                TEXT NOT NULL,
  blocks_per_hour        DOUBLE PRECISION NOT NULL,
  chain_blocks_per_hour  DOUBLE PRECISION NOT NULL,
  indexers               INTEGER NOT NULL,
  head_block             BIGINT NOT NULL,
  measured_at            TIMESTAMPTZ NOT NULL
);

-- The rolling APYs live beside `indexers` rather than in it: that table is the platform's and a
-- column there would be a schema change in another repository, which is the problem this file
-- exists to end. A side table joined on read costs one join and stays reversible.
CREATE TABLE IF NOT EXISTS indexer_rolling_apy (
  indexer_address  TEXT PRIMARY KEY,
  rolling_apy_30d  NUMERIC,
  rolling_apy_90d  NUMERIC,
  computed_at      TIMESTAMPTZ NOT NULL
);

-- Each indexer's per-dimension score, written by `refresh` beside the composite in `indexers` and
-- weighted by the delegate sliders. A side table for the same reason as the APYs. #87
CREATE TABLE IF NOT EXISTS indexer_score_breakdown (
  indexer_address  TEXT PRIMARY KEY,
  breakdown        JSONB NOT NULL,
  computed_at      TIMESTAMPTZ NOT NULL
);

-- The directory's QoS Quality score, recomputed by `score-qos` over thirty days of qos-reo-nest.
-- A side table for the same reason as the APYs, and not named `indexer_qos_score`: that was the
-- incumbent's per-day history, and lodestar's migration 020 that drops it is run by hand.
CREATE TABLE IF NOT EXISTS indexer_qos_quality (
  indexer_address  TEXT PRIMARY KEY,
  day              DATE NOT NULL,
  q_score          DOUBLE PRECISION NOT NULL,
  reliability      DOUBLE PRECISION NOT NULL,
  lat_util         DOUBLE PRECISION NOT NULL,
  fresh_util       DOUBLE PRECISION,
  coverage         DOUBLE PRECISION NOT NULL,
  served_gap       DOUBLE PRECISION,
  efficiency       DOUBLE PRECISION,
  computed_at      TIMESTAMPTZ NOT NULL
);

-- qos-reo-nest's daily results, kept here because the nest cannot answer more than one day per
-- statement inside its memory budget. One `qos_day` row per UTC day, carrying the provenance the
-- day's rows were computed from and what the job compares to decide whether to compute it again.
CREATE TABLE IF NOT EXISTS qos_day (
  day              DATE PRIMARY KEY,
  registry_hash    TEXT,
  nid              TEXT,
  as_of            BIGINT,
  sealed_through   BIGINT,
  closing_block    BIGINT,
  indexer_buckets  INTEGER NOT NULL,
  query_buckets    INTEGER NOT NULL,
  settled          BOOLEAN NOT NULL,
  computed_at      TIMESTAMPTZ NOT NULL
);

-- `qos_indexer_daily` joined to `qos_indexer_seconds_behind`, per indexer per day: the chart.
CREATE TABLE IF NOT EXISTS qos_indexer_day (
  indexer                         TEXT NOT NULL,
  day                             DATE NOT NULL,
  query_count                     DOUBLE PRECISION NOT NULL,
  num_200                         DOUBLE PRECISION,
  success_rate                    DOUBLE PRECISION,
  latency_ms                      DOUBLE PRECISION,
  blocks_behind                   DOUBLE PRECISION,
  total_query_fees                DOUBLE PRECISION NOT NULL,
  avg_query_fee                   DOUBLE PRECISION,
  allocations                     INTEGER,
  buckets                         INTEGER NOT NULL,
  bad_buckets                     INTEGER NOT NULL,
  worst_bucket_start              BIGINT,
  worst_bucket_success_rate       DOUBLE PRECISION,
  worst_bucket_queries            DOUBLE PRECISION,
  seconds_behind_peers            DOUBLE PRECISION,
  share_queries_over_5min_behind  DOUBLE PRECISION,
  share_with_block_time           DOUBLE PRECISION,
  gateways                        TEXT NOT NULL,
  PRIMARY KEY (indexer, day)
);

-- `qos_allocation_daily` for every indexer, per day: the score's input. A deployment's attempts on a
-- day are the sum of its rows, which is the served-share denominator. `chain` is '' when the oracle
-- named none, because a key column cannot be null.
CREATE TABLE IF NOT EXISTS qos_allocation_day (
  indexer           TEXT NOT NULL,
  deployment        TEXT NOT NULL,
  chain             TEXT NOT NULL,
  gateway           TEXT NOT NULL,
  day               DATE NOT NULL,
  query_count       DOUBLE PRECISION NOT NULL,
  num_200           DOUBLE PRECISION,
  latency_ms        DOUBLE PRECISION,
  blocks_behind     DOUBLE PRECISION,
  total_query_fees  DOUBLE PRECISION NOT NULL,
  buckets           INTEGER NOT NULL,
  PRIMARY KEY (indexer, deployment, chain, gateway, day)
);
CREATE INDEX IF NOT EXISTS qos_allocation_day_deployment ON qos_allocation_day (deployment, day);
