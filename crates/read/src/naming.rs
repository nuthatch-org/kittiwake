//! Names on an indexer's page: the verified name on its account, from Postgres, and the name of
//! each deployment its allocations point at, from IPFS documents keyed off the GNS nest. And the
//! labels on the subgraph directory: those names, their categories, and what each deployment's
//! manifest says about its network and complexity.
//!
//! Put on the answer when it is fetched, so they are cached with it. Put on after the cache, every
//! request for the largest indexer's page named its 2,022 allocations again, and took 10.5 s from
//! the cache (2026-09-15).

use std::collections::{BTreeMap, HashMap};
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

/// Manifests read from Postgres at once, so the texts in memory stay a few megabytes. The first
/// refresh after a start reads all of them, about 44 MB; analysing them is cheap, 81 real ones in
/// 23 ms including the largest at 1.7 MB.
const MANIFEST_BATCH: usize = 250;

/// What a manifest says, kept for good because a CID never changes. Empty for one that does not
/// parse.
#[derive(Debug, Clone, Default)]
struct ManifestFacts {
    network: Option<String>,
    complexity: Option<String>,
    start_block: Option<i64>,
}

impl ManifestFacts {
    fn of(yaml: &str) -> Self {
        let Ok(a) = crate::manifest::analyse(yaml) else {
            return Self::default();
        };
        Self {
            // `analyse` says "unknown" for a manifest with no data sources, which is not a network.
            network: Some(a.network).filter(|n| n != "unknown"),
            complexity: serde_json::to_value(&a.category)
                .ok()
                .and_then(|c| c.as_str().map(str::to_string)),
            start_block: a.data_sources.iter().map(|s| s.start_block).min(),
        }
    }
}

/// Cheap to clone; clones share the kept names.
#[derive(Clone)]
pub struct Naming {
    pool: Pool,
    ipfs: Ipfs,
    deployments: moka::future::Cache<String, Option<String>>,
    manifests: moka::future::Cache<String, ManifestFacts>,
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
            manifests: moka::future::Cache::builder()
                .max_capacity(NAMES_HELD)
                .build(),
        }
    }

    /// Label the directory's rows in place, and say how many have no manifest read yet.
    ///
    /// A label that cannot be read is left empty, never an error. The unread count is what keeps
    /// that honest, because a row with no network cannot match a network filter.
    pub async fn label_directory(
        &self,
        gate: &NestGate,
        lane: Lane,
        rows: &mut [serde_json::Value],
    ) -> usize {
        let field = |r: &serde_json::Value, k: &str| r.get(k)?.as_str().map(str::to_string);
        let id_of = |r: &serde_json::Value| field(r, "id").map(|id| id.to_lowercase());

        // From the document cache alone: on 2026-09-19 it held 5,367 of the 5,389 the directory
        // wants, and fetching the rest from IPFS inline cost the 11.8 s a thousand rows once took.
        let publishers = search::metadata_cids_by_deployment(gate, lane)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("directory names unreadable, serving without them: {e}");
                Default::default()
            });
        let wanted: Vec<String> = rows
            .iter()
            .filter_map(|r| publishers.get(&id_of(r)?).cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let documents: HashMap<String, serde_json::Value> =
            match kittiwake_ext::cached_docs(&self.pool, &wanted).await {
                Ok(docs) => docs
                    .into_iter()
                    .filter_map(|d| Some((d.cid, d.json?)))
                    .collect(),
                Err(e) => {
                    tracing::warn!("directory names unreadable, serving without them: {e}");
                    HashMap::new()
                }
            };

        let mut facts: HashMap<String, ManifestFacts> = HashMap::new();
        let mut unread = Vec::new();
        for cid in rows.iter().filter_map(|r| field(r, "ipfsHash")) {
            match self.manifests.get(&cid).await {
                Some(f) => {
                    facts.insert(cid, f);
                }
                None => unread.push(cid),
            }
        }
        let started = std::time::Instant::now();
        let mut analysed = 0;
        for batch in unread.chunks(MANIFEST_BATCH) {
            let docs = match kittiwake_ext::cached_docs(&self.pool, batch).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("directory manifests unreadable, counted as unread: {e}");
                    break;
                }
            };
            // One not fetched yet stays unread. The warm job fetches every signalled manifest.
            let texts: Vec<(String, String)> = docs
                .into_iter()
                .filter_map(|d| Some((d.cid, d.text?)))
                .collect();
            let Ok(read) = tokio::task::spawn_blocking(move || {
                texts
                    .into_iter()
                    .map(|(cid, yaml)| (cid, ManifestFacts::of(&yaml)))
                    .collect::<Vec<_>>()
            })
            .await
            else {
                break;
            };
            analysed += read.len();
            for (cid, f) in read {
                self.manifests.insert(cid.clone(), f.clone()).await;
                facts.insert(cid, f);
            }
        }
        if analysed > 0 {
            tracing::info!(
                analysed,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "directory manifests analysed"
            );
        }

        let speeds = self.sync_speeds(None).await;

        let mut unanalysed = 0;
        for r in rows.iter_mut() {
            if let Some(doc) = id_of(r)
                .and_then(|id| publishers.get(&id))
                .and_then(|cid| documents.get(cid))
            {
                if let Some(name) = doc.get("displayName").filter(|n| n.is_string()) {
                    r["displayName"] = name.clone();
                }
                let categories: Vec<&str> = doc
                    .get("categories")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str())
                    .collect();
                r["categories"] = serde_json::json!(categories);
            }
            let cid = field(r, "ipfsHash").unwrap_or_default();
            let fact = facts.get(&cid);
            match fact {
                Some(f) => {
                    r["network"] = serde_json::json!(f.network);
                    r["complexity"] = serde_json::json!(f.complexity);
                }
                None => unanalysed += 1,
            }
            crate::directory::put_difficulty(r, speeds.get(&cid), fact.and_then(|f| f.start_block));
        }
        unanalysed
    }

    /// The measured sync speeds, keyed by CID; only `cid`'s when one is named. Empty when the
    /// database cannot say, so the manifest estimate stands alone.
    async fn sync_speeds(&self, cid: Option<&str>) -> HashMap<String, crate::sync_speed::Speed> {
        let since = chrono::Utc::now() - chrono::Duration::days(crate::sync_speed::KEPT_DAYS);
        kittiwake_db::sync_speeds(&self.pool, since, cid)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("sync speeds unreadable, serving the manifest estimate alone: {e}");
                Vec::new()
            })
            .into_iter()
            .map(|(deployment, network, bph, cbph, indexers, head, at)| {
                (
                    deployment.clone(),
                    crate::sync_speed::Speed {
                        deployment,
                        network,
                        blocks_per_hour: bph,
                        chain_blocks_per_hour: cbph,
                        indexers: indexers.max(0) as usize,
                        head,
                        measured_at_ms: at,
                    },
                )
            })
            .collect()
    }

    /// Put the directory's `complexity`, `syncSpeed` and `difficulty` on one deployment's rows.
    pub async fn label_difficulty(&self, rows: &mut [serde_json::Value]) {
        for r in rows.iter_mut() {
            let Some(cid) = r
                .get("ipfsHash")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            let facts = match self.manifests.get(&cid).await {
                Some(f) => Some(f),
                None => kittiwake_ext::cached_docs(&self.pool, std::slice::from_ref(&cid))
                    .await
                    .ok()
                    .and_then(|d| d.into_iter().next())
                    .and_then(|d| d.text)
                    .map(|yaml| ManifestFacts::of(&yaml)),
            };
            if let Some(f) = &facts {
                r["complexity"] = serde_json::json!(f.complexity);
            }
            let speeds = self.sync_speeds(Some(&cid)).await;
            crate::directory::put_difficulty(
                r,
                speeds.get(&cid),
                facts.and_then(|f| f.start_block),
            );
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

    #[test]
    fn a_manifest_gives_its_network_and_complexity_and_one_that_will_not_parse_gives_neither() {
        let yaml = "specVersion: 0.0.5
dataSources:
  - kind: ethereum/contract
    name: Pool
    network: arbitrum-one
    source:
      address: '0x0000000000000000000000000000000000000001'
      startBlock: 1
    mapping:
      eventHandlers:
        - event: Swap(address)
          handler: handleSwap
";
        let f = ManifestFacts::of(yaml);
        assert_eq!(f.network.as_deref(), Some("arbitrum-one"));
        let category = crate::manifest::analyse(yaml).unwrap().category;
        assert_eq!(f.complexity, Some(format!("{category:?}")));

        assert_eq!(
            ManifestFacts::of("specVersion: 0.0.5\ndataSources: []\n").network,
            None
        );
        let junk = ManifestFacts::of(": : :");
        assert_eq!((junk.network, junk.complexity), (None, None));
    }

    /// Naming the directory's 5,594 rows through IPFS is what took a thousand of them 11.8 s, so it
    /// reads the document cache and nothing else.
    #[tokio::test]
    async fn the_directory_asks_the_gns_nest_once_and_never_ipfs() {
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
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("a lazy pool needs no server");
        let naming = Naming::new(pool, Ipfs::new(&server.uri()));

        let mut rows: Vec<serde_json::Value> = ["aa", "bb"]
            .iter()
            .map(|b| {
                crate::directory::row(
                    &serde_json::json!({ "id": format!("0x{}", b.repeat(32)) }),
                    None,
                )
            })
            .collect();
        let unread = naming.label_directory(&gate, Lane::Read, &mut rows).await;

        let seen = seen.lock().unwrap();
        let asked = |prefix: &str| seen.iter().filter(|p| p.starts_with(prefix)).count();
        assert_eq!((asked("/gns/"), asked("/api/v0/cat")), (1, 0));
        assert_eq!(rows[0]["displayName"], serde_json::Value::Null);
        assert_eq!(unread, 2, "no database, so no manifest was read");
    }
}
