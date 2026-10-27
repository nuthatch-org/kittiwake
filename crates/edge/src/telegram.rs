//! Messages to the chat the Telegram bot writes to: support reports and alerts.

use kittiwake_config::TelegramSettings;

/// Telegram refuses a message over 4096 characters; the margin leaves room for the part marker.
const PART_CHARS: usize = 4000;
/// A report is at most a handful of fields of `MAX_FIELD_CHARS`. Past this it is not a report.
const MAX_PARTS: usize = 8;

/// The text in pieces Telegram will take, split at line ends where one is near enough.
pub(crate) fn parts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while !rest.is_empty() {
        if out.len() == MAX_PARTS - 1 && rest.len() > PART_CHARS {
            let mut last: String = rest[..PART_CHARS - 40].iter().collect();
            last.push_str("\n\n[cut: longer than Telegram takes]");
            out.push(last);
            break;
        }
        let take = if rest.len() <= PART_CHARS {
            rest.len()
        } else {
            match rest[..PART_CHARS].iter().rposition(|c| *c == '\n') {
                Some(i) if i > PART_CHARS / 2 => i + 1,
                _ => PART_CHARS,
            }
        };
        out.push(rest[..take].iter().collect());
        rest.drain(..take);
    }
    out
}

/// Send `text`, in as many messages as it takes. Plain text, so nothing a reporter types is
/// read as markup.
pub async fn send(t: &TelegramSettings, text: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", t.bot_token);
    for part in parts(text) {
        let res = client
            .post(&url)
            .json(&serde_json::json!({ "chat_id": t.chat_id, "text": part }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("Telegram could not be reached: {}", e.without_url()))?;
        if !res.status().is_success() {
            return Err(format!("Telegram answered {}", res.status().as_u16()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_message_is_one_part() {
        assert_eq!(parts("hello"), vec!["hello".to_string()]);
    }

    #[test]
    fn a_long_message_splits_at_a_line_end_and_loses_nothing() {
        let line = "x".repeat(99) + "\n";
        let text = line.repeat(100);
        let got = parts(&text);
        assert!(got.len() > 1);
        assert!(got.iter().all(|p| p.chars().count() <= PART_CHARS));
        assert!(got[..got.len() - 1].iter().all(|p| p.ends_with('\n')));
        assert_eq!(got.concat(), text);
    }

    #[test]
    fn multibyte_text_splits_on_characters() {
        let text = "\u{00e9}".repeat(PART_CHARS + 10);
        let got = parts(&text);
        assert_eq!(got.len(), 2);
        assert_eq!(got.concat(), text);
    }

    #[test]
    fn past_the_cap_it_says_it_was_cut() {
        let text = "y".repeat(PART_CHARS * (MAX_PARTS + 3));
        let got = parts(&text);
        assert_eq!(got.len(), MAX_PARTS);
        assert!(
            got.last()
                .unwrap()
                .ends_with("[cut: longer than Telegram takes]")
        );
        assert!(got.iter().all(|p| p.chars().count() <= PART_CHARS));
    }
}
