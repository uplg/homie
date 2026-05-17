use std::{process::ExitCode, sync::Arc, time::Duration};

use homie::{
    Result,
    config::AppConfig,
    error::Error,
    maison::MaisonClient,
    obs::ObsRestarter,
    push_server,
    tui::{self, LogBuffer, UiEvent},
    twitch::{
        auth::{self, DevicePrompt},
        eventsub::{self, EventSubContext},
    },
    yt_queue::{self, YtQueue},
};
use tokio::{
    sync::{broadcast, watch},
    time::sleep,
};
use tracing_subscriber::EnvFilter;
use twitch_api::HelixClient;

#[tokio::main]
async fn main() -> ExitCode {
    // Load `.env` before init_tracing so HOMIE_TUI from the file is honoured.
    dotenvy::dotenv().ok();
    let log_buf = init_tracing(env_flag("HOMIE_TUI"));
    install_rustls_provider();

    match Box::pin(run(log_buf)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "homie stopped on error");
            // Belt-and-suspenders: a fatal error must be visible even if the
            // log filter is restrictive or output was redirected to the TUI.
            eprintln!("homie stopped on error: {err}");
            ExitCode::from(1)
        }
    }
}

/// Truthy-string env flag (`1`/`true`/`yes`/`on`, case-insensitive).
fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Several of our deps pull `rustls 0.23` transitively (`reqwest`, `tokio-tungstenite`,
/// `twitch_api`). With multiple `rustls-tls` features active, rustls cannot auto-pick
/// a `CryptoProvider` and panics on first TLS handshake. We install one explicitly
/// before any networking happens.
fn install_rustls_provider() {
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        // Another part of the process beat us to it; that's fine.
    }
}

async fn run(log_buf: Option<LogBuffer>) -> Result<()> {
    let cfg = AppConfig::load()?;
    tracing::info!(
        broadcaster = %cfg.env.twitch_broadcaster_login,
        rules = cfg.rewards.rules.len(),
        tui = log_buf.is_some(),
        "configuration loaded",
    );

    // The UI event bus exists only in TUI mode; emission is a no-op otherwise.
    let ui_tx: Option<broadcast::Sender<UiEvent>> = log_buf
        .is_some()
        .then(|| broadcast::channel::<UiEvent>(1024).0);

    let (ctx, yt) = Box::pin(assemble(&cfg, ui_tx.clone())).await?;
    Box::pin(drive(ctx, yt, log_buf, ui_tx)).await
}

/// Build every runtime dependency and the `EventSub` context. Performs the
/// Maison login, Twitch device-code auth, orphan-subscription cleanup, music
/// worker start and optional-feature wiring (OBS, push server, club/discord).
async fn assemble(
    cfg: &AppConfig,
    events: Option<broadcast::Sender<UiEvent>>,
) -> Result<(EventSubContext, Arc<YtQueue>)> {
    let maison = Arc::new(MaisonClient::new(
        &cfg.env.maison_base_url,
        cfg.env.maison_username.clone(),
        cfg.env.maison_password.clone(),
    )?);
    maison.login().await?;
    tracing::info!(base = %cfg.env.maison_base_url, "Maison login OK");

    let token = auth::acquire_user_token(&cfg.env.state_dir, &cfg.env.twitch_client_id, |prompt| {
        announce_device_code(prompt);
    })
    .await?;
    let token = Arc::new(token);
    tracing::info!(login = %token.login, "Twitch token ready");

    let helix: HelixClient<'static, reqwest::Client> = HelixClient::default();
    let broadcaster = helix
        .get_user_from_login(cfg.env.twitch_broadcaster_login.as_str(), token.as_ref())
        .await
        .map_err(|err| Error::twitch(format!("get_user_from_login: {err}")))?
        .ok_or_else(|| {
            Error::twitch(format!(
                "no Twitch user with login '{}'",
                cfg.env.twitch_broadcaster_login
            ))
        })?;

    // Wipe orphan EventSub subscriptions so the bot only sees events from
    // subscriptions it creates this run.
    eventsub::cleanup_existing_subscriptions(&helix, token.as_ref()).await?;

    let yt_cache = cfg.env.state_dir.join("yt-cache");
    let yt_cfg = yt_queue::Config {
        audio_device: cfg.env.audio_device.clone(),
        initial_volume_percent: cfg.env.initial_volume_percent,
        ..Default::default()
    };
    let yt = Arc::new(YtQueue::start(yt_cache, yt_cfg).await);

    let obs = cfg.env.obs.clone().map(ObsRestarter::new);
    if let Some(restarter) = obs.as_ref() {
        tracing::info!("OBS WebSocket configured; !screen command enabled");
        if restarter.spawn_monitor().is_some() {
            tracing::info!("OBS capture-freeze monitor enabled");
        }
    }
    let club_url = cfg.env.club_url.clone().map(Arc::new);
    let discord_url = cfg.env.discord_url.clone().map(Arc::new);
    if club_url.is_some() {
        tracing::info!("!club command enabled");
    }
    if discord_url.is_some() {
        tracing::info!("!discord command enabled");
    }

    let melodie_url_file = Arc::new(cfg.env.melodie_url_file.clone());
    tracing::info!(
        path = %melodie_url_file.display(),
        "!melodie command enabled (reads URL from this file)",
    );

    if let Some(push_cfg) = cfg.env.push.clone() {
        push_server::spawn(push_cfg, yt.clone())
            .await
            .map_err(|err| Error::twitch(format!("push server bind failed: {err}")))?;
    } else {
        tracing::info!(
            "push server disabled (set HOMIE_PUSH_TOKEN to enable melodie's 'push to live')",
        );
    }

    let ctx = EventSubContext {
        helix,
        token: token.clone(),
        broadcaster_user_id: broadcaster.id,
        rewards: Arc::new(cfg.rewards.clone()),
        maison: maison.clone(),
        yt: yt.clone(),
        obs,
        club_url,
        discord_url,
        melodie_url_file,
        state: Arc::new(tokio::sync::Mutex::new(eventsub::EventSubState::default())),
        events,
    };
    Ok((ctx, yt))
}

/// Headless: just run the `EventSub` loop. TUI mode: spawn the dashboard on
/// its own thread, run the loop concurrently, and tie their lifetimes
/// together via a shared shutdown signal.
async fn drive(
    ctx: EventSubContext,
    yt: Arc<YtQueue>,
    log_buf: Option<LogBuffer>,
    ui_tx: Option<broadcast::Sender<UiEvent>>,
) -> Result<()> {
    let shutdown = Arc::new(watch::channel(false).0);
    match (log_buf, ui_tx) {
        (Some(buf), Some(tx)) => {
            let rx = tx.subscribe();
            let sd = shutdown.clone();
            tracing::info!("starting TUI dashboard (press q or Esc to quit)");
            let ui = std::thread::spawn(move || tui::run(rx, buf, sd));
            let result = Box::pin(run_eventsub_loop(ctx, yt, shutdown.clone())).await;
            // Whatever stopped the loop, make sure the dashboard stops too.
            let _ = shutdown.send(true);
            match ui.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!(error = %err, "TUI exited with error"),
                Err(_) => tracing::error!("TUI thread panicked"),
            }
            tracing::info!("dashboard closed");
            result
        }
        _ => Box::pin(run_eventsub_loop(ctx, yt, shutdown)).await,
    }
}

/// `EventSub` session loop: run a session, reconnect with bounded backoff,
/// and exit on `Ctrl+C` or an external shutdown (TUI quit), stopping the
/// music worker on the way out.
async fn run_eventsub_loop(
    ctx: EventSubContext,
    yt: Arc<YtQueue>,
    shutdown: Arc<watch::Sender<bool>>,
) -> Result<()> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut sd = shutdown.subscribe();

    let mut url = eventsub::default_websocket_url().to_string();
    let mut backoff = Duration::from_secs(1);
    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                tracing::info!("Ctrl+C received, exiting");
                yt.shutdown();
                let _ = shutdown.send(true);
                return Ok(());
            }
            changed = sd.changed() => {
                if changed.is_err() || *sd.borrow() {
                    tracing::info!("shutdown requested, exiting");
                    yt.shutdown();
                    return Ok(());
                }
            }
            res = eventsub::run_session(&ctx, &url) => {
                match res {
                    Ok(Some(new_url)) => {
                        tracing::info!(new_url = %new_url, "reconnecting on Twitch's request");
                        url = new_url;
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    Ok(None) => {
                        tracing::warn!(?backoff, "session ended cleanly, reconnecting after backoff");
                    }
                    Err(err) => {
                        tracing::error!(error = %err, ?backoff, "session error, reconnecting after backoff");
                    }
                }
                sleep(backoff).await;
                backoff = grow_backoff(backoff);
                url = eventsub::default_websocket_url().to_string();
            }
        }
    }
}

fn announce_device_code(prompt: &DevicePrompt) {
    tracing::warn!(
        verification_uri = %prompt.verification_uri,
        user_code = %prompt.user_code,
        expires_in_secs = prompt.expires_in_secs,
        "Twitch device code: open the URL and enter the code",
    );
    eprintln!("\n────────────────────────────────────────────────────────");
    eprintln!("  Authorise homie by visiting:");
    eprintln!("    {}", prompt.verification_uri);
    eprintln!("  Code: {}", prompt.user_code);
    eprintln!(
        "  (valid for {} minutes)",
        prompt.expires_in_secs.saturating_div(60)
    );
    eprintln!("────────────────────────────────────────────────────────\n");
}

fn grow_backoff(current: Duration) -> Duration {
    const MAX: Duration = Duration::from_secs(60);
    let doubled = current.saturating_mul(2);
    if doubled > MAX { MAX } else { doubled }
}

/// Short, fixed UTC+2 log timestamp (`HH:MM:SS`) — no date, no sub-seconds,
/// no `Z`. UTC+2 is a fixed offset (no DST handling), as requested.
struct LocalShortTime;

impl tracing_subscriber::fmt::time::FormatTime for LocalShortTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let local = secs.saturating_add(2 * 3600); // UTC+2
        let day = local % 86_400;
        write!(
            w,
            "{:02}:{:02}:{:02}",
            day / 3600,
            (day % 3600) / 60,
            day % 60
        )
    }
}

/// Initialise `tracing`. In TUI mode the fmt layer writes (ANSI-free) into a
/// [`LogBuffer`] the dashboard renders, instead of stdout (which the TUI
/// owns). Returns that buffer iff TUI mode is on.
fn init_tracing(tui: bool) -> Option<LogBuffer> {
    // RUST_LOG handling, robust to an ambient value (shell export *or* a
    // `.env` line dotenvy loaded): unset/blank → sensible default; set but
    // not mentioning `homie` (e.g. `warn`, `off`, `info`) → keep the user's
    // directives but still surface the app's own logs by appending
    // `homie=info`; set and mentioning `homie` → respect it verbatim
    // (so `homie=debug` or even `homie=off` still work).
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) if !v.trim().is_empty() => {
            if v.split(',').any(|d| d.trim().starts_with("homie")) {
                EnvFilter::new(v)
            } else {
                EnvFilter::new(format!("{v},homie=info"))
            }
        }
        _ => EnvFilter::new("homie=info,warn"),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(LocalShortTime)
        .with_target(false);

    if tui {
        let buf = LogBuffer::new();
        builder.with_ansi(false).with_writer(buf.clone()).init();
        Some(buf)
    } else {
        builder.init();
        None
    }
}
