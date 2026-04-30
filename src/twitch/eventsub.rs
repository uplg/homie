//! `EventSub` WebSocket loop.
//!
//! Connects to `wss://eventsub.wss.twitch.tv/ws`, waits for the welcome
//! frame to learn the `session_id`, then creates the subscriptions we need
//! via Helix and dispatches notifications to the rest of the bot.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use twitch_api::{
    HelixClient,
    eventsub::{
        Event, EventsubWebsocketData, Message as EventMessage, Transport,
        channel::{ChannelChatMessageV1, ChannelPointsCustomRewardRedemptionAddV1},
    },
    helix::eventsub::{
        CreateEventSubSubscriptionRequest, DeleteEventSubSubscriptionRequest,
        GetEventSubSubscriptionsRequest,
    },
    types::UserId,
};
use twitch_oauth2::UserToken;
use url::Url;

use crate::{
    actions,
    config::RewardsConfig,
    error::{Error, Result},
    maison::MaisonClient,
    twitch::chat,
    yt_queue::YtQueue,
};

const DEFAULT_WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
/// How many recent `EventSub` `message_id`s we remember to drop duplicates.
/// Twitch's at-least-once delivery occasionally redelivers, and reconnect
/// flows can carry over notifications across sockets. 256 covers any
/// realistic backlog window.
const SEEN_MESSAGE_CAPACITY: usize = 256;

/// Mutable state that has to survive across consecutive `run_session`
/// invocations: which session we already subscribed against (so we don't
/// re-subscribe on a Twitch-driven reconnect — the carryover session
/// keeps existing subscriptions live), and a small ring of seen
/// `message_ids` for dedupe.
#[derive(Default)]
pub struct EventSubState {
    subscribed_session_id: Option<String>,
    seen_message_ids: VecDeque<String>,
}

impl EventSubState {
    fn already_subscribed(&self, session_id: &str) -> bool {
        self.subscribed_session_id.as_deref() == Some(session_id)
    }

    fn mark_subscribed(&mut self, session_id: &str) {
        self.subscribed_session_id = Some(session_id.to_string());
    }

    fn note_message(&mut self, message_id: &str) -> bool {
        if self.seen_message_ids.iter().any(|id| id == message_id) {
            return false;
        }
        if self.seen_message_ids.len() >= SEEN_MESSAGE_CAPACITY {
            self.seen_message_ids.pop_front();
        }
        self.seen_message_ids.push_back(message_id.to_string());
        true
    }
}

/// Shared dependencies the WS loop needs at runtime.
#[derive(Clone)]
pub struct EventSubContext {
    pub helix: HelixClient<'static, reqwest::Client>,
    pub token: Arc<UserToken>,
    pub broadcaster_user_id: UserId,
    pub rewards: Arc<RewardsConfig>,
    pub maison: Arc<MaisonClient>,
    pub yt: Arc<YtQueue>,
    pub state: Arc<Mutex<EventSubState>>,
}

/// Run a single WebSocket session against `url` until the server closes
/// it or asks us to reconnect.
///
/// Returns `Ok(Some(new_url))` when the server requested a reconnect,
/// `Ok(None)` on a clean close, and `Err(_)` on a fatal error.
pub async fn run_session(ctx: &EventSubContext, url: &str) -> Result<Option<String>> {
    let parsed = Url::parse(url)?;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(parsed.as_str())
        .await
        .map_err(|err| Error::twitch(format!("WebSocket connect failed: {err}")))?;

    tracing::info!(url = %url, "EventSub WebSocket connected");

    while let Some(frame) = ws.next().await {
        let frame = frame.map_err(|err| Error::twitch(format!("WebSocket recv error: {err}")))?;
        match frame {
            Message::Text(text) => {
                if let Some(reconnect_url) = handle_text_frame(ctx, &text).await? {
                    return Ok(Some(reconnect_url));
                }
            }
            Message::Binary(_) => {
                tracing::debug!("ignored binary frame");
            }
            Message::Ping(payload) => {
                ws.send(Message::Pong(payload)).await.ok();
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(reason) => {
                tracing::info!(?reason, "EventSub WebSocket closed by server");
                return Ok(None);
            }
        }
    }

    Ok(None)
}

async fn handle_text_frame(ctx: &EventSubContext, text: &str) -> Result<Option<String>> {
    let parsed = Event::parse_websocket(text)
        .map_err(|err| Error::twitch(format!("invalid EventSub frame: {err}")))?;

    match parsed {
        EventsubWebsocketData::Welcome { payload, .. } => {
            let session_id = payload.session.id.to_string();
            // Twitch's reconnect flow hands us a new URL but the session_id
            // carries over and the existing subscriptions stay live. The doc is
            // explicit: "Do not recreate your subscriptions". We track the last
            // session_id we subscribed against and skip if it's the same.
            let state = ctx.state.lock().await;
            if state.already_subscribed(&session_id) {
                tracing::info!(
                    session_id = %session_id,
                    "EventSub session welcomed (reconnect carryover, subscriptions reused)"
                );
            } else {
                tracing::info!(session_id = %session_id, "EventSub session welcomed");
                drop(state);
                create_subscriptions(ctx, &session_id).await?;
                ctx.state.lock().await.mark_subscribed(&session_id);
            }
            Ok(None)
        }
        EventsubWebsocketData::Keepalive { .. } => {
            tracing::trace!("EventSub keepalive");
            Ok(None)
        }
        EventsubWebsocketData::Reconnect { payload, .. } => {
            // Twitch hands us the new URL on the metadata's session.reconnect_url.
            let new_url = payload
                .session
                .reconnect_url
                .map(|u| u.to_string())
                .ok_or_else(|| {
                    Error::twitch("reconnect frame without reconnect_url".to_string())
                })?;
            tracing::info!(new_url = %new_url, "EventSub asked to reconnect");
            Ok(Some(new_url))
        }
        EventsubWebsocketData::Revocation { metadata, .. } => {
            tracing::warn!(
                subscription_type = %metadata.subscription_type,
                subscription_version = %metadata.subscription_version,
                "EventSub subscription revoked",
            );
            Ok(None)
        }
        EventsubWebsocketData::Notification { payload, .. } => {
            handle_notification(ctx, payload).await?;
            Ok(None)
        }
        // Forward-compat: skip any future variant.
        _ => Ok(None),
    }
}

/// Delete every existing `EventSub` subscription this `client_id` owns on
/// Twitch's backend. Run once at startup before the WS loop. Without this,
/// orphan subscriptions left over from previous bot runs (crashes, kill -9,
/// hot reloads) keep delivering the same chat messages alongside the new
/// session's subs and you see every reply two-, three- or N-fold in chat.
pub async fn cleanup_existing_subscriptions(
    helix: &HelixClient<'static, reqwest::Client>,
    token: &UserToken,
) -> Result<()> {
    let req = GetEventSubSubscriptionsRequest::default();
    let response = helix
        .req_get(req, token)
        .await
        .map_err(|err| Error::twitch(format!("list eventsub subscriptions: {err}")))?;

    let subs = response.data.subscriptions;
    if subs.is_empty() {
        tracing::info!("no existing EventSub subscriptions to clean up");
        return Ok(());
    }

    tracing::info!(
        count = subs.len(),
        "deleting existing EventSub subscriptions"
    );
    for sub in subs {
        let id = sub.id.clone();
        match helix
            .req_delete(DeleteEventSubSubscriptionRequest::id(&id), token)
            .await
        {
            Ok(_) => tracing::debug!(id = %id, "deleted EventSub subscription"),
            Err(err) => tracing::warn!(id = %id, error = %err, "could not delete subscription"),
        }
    }
    Ok(())
}

async fn create_subscriptions(ctx: &EventSubContext, session_id: &str) -> Result<()> {
    let transport = Transport::websocket(session_id);

    // Channel point redemptions.
    let body_redemption = twitch_api::helix::eventsub::CreateEventSubSubscriptionBody::new(
        ChannelPointsCustomRewardRedemptionAddV1::broadcaster_user_id(
            ctx.broadcaster_user_id.clone(),
        ),
        transport.clone(),
    );
    ctx.helix
        .req_post(
            CreateEventSubSubscriptionRequest::default(),
            body_redemption,
            ctx.token.as_ref(),
        )
        .await
        .map_err(|err| Error::twitch(format!("create redemption subscription: {err}")))?;
    tracing::info!("subscribed to channel.channel_points_custom_reward_redemption.add");

    // Chat messages — bot listens as itself (token.user_id) on the broadcaster's channel.
    let body_chat = twitch_api::helix::eventsub::CreateEventSubSubscriptionBody::new(
        ChannelChatMessageV1::new(ctx.broadcaster_user_id.clone(), ctx.token.user_id.clone()),
        transport,
    );
    ctx.helix
        .req_post(
            CreateEventSubSubscriptionRequest::default(),
            body_chat,
            ctx.token.as_ref(),
        )
        .await
        .map_err(|err| Error::twitch(format!("create chat subscription: {err}")))?;
    tracing::info!("subscribed to channel.chat.message");

    Ok(())
}

async fn handle_notification(ctx: &EventSubContext, event: Event) -> Result<()> {
    match event {
        Event::ChannelPointsCustomRewardRedemptionAddV1(payload) => {
            if let EventMessage::Notification(redemption) = payload.message {
                // Dedup by the redemption's stable id. Multiple subscriptions
                // delivering the same redemption — orphans, reconnect replays
                // — share the same payload id even though their EventSub
                // envelopes differ.
                let key = format!("redemption:{}", redemption.id);
                if !ctx.state.lock().await.note_message(&key) {
                    tracing::debug!(key, "duplicate redemption, skipping");
                    return Ok(());
                }
                return handle_redemption(ctx, &redemption).await;
            }
        }
        Event::ChannelChatMessageV1(payload) => {
            if let EventMessage::Notification(message) = payload.message {
                let key = format!("chat:{}", message.message_id);
                if !ctx.state.lock().await.note_message(&key) {
                    tracing::debug!(key, "duplicate chat message, skipping");
                    return Ok(());
                }
                return chat::dispatch(
                    &message,
                    &ctx.rewards,
                    &ctx.maison,
                    &ctx.helix,
                    ctx.token.as_ref(),
                    &ctx.yt,
                )
                .await;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_redemption(
    ctx: &EventSubContext,
    redemption: &twitch_api::eventsub::channel::ChannelPointsCustomRewardRedemptionAddV1Payload,
) -> Result<()> {
    let title = redemption.reward.title.as_str();
    tracing::info!(
        user = %redemption.user_login,
        reward = %title,
        "channel point redemption received",
    );

    let Some(rule) = actions::rule_for_reward(&ctx.rewards, title) else {
        tracing::debug!(reward = %title, "no rule matches this reward");
        return Ok(());
    };

    match actions::execute(rule, &ctx.maison).await {
        Ok(message) => tracing::info!(reward = %title, %message, "action executed"),
        Err(err) => tracing::error!(reward = %title, error = %err, "action failed"),
    }
    Ok(())
}

/// Default WebSocket entry point.
#[must_use]
pub fn default_websocket_url() -> &'static str {
    DEFAULT_WS_URL
}
