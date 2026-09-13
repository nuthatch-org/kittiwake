//! Read-only live probe: DUNE_API_KEY=... cargo run -p dune-client --example snapshot
use dune_client::DuneClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("DUNE_API_KEY").map_err(|_| "DUNE_API_KEY is required")?;
    let client = DuneClient::new(key.into())?;
    let snapshot = client.snapshot(8699345, 10, 31).await?;
    let status = client.status(&snapshot.first_page.execution_id).await?;
    println!(
        "execution={} rows={} columns={} credits={:?}",
        snapshot.first_page.execution_id,
        snapshot.rows.len(),
        snapshot.first_page.result.metadata.column_names.len(),
        status.execution_cost_credits
    );
    Ok(())
}
