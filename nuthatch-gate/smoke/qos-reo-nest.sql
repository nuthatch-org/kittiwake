-- What kittiwake's QoS day job sends the QoS nest (/qos) for one day, here 2026-10-01 (unix day 20727).
-- Never a full-history count: production asks one day at a time.
-- kittiwake ingest/src/qos_days.rs probe_day(): qos_day_probe_sql
SELECT (SELECT sum(buckets_stored) FROM qos_day_resolution WHERE day = DATE '1970-01-01' + 20727 AND topic = 'indexer') AS indexer_buckets, (SELECT sum(buckets_stored) FROM qos_day_resolution WHERE day = DATE '1970-01-01' + 20727 AND topic = 'query') AS query_buckets, (SELECT sum(unresolved_documents) FROM qos_day_resolution WHERE day = DATE '1970-01-01' + 20727) AS unresolved_documents, (SELECT max(closing_block) FROM qos_day_resolution WHERE day = DATE '1970-01-01' + 20727) AS closing_block, (SELECT max(bucket_start) FROM qos_posting) AS newest_posted_bucket_start
-- kittiwake ingest/src/qos_days.rs compute_day(): qos_indexer_day_sql
SELECT indexer, CAST(day AS VARCHAR) AS date, query_count, num_200, success_rate, latency_ms, blocks_behind, total_query_fees, avg_query_fee, allocations, buckets, bad_buckets, worst_bucket_start, worst_bucket_success_rate, worst_bucket_queries FROM qos_indexer_daily WHERE day = DATE '1970-01-01' + 20727
-- kittiwake ingest/src/qos_days.rs compute_day(): qos_seconds_behind_day_sql
SELECT indexer, seconds_behind_peers, share_queries_over_5min_behind, share_with_block_time FROM qos_indexer_seconds_behind WHERE day = DATE '1970-01-01' + 20727
-- kittiwake ingest/src/qos_days.rs compute_day(): qos_network_rows_sql
SELECT indexer, deployment, chain, gateway, datediff('day', DATE '1970-01-01', day) AS unix_day, query_count, num_200, latency_ms, blocks_behind, total_query_fees, buckets FROM qos_allocation_daily WHERE day = DATE '1970-01-01' + 20727
