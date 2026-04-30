use std::{env, path::PathBuf};

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

const DEFAULT_STATE_DIR: &str = "./.twitchy";
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
        let state_dir = env::var("TWITCHY_STATE_DIR")
            .unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string())
            .into();
        let rewards_file = env::var("REWARDS_FILE")
            .unwrap_or_else(|_| DEFAULT_REWARDS_FILE.to_string())
            .into();
        let audio_device = env::var("TWITCHY_AUDIO_DEVICE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        Ok(Self {
            twitch_client_id,
            twitch_broadcaster_login,
            maison_base_url,
            maison_username,
            maison_password,
            state_dir,
            rewards_file,
            audio_device,
        })
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
