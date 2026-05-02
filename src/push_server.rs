//! Tiny loopback HTTP server used by the sister `melodie` project to push a
//! generated clip's CDN URL into the music queue.
//!
//! Bound to `127.0.0.1` only — never exposed publicly. Auth is a Bearer
//! token compared in constant time. The handler delegates to
//! [`YtQueue::enqueue`] which already accepts "direct audio URLs" (the
//! suno CDN links land in that branch).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::config::PushServerConfig;
use crate::yt_queue::{EnqueueOutcome, YtQueue};

#[derive(Clone)]
struct PushState {
    yt: Arc<YtQueue>,
    token: Arc<String>,
}

#[derive(Debug, Deserialize)]
struct PushRequest {
    url: String,
    /// Display name for who requested the track. Defaults to `"melodie"` when
    /// omitted — the queue uses this for `!queue` listings and log lines.
    #[serde(default)]
    requested_by: Option<String>,
}

#[derive(Debug, Serialize)]
struct PushResponse {
    queued: bool,
    title: Option<String>,
    position: Option<usize>,
    /// Set when the track was rejected (queue full, unsupported URL, …) so
    /// the caller can surface the message to the user.
    error: Option<String>,
}

/// Spawn the push server in the background. Returns a handle that, when
/// dropped, does NOT stop the server — callers rely on tokio runtime shutdown
/// (Ctrl+C) to bring everything down.
pub async fn spawn(cfg: PushServerConfig, yt: Arc<YtQueue>) -> std::io::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let state = PushState {
        yt,
        token: Arc::new(cfg.token),
    };
    let app = Router::new()
        .route("/push", post(handle_push))
        .with_state(state);

    tracing::info!(%addr, "push server listening (POST /push, Bearer auth)");

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "push server stopped");
        }
    });

    Ok(())
}

async fn handle_push(
    State(state): State<PushState>,
    headers: HeaderMap,
    Json(req): Json<PushRequest>,
) -> Response {
    if !auth_ok(&headers, state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }

    let url = req.url.trim();
    if url.is_empty() {
        return (StatusCode::BAD_REQUEST, "url must not be empty").into_response();
    }

    let requested_by = req
        .requested_by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("melodie");

    let outcome = state.yt.enqueue(url, requested_by).await;
    match outcome {
        EnqueueOutcome::StartingNow { title } => (
            StatusCode::OK,
            Json(PushResponse {
                queued: true,
                title: Some(title),
                position: Some(0),
                error: None,
            }),
        )
            .into_response(),
        EnqueueOutcome::Queued { title, position } => (
            StatusCode::OK,
            Json(PushResponse {
                queued: true,
                title: Some(title),
                position: Some(position),
                error: None,
            }),
        )
            .into_response(),
        EnqueueOutcome::Rejected(reason) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(PushResponse {
                queued: false,
                title: None,
                position: None,
                error: Some(reason),
            }),
        )
            .into_response(),
    }
}

/// Constant-time check against `Authorization: Bearer <token>`.
fn auth_ok(headers: &HeaderMap, expected: &str) -> bool {
    let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = auth.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn auth_ok_requires_bearer_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer secret".parse().unwrap(),
        );
        assert!(auth_ok(&headers, "secret"));

        let mut bare = HeaderMap::new();
        bare.insert(
            axum::http::header::AUTHORIZATION,
            "secret".parse().unwrap(),
        );
        assert!(!auth_ok(&bare, "secret"));

        assert!(!auth_ok(&HeaderMap::new(), "secret"));
    }
}
