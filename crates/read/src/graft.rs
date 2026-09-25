//! Graft children known to the IPFS document cache.

use kittiwake_db::Pool;
use kittiwake_ext::Ipfs;
use kittiwake_nest::{Lane, NestGate};
use kittiwake_types::ApiError;

use crate::{manifest, search, shape};

fn matching_grafts(base: &str, docs: Vec<(String, String)>) -> Vec<(String, i64)> {
    docs.into_iter()
        .filter_map(|(cid, yaml)| {
            let graft = manifest::analyse(&yaml).ok()?.graft?;
            (graft.base == base).then_some((cid, graft.block))
        })
        .collect()
}

/// This lookup covers manifests fetched into the cache by the warm job or by a reader. A hash
/// absent from the cache cannot be searched, so callers must describe these as known children.
pub async fn children(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    base: &str,
) -> Result<serde_json::Value, ApiError> {
    let docs: Vec<(String, String)> =
        sqlx::query_as("SELECT cid, text FROM ipfs_metadata WHERE text LIKE $1")
            .bind(format!("%{base}%"))
            .fetch_all(pool)
            .await
            .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;
    let mut grafts = matching_grafts(base, docs);
    grafts.sort_by(|a, b| a.0.cmp(&b.0));
    let ids: Vec<String> = grafts
        .iter()
        .filter_map(|(cid, _)| shape::ipfs_hash_to_bytes32(cid))
        .collect();
    let metadata = search::subgraph_metadata_for_deployments(pool, ipfs, gate, lane, &ids).await?;

    let children: Vec<serde_json::Value> = grafts
        .into_iter()
        .map(|(cid, block)| {
            let meta =
                shape::ipfs_hash_to_bytes32(&cid).and_then(|id| metadata.get(&id.to_lowercase()));
            serde_json::json!({
                "ipfsHash": cid,
                "graftBlock": block,
                "displayName": meta.and_then(|m| m.display_name()),
                "label": meta.and_then(|m| m.version.as_ref())
                    .and_then(|v| v.get("label"))
                    .and_then(|v| v.as_str()),
            })
        })
        .collect();
    Ok(serde_json::json!({ "children": children, "source": "cached_manifests" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_mentioned_elsewhere_is_not_a_graft_child() {
        let base = "QmBase";
        let docs = vec![
            (
                "QmChild".into(),
                "specVersion: 1.0.0\ngraft:\n  base: QmBase\n  block: 505750185\n".into(),
            ),
            (
                "QmOther".into(),
                "specVersion: 1.0.0\ndescription: QmBase\ngraft:\n  base: QmElse\n  block: 4\n"
                    .into(),
            ),
        ];
        assert_eq!(
            matching_grafts(base, docs),
            vec![("QmChild".into(), 505750185)]
        );
    }
}
