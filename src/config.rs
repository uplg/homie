use std::{env, path::PathBuf};

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

const DEFAULT_STATE_DIR: &str = "./.homie";
const DEFAULT_REWARDS_FILE: &str = "config/rewards.toml";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub env: EnvConfig,
    pub rewards: RewardsConfig,
}

#[derive(Debug, Clone)]
pub struct EnvConfig {
    pub twitch_client_id: String,
    pub twitch_broadcaster_login: String,
    pub maison_base_url: Url,
    pub maison_username: String,
    pub maison_password: String,
    pub state_dir: PathBuf,
    pub rewards_file: PathBuf,
    /// Optional substring matched against macOS output device names to pick
    /// the audio output the bot writes to. Useful to route the bot through
    /// a virtual device (`BlackHole`, Loopback) so OBS can capture it. If
    /// `None`, the system default output device is used.
    pub audio_device: Option<String>,
    /// Initial volume (0–100) the music queue starts at. Defaults to 80.
    pub initial_volume_percent: u8,
    /// Optional URL the `!club` chat command echoes back. When `None`, the
    /// command is disabled (no reply, not advertised in `!commands`).
    pub club_url: Option<String>,
    /// Optional URL the `!discord` chat command echoes back. When `None`,
    /// the command is disabled (no reply, not advertised in `!commands`).
    pub discord_url: Option<String>,
    /// Optional OBS WebSocket configuration for the `!screen` command. When
    /// `None`, `!screen` is disabled.
    pub obs: Option<ObsConfig>,
}

/// OBS WebSocket connection details for the `!screen` capture-restart command.
#[derive(Debug, Clone)]
pub struct ObsConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// Names of OBS input sources (typically Display/Window/macOS Screen
    /// Capture) the `!screen` command should restart. Restarted in the
    /// listed order.
    pub sources: Vec<String>,
    /// When `Some`, the bot polls each configured source via OBS screenshot
    /// API and force-restarts it when the screenshot stops changing. The
    /// `!screen` command stays available as a manual override regardless.
    pub monitor: Option<ObsMonitorConfig>,
}

/// Tunables for the automatic capture-freeze monitor.
#[derive(Debug, Clone, Copy)]
pub struct ObsMonitorConfig {
    /// Delay between two consecutive checks of a single source.
    pub interval: std::time::Duration,
    /// Number of consecutive identical screenshots required before declaring
    /// the source frozen and triggering a restart. The Python reference uses
    /// 2 (i.e. one interval = a clear stall, two = restart).
    pub freeze_threshold: u32,
    /// Quiet period after a successful restart to let OBS reinitialise the
    /// source before resuming the comparisons. Avoids restart loops.
    pub cooldown: std::time::Duration,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RewardsConfig {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(rename = "match")]
    pub matcher: Matcher,
    pub action: Action,
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub admin_only: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Matcher {
    Reward { reward: String },
    ChatCommand { chat_command: String },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    LampPower {
        lamp_id: String,
        enabled: bool,
    },
    LampBrightness {
        lamp_id: String,
        brightness: u8,
    },
    LampTemperature {
        lamp_id: String,
        temperature: u8,
    },
    LampColor {
        lamp_id: String,
        x: f32,
        y: f32,
    },
    LampEffect {
        lamp_id: String,
        effect: String,
    },
    AcMitsubishi {
        host: String,
        command: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        local_ip: Option<String>,
    },
    FeederFeed {
        device_id: String,
        #[serde(default = "default_portion")]
        portion: u64,
    },
}

fn default_portion() -> u64 {
    1
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let env = EnvConfig::from_env()?;
        let rewards = RewardsConfig::from_path(&env.rewards_file)?;
        Ok(Self { env, rewards })
    }
}

impl EnvConfig {
    pub fn from_env() -> Result<Self> {
        let twitch_client_id = required("TWITCH_CLIENT_ID")?;
        let twitch_broadcaster_login = required("TWITCH_BROADCASTER_LOGIN")?;
        let maison_base_url = Url::parse(&required("MAISON_BASE_URL")?)?;
        let maison_username = required("MAISON_USERNAME")?;
        let maison_password = required("MAISON_PASSWORD")?;
        let state_dir = env::var("HOMIE_STATE_DIR")
            .unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string())
            .into();
        let rewards_file = env::var("REWARDS_FILE")
            .unwrap_or_else(|_| DEFAULT_REWARDS_FILE.to_string())
            .into();
        let audio_device = env::var("HOMIE_AUDIO_DEVICE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let initial_volume_percent = match env::var("HOMIE_INITIAL_VOLUME") {
            Ok(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    80
                } else {
                    let parsed = trimmed.parse::<u8>().map_err(|err| {
                        Error::config(format!(
                            "HOMIE_INITIAL_VOLUME must be an integer 0-100: {err}"
                        ))
                    })?;
                    if parsed > 100 {
                        return Err(Error::config(
                            "HOMIE_INITIAL_VOLUME must be between 0 and 100",
                        ));
                    }
                    parsed
                }
            }
            Err(_) => 80,
        };

        let club_url = env::var("HOMIE_CLUB_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let discord_url = env::var("HOMIE_DISCORD_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let obs = ObsConfig::from_env()?;

        Ok(Self {
            twitch_client_id,
            twitch_broadcaster_login,
            maison_base_url,
            maison_username,
            maison_password,
            state_dir,
            rewards_file,
            audio_device,
            initial_volume_percent,
            club_url,
            discord_url,
            obs,
        })
    }
}

impl ObsConfig {
    /// Parse OBS settings from `OBS_WS_HOST`/`OBS_WS_PORT`/`OBS_WS_PASSWORD`/
    /// `OBS_SOURCES`. Returns `None` when neither host nor sources are set,
    /// so users who don't run OBS can simply omit the variables.
    fn from_env() -> Result<Option<Self>> {
        let host = env::var("OBS_WS_HOST")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let sources_raw = env::var("OBS_SOURCES")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        if host.is_none() && sources_raw.is_none() {
            return Ok(None);
        }

        let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
        let port = match env::var("OBS_WS_PORT") {
            Ok(raw) => raw.trim().parse::<u16>().map_err(|err| {
                Error::config(format!("OBS_WS_PORT must be a 0-65535 integer: {err}"))
            })?,
            Err(_) => 4455,
        };
        let password = env::var("OBS_WS_PASSWORD")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let sources: Vec<String> = sources_raw
            .map(|raw| {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if sources.is_empty() {
            return Err(Error::config(
                "OBS_SOURCES must list at least one capture source name (comma-separated)",
            ));
        }

        let monitor = ObsMonitorConfig::from_env()?;

        Ok(Some(Self {
            host,
            port,
            password,
            sources,
            monitor,
        }))
    }
}

impl ObsMonitorConfig {
    /// Parse the auto-monitor settings. Disabled by default; set
    /// `OBS_MONITOR_ENABLED=true` (or `1`) to opt in.
    fn from_env() -> Result<Option<Self>> {
        let enabled = env::var("OBS_MONITOR_ENABLED")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        if !enabled {
            return Ok(None);
        }

        let interval_secs = parse_positive_u64_env("OBS_MONITOR_INTERVAL_SECS", 60)?;
        let freeze_threshold =
            u32::try_from(parse_positive_u64_env("OBS_MONITOR_FREEZE_THRESHOLD", 2)?)
                .map_err(|_| Error::config("OBS_MONITOR_FREEZE_THRESHOLD must fit in a u32"))?;
        if freeze_threshold == 0 {
            return Err(Error::config(
                "OBS_MONITOR_FREEZE_THRESHOLD must be at least 1",
            ));
        }
        let cooldown_secs = parse_positive_u64_env("OBS_MONITOR_COOLDOWN_SECS", 30)?;

        Ok(Some(Self {
            interval: std::time::Duration::from_secs(interval_secs),
            freeze_threshold,
            cooldown: std::time::Duration::from_secs(cooldown_secs),
        }))
    }
}

fn parse_positive_u64_env(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            let value = trimmed.parse::<u64>().map_err(|err| {
                Error::config(format!("{name} must be a positive integer: {err}"))
            })?;
            if value == 0 {
                return Err(Error::config(format!("{name} must be greater than zero")));
            }
            Ok(value)
        }
        Err(_) => Ok(default),
    }
}

impl RewardsConfig {
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|err| {
            Error::config(format!(
                "cannot read rewards file at {}: {err}",
                path.display()
            ))
        })?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self> {
        let parsed: Self = toml::from_str(content)?;
        Ok(parsed)
    }

    #[must_use]
    pub fn find_for_reward(&self, title: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| matches!(&rule.matcher, Matcher::Reward { reward } if reward == title))
    }

    #[must_use]
    pub fn find_for_chat_command(&self, command: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            matches!(&rule.matcher, Matcher::ChatCommand { chat_command } if chat_command == command)
        })
    }
}

fn required(key: &str) -> Result<String> {
    let value = env::var(key).map_err(|_| Error::config(format!("missing env var {key}")))?;
    if value.trim().is_empty() {
        return Err(Error::config(format!("env var {key} is empty")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lamp_power_rule() {
        let toml_input = r#"
            [[rules]]
            match = { reward = "Turn lamp on" }
            action = { kind = "lamp_power", lamp_id = "00:17:88:01:09:04:ff:17", enabled = true }
            reply = "ON"
        "#;
        let cfg = RewardsConfig::parse(toml_input).expect("parse");
        assert_eq!(cfg.rules.len(), 1);
        let rule = &cfg.rules[0];
        assert_eq!(
            rule.matcher,
            Matcher::Reward {
                reward: "Turn lamp on".into()
            }
        );
        match &rule.action {
            Action::LampPower { lamp_id, enabled } => {
                assert_eq!(lamp_id, "00:17:88:01:09:04:ff:17");
                assert!(*enabled);
            }
            other => panic!("unexpected action: {other:?}"),
        }
        assert_eq!(rule.reply.as_deref(), Some("ON"));
        assert!(!rule.admin_only);
    }

    #[test]
    fn parses_feeder_with_default_portion() {
        let toml_input = r#"
            [[rules]]
            match = { reward = "Nourrir Apollo" }
            action = { kind = "feeder_feed", device_id = "feeder-1" }
        "#;
        let cfg = RewardsConfig::parse(toml_input).unwrap();
        match &cfg.rules[0].action {
            Action::FeederFeed { device_id, portion } => {
                assert_eq!(device_id, "feeder-1");
                assert_eq!(*portion, 1);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn parses_chat_command_admin_only() {
        let toml_input = r#"
            [[rules]]
            match = { chat_command = "!clim_on" }
            admin_only = true
            action = { kind = "ac_mitsubishi", host = "192.168.1.42", command = "power_on", model = "msz" }
        "#;
        let cfg = RewardsConfig::parse(toml_input).unwrap();
        let rule = &cfg.rules[0];
        assert!(rule.admin_only);
        assert_eq!(
            rule.matcher,
            Matcher::ChatCommand {
                chat_command: "!clim_on".into()
            }
        );
    }

    #[test]
    fn rejects_unknown_action_kind() {
        let toml_input = r#"
            [[rules]]
            match = { reward = "X" }
            action = { kind = "shenanigans" }
        "#;
        assert!(RewardsConfig::parse(toml_input).is_err());
    }

    #[test]
    fn empty_rewards_config_is_ok() {
        let cfg = RewardsConfig::parse("").unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn find_for_reward_returns_match() {
        let cfg = RewardsConfig::parse(
            r#"
            [[rules]]
            match = { reward = "A" }
            action = { kind = "lamp_power", lamp_id = "x", enabled = true }

            [[rules]]
            match = { reward = "B" }
            action = { kind = "lamp_power", lamp_id = "y", enabled = false }
            "#,
        )
        .unwrap();
        assert!(cfg.find_for_reward("A").is_some());
        assert!(cfg.find_for_reward("B").is_some());
        assert!(cfg.find_for_reward("C").is_none());
    }
}
