//! Glue between rule resolution and the Maison HTTP client.
//!
//! Resolution is split from execution so the matching can be unit-tested
//! against `EventSub` fixtures without spinning a real Maison server.

use std::sync::Arc;

use crate::{
    config::{Action, RewardsConfig, Rule},
    error::Result,
    maison::MaisonClient,
};

/// Resolve the rule that should fire for a redeemed channel-point reward.
#[must_use]
pub fn rule_for_reward<'cfg>(rewards: &'cfg RewardsConfig, title: &str) -> Option<&'cfg Rule> {
    rewards.find_for_reward(title)
}

/// Resolve the rule that should fire for a chat command (already parsed).
#[must_use]
pub fn rule_for_chat_command<'cfg>(
    rewards: &'cfg RewardsConfig,
    command: &str,
) -> Option<&'cfg Rule> {
    rewards.find_for_chat_command(command)
}

/// Execute the action attached to a rule.
pub async fn execute(rule: &Rule, maison: &Arc<MaisonClient>) -> Result<String> {
    match &rule.action {
        Action::LampPower { lamp_id, enabled } => {
            let resp = maison.set_lamp_power(lamp_id, *enabled).await?;
            Ok(resp.message)
        }
        Action::LampBrightness {
            lamp_id,
            brightness,
        } => {
            let resp = maison.set_lamp_brightness(lamp_id, *brightness).await?;
            Ok(resp.message)
        }
        Action::LampTemperature {
            lamp_id,
            temperature,
        } => {
            let resp = maison.set_lamp_temperature(lamp_id, *temperature).await?;
            Ok(resp.message)
        }
        Action::LampColor { lamp_id, x, y } => {
            let resp = maison.set_lamp_color(lamp_id, *x, *y).await?;
            Ok(resp.message)
        }
        Action::LampEffect { lamp_id, effect } => {
            let resp = maison.set_lamp_effect(lamp_id, effect).await?;
            Ok(resp.message)
        }
        Action::AcMitsubishi {
            host,
            command,
            model,
            local_ip,
        } => {
            let resp = maison
                .send_mitsubishi_command(host, command, model.as_deref(), local_ip.as_deref())
                .await?;
            Ok(resp.message)
        }
        Action::FeederFeed { device_id, portion } => {
            let resp = maison.feeder_feed(device_id, *portion).await?;
            Ok(resp.message)
        }
    }
}
