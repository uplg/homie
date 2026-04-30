use std::fs;

use twitchy::config::{Action, Matcher, RewardsConfig};

#[test]
fn example_rewards_file_parses_cleanly() {
    let path = std::path::Path::new("config/rewards.example.toml");
    let content = fs::read_to_string(path).expect("read example rewards");
    let cfg = RewardsConfig::parse(&content).expect("parse example rewards");

    assert!(
        !cfg.rules.is_empty(),
        "the example file should ship at least one rule"
    );

    // The Apollo feeder rule must be present and routed to a feeder action.
    let apollo = cfg
        .find_for_chat_command("!feed_apollo")
        .expect("`!feed_apollo` rule must exist in the example");
    match &apollo.action {
        Action::FeederFeed { portion, .. } => {
            assert!(*portion >= 1, "portion must default to >= 1");
        }
        other => panic!("Apollo command must trigger a feeder action, got {other:?}"),
    }

    // At least one chat command should be admin-only (the AC commands).
    let ac = cfg
        .find_for_chat_command("!ac_on")
        .expect("`!ac_on` chat command should exist");
    assert!(ac.admin_only);
    assert!(matches!(ac.matcher, Matcher::ChatCommand { .. }));
}

#[test]
fn invalid_toml_is_rejected_as_config_error() {
    let bad_input = r#"
        [[rules]]
        match = { reward = "X" }
        action = { kind = "lamp_power" }
    "#;
    let err = RewardsConfig::parse(bad_input).expect_err("missing fields must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("lamp_id") || msg.contains("missing"),
        "unexpected error message: {msg}"
    );
}
