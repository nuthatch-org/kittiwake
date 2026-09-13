//! One-shot Phase 0 pilot. Recurring scheduling and production wiring are separate work.
use dune_client::DuneClient;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("DUNE_API_KEY").map_err(|_| "DUNE_API_KEY is required")?;
    let database =
        std::env::var("DUNE_DATABASE_URL").map_err(|_| "DUNE_DATABASE_URL is required")?;
    let client = DuneClient::new(key.into())?;
    let snapshot = client.snapshot(8699345, 10, 31).await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database)
        .await?;
    let changed = dune_poller::materialize(&pool, &snapshot).await?;
    println!(
        "query=grt_price_daily execution={} rows={} inserted={changed}",
        snapshot.first_page.execution_id,
        snapshot.rows.len()
    );
    pool.close().await;
    Ok(())
}
