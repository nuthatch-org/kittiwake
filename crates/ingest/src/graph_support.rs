//! Mirroring the community triage archive at nightswatchhq/graph-support.
//!
//! The dashboard's `/support` page used to call GitHub itself on a cache miss, which made a page
//! of thirty-three write-ups depend on a credential in the frontend's environment. That credential
//! expired and the page went blank, while the Intel Feed kept answering 200 with every GitHub
//! category silently missing from it.
//!
//! Twice a day, from here, needs no credential at all: the repository is public and two requests a
//! day is two of the sixty an hour GitHub allows unauthenticated. There is nothing left to expire.
//!
//! Worth knowing before anyone reaches for `git clone`: the issues are not in that repository. It
//! holds `README.md`, `TRIAGE.md`, `reference/` and the issue templates. The threads live only in
//! the API, so this mirrors an API and not a checkout.

use kittiwake_db::Pool;
use kittiwake_ext::Ext;

use crate::{IngestError, Ingested, state};

/// Fetch the archive and make the table equal it.
///
/// A full replace rather than an append, because the fetch returns the complete set every time: an
/// issue deleted or transferred on GitHub must leave here too, and an upsert-only mirror would
/// serve it for ever. `Ext::graph_support_issues` refuses an empty answer, so the delete below
/// cannot be handed an empty set and empty the table.
pub async fn graph_support(pool: &Pool, ext: &Ext) -> Result<Ingested, IngestError> {
    let issues = ext
        .graph_support_issues()
        .await
        .map_err(|e| IngestError::Refused(e.to_string()))?;

    // Fetched before the transaction opens, so a slow GitHub does not hold a write lock. Two
    // requests for the whole archive: the issues carry their own bodies, and every comment in the
    // repository arrives in one call rather than one per issue.
    let comments = ext
        .graph_support_thread_comments()
        .await
        .map_err(|e| IngestError::Refused(e.to_string()))?;

    // One statement per issue rather than the UNNEST bulk insert the other jobs use. `labels` is a
    // per-row `text[]` of varying length, and Postgres array parameters must be rectangular, so
    // there is no honest way to carry thirty-three label lists through one UNNEST. At two runs a
    // day and thirty-odd rows the round trips cost nothing; the transaction is what matters, so a
    // failure halfway leaves the previous mirror intact rather than a half-updated one.
    let mut tx = pool.begin().await?;

    for issue in &issues {
        sqlx::query(
            "INSERT INTO graph_support_issues \
             (number, title, body, url, state, labels, comments, created_at, updated_at, ingested_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW()) \
             ON CONFLICT (number) DO UPDATE SET \
               title = EXCLUDED.title, body = EXCLUDED.body, url = EXCLUDED.url, \
               state = EXCLUDED.state, labels = EXCLUDED.labels, comments = EXCLUDED.comments, \
               created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at, \
               ingested_at = NOW()",
        )
        .bind(issue.number)
        .bind(&issue.title)
        .bind(&issue.body)
        .bind(&issue.url)
        .bind(&issue.state)
        .bind(&issue.labels)
        .bind(issue.comments)
        .bind(issue.created_at)
        .bind(issue.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    let seen: Vec<i32> = issues.iter().map(|i| i.number).collect();
    sqlx::query("DELETE FROM graph_support_issues WHERE number <> ALL($1::int[])")
        .bind(&seen)
        .execute(&mut *tx)
        .await?;

    for c in &comments {
        sqlx::query(
            "INSERT INTO graph_support_comments \
             (id, issue_number, author, body, url, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (id) DO UPDATE SET \
               author = EXCLUDED.author, body = EXCLUDED.body, url = EXCLUDED.url, \
               updated_at = EXCLUDED.updated_at",
        )
        .bind(c.id)
        .bind(c.issue_number)
        .bind(&c.author)
        .bind(&c.body)
        .bind(&c.url)
        .bind(c.created_at)
        .bind(c.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    // An edited-then-deleted comment must leave here too. Guarded on the fetch having returned
    // something: an empty comment list is legitimate for a repository nobody has replied in, but
    // deleting the whole thread store on the strength of it would not be.
    if !comments.is_empty() {
        let kept: Vec<i64> = comments.iter().map(|c| c.id).collect();
        sqlx::query("DELETE FROM graph_support_comments WHERE id <> ALL($1::bigint[])")
            .bind(&kept)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    state::touch(pool, "graph_support").await?;

    Ok(Ingested {
        rows: (issues.len() + comments.len()) as u64,
    })
}
