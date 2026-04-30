//! HTTP client for the Maison home API.
//!
//! Login obtains a JWT from the `Set-Cookie: maison_session=…` header on
//! `POST /api/auth/login`, then every subsequent request is sent with
//! `Authorization: Bearer <jwt>`. On a 401 the client re-logs in once and
//! replays the request.

use std::time::Duration;

use reqwest::{
    Method, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderValue, SET_COOKIE},
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::RwLock;
use url::Url;

use crate::error::{Error, Result};

pub mod ac;
pub mod feeder;
pub mod lamps;

const SESSION_COOKIE: &str = "maison_session";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct MaisonClient {
    http: reqwest::Client,
    /// Already includes the `/api` suffix.
    api_root: Url,
    username: String,
    password: String,
    token: RwLock<Option<String>>,
}

impl MaisonClient {
    pub fn new(base_url: &Url, username: String, password: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()?;

        let api_root = base_url
            .join("api/")
            .map_err(|err| Error::maison(format!("invalid base URL: {err}")))?;

        Ok(Self {
            http,
            api_root,
            username,
            password,
            token: RwLock::new(None),
        })
    }

    pub async fn login(&self) -> Result<()> {
        let url = self.endpoint("auth/login")?;
        let body = serde_json::json!({
            "username": self.username,
            "password": self.password,
        });
        let response = self.http.post(url).json(&body).send().await?;

        if !response.status().is_success() {
            return Err(Error::maison(format!(
                "login failed: HTTP {}",
                response.status()
            )));
        }

        let token = extract_session_jwt(response.headers()).ok_or_else(|| {
            Error::maison("login response missing maison_session cookie".to_string())
        })?;

        *self.token.write().await = Some(token);
        Ok(())
    }

    pub(crate) async fn request<B, R>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        // Make sure we have a token before the first attempt.
        if self.token.read().await.is_none() {
            self.login().await?;
        }

        // First attempt with current token.
        let response = self.send_once(&method, path, body).await?;
        let status = response.status();

        if status == StatusCode::UNAUTHORIZED {
            // Token may have expired — refresh once and replay.
            self.login().await?;
            let retry = self.send_once(&method, path, body).await?;
            return Self::decode(retry).await;
        }

        Self::decode(response).await
    }

    async fn send_once<B>(
        &self,
        method: &Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response>
    where
        B: Serialize + ?Sized,
    {
        let url = self.endpoint(path)?;
        let mut req = self.http.request(method.clone(), url);

        if let Some(token) = self.token.read().await.as_ref() {
            let header = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|err| Error::maison(format!("invalid bearer token: {err}")))?;
            req = req.header(AUTHORIZATION, header);
        }

        if let Some(body) = body {
            req = req.json(body);
        }

        Ok(req.send().await?)
    }

    async fn decode<R>(response: reqwest::Response) -> Result<R>
    where
        R: DeserializeOwned,
    {
        let status = response.status();
        if !status.is_success() {
            let snippet = response.text().await.unwrap_or_default();
            return Err(Error::maison(format!(
                "request failed: HTTP {status} — {}",
                truncate(&snippet, 256),
            )));
        }
        let parsed = response.json::<R>().await?;
        Ok(parsed)
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        let path = path.trim_start_matches('/');
        self.api_root
            .join(path)
            .map_err(|err| Error::maison(format!("invalid endpoint path `{path}`: {err}")))
    }
}

fn extract_session_jwt(headers: &HeaderMap) -> Option<String> {
    for cookie in headers.get_all(SET_COOKIE) {
        let raw = cookie.to_str().ok()?;
        // Each Set-Cookie value is `name=value; Attr; Attr; …`
        // Take the first segment, then check the cookie name.
        let first = raw.split(';').next()?.trim();
        if let Some((name, value)) = first.split_once('=') {
            if name == SESSION_COOKIE && !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn truncate(input: &str, max: usize) -> &str {
    if input.len() <= max {
        input
    } else {
        // Best-effort UTF-8 safe truncation.
        let mut end = max;
        while end > 0 && !input.is_char_boundary(end) {
            end -= 1;
        }
        &input[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn extract_jwt_picks_session_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("maison_session=abc.def.ghi; Path=/; HttpOnly; Max-Age=900"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("maison_refresh=uuid; Path=/api/auth"),
        );
        assert_eq!(
            extract_session_jwt(&headers).as_deref(),
            Some("abc.def.ghi")
        );
    }

    #[test]
    fn extract_jwt_returns_none_when_missing() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("maison_refresh=uuid; Path=/api/auth"),
        );
        assert!(extract_session_jwt(&headers).is_none());
    }

    #[test]
    fn endpoint_resolves_relative_path() {
        let url = Url::parse("http://192.168.1.10:3033").unwrap();
        let client = MaisonClient::new(&url, "u".into(), "p".into()).unwrap();
        let url = client.endpoint("zigbee/lamps/abc/power").unwrap();
        assert_eq!(
            url.as_str(),
            "http://192.168.1.10:3033/api/zigbee/lamps/abc/power"
        );
    }
}
