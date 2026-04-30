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
    obs::ObsRestarter,
    yt_queue::{EnqueueOutcome, YtQueue},
};

/// Built-in commands. `!commands` lists every available command, `!yt` and
/// `!queue` are public, the rest is admin-only.
pub const COMMANDS_BUILTIN: &str = "!commands";
pub const YT_BUILTIN: &str = "!yt";
pub const QUEUE_BUILTIN: &str = "!queue";
pub const VOLUME_BUILTIN: &str = "!volume";
pub const SKIP_BUILTIN: &str = "!skip";
pub const CLUB_BUILTIN: &str = "!club";
pub const SCREEN_BUILTIN: &str = "!screen";

/// Extract a `!command` from the start of a chat line.
///
/// Returns the literal token starting with `!` (no arguments). Whitespace
/// before the bang is tolerated. Returns `None` when the line is empty,
/// has no leading `!`, or only contains a lone `!`.
#[must_use]
pub fn parse_command(text: &str) -> Option<&str> {
    parse_command_with_args(text).map(|(cmd, _)| cmd)
}

/// Like [`parse_command`] but also returns the trimmed remainder of the line.
///
/// `parse_command_with_args("!yt https://...")` returns `Some(("!yt", "https://..."))`.
/// The remainder may be empty.
#[must_use]
pub fn parse_command_with_args(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('!') {
        return None;
    }
    let token = trimmed.split_whitespace().next()?;
    if token.len() < 2 {
        return None;
    }
    let rest = trimmed[token.len()..].trim();
    Some((token, rest))
}

/// True if the badge list contains a broadcaster or moderator badge.
#[must_use]
pub fn is_admin(badges: &[Badge]) -> bool {
    badges
        .iter()
        .any(|badge| badge.set_id.as_str() == "broadcaster" || badge.set_id.as_str() == "moderator")
}

/// Built-ins that are always available, in display order.
const PUBLIC_BUILTINS: &[&str] = &[COMMANDS_BUILTIN, YT_BUILTIN, QUEUE_BUILTIN];
const ADMIN_BUILTINS: &[&str] = &[VOLUME_BUILTIN, SKIP_BUILTIN, SCREEN_BUILTIN];

fn is_builtin(command: &str) -> bool {
    if command == CLUB_BUILTIN {
        // !club only exists when an URL has been configured; the dispatcher
        // checks ChatDeps before treating it as a built-in.
        return false;
    }
    PUBLIC_BUILTINS.contains(&command) || ADMIN_BUILTINS.contains(&command)
}

/// Runtime dependencies the chat dispatcher needs.
#[derive(Clone)]
pub struct ChatDeps {
    pub helix: HelixClient<'static, reqwest::Client>,
    pub token: Arc<UserToken>,
    pub rewards: Arc<RewardsConfig>,
    pub maison: Arc<MaisonClient>,
    pub yt: Arc<YtQueue>,
    pub obs: Option<Arc<ObsRestarter>>,
    pub club_url: Option<Arc<String>>,
}

/// Minimal flags driving optional built-ins in the `!commands` listing.
/// Exposed separately from `ChatDeps` so it stays trivially constructible
/// from tests (no Helix/Twitch token / Maison client needed).
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinFlags {
    pub club_enabled: bool,
    pub screen_enabled: bool,
}

impl BuiltinFlags {
    fn from_deps(deps: &ChatDeps) -> Self {
        Self {
            club_enabled: deps.club_url.is_some(),
            screen_enabled: deps.obs.is_some(),
        }
    }
}

/// Build the reply for the built-in `!commands` listing.
///
/// Lists every built-in plus every user-defined `chat_command` rule from
/// `rewards`, split between public and admin-only. User rules whose name
/// collides with a built-in are dropped (built-ins always win).
#[must_use]
pub fn format_commands_list(rewards: &RewardsConfig, flags: BuiltinFlags) -> String {
    let mut public: Vec<&str> = PUBLIC_BUILTINS.to_vec();
    let mut admin: Vec<&str> = ADMIN_BUILTINS.to_vec();

    if flags.club_enabled {
        public.push(CLUB_BUILTIN);
    }
    if !flags.screen_enabled {
        admin.retain(|c| *c != SCREEN_BUILTIN);
    }

    for rule in &rewards.rules {
        if let Matcher::ChatCommand { chat_command } = &rule.matcher {
            if is_builtin(chat_command)
                || chat_command == CLUB_BUILTIN
                || chat_command == SCREEN_BUILTIN
            {
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
pub async fn dispatch(payload: &ChannelChatMessageV1Payload, deps: &ChatDeps) -> Result<()> {
    let Some((command, args)) = parse_command_with_args(&payload.message.text) else {
        return Ok(());
    };

    // !club is only a built-in when configured; otherwise fall through to
    // the user-defined rules so it can be customised in rewards.toml.
    let club_active = command == CLUB_BUILTIN && deps.club_url.is_some();
    // !screen likewise depends on OBS being configured.
    let screen_active = command == SCREEN_BUILTIN && deps.obs.is_some();

    if is_builtin(command) || club_active || screen_active {
        return handle_builtin(command, args, payload, deps).await;
    }

    let Some(rule) = actions::rule_for_chat_command(&deps.rewards, command) else {
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

    match actions::execute(rule, &deps.maison).await {
        Ok(message) => {
            tracing::info!(command = %command, %message, "chat command executed");
            if let Some(reply) = effective_reply(rule, &message) {
                send_message(
                    &deps.helix,
                    &deps.token,
                    &payload.broadcaster_user_id,
                    reply,
                )
                .await?;
            }
        }
        Err(err) => {
            tracing::error!(command = %command, error = %err, "chat command failed");
            send_message(
                &deps.helix,
                &deps.token,
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

async fn handle_builtin(
    command: &str,
    args: &str,
    payload: &ChannelChatMessageV1Payload,
    deps: &ChatDeps,
) -> Result<()> {
    match command {
        COMMANDS_BUILTIN => {
            let listing = format_commands_list(&deps.rewards, BuiltinFlags::from_deps(deps));
            send_message(
                &deps.helix,
                &deps.token,
                &payload.broadcaster_user_id,
                &listing,
            )
            .await
        }
        YT_BUILTIN => handle_yt(args, payload, deps).await,
        QUEUE_BUILTIN => handle_queue(payload, deps).await,
        CLUB_BUILTIN => {
            let Some(url) = deps.club_url.as_ref() else {
                return Ok(());
            };
            send_message(
                &deps.helix,
                &deps.token,
                &payload.broadcaster_user_id,
                url.as_str(),
            )
            .await
        }
        VOLUME_BUILTIN => {
            if !is_admin(&payload.badges) {
                return Ok(());
            }
            handle_volume(args, payload, deps).await
        }
        SKIP_BUILTIN => {
            if !is_admin(&payload.badges) {
                return Ok(());
            }
            deps.yt.skip();
            send_message(
                &deps.helix,
                &deps.token,
                &payload.broadcaster_user_id,
                "Skipping current track",
            )
            .await
        }
        SCREEN_BUILTIN => {
            if !is_admin(&payload.badges) {
                return Ok(());
            }
            handle_screen(payload, deps).await
        }
        _ => Ok(()),
    }
}

async fn handle_yt(
    args: &str,
    payload: &ChannelChatMessageV1Payload,
    deps: &ChatDeps,
) -> Result<()> {
    let url = args.split_whitespace().next().unwrap_or("");
    if url.is_empty() {
        return send_message(
            &deps.helix,
            &deps.token,
            &payload.broadcaster_user_id,
            "Usage: !yt <YouTube URL>",
        )
        .await;
    }
    let outcome = deps
        .yt
        .enqueue(url, payload.chatter_user_login.as_str())
        .await;
    let reply = match outcome {
        EnqueueOutcome::StartingNow { title } => format!("Now playing: {title}"),
        EnqueueOutcome::Queued { title, position } => {
            format!("Queued at #{position}: {title}")
        }
        EnqueueOutcome::Rejected(reason) => format!("⚠ {reason}"),
    };
    send_message(
        &deps.helix,
        &deps.token,
        &payload.broadcaster_user_id,
        &reply,
    )
    .await
}

async fn handle_queue(payload: &ChannelChatMessageV1Payload, deps: &ChatDeps) -> Result<()> {
    let state = deps.yt.snapshot().await;
    let mut parts = Vec::new();
    if let Some(current) = state.current.as_ref() {
        parts.push(format!("Now: {} ({})", current.title, current.requested_by));
    }
    if !state.pending.is_empty() {
        let queued: Vec<String> = state
            .pending
            .iter()
            .take(5)
            .enumerate()
            .map(|(i, t)| format!("#{} {} ({})", i + 1, t.title, t.requested_by))
            .collect();
        let suffix = if state.pending.len() > 5 {
            format!(" + {} more", state.pending.len() - 5)
        } else {
            String::new()
        };
        parts.push(format!("Up next: {}{suffix}", queued.join(" | ")));
    }
    let reply = if parts.is_empty() {
        "Queue is empty".to_string()
    } else {
        parts.join(" || ")
    };
    send_message(
        &deps.helix,
        &deps.token,
        &payload.broadcaster_user_id,
        &reply,
    )
    .await
}

async fn handle_volume(
    args: &str,
    payload: &ChannelChatMessageV1Payload,
    deps: &ChatDeps,
) -> Result<()> {
    let arg = args.split_whitespace().next().unwrap_or("");
    if arg.is_empty() {
        let current = deps.yt.snapshot().await.volume_percent;
        return send_message(
            &deps.helix,
            &deps.token,
            &payload.broadcaster_user_id,
            &format!("Volume: {current}% (use `!volume <0-100>` to change)"),
        )
        .await;
    }
    let parsed: std::result::Result<u8, _> = arg.parse();
    let percent = match parsed {
        Ok(value) if value <= 100 => value,
        _ => {
            return send_message(
                &deps.helix,
                &deps.token,
                &payload.broadcaster_user_id,
                "Usage: !volume <0-100>",
            )
            .await;
        }
    };
    let applied = deps.yt.set_volume(percent);
    send_message(
        &deps.helix,
        &deps.token,
        &payload.broadcaster_user_id,
        &format!("Volume set to {applied}%"),
    )
    .await
}

async fn handle_screen(payload: &ChannelChatMessageV1Payload, deps: &ChatDeps) -> Result<()> {
    let Some(obs) = deps.obs.as_ref() else {
        return Ok(());
    };
    let reply = match obs.restart_all().await {
        Ok(report) => format!("OBS: {report}"),
        Err(err) => {
            tracing::error!(error = %err, "OBS restart failed");
            format!("⚠ OBS: {err}")
        }
    };
    send_message(
        &deps.helix,
        &deps.token,
        &payload.broadcaster_user_id,
        &reply,
    )
    .await
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

    const BUILTINS_ONLY: &str = "Commands: !commands, !yt, !queue | Admin only: !volume, !skip";

    #[test]
    fn format_commands_list_empty_config_only_builtins() {
        let cfg = RewardsConfig::parse("").unwrap();
        assert_eq!(
            format_commands_list(&cfg, BuiltinFlags::default()),
            BUILTINS_ONLY
        );
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

        let listing = format_commands_list(&cfg, BuiltinFlags::default());
        assert_eq!(
            listing,
            "Commands: !commands, !yt, !queue, !lamp_on, !feed_apollo \
             | Admin only: !volume, !skip, !ac_on, !ac_off"
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
        assert_eq!(
            format_commands_list(&cfg, BuiltinFlags::default()),
            "Commands: !commands, !yt, !queue, !lamp_on | Admin only: !volume, !skip"
        );
    }

    #[test]
    fn format_commands_list_dedupes_user_defined_builtin_alias() {
        // If the user redeclares one of the built-ins, the built-in still
        // wins and we don't list it twice.
        let cfg = RewardsConfig::parse(
            r#"
            [[rules]]
            match = { chat_command = "!yt" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }

            [[rules]]
            match = { chat_command = "!skip" }
            admin_only = true
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }
            "#,
        )
        .unwrap();
        assert_eq!(
            format_commands_list(&cfg, BuiltinFlags::default()),
            BUILTINS_ONLY
        );
    }

    #[test]
    fn format_commands_list_includes_club_and_screen_when_enabled() {
        let cfg = RewardsConfig::parse("").unwrap();
        let flags = BuiltinFlags {
            club_enabled: true,
            screen_enabled: true,
        };
        assert_eq!(
            format_commands_list(&cfg, flags),
            "Commands: !commands, !yt, !queue, !club | Admin only: !volume, !skip, !screen"
        );
    }

    #[test]
    fn parse_command_with_args_splits_url() {
        let (cmd, rest) =
            parse_command_with_args("!yt https://www.youtube.com/watch?v=foo extra args").unwrap();
        assert_eq!(cmd, "!yt");
        assert_eq!(rest, "https://www.youtube.com/watch?v=foo extra args");
    }

    #[test]
    fn parse_command_with_args_handles_no_args() {
        let (cmd, rest) = parse_command_with_args("!queue").unwrap();
        assert_eq!(cmd, "!queue");
        assert_eq!(rest, "");
    }
}
