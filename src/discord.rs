//! Discord "go live" webhook.
//!
//! Fired once, best-effort, when homie starts (the operator launches the bot
//! when they go live). Never aborts the bot: the caller logs a warning on
//! failure and continues.

use serde_json::json;

use crate::error::{Error, Result};

/// Twitch brand purple, used for the embed's accent bar.
const TWITCH_PURPLE: u32 = 0x0091_46FF;

/// What to announce.
pub struct GoLive<'a> {
    /// Display name shown in the message (operator override or Twitch name).
    pub name: &'a str,
    /// Twitch login, used to build the channel URL.
    pub login: &'a str,
    /// Current stream title (may be empty if it couldn't be fetched).
    pub title: &'a str,
    /// Current category / game name (may be empty).
    pub category: &'a str,
}

/// POST the go-live notification to a Discord webhook URL.
///
/// # Errors
/// Returns [`Error::Discord`] on a transport failure or a non-2xx response.
pub async fn notify_go_live(
    http: &reqwest::Client,
    webhook_url: &str,
    live: &GoLive<'_>,
) -> Result<()> {
    let channel_url = format!("https://www.twitch.tv/{}", live.login);
    let title = if live.title.is_empty() {
        "Live on Twitch"
    } else {
        live.title
    };
    let category = if live.category.is_empty() {
        "—"
    } else {
        live.category
    };

    let payload = json!({
        "content": format!("**{}** just went live on Twitch", live.name),
        "embeds": [{
            "author": { "name": live.name, "url": channel_url },
            "title": title,
            "url": channel_url,
            "color": TWITCH_PURPLE,
            "fields": [
                { "name": "Category", "value": category, "inline": true },
                {
                    "name": "Watch now",
                    "value": format!("[Click here to watch]({channel_url})"),
                    "inline": true
                }
            ]
        }]
    });

    let resp = http
        .post(webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| Error::discord(format!("request failed: {err}")))?;

    if !resp.status().is_success() {
        return Err(Error::discord(format!("HTTP {}", resp.status())));
    }
    Ok(())
}
