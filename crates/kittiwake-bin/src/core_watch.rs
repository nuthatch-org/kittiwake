//! Posts to Discord when a core subgraph publishes a new version.
//!
//! The network subgraph moved to a new version on 2026-11-24 with no notice to the indexers whose
//! indexer-service reads it. Nothing allocates to a new version before it is published, so the
//! publish is the earliest moment there is anything to see.

use std::collections::HashMap;
use std::time::Duration;

use kittiwake_config::{CoreSubgraph, DiscordWebhook};
use kittiwake_db::Pool;
use kittiwake_sched::{JobDef, JobOutcome};

const STEP: &str = "watch-core-subgraphs";
const PAGE: &str = "https://lodestar-dashboard.com/core-subgraphs";

/// The newest and previous versions of one core subgraph, as `/api/core-subgraphs` shapes them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Latest {
    pub subgraph_id: String,
    pub name: String,
    pub deployment: String,
    pub version: Option<i64>,
    /// The publisher's own label from the version metadata, such as `2.3.3`.
    pub label: Option<String>,
    pub created_at: Option<i64>,
    pub previous: Option<String>,
}

pub(crate) fn latest_from(answer: &serde_json::Value) -> Vec<Latest> {
    let mut out = Vec::new();
    for s in answer["subgraphs"].as_array().into_iter().flatten() {
        let versions = s["versions"].as_array();
        let Some(newest) = versions.and_then(|v| v.first()) else {
            continue;
        };
        let Some(deployment) = newest["ipfsHash"].as_str() else {
            continue;
        };
        out.push(Latest {
            subgraph_id: s["subgraphId"].as_str().unwrap_or_default().to_string(),
            name: s["name"].as_str().unwrap_or_default().to_string(),
            deployment: deployment.to_string(),
            version: newest["version"].as_i64(),
            label: newest["label"]
                .as_str()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string()),
            created_at: newest["createdAt"].as_i64(),
            previous: versions
                .and_then(|v| v.get(1))
                .and_then(|p| p["ipfsHash"].as_str())
                .map(str::to_string),
        });
    }
    out
}

/// What to post, and what to record, given the newest versions and what was last recorded. A
/// subgraph seen for the first time is recorded without a post, so adding one to the list does not
/// announce a version that may be months old.
pub(crate) fn decide(
    latest: &[Latest],
    announced: &HashMap<String, String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut posts = Vec::new();
    let mut record = Vec::new();
    for l in latest {
        match announced.get(&l.subgraph_id) {
            Some(d) if *d == l.deployment => {}
            Some(_) => {
                posts.push(message(l));
                record.push((l.subgraph_id.clone(), l.deployment.clone()));
            }
            None => record.push((l.subgraph_id.clone(), l.deployment.clone())),
        }
    }
    (posts, record)
}

pub(crate) fn message(l: &Latest) -> String {
    let version = match (&l.label, l.version) {
        (Some(label), _) => format!(" ({label})"),
        (None, Some(v)) => format!(" (version {v})"),
        _ => String::new(),
    };
    let when = l
        .created_at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| format!(", published {} UTC", t.format("%-d %b %H:%M")))
        .unwrap_or_default();
    let mut lines = vec![
        format!("**{}** has a new version{version}{when}.", l.name),
        format!("New: `{}`", l.deployment),
    ];
    if let Some(p) = &l.previous {
        lines.push(format!("Was: `{p}`"));
    }
    lines.push(format!("Who has moved to it: <{PAGE}>"));
    lines.join("\n")
}

pub(crate) fn job(pool: Pool, hook: DiscordWebhook, list: Vec<CoreSubgraph>) -> JobDef {
    let list: Vec<(String, String)> = list.into_iter().map(|c| (c.name, c.subgraph_id)).collect();
    JobDef::every(
        STEP,
        Duration::from_secs(300),
        std::sync::Arc::new(move |ctx| {
            let pool = pool.clone();
            let hook = hook.clone();
            let fetch = kittiwake_read::routes::core_subgraphs_fetcher(list.clone());
            Box::pin(async move {
                let (answer, _) = fetch(ctx.gate.clone(), ctx.lane)
                    .await
                    .map_err(|e| format!("reading core subgraph versions: {e}"))?;
                let mut answer = answer;
                if let Some(list) = answer["subgraphs"].as_array_mut() {
                    for s in list {
                        kittiwake_edge::handlers::label_versions(&pool, s).await;
                    }
                }
                let announced: HashMap<String, String> =
                    kittiwake_db::core_subgraphs_announced(&pool)
                        .await
                        .map_err(|e| format!("reading announced versions: {e}"))?
                        .into_iter()
                        .collect();
                let (posts, record) = decide(&latest_from(&answer), &announced);
                for text in &posts {
                    kittiwake_edge::discord::send(&hook, text).await?;
                }
                let now = chrono::Utc::now().timestamp();
                for (id, dep) in &record {
                    kittiwake_db::record_core_subgraph_announced(&pool, id, dep, now)
                        .await
                        .map_err(|e| format!("recording an announced version: {e}"))?;
                }
                Ok(JobOutcome {
                    rows_written: record.len() as u64,
                })
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(deployments: &[&str]) -> serde_json::Value {
        let versions: Vec<_> = deployments
            .iter()
            .enumerate()
            .map(|(i, d)| serde_json::json!({ "ipfsHash": d, "version": 40 - i as i64, "createdAt": 1795543165 }))
            .collect();
        serde_json::json!({ "subgraphs": [
            { "name": "Graph Network Arbitrum", "subgraphId": "8446", "versions": versions }
        ]})
    }

    #[test]
    fn a_first_sighting_is_recorded_and_not_posted() {
        let (posts, record) = decide(&latest_from(&answer(&["QmNew", "QmOld"])), &HashMap::new());
        assert!(posts.is_empty());
        assert_eq!(record, vec![("8446".to_string(), "QmNew".to_string())]);
    }

    #[test]
    fn a_new_version_is_posted_once() {
        let seen = HashMap::from([("8446".to_string(), "QmOld".to_string())]);
        let (posts, record) = decide(&latest_from(&answer(&["QmNew", "QmOld"])), &seen);
        assert_eq!(posts.len(), 1);
        assert!(posts[0].contains("**Graph Network Arbitrum** has a new version (version 40), published 24 Nov 17:59 UTC."), "{}", posts[0]);
        assert!(
            posts[0].contains("New: `QmNew`") && posts[0].contains("Was: `QmOld`"),
            "{}",
            posts[0]
        );
        assert_eq!(record.len(), 1);

        let seen = HashMap::from([("8446".to_string(), "QmNew".to_string())]);
        let (posts, record) = decide(&latest_from(&answer(&["QmNew", "QmOld"])), &seen);
        assert!(posts.is_empty() && record.is_empty());
    }

    #[test]
    fn the_publishers_label_is_preferred_to_the_version_number() {
        let mut a = answer(&["QmNew", "QmOld"]);
        a["subgraphs"][0]["versions"][0]["label"] = "2.3.4".into();
        let seen = HashMap::from([("8446".to_string(), "QmOld".to_string())]);
        let (posts, _) = decide(&latest_from(&a), &seen);
        assert!(
            posts[0].contains("has a new version (2.3.4), published"),
            "{}",
            posts[0]
        );
    }

    #[test]
    fn a_subgraph_with_no_versions_is_skipped() {
        let (posts, record) = decide(&latest_from(&answer(&[])), &HashMap::new());
        assert!(posts.is_empty() && record.is_empty());
    }
}
