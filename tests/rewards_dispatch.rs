//! Integration tests for the redemption → action mapping logic.
//!
//! We feed the dispatcher with the exact `EventSub` WebSocket frame shape
//! Twitch documents, ensure the rule resolver picks the right `Rule`, and
//! double-check that the action variant in `RewardsConfig` matches what
//! the live client would translate into HTTP.

use homie::actions;
use homie::config::{Action, RewardsConfig};

const REWARDS_TOML: &str = r#"
[[rules]]
match = { reward = "Turn lamp on" }
action = { kind = "lamp_power", lamp_id = "00:17:88:01:09:04:ff:17", enabled = true }
reply = "Lamp -> ON"

[[rules]]
match = { reward = "Nourrir Apollo" }
action = { kind = "feeder_feed", device_id = "bf123abc456def", portion = 1 }
reply = "Apollo is being fed!"
"#;

const APOLLO_REDEMPTION_FRAME: &str = r#"
{
    "metadata": {
        "message_id": "msg-1",
        "message_type": "notification",
        "message_timestamp": "2026-04-30T20:00:00.000Z",
        "subscription_type": "channel.channel_points_custom_reward_redemption.add",
        "subscription_version": "1"
    },
    "payload": {
        "subscription": {
            "id": "sub-1",
            "type": "channel.channel_points_custom_reward_redemption.add",
            "version": "1",
            "status": "enabled",
            "cost": 0,
            "condition": {"broadcaster_user_id": "1337"},
            "transport": {"method": "websocket", "session_id": "sess-1"},
            "created_at": "2026-04-30T19:59:00Z"
        },
        "event": {
            "id": "redemption-1",
            "broadcaster_user_id": "1337",
            "broadcaster_user_login": "leonard",
            "broadcaster_user_name": "Leonard",
            "user_id": "9001",
            "user_login": "viewer",
            "user_name": "Viewer",
            "user_input": "",
            "status": "unfulfilled",
            "reward": {
                "id": "reward-apollo",
                "title": "Nourrir Apollo",
                "cost": 500,
                "prompt": "Feed Apollo one portion"
            },
            "redeemed_at": "2026-04-30T20:00:00Z"
        }
    }
}
"#;

#[test]
fn apollo_reward_resolves_to_feeder_action() {
    let rewards = RewardsConfig::parse(REWARDS_TOML).expect("parse rewards");

    // Parse the EventSub frame the same way the runtime would.
    let parsed = twitch_api::eventsub::Event::parse_websocket(APOLLO_REDEMPTION_FRAME)
        .expect("parse_websocket");

    // Pull the redemption title out of the typed event.
    let title = match parsed {
        twitch_api::eventsub::EventsubWebsocketData::Notification { payload, .. } => {
            match payload {
                twitch_api::eventsub::Event::ChannelPointsCustomRewardRedemptionAddV1(env) => {
                    match env.message {
                        twitch_api::eventsub::Message::Notification(redemption) => {
                            redemption.reward.title
                        }
                        other => panic!("expected a notification message, got {other:?}"),
                    }
                }
                other => panic!("expected redemption event, got {other:?}"),
            }
        }
        other => panic!("expected a notification frame, got {other:?}"),
    };

    let rule =
        actions::rule_for_reward(&rewards, &title).expect("'Nourrir Apollo' rule must resolve");

    match &rule.action {
        Action::FeederFeed { device_id, portion } => {
            assert_eq!(device_id, "bf123abc456def");
            assert_eq!(*portion, 1);
        }
        other => panic!("expected feeder_feed action, got {other:?}"),
    }
}

#[test]
fn unknown_reward_returns_no_rule() {
    let rewards = RewardsConfig::parse(REWARDS_TOML).expect("parse rewards");
    assert!(actions::rule_for_reward(&rewards, "Unknown reward").is_none());
}

#[test]
fn chat_command_lookup_is_case_sensitive_by_design() {
    // The TOML stores commands as written. We treat lookups as case-sensitive
    // so streamers can keep `!Foo` and `!foo` distinct if they want to.
    let toml = r#"
        [[rules]]
        match = { chat_command = "!ac_on" }
        admin_only = true
        action = { kind = "ac_mitsubishi", host = "10.0.0.1", command = "on", model = "msz" }
    "#;
    let rewards = RewardsConfig::parse(toml).expect("parse");

    assert!(actions::rule_for_chat_command(&rewards, "!ac_on").is_some());
    assert!(actions::rule_for_chat_command(&rewards, "!AC_ON").is_none());
    assert!(actions::rule_for_chat_command(&rewards, "ac_on").is_none());
}
