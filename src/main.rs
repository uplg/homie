use std::{process::ExitCode, sync::Arc, time::Duration};

use tokio::time::sleep;
use tracing_subscriber::EnvFilter;
use twitch_api::HelixClient;
use twitchy::{
    Result,
    config::AppConfig,
    error::Error,
    maison::MaisonClient,
    twitch::{
        auth::{self, DevicePrompt},
        eventsub::{self, EventSubContext},
    },
    yt_queue::{self, YtQueue},
};

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    dotenvy::dotenv().ok();
    install_rustls_provider();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "twitchy stopped on error");
            ExitCode::from(1)
        }
    }
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

async fn run() -> Result<()> {
    let cfg = AppConfig::load()?;
    tracing::info!(
        broadcaster = %cfg.env.twitch_broadcaster_login,
        rules = cfg.rewards.rules.len(),
        "configuration loaded",
    );

    // 1) Maison ----------
    let maison = Arc::new(MaisonClient::new(
        &cfg.env.maison_base_url,
        cfg.env.maison_username.clone(),
        cfg.env.maison_password.clone(),
    )?);
    maison.login().await?;
    tracing::info!(base = %cfg.env.maison_base_url, "Maison login OK");

    // 2) Twitch token (may block on Device Code Flow on first run) ----------
    let token = auth::acquire_user_token(&cfg.env.state_dir, &cfg.env.twitch_client_id, |prompt| {
        announce_device_code(prompt);
    })
    .await?;
    let token = Arc::new(token);
    tracing::info!(login = %token.login, "Twitch token ready");

    // 3) Resolve broadcaster user_id via Helix ----------
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

    let yt_cache = cfg.env.state_dir.join("yt-cache");
    let yt_cfg = yt_queue::Config {
        audio_device: cfg.env.audio_device.clone(),
        ..Default::default()
    };
    let yt = Arc::new(YtQueue::start(yt_cache, yt_cfg).await);

    let ctx = EventSubContext {
        helix,
        token: token.clone(),
        broadcaster_user_id: broadcaster.id,
        rewards: Arc::new(cfg.rewards.clone()),
        maison: maison.clone(),
        yt: yt.clone(),
    };

    // 4) Run EventSub loop with bounded reconnect backoff, alongside Ctrl+C ----------
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    let mut url = eventsub::default_websocket_url().to_string();
    let mut backoff = Duration::from_secs(1);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received, exiting");
                yt.shutdown();
                return Ok(());
            }
            res = eventsub::run_session(&ctx, &url) => {
                match res {
                    Ok(Some(new_url)) => {
                        tracing::info!(new_url = %new_url, "reconnecting on Twitch's request");
                        url = new_url;
                        backoff = Duration::from_secs(1);
                    }
                    Ok(None) => {
                        tracing::warn!(?backoff, "session ended cleanly, reconnecting after backoff");
                        sleep(backoff).await;
                        backoff = grow_backoff(backoff);
                        url = eventsub::default_websocket_url().to_string();
                    }
                    Err(err) => {
                        tracing::error!(error = %err, ?backoff, "session error, reconnecting after backoff");
                        sleep(backoff).await;
                        backoff = grow_backoff(backoff);
                        url = eventsub::default_websocket_url().to_string();
                    }
                }
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
    eprintln!("  Authorise twitchy by visiting:");
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

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("twitchy=info,warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
