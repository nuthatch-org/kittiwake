-- What kittiwake sends the GNS nest (/gns). One statement per line.
-- kittiwake ingest/src/names.rs names(): DEFAULT_NAMES_SQL
SELECT account, name, name_system, name_identifier FROM account_default_names
-- kittiwake read/src/search.rs metadata_cids_by_deployment()
SELECT LOWER(deployment_id) AS deployment_id, subgraph_metadata FROM (SELECT deployment_id, subgraph_metadata, ROW_NUMBER() OVER (PARTITION BY LOWER(deployment_id) ORDER BY created_at DESC, subgraph_id) AS rn FROM deployment_subgraphs) WHERE rn = 1
-- kittiwake read/src/search.rs warm(): SUBGRAPH_METADATA_SQL
SELECT subgraph_metadata FROM subgraph_current WHERE NOT deprecated AND subgraph_metadata IS NOT NULL
-- kittiwake read/src/routes.rs developer_activity(): subgraphs_published_since_sql, a twelve-month cutoff
SELECT block_timestamp AS "createdAt" FROM "gns__subgraph_published" WHERE block_timestamp >= 1759708800 ORDER BY block_timestamp
