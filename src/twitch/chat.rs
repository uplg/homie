//! Chat command parsing + outgoing-message helper.
//!
//! Pure logic lives here so it can be unit-tested without a real chat
//! connection. The `EventSub` loop calls `parse_command` on every chat
//! message and `dispatch` to actually run the matched rule.

use std::sync::Arc;

use twitch_api::{
    HelixClient,
    eventsub::channel::{ChannelChatMessageV1Payload, chat::message::Badge},
    helix::chat::SendChatMessageBody,
    types::UserId,
};
use twitch_oauth2::UserToken;

use crate::{
    actions,
    config::{Matcher, RewardsConfig, Rule},
    error::{Error, Result},
    maison::MaisonClient,
};

/// Built-in command name listing every available chat command.
pub const COMMANDS_BUILTIN: &str = "!commands";

/// Extract a `!command` from the start of a chat line.
///
/// Returns the literal token starting with `!` (no arguments). Whitespace
/// before the bang is tolerated. Returns `None` when the line is empty,
/// has no leading `!`, or only contains a lone `!`.
#[must_use]
pub fn parse_command(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('!') {
        return None;
    }
    let token = trimmed.split_whitespace().next()?;
    if token.len() < 2 {
        return None;
    }
    Some(token)
}

/// True if the badge list contains a broadcaster or moderator badge.
#[must_use]
pub fn is_admin(badges: &[Badge]) -> bool {
    badges
        .iter()
        .any(|badge| badge.set_id.as_str() == "broadcaster" || badge.set_id.as_str() == "moderator")
}

/// Build the reply for the built-in `!commands` listing.
///
/// Pulls every `chat_command` rule from `rewards`, splits between public
/// and admin-only, and prepends `!commands` itself so the built-in
/// appears in its own listing. The user can also define a `!commands`
/// rule of their own; the built-in takes precedence and the user rule
/// is shown but not duplicated.
#[must_use]
pub fn format_commands_list(rewards: &RewardsConfig) -> String {
    let mut public: Vec<&str> = vec![COMMANDS_BUILTIN];
    let mut admin: Vec<&str> = Vec::new();

    for rule in &rewards.rules {
        if let Matcher::ChatCommand { chat_command } = &rule.matcher {
            if chat_command == COMMANDS_BUILTIN {
                continue;
            }
            if rule.admin_only {
                admin.push(chat_command);
            } else {
                public.push(chat_command);
            }
        }
    }

    let mut out = format!("Commands: {}", public.join(", "));
    if !admin.is_empty() {
        out.push_str(" | Admin only: ");
        out.push_str(&admin.join(", "));
    }
    out
}

/// Process a single chat message: parse → match → execute → reply.
pub async fn dispatch(
    payload: &ChannelChatMessageV1Payload,
    rewards: &RewardsConfig,
    maison: &Arc<MaisonClient>,
    helix: &HelixClient<'static, reqwest::Client>,
    token: &UserToken,
) -> Result<()> {
    let Some(command) = parse_command(&payload.message.text) else {
        return Ok(());
    };

    // Built-in: list every configured chat command.
    if command == COMMANDS_BUILTIN {
        let listing = format_commands_list(rewards);
        send_message(helix, token, &payload.broadcaster_user_id, &listing).await?;
        return Ok(());
    }

    let Some(rule) = actions::rule_for_chat_command(rewards, command) else {
        tracing::trace!(command = %command, "no rule matches this chat command");
        return Ok(());
    };

    if rule.admin_only && !is_admin(&payload.badges) {
        tracing::info!(
            command = %command,
            user = %payload.chatter_user_login,
            "ignoring admin-only command from non-admin",
        );
        return Ok(());
    }

    match actions::execute(rule, maison).await {
        Ok(message) => {
            tracing::info!(command = %command, %message, "chat command executed");
            if let Some(reply) = effective_reply(rule, &message) {
                send_message(helix, token, &payload.broadcaster_user_id, reply).await?;
            }
        }
        Err(err) => {
            tracing::error!(command = %command, error = %err, "chat command failed");
            send_message(
                helix,
                token,
                &payload.broadcaster_user_id,
                &format!("⚠ error: {err}"),
            )
            .await
            .ok();
        }
    }

    Ok(())
}

fn effective_reply<'a>(rule: &'a Rule, fallback: &'a str) -> Option<&'a str> {
    match rule.reply.as_deref() {
        Some(text) if !text.is_empty() => Some(text),
        Some(_) => None, // explicit empty string disables reply
        None => Some(fallback),
    }
}

async fn send_message(
    helix: &HelixClient<'static, reqwest::Client>,
    token: &UserToken,
    broadcaster: &UserId,
    text: &str,
) -> Result<()> {
    let broadcaster_ref: &twitch_api::types::UserIdRef = broadcaster.as_ref();
    let sender_ref: &twitch_api::types::UserIdRef = token.user_id.as_ref();
    let body = SendChatMessageBody::new(broadcaster_ref, sender_ref, text.to_string());
    helix
        .req_post(
            twitch_api::helix::chat::SendChatMessageRequest::new(),
            body,
            token,
        )
        .await
        .map_err(|err| Error::twitch(format!("send_chat_message: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_extracts_token() {
        assert_eq!(parse_command("!lamp on"), Some("!lamp"));
        assert_eq!(parse_command("   !ping"), Some("!ping"));
        assert_eq!(parse_command("!a"), Some("!a"));
    }

    #[test]
    fn parse_command_rejects_non_command_lines() {
        assert!(parse_command("hello").is_none());
        assert!(parse_command("").is_none());
        assert!(parse_command("   ").is_none());
        assert!(parse_command("!").is_none()); // a lone bang isn't a command
        assert!(parse_command("foo !bar").is_none()); // bang must be at the start
    }

    #[test]
    fn parse_command_keeps_case() {
        // We chose case-sensitive matching in phase 4 — make sure the parser
        // doesn't silently lowercase the token.
        assert_eq!(parse_command("!Foo"), Some("!Foo"));
    }

    #[test]
    fn format_commands_list_empty_config_only_builtin() {
        let cfg = RewardsConfig::parse("").unwrap();
        assert_eq!(format_commands_list(&cfg), "Commands: !commands");
    }

    #[test]
    fn format_commands_list_separates_public_and_admin() {
        let cfg = RewardsConfig::parse(
            r#"
            [[rules]]
            match = { chat_command = "!lamp_on" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }

            [[rules]]
            match = { chat_command = "!feed_apollo" }
            action = { kind = "feeder_feed", device_id = "y", portion = 1 }

            [[rules]]
            match = { chat_command = "!ac_on" }
            admin_only = true
            action = { kind = "ac_mitsubishi", host = "1.1.1.1", command = "on" }

            [[rules]]
            match = { chat_command = "!ac_off" }
            admin_only = true
            action = { kind = "ac_mitsubishi", host = "1.1.1.1", command = "off" }
            "#,
        )
        .unwrap();

        let listing = format_commands_list(&cfg);
        assert_eq!(
            listing,
            "Commands: !commands, !lamp_on, !feed_apollo | Admin only: !ac_on, !ac_off"
        );
    }

    #[test]
    fn format_commands_list_skips_reward_rules() {
        // Reward redemption rules don't appear in the chat-commands listing.
        let cfg = RewardsConfig::parse(
            r#"
            [[rules]]
            match = { reward = "Some Reward" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }

            [[rules]]
            match = { chat_command = "!lamp_on" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }
            "#,
        )
        .unwrap();
        assert_eq!(format_commands_list(&cfg), "Commands: !commands, !lamp_on");
    }

    #[test]
    fn format_commands_list_dedupes_user_defined_commands_alias() {
        // If the user redeclares !commands, the built-in still wins and we
        // don't list it twice.
        let cfg = RewardsConfig::parse(
            r#"
            [[rules]]
            match = { chat_command = "!commands" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }
            "#,
        )
        .unwrap();
        assert_eq!(format_commands_list(&cfg), "Commands: !commands");
    }
}
