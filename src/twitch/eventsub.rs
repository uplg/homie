//! `EventSub` WebSocket loop.
//!
//! Connects to `wss://eventsub.wss.twitch.tv/ws`, waits for the welcome
//! frame to learn the `session_id`, then creates the subscriptions we need
//! via Helix and dispatches notifications to the rest of the bot.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, broadcast};
use tokio_tungstenite::tungstenite::Message;
use twitch_api::{
    HelixClient,
    eventsub::{
        Event, EventSubscription, EventsubWebsocketData, Message as EventMessage, Transport,
        channel::{
            ChannelChatMessageV1, ChannelFollowV2, ChannelPointsCustomRewardRedemptionAddV1,
            ChannelRaidV1, ChannelSubscribeV1, ChannelSubscriptionGiftV1,
            ChannelSubscriptionMessageV1,
        },
        stream::StreamOnlineV1,
    },
    helix::{
        eventsub::{
            CreateEventSubSubscriptionRequest, DeleteEventSubSubscriptionRequest,
            GetEventSubSubscriptionsRequest,
        },
        streams::GetStreamsRequest,
    },
    types::{SubscriptionTier, UserId},
};
use twitch_oauth2::UserToken;
use url::Url;

use crate::{
    actions,
    config::RewardsConfig,
    error::{Error, Result},
    maison::MaisonClient,
    tui::UiEvent,
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
    /// In-memory "zoom notice already fired" guard, used only when the live
    /// id is unknown (no `stream.online`/startup live-check this run).
    zoom_notice_sent: bool,
    /// Current live's stream id, learned from `stream.online` or the startup
    /// live-check. Keys the on-disk zoom marker so the notice fires once per
    /// live even across a bot restart.
    current_live: Option<String>,
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

    /// Returns `true` exactly once per live for the zoom-user notice; marks
    /// it sent so subsequent messages this live don't repeat it.
    fn claim_zoom_notice(&mut self) -> bool {
        if self.zoom_notice_sent {
            return false;
        }
        self.zoom_notice_sent = true;
        true
    }

    /// A live started/was detected: record its id and re-arm the in-memory
    /// guard so the zoom notice can fire once for this new live.
    fn start_live(&mut self, stream_id: &str) {
        self.current_live = Some(stream_id.to_string());
        self.zoom_notice_sent = false;
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
    pub obs: Option<Arc<crate::obs::ObsRestarter>>,
    pub club_url: Option<Arc<String>>,
    pub discord_url: Option<Arc<String>>,
    pub melodie_url_file: Arc<PathBuf>,
    pub state: Arc<Mutex<EventSubState>>,
    /// When `Some` (TUI mode), observable events are mirrored here for the
    /// dashboard. `None` in headless mode — emission is then a no-op.
    pub events: Option<broadcast::Sender<UiEvent>>,
    /// Go-live Discord notification config; `None` disables it.
    pub discord: Option<DiscordNotify>,
    /// Optional user to spotlight with a "Please zoom" notice when they
    /// speak (matched case-insensitively on login or display name).
    pub zoom_user: Option<Arc<String>>,
    /// File recording the live id the zoom notice last fired for, so it
    /// stays once-per-live across bot restarts. `Some` iff `zoom_user`.
    pub zoom_marker: Option<Arc<PathBuf>>,
}

/// Everything needed to post (and de-duplicate) the go-live webhook.
#[derive(Clone)]
pub struct DiscordNotify {
    pub webhook_url: Arc<String>,
    /// Overrides the live title shown in the embed when set.
    pub live_title: Option<Arc<String>>,
    pub broadcaster_login: Arc<String>,
    /// Holds the last-notified stream id, so a bot restart during the same
    /// live (or offline dev work) never re-notifies.
    pub marker_file: Arc<PathBuf>,
}

/// Best-effort mirror of an observable event to the TUI. A slow/absent
/// dashboard just means the event is dropped.
fn emit(ctx: &EventSubContext, ev: UiEvent) {
    if let Some(tx) = &ctx.events {
        let _ = tx.send(ev);
    }
}

fn tier_label(tier: &SubscriptionTier) -> String {
    match tier {
        SubscriptionTier::Tier1 => "T1".to_string(),
        SubscriptionTier::Tier2 => "T2".to_string(),
        SubscriptionTier::Tier3 => "T3".to_string(),
        SubscriptionTier::Prime => "Prime".to_string(),
        SubscriptionTier::Other(other) => other.clone(),
    }
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
        transport.clone(),
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

    // Go-live: stream.online (no scope required). Lets the operator start
    // homie before the live without spamming, and only fires on the real
    // transition. Best-effort.
    if ctx.discord.is_some() {
        try_subscribe(
            ctx,
            &transport,
            StreamOnlineV1::broadcaster_user_id(ctx.broadcaster_user_id.clone()),
            "stream.online",
        )
        .await;
    }

    // Optional informational feeds for the TUI Activity panel. Best-effort:
    // they need extra scopes the operator may not have granted yet, so a
    // failure is logged and the bot keeps running.
    if ctx.events.is_some() {
        let bid = &ctx.broadcaster_user_id;
        try_subscribe(
            ctx,
            &transport,
            ChannelSubscribeV1::broadcaster_user_id(bid.clone()),
            "channel.subscribe",
        )
        .await;
        try_subscribe(
            ctx,
            &transport,
            ChannelSubscriptionMessageV1::broadcaster_user_id(bid.clone()),
            "channel.subscription.message",
        )
        .await;
        try_subscribe(
            ctx,
            &transport,
            ChannelSubscriptionGiftV1::broadcaster_user_id(bid.clone()),
            "channel.subscription.gift",
        )
        .await;
        try_subscribe(
            ctx,
            &transport,
            ChannelFollowV2::new(bid.clone(), ctx.token.user_id.clone()),
            "channel.follow",
        )
        .await;
        try_subscribe(
            ctx,
            &transport,
            ChannelRaidV1::to_broadcaster_user_id(bid.clone()),
            "channel.raid",
        )
        .await;
    }

    Ok(())
}

/// Create one best-effort `EventSub` subscription. Logs and swallows
/// failures (typically a missing scope) so the bot is not aborted.
async fn try_subscribe<S>(ctx: &EventSubContext, transport: &Transport, sub: S, name: &str)
where
    S: EventSubscription,
{
    let body =
        twitch_api::helix::eventsub::CreateEventSubSubscriptionBody::new(sub, transport.clone());
    match ctx
        .helix
        .req_post(
            CreateEventSubSubscriptionRequest::default(),
            body,
            ctx.token.as_ref(),
        )
        .await
    {
        Ok(_) => tracing::info!(subscription = %name, "optional EventSub subscription created"),
        Err(err) => tracing::warn!(
            subscription = %name,
            error = %err,
            "optional EventSub subscription failed (missing scope? best-effort, continuing)"
        ),
    }
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
                emit(
                    ctx,
                    UiEvent::Redemption {
                        user: redemption.user_name.as_str().to_owned(),
                        reward: redemption.reward.title.clone(),
                        cost: redemption.reward.cost,
                        input: (!redemption.user_input.is_empty())
                            .then(|| redemption.user_input.clone()),
                    },
                );
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
                emit(
                    ctx,
                    UiEvent::Chat {
                        user: message.chatter_user_name.as_str().to_owned(),
                        privileged: chat::is_admin(&message.badges),
                        text: message.message.text.clone(),
                    },
                );
                if let Some(zoom) = &ctx.zoom_user {
                    let login = message.chatter_user_login.as_str();
                    let name = message.chatter_user_name.as_str();
                    // Once per live, persisted across restarts via the zoom
                    // marker keyed on the current live id.
                    if (login.eq_ignore_ascii_case(zoom) || name.eq_ignore_ascii_case(zoom))
                        && claim_zoom(ctx).await
                    {
                        tracing::info!(user = %name, "zoom user speaking (first time this live)");
                        emit(
                            ctx,
                            UiEvent::Notice {
                                text: format!("Please zoom for {name}"),
                            },
                        );
                    }
                }
                return chat::dispatch(
                    &message,
                    &chat::ChatDeps {
                        helix: ctx.helix.clone(),
                        token: ctx.token.clone(),
                        rewards: ctx.rewards.clone(),
                        maison: ctx.maison.clone(),
                        yt: ctx.yt.clone(),
                        obs: ctx.obs.clone(),
                        club_url: ctx.club_url.clone(),
                        discord_url: ctx.discord_url.clone(),
                        melodie_url_file: ctx.melodie_url_file.clone(),
                    },
                )
                .await;
            }
        }
        Event::StreamOnlineV1(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                let key = format!("live:{}", n.id);
                {
                    let mut st = ctx.state.lock().await;
                    if !st.note_message(&key) {
                        tracing::debug!(key, "duplicate stream.online, skipping");
                        return Ok(());
                    }
                    // Genuinely new live → record its id, re-arm the notice.
                    st.start_live(&n.id);
                }
                notify_live(
                    ctx,
                    &n.id,
                    n.broadcaster_user_login.as_str(),
                    n.broadcaster_user_name.as_str(),
                    None,
                    None,
                )
                .await;
                return Ok(());
            }
        }
        other => emit_activity(ctx, other),
    }
    Ok(())
}

/// True if `marker_file` already records `stream_id` (already notified).
async fn already_notified(marker: &std::path::Path, stream_id: &str) -> bool {
    (tokio::fs::read_to_string(marker).await).is_ok_and(|c| c.trim() == stream_id)
}

/// Decide whether the "please zoom" notice fires now: once per live,
/// persisted across restarts via the zoom marker keyed on the current live
/// id. Falls back to an in-memory once-per-run guard when the live id is
/// unknown (no `stream.online`/startup live-check this run).
async fn claim_zoom(ctx: &EventSubContext) -> bool {
    let live = ctx.state.lock().await.current_live.clone();
    match (live, &ctx.zoom_marker) {
        (Some(id), Some(marker)) => {
            let path: &std::path::Path = marker;
            if already_notified(path, &id).await {
                return false;
            }
            if let Err(err) = tokio::fs::write(path, &id).await {
                tracing::warn!(error = %err, "could not persist zoom marker");
            }
            true
        }
        _ => ctx.state.lock().await.claim_zoom_notice(),
    }
}

/// Post the go-live webhook for `stream_id` unless it was already sent
/// (persisted in the marker file). `title`/`category` are passed when known
/// (startup path) and fetched from Helix otherwise (stream.online path).
async fn notify_live(
    ctx: &EventSubContext,
    stream_id: &str,
    login: &str,
    name: &str,
    title: Option<String>,
    category: Option<String>,
) {
    let Some(d) = &ctx.discord else {
        return;
    };
    if already_notified(d.marker_file.as_ref(), stream_id).await {
        tracing::debug!(stream_id, "go-live already sent for this stream, skipping");
        return;
    }

    let (mut title, category) = match (title, category) {
        (Some(t), Some(c)) => (t, c),
        _ => match ctx
            .helix
            .get_channel_from_id(&ctx.broadcaster_user_id, ctx.token.as_ref())
            .await
        {
            Ok(Some(ch)) => (ch.title, ch.game_name.as_str().to_owned()),
            Ok(None) => (String::new(), String::new()),
            Err(err) => {
                tracing::warn!(error = %err, "could not fetch channel info for go-live");
                (String::new(), String::new())
            }
        },
    };
    if let Some(custom) = &d.live_title {
        title = custom.as_str().to_owned();
    }

    let http = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(error = %err, "go-live: HTTP client build failed");
            return;
        }
    };
    match crate::discord::notify_go_live(
        &http,
        &d.webhook_url,
        &crate::discord::GoLive {
            name,
            login,
            title: &title,
            category: &category,
        },
    )
    .await
    {
        Ok(()) => {
            tracing::info!(broadcaster = %name, stream_id, "Discord go-live notification sent");
            if let Err(err) = tokio::fs::write(d.marker_file.as_ref(), stream_id).await {
                tracing::warn!(error = %err, "could not persist go-live marker (may re-notify)");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "Discord go-live notification failed (continuing)");
        }
    }
}

/// Startup path: if the channel is already live, notify once (guarded by
/// the marker file). Covers launching homie mid-stream; when offline this
/// is a no-op and `stream.online` handles the later transition.
pub async fn notify_if_live_now(ctx: &EventSubContext) {
    let Some(d) = &ctx.discord else {
        return;
    };
    let login = d.broadcaster_login.as_str();
    let logins = [login];
    let req = GetStreamsRequest::user_logins(&logins);
    match ctx.helix.req_get(req, ctx.token.as_ref()).await {
        Ok(resp) => {
            if let Some(s) = resp.data.into_iter().next() {
                // Repopulate the live id after a restart so the persisted
                // zoom marker stays once-per-live.
                ctx.state.lock().await.start_live(s.id.as_str());
                notify_live(
                    ctx,
                    s.id.as_str(),
                    s.user_login.as_str(),
                    s.user_name.as_str(),
                    Some(s.title),
                    Some(s.game_name),
                )
                .await;
            } else {
                tracing::info!("channel offline at startup; go-live will fire on stream.online");
            }
        }
        Err(err) => tracing::warn!(error = %err, "could not check live status at startup"),
    }
}

/// Mirror sub/follow/raid notifications to the TUI Activity panel.
/// Informational only: no dedup (a rare duplicate alert is harmless) and
/// no bot side effects.
fn emit_activity(ctx: &EventSubContext, event: Event) {
    match event {
        Event::ChannelSubscribeV1(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                // Gift recipients also surface here with is_gift = true; the
                // gift event already summarises them, so skip those.
                if !n.is_gift {
                    emit(
                        ctx,
                        UiEvent::Sub {
                            user: n.user_name.as_str().to_owned(),
                            tier: tier_label(&n.tier),
                        },
                    );
                }
            }
        }
        Event::ChannelSubscriptionMessageV1(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                let msg = n.message.text.trim();
                emit(
                    ctx,
                    UiEvent::Resub {
                        user: n.user_name.as_str().to_owned(),
                        tier: tier_label(&n.tier),
                        months: n.cumulative_months,
                        message: (!msg.is_empty()).then(|| msg.to_owned()),
                    },
                );
            }
        }
        Event::ChannelSubscriptionGiftV1(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                let gifter = if n.is_anonymous {
                    "Anonymous".to_owned()
                } else {
                    n.user_name
                        .as_ref()
                        .map_or_else(|| "Anonymous".to_owned(), |d| d.as_str().to_owned())
                };
                emit(
                    ctx,
                    UiEvent::GiftSub {
                        gifter,
                        total: n.total,
                        tier: tier_label(&n.tier),
                    },
                );
            }
        }
        Event::ChannelFollowV2(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                emit(
                    ctx,
                    UiEvent::Follow {
                        user: n.user_name.as_str().to_owned(),
                    },
                );
            }
        }
        Event::ChannelRaidV1(payload) => {
            if let EventMessage::Notification(n) = payload.message {
                emit(
                    ctx,
                    UiEvent::Raid {
                        from: n.from_broadcaster_user_name.as_str().to_owned(),
                        viewers: n.viewers,
                    },
                );
            }
        }
        _ => {}
    }
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
        Ok(message) => {
            tracing::info!(reward = %title, %message, "action executed");
            // Mirror the chat-command behaviour: confirm in chat. For an
            // `announce` action this *is* the whole point (the streamer
            // watches chat); for the others it's a nice acknowledgement.
            if let Some(reply) = chat::effective_reply(rule, &message) {
                if !reply.is_empty() {
                    chat::send_message(&ctx.helix, &ctx.token, &ctx.broadcaster_user_id, reply)
                        .await?;
                }
            }
        }
        Err(err) => {
            tracing::error!(reward = %title, error = %err, "action failed");
            chat::send_message(
                &ctx.helix,
                &ctx.token,
                &ctx.broadcaster_user_id,
                &format!("⚠ {title}: {err}"),
            )
            .await
            .ok();
        }
    }
    Ok(())
}

/// Default WebSocket entry point.
#[must_use]
pub fn default_websocket_url() -> &'static str {
    DEFAULT_WS_URL
}
