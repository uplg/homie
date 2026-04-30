//! OBS WebSocket client used by the `!screen` chat command.
//!
//! When macOS Screen Capture sources freeze on stream — typically after the
//! display configuration changes or the session is locked — the only
//! reliable recovery is to force OBS to reinstantiate the source. The
//! technique used here mirrors the `obs_capture_freeze_monitor` Python
//! reference: read the input's current settings, write them back with the
//! `type` field flipped, wait briefly, then write the original settings
//! again. The first write tears the source down (OBS only acts on a real
//! diff), the second restores the user configuration.

use std::{collections::HashMap, sync::Arc};

use obws::{
    Client,
    requests::{
        inputs::{InputId, SetSettings},
        sources::{SourceId, TakeScreenshot},
    },
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    config::{ObsConfig, ObsMonitorConfig},
    error::{Error, Result},
};

/// PNG screenshot dimensions used for freeze detection. Kept small: comparing
/// strict equality on a tiny base64 blob is cheap and PNG is deterministic
/// for an unchanged frame so any pixel difference flips at least one byte.
const MONITOR_SCREENSHOT_WIDTH: u32 = 320;
const MONITOR_SCREENSHOT_HEIGHT: u32 = 180;
const MONITOR_SCREENSHOT_FORMAT: &str = "png";

/// Restarts a configured set of OBS capture sources on demand.
///
/// The connection is created lazily on the first call and re-used afterwards;
/// if a request fails (typical when OBS was restarted), the next call
/// transparently reconnects.
pub struct ObsRestarter {
    config: ObsConfig,
    client: Mutex<Option<Client>>,
}

impl ObsRestarter {
    #[must_use]
    pub fn new(config: ObsConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            client: Mutex::new(None),
        })
    }

    async fn connect(&self) -> Result<Client> {
        Client::connect(
            self.config.host.as_str(),
            self.config.port,
            self.config.password.as_deref(),
        )
        .await
        .map_err(|err| Error::config(format!("OBS WebSocket connect failed: {err}")))
    }

    /// Restart every source listed in the config. Returns a short human
    /// readable summary suitable for chat (e.g. `"restarted 2/3 source(s);
    /// failed: Display 3"`).
    pub async fn restart_all(&self) -> Result<String> {
        if self.config.sources.is_empty() {
            return Err(Error::config("no OBS sources configured"));
        }

        let mut ok = 0_usize;
        let mut failures: Vec<String> = Vec::new();

        for name in &self.config.sources {
            match self.restart_one(name).await {
                Ok(()) => ok += 1,
                Err(err) => {
                    tracing::warn!(source = %name, error = %err, "restart source failed");
                    failures.push(format!("{name} ({err})"));
                }
            }
        }

        let total = self.config.sources.len();
        if failures.is_empty() {
            Ok(format!("restarted {ok}/{total} source(s)"))
        } else {
            Ok(format!(
                "restarted {ok}/{total} source(s); failed: {}",
                failures.join(", "),
            ))
        }
    }

    async fn restart_one(&self, name: &str) -> Result<()> {
        // Two attempts: if the cached client is stale (OBS restarted) the
        // first call fails, we drop the cache and reconnect once.
        for attempt in 0..2 {
            let result = self.try_restart(name).await;
            match result {
                Ok(()) => return Ok(()),
                Err(err) if attempt == 0 => {
                    tracing::debug!(error = %err, "OBS call failed, will reconnect once");
                    let mut guard = self.client.lock().await;
                    *guard = None;
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("loop above always returns")
    }

    async fn try_restart(&self, name: &str) -> Result<()> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        let client = guard.as_ref().expect("client just inserted");

        let input = InputId::Name(name);
        let response = client
            .inputs()
            .settings::<Value>(input)
            .await
            .map_err(|err| Error::config(format!("get settings: {err}")))?;

        // OBS macOS Screen / Display / Window Capture inputs only restart
        // when at least one setting actually changes. Re-applying the exact
        // same settings is a no-op. The reliable trick (used by the Python
        // `obs_capture_freeze_monitor` reference) is to flip the `type`
        // field, wait briefly, and write the original settings back. The
        // first SetSettings forces OBS to tear the source down; the second
        // restores the user's configuration so the next frame matches.
        let mut original = response.settings;
        let original_type = original
            .as_object()
            .and_then(|obj| obj.get("type"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let toggled_type = i64::from(original_type == 0);

        let mut toggled = original.clone();
        if let Some(obj) = toggled.as_object_mut() {
            obj.insert("type".to_string(), Value::from(toggled_type));
        } else {
            // Settings was not a JSON object (very unlikely for a capture
            // input) — fall back to a single overlay=false write so we at
            // least notify OBS something changed.
            client
                .inputs()
                .set_settings(SetSettings {
                    input: InputId::Name(name),
                    settings: &original,
                    overlay: Some(false),
                })
                .await
                .map_err(|err| Error::config(format!("set settings: {err}")))?;
            return Ok(());
        }

        client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(name),
                settings: &toggled,
                overlay: Some(false),
            })
            .await
            .map_err(|err| Error::config(format!("set settings (toggle): {err}")))?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Make sure the type field is the original one (in case the input
        // had no `type` key originally we still write it explicitly so OBS
        // sees a real diff).
        if let Some(obj) = original.as_object_mut() {
            obj.insert("type".to_string(), Value::from(original_type));
        }
        client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(name),
                settings: &original,
                overlay: Some(false),
            })
            .await
            .map_err(|err| Error::config(format!("set settings (restore): {err}")))?;

        Ok(())
    }

    /// Take a small PNG screenshot of `name`, returned as the raw base64
    /// payload OBS hands back. Used by the freeze monitor: comparing two
    /// consecutive payloads with `==` is enough to detect a stalled source.
    async fn screenshot(&self, name: &str) -> Result<String> {
        for attempt in 0..2 {
            let result = self.try_screenshot(name).await;
            match result {
                Ok(data) => return Ok(data),
                Err(err) if attempt == 0 => {
                    tracing::debug!(error = %err, "OBS screenshot failed, will reconnect once");
                    let mut guard = self.client.lock().await;
                    *guard = None;
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("loop above always returns")
    }

    async fn try_screenshot(&self, name: &str) -> Result<String> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        let client = guard.as_ref().expect("client just inserted");

        client
            .sources()
            .take_screenshot(TakeScreenshot {
                source: SourceId::Name(name),
                format: MONITOR_SCREENSHOT_FORMAT,
                width: Some(MONITOR_SCREENSHOT_WIDTH),
                height: Some(MONITOR_SCREENSHOT_HEIGHT),
                compression_quality: Some(-1),
            })
            .await
            .map_err(|err| Error::config(format!("take screenshot: {err}")))
    }

    /// Spawn the automatic capture-freeze monitor on the current Tokio
    /// runtime when the config opts in. Returns `None` when monitoring is
    /// disabled. The returned [`tokio::task::JoinHandle`] is detached by the
    /// caller; the task runs until the process exits.
    #[must_use]
    pub fn spawn_monitor(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let monitor = self.config.monitor?;
        if self.config.sources.is_empty() {
            return None;
        }
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            this.run_monitor(monitor).await;
        });
        Some(handle)
    }

    async fn run_monitor(self: Arc<Self>, cfg: ObsMonitorConfig) {
        tracing::info!(
            interval_secs = cfg.interval.as_secs(),
            threshold = cfg.freeze_threshold,
            cooldown_secs = cfg.cooldown.as_secs(),
            sources = ?self.config.sources,
            "OBS capture-freeze monitor started",
        );

        // Per-source state: last screenshot payload + how many consecutive
        // identical screenshots we have observed so far. We only count after
        // the second sample so a single observation never triggers a restart.
        let mut last: HashMap<String, String> = HashMap::new();
        let mut identical: HashMap<String, u32> = HashMap::new();

        let mut ticker = tokio::time::interval(cfg.interval);
        // Skip the immediate first tick so the first screenshot is taken
        // after one full interval, giving OBS time to stabilise on startup.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            for name in &self.config.sources {
                match self.screenshot(name).await {
                    Ok(current) => {
                        let previous = last.get(name);
                        if previous == Some(&current) {
                            let count = identical.entry(name.clone()).or_insert(0);
                            *count += 1;
                            tracing::debug!(
                                source = %name,
                                count = *count,
                                threshold = cfg.freeze_threshold,
                                "OBS source unchanged",
                            );
                            if *count >= cfg.freeze_threshold {
                                tracing::warn!(
                                    source = %name,
                                    count = *count,
                                    "OBS source appears frozen, restarting",
                                );
                                match self.restart_one(name).await {
                                    Ok(()) => {
                                        tracing::info!(
                                            source = %name,
                                            "OBS source restarted by freeze monitor",
                                        );
                                    }
                                    Err(err) => {
                                        tracing::warn!(
                                            source = %name,
                                            error = %err,
                                            "freeze monitor restart failed",
                                        );
                                    }
                                }
                                // Reset state and let OBS settle. The next
                                // screenshot after the cooldown becomes the
                                // new baseline.
                                last.remove(name);
                                identical.remove(name);
                                tokio::time::sleep(cfg.cooldown).await;
                            }
                        } else {
                            identical.remove(name);
                            last.insert(name.clone(), current);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(
                            source = %name,
                            error = %err,
                            "freeze monitor screenshot failed; will retry next tick",
                        );
                        // Don't reset counters on a transient error: a brief
                        // OBS hiccup shouldn't mask an ongoing freeze.
                    }
                }
            }
        }
    }
}
