//! The embedded baseline is the one configuration every user starts from, so it gets its own
//! tests. If it stops parsing, nothing works and no user has done anything wrong.
// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the shared helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used)]

use vc_core::config::{CaptureMode, Config, GapMode, Layer, TriggerMode};

fn load(extra: &str) -> vc_core::config::Loaded {
    let layers = vec![
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("<test>", extra),
    ];
    Config::from_layers(&layers).expect("configuration should load")
}

#[test]
fn embedded_default_loads_with_no_user_config() {
    let loaded =
        Config::from_layers(&[Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT)])
            .expect("the built-in default must always load");

    let profile = loaded
        .config
        .profiles
        .get("default")
        .expect("baseline ships a `default` profile");

    // A fresh install must be useful with no API key and no external tools: it records, and
    // the recording is kept. Everything beyond that is opt-in.
    assert_eq!(profile.transcriber, None);
    assert!(profile.sinks.is_empty());
    assert_eq!(profile.trigger, TriggerMode::PushToTalk);
}

#[test]
fn embedded_default_values_match_the_documented_ones() {
    let loaded = load("");
    let config = &loaded.config;

    assert_eq!(config.audio.sample_rate, 16_000);
    assert_eq!(config.audio.channels, 1);
    assert_eq!(config.defaults.capture.mode, CaptureMode::Preroll);
    assert_eq!(config.defaults.capture.pre_roll_ms, 500);
    assert_eq!(config.defaults.capture.idle_release_secs, 300);
    assert_eq!(config.defaults.session.cooldown_ms, 1_500);
    assert_eq!(config.defaults.session.max_recording_secs, 120);
    assert_eq!(config.defaults.continuation.gap, GapMode::Keep);
    assert_eq!(config.defaults.continuation.max_segments, 5);
    assert_eq!(config.feedback.level_interval_ms, 50);
}

#[test]
fn shipped_example_config_is_valid() {
    // `config init` writes this file out. If it does not load, the first thing a new user
    // does is hit an error.
    let layers = vec![
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("example.toml", vc_core::config::EXAMPLE_CONFIG),
    ];
    let loaded = Config::from_layers(&layers).expect("the shipped example must be valid");

    // It is documentation, so it should demonstrate the interesting cases.
    assert!(loaded.config.profiles.contains_key("dictate"));
    assert!(loaded.config.profiles.contains_key("memo"));
    assert_eq!(loaded.config.profiles["memo"].transcriber, None);
    assert_eq!(
        loaded.config.profiles["meeting"].trigger,
        TriggerMode::Toggle
    );
}

#[test]
fn shipped_example_config_raises_no_warnings_about_itself() {
    let layers = vec![
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("example.toml", vc_core::config::EXAMPLE_CONFIG),
    ];
    let loaded = Config::from_layers(&layers).expect("valid");

    // The example defines every sink it defines *and uses every one of them*, so the
    // unused-definition warnings should be silent. A warning here means the example is
    // teaching something it does not then demonstrate.
    let unused: Vec<&vc_core::config::Issue> = loaded
        .warnings
        .iter()
        .filter(|issue| issue.message.contains("no profile uses it"))
        .collect();
    assert!(
        unused.is_empty(),
        "example has unused definitions: {unused:?}"
    );
}
