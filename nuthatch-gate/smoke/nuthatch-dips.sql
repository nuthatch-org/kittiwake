-- What kittiwake sends the dips nest (/dips). One statement per line.
-- kittiwake ingest/src/refresh.rs directory refresh: DIPS_CURRENT_ALLOCATION_SQL
SELECT * FROM dips_current_allocation
-- kittiwake read/src/routes.rs dips(): dips_latest_per_target_sql("issuance_allocator__issuance_distributed")
SELECT d.target, d.block_number, d.block_timestamp, d.amount_dec FROM "issuance_allocator__issuance_distributed" d JOIN (SELECT target AS t, max(block_number) AS bn FROM "issuance_allocator__issuance_distributed" GROUP BY target) l ON d.target = l.t AND d.block_number = l.bn
-- kittiwake read/src/routes.rs dips_agreements(): one of its nine stage reads
SELECT * FROM recurring_collector__agreement_accepted LIMIT 5000
