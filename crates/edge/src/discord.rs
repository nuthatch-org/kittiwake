//! Posts to a Discord channel through a webhook.

use kittiwake_config::DiscordWebhook;

/// Discord refuses content over 2000 characters.
const MAX_CHARS: usize = 2000;

/// Post `text` as one message. Mentions are switched off, so nothing in a subgraph name can ping.
pub async fn send(hook: &DiscordWebhook, text: &str) -> Result<(), String> {
    let content: String = text.chars().take(MAX_CHARS).collect();
    let res = reqwest::Client::new()
        .post(&hook.url)
        .json(&serde_json::json!({ "content": content, "allowed_mentions": { "parse": [] } }))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("discord webhook: {}", e.without_url()))?;
    if !res.status().is_success() {
        return Err(format!("discord webhook answered {}", res.status()));
    }
    Ok(())
}
