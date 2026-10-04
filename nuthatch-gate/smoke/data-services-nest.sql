-- What kittiwake's service census sends the data services nest (/services). One statement per line.
-- kittiwake edge/src/handlers.rs registry_events(): census::registry_sql for Dispatch
SELECT provider, endpoint, block_number, log_index, true AS registered FROM dispatch__provider_registered UNION ALL SELECT provider, NULL AS endpoint, block_number, log_index, false AS registered FROM dispatch__provider_deregistered
-- kittiwake edge/src/handlers.rs registry_events(): census::registry_sql for the Nuthatch Data Service
SELECT provider, endpoint, block_number, log_index, true AS registered FROM nuthatch_data_service__provider_registered UNION ALL SELECT provider, NULL AS endpoint, block_number, log_index, false AS registered FROM nuthatch_data_service__provider_deregistered
