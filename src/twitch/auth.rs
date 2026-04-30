//! Twitch OAuth — Device Code Flow with disk-persisted token cache.
//!
//! `acquire_user_token` is the only entry point: it consumes the cached
//! token if it is still fresh, refreshes it via the refresh token if not,
//! and falls back to a Device Code Flow as a last resort. Whatever the
//! path, the resulting token is persisted back to `<state_dir>/token.json`.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::fs;
use twitch_oauth2::{
    AccessToken, ClientId, RefreshToken, Scope, TwitchToken, UserToken,
    tokens::DeviceUserTokenBuilder,
};

use crate::error::{Error, Result};

const TOKEN_FILE: &str = "token.json";
/// Refuse a cached access token that would expire in less than this many seconds.
const MIN_REMAINING_LIFETIME_SECS: u64 = 60;

/// Scopes required by the bot.
#[must_use]
pub fn required_scopes() -> Vec<Scope> {
    vec![
        Scope::ChannelReadRedemptions,
        Scope::UserReadChat,
        Scope::UserWriteChat,
        Scope::UserBot,
        Scope::ChannelBot,
    ]
}

/// Persisted token cache.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenCache {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl TokenCache {
    pub fn from_user_token(token: &UserToken) -> Self {
        let now = unix_now();
        let expires_in = token.expires_in().as_secs();
        Self {
            access_token: token.access_token.secret().to_string(),
            refresh_token: token.refresh_token.as_ref().map(|r| r.secret().to_string()),
            expires_at: now.saturating_add(expires_in),
            scopes: token.scopes().iter().map(ToString::to_string).collect(),
        }
    }

    #[must_use]
    pub fn is_fresh(&self) -> bool {
        let now = unix_now();
        self.expires_at > now.saturating_add(MIN_REMAINING_LIFETIME_SECS)
    }
}

/// Acquire a Twitch user token, using the cache if possible.
///
/// `state_dir` is created if missing. `prompt` is invoked when an
/// interactive Device Code Flow is required, with the verification URL
/// and the user code to display.
pub async fn acquire_user_token<F>(
    state_dir: &Path,
    client_id: &str,
    prompt: F,
) -> Result<UserToken>
where
    F: FnOnce(&DevicePrompt),
{
    fs::create_dir_all(state_dir).await?;
    let cache_path = state_dir.join(TOKEN_FILE);
    let http = build_http_client()?;

    // 1) Try the cached token (validate / refresh as needed).
    if let Some(cache) = read_cache(&cache_path).await? {
        match try_use_cache(&http, &cache, client_id).await {
            Ok(token) => {
                let cache = TokenCache::from_user_token(&token);
                write_cache(&cache_path, &cache).await?;
                return Ok(token);
            }
            Err(err) => {
                tracing::warn!(error = %err, "cached Twitch token invalid, falling back to device flow");
            }
        }
    }

    // 2) Fall back to a fresh device code flow.
    let mut builder =
        DeviceUserTokenBuilder::new(ClientId::new(client_id.to_string()), required_scopes());
    let response = builder
        .start(&http)
        .await
        .map_err(|err| Error::twitch(format!("device code start failed: {err}")))?;

    prompt(&DevicePrompt {
        verification_uri: response.verification_uri.clone(),
        user_code: response.user_code.clone(),
        expires_in_secs: response.expires_in,
    });

    let token = builder
        .wait_for_code(&http, tokio::time::sleep)
        .await
        .map_err(|err| Error::twitch(format!("device code completion failed: {err}")))?;

    let cache = TokenCache::from_user_token(&token);
    write_cache(&cache_path, &cache).await?;
    Ok(token)
}

#[derive(Debug, Clone)]
pub struct DevicePrompt {
    pub verification_uri: String,
    pub user_code: String,
    pub expires_in_secs: u64,
}

async fn try_use_cache(
    http: &reqwest::Client,
    cache: &TokenCache,
    client_id: &str,
) -> Result<UserToken> {
    let access = AccessToken::new(cache.access_token.clone());
    let refresh = cache.refresh_token.clone().map(RefreshToken::new);

    if cache.is_fresh() {
        let token = UserToken::from_existing(http, access, refresh, None)
            .await
            .map_err(|err| Error::twitch(format!("validate cached token: {err}")))?;
        return Ok(token);
    }

    let Some(refresh) = refresh else {
        return Err(Error::twitch(
            "cached token expired and no refresh token available".to_string(),
        ));
    };

    let token = UserToken::from_existing_or_refresh_token(
        http,
        access,
        refresh,
        ClientId::new(client_id.to_string()),
        None,
    )
    .await
    .map_err(|err| Error::twitch(format!("refresh cached token: {err}")))?;
    Ok(token)
}

async fn read_cache(path: &Path) -> Result<Option<TokenCache>> {
    match fs::read(path).await {
        Ok(bytes) => {
            let parsed: TokenCache = serde_json::from_slice(&bytes)?;
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn write_cache(path: &Path, cache: &TokenCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let serialized = serde_json::to_vec_pretty(cache)?;
    fs::write(path, serialized).await?;
    Ok(())
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Twitch's auth endpoints rely on us not following redirects opaquely.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Error::from)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Convenience helper that resolves the canonical token cache path for a state directory.
#[must_use]
pub fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join(TOKEN_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_cache() -> TokenCache {
        TokenCache {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: unix_now() + 3600,
            scopes: vec!["channel:read:redemptions".into(), "user:read:chat".into()],
        }
    }

    #[tokio::test]
    async fn cache_round_trip() {
        let dir = tempdir().unwrap();
        let path = cache_path(dir.path());
        let cache = sample_cache();

        write_cache(&path, &cache).await.unwrap();
        let loaded = read_cache(&path).await.unwrap().expect("cache file");
        assert_eq!(loaded, cache);
    }

    #[tokio::test]
    async fn missing_cache_returns_none() {
        let dir = tempdir().unwrap();
        let path = cache_path(dir.path());
        let loaded = read_cache(&path).await.unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn cache_freshness_respects_grace() {
        let mut cache = sample_cache();
        // Fresh.
        assert!(cache.is_fresh());

        // Token already expired.
        cache.expires_at = unix_now().saturating_sub(10);
        assert!(!cache.is_fresh());

        // Token expires within the grace window — should be considered stale.
        cache.expires_at = unix_now() + (MIN_REMAINING_LIFETIME_SECS / 2);
        assert!(!cache.is_fresh());
    }

    #[test]
    fn required_scopes_includes_redemptions_and_chat() {
        let scopes = required_scopes();
        assert!(scopes.contains(&Scope::ChannelReadRedemptions));
        assert!(scopes.contains(&Scope::UserReadChat));
        assert!(scopes.contains(&Scope::UserWriteChat));
    }
}
