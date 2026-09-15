//! Names on an indexer's page: the verified name on its account, from Postgres, and the name of
//! each deployment its allocations point at, from IPFS documents keyed off the GNS nest.
//!
//! Put on the answer when it is fetched, so they are cached with it. Put on after the cache, every
//! request for the largest indexer's page named its 2,022 allocations again, and took 10.5 s from
//! the cache (2026-09-15).

use std::collections::BTreeMap;
use std::time::Duration;

use kittiwake_db::Pool;
use kittiwake_ext::Ipfs;
use kittiwake_nest::{Lane, NestGate};

use crate::{search, shape};

/// How long a deployment's name is kept. A rename is rare, and a document IPFS would not give is
/// not asked for again for as long in any case.
pub const NAME_TTL: Duration = Duration::from_secs(60 * 60);

/// A bound on memory rather than a working set: an entry is a hash and a name.
const NAMES_HELD: u64 = 50_000;

/// Cheap to clone; clones share the kept names.
#[derive(Clone)]
pub struct Naming {
    pool: Pool,
    ipfs: Ipfs,
    deployments: moka::future::Cache<String, Option<String>>,
}

impl Naming {
    pub fn new(pool: Pool, ipfs: Ipfs) -> Self {
        Self::with_ttl(pool, ipfs, NAME_TTL)
    }

    pub fn with_ttl(pool: Pool, ipfs: Ipfs, ttl: Duration) -> Self {
        Self {
            pool,
            ipfs,
            deployments: moka::future::Cache::builder()
                .max_capacity(NAMES_HELD)
                .time_to_live(ttl)
                .build(),
        }
    }

    /// Name the account and the allocations on `/api/indexer`'s answer, asking on `lane`.
    ///
    /// A name is decoration on the figures: one that cannot be read is left null, never an error.
    pub async fn indexer_page(&self, gate: &NestGate, lane: Lane, value: &mut serde_json::Value) {
        let ids = shape::indexer_account_ids(value);
        if !ids.is_empty() {
            match kittiwake_db::indexer_names(&self.pool, &ids).await {
                Ok(names) => shape::put_indexer_names(value, &names),
                Err(e) => tracing::warn!("indexer names unreadable, serving without them: {e}"),
            }
        }
        let Some(indexer) = value.get_mut("indexer") else {
            return;
        };
        let hashes = search::allocation_deployment_hashes(indexer);
        if hashes.is_empty() {
            return;
        }
        let names = self.deployment_names(gate, lane, hashes).await;
        search::apply_allocation_names(indexer, &names);
    }

    /// Names by `Qm…` hash: the kept ones, and the rest from the GNS nest and IPFS.
    async fn deployment_names(
        &self,
        gate: &NestGate,
        lane: Lane,
        hashes: Vec<String>,
    ) -> BTreeMap<String, Option<String>> {
        let mut names = BTreeMap::new();
        let mut unknown = Vec::new();
        for hash in hashes {
            match self.deployments.get(&hash).await {
                Some(name) => {
                    names.insert(hash, name);
                }
                None => unknown.push(hash),
            }
        }
        if unknown.is_empty() {
            return names;
        }
        match search::display_names_for_deployments(&self.pool, &self.ipfs, gate, lane, &unknown)
            .await
        {
            Ok(found) => {
                for (hash, name) in found {
                    self.deployments.insert(hash.clone(), name.clone()).await;
                    names.insert(hash, name);
                }
            }
            // Nothing is kept, so the next fetch asks again.
            Err(e) => {
                tracing::debug!(err = %e, "allocation deployment names unavailable; the table keeps its hashes");
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const DEPLOYMENT: &str = "QmStfa6zwPUzKHyGrgdNMh8rgbc91sgvmskidBSujG92FR";
    const METADATA: &str = "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff";

    /// The GNS nest and IPFS behind one stand-in, recording every call: each deployment asked about
    /// was published with one metadata document, and that document names it.
    struct Upstream(Arc<Mutex<Vec<String>>>);

    impl Respond for Upstream {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let path = req.url.path().to_string();
            if path.ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            self.0.lock().unwrap().push(path.clone());
            if path == "/api/v0/cat" {
                return ResponseTemplate::new(200)
                    .set_body_string(r#"{"displayName":"Uniswap V3"}"#);
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            let rows: Vec<serde_json::Value> = q
                .split('\'')
                .skip(1)
                .step_by(2)
                .filter(|s| s.len() == 66 && s.starts_with("0x"))
                .map(|id| {
                    serde_json::json!({
                        "deployment_id": id, "subgraph_id": "1",
                        "subgraph_metadata": METADATA, "version_metadata": null
                    })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": rows.len(), "rows": rows,
                "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
            }))
        }
    }

    fn page() -> serde_json::Value {
        serde_json::json!({ "indexer": {
            "allocations": [{ "subgraphDeployment": { "ipfsHash": DEPLOYMENT, "displayName": null } }],
            "closedAllocations": [],
        }})
    }

    fn name(page: &serde_json::Value) -> &serde_json::Value {
        &page["indexer"]["allocations"][0]["subgraphDeployment"]["displayName"]
    }

    #[tokio::test]
    async fn a_deployment_is_named_once_and_asked_about_again_only_after_its_ttl() {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(any())
            .respond_with(Upstream(seen.clone()))
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in kittiwake_types::NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let gate = NestGate::new(cfg).unwrap();
        // Never reached: the document cache misses, so every document asked for is asked of IPFS.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("a lazy pool needs no server");
        let ttl = Duration::from_secs(1);
        let naming = Naming::with_ttl(pool, Ipfs::new(&server.uri()), ttl);
        let asked = |prefix: &str| {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with(prefix))
                .count()
        };

        let mut first = page();
        naming.indexer_page(&gate, Lane::Read, &mut first).await;
        assert_eq!(name(&first), "Uniswap V3");
        assert_eq!((asked("/gns/"), asked("/api/v0/cat")), (1, 1));

        let mut again = page();
        naming.indexer_page(&gate, Lane::Read, &mut again).await;
        assert_eq!(name(&again), "Uniswap V3");
        assert_eq!(
            (asked("/gns/"), asked("/api/v0/cat")),
            (1, 1),
            "a kept name asks neither the GNS nest nor IPFS"
        );

        tokio::time::sleep(ttl + Duration::from_millis(200)).await;
        let mut later = page();
        naming.indexer_page(&gate, Lane::Read, &mut later).await;
        assert_eq!(name(&later), "Uniswap V3");
        assert_eq!(asked("/gns/"), 2, "a name past its TTL is asked for again");
    }
}
