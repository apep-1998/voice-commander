//! Layering and inheritance: the two mechanisms that let a user write three lines instead of
//! a hundred, and the two most likely to surprise them if they are subtly wrong.
// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the shared helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used)]

use vc_core::config::{CaptureMode, Config, GapMode, Layer, Loaded, SinkMode};

fn load(layers: &[(&str, &str)]) -> Loaded {
    let mut all = vec![Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT)];
    all.extend(layers.iter().map(|(name, text)| Layer::new(*name, *text)));
    Config::from_layers(&all).expect("configuration should load")
}

const SINKS: &str = r#"
[sinks.a]
type = "command"
cmd = ["true"]
[sinks.b]
type = "command"
cmd = ["true"]
"#;

#[test]
fn later_layers_win_key_by_key() {
    let loaded = load(&[
        ("config.toml", "[audio]\nsample_rate = 48000\n"),
        ("conf.d/10-tweak.toml", "[audio]\nchannels = 2\n"),
    ]);

    // The second layer set only `channels`; `sample_rate` from the first must survive. A
    // whole-table replace here would silently reset every neighbouring key.
    assert_eq!(loaded.config.audio.sample_rate, 48_000);
    assert_eq!(loaded.config.audio.channels, 2);
    // And the baseline's own value is still there for anything nobody touched.
    assert_eq!(loaded.config.feedback.level_interval_ms, 50);
}

#[test]
fn arrays_replace_rather_than_accumulate() {
    let loaded = load(&[
        ("config.toml", SINKS),
        ("config.toml", "[profiles.p]\nsinks = [\"a\", \"b\"]\n"),
        ("conf.d/99-override.toml", "[profiles.p]\nsinks = [\"a\"]\n"),
    ]);

    // Concatenating would make it impossible to remove a sink in a drop-in file, which is
    // the main reason to have drop-in files at all.
    assert_eq!(loaded.config.profiles["p"].sinks, vec!["a".to_owned()]);
}

#[test]
fn profiles_inherit_every_default_section() {
    let loaded = load(&[("config.toml", "[profiles.p]\n")]);
    let profile = &loaded.config.profiles["p"];

    assert_eq!(profile.capture.mode, CaptureMode::Preroll);
    assert_eq!(profile.capture.pre_roll_ms, 500);
    assert_eq!(profile.session.cooldown_ms, 1_500);
    assert_eq!(profile.continuation.gap, GapMode::Keep);
}

#[test]
fn a_profile_override_is_a_patch_not_a_replacement() {
    let loaded = load(&[(
        "config.toml",
        r#"
[profiles.p]
capture = { pre_roll_ms = 900 }
session = { cooldown_ms = 4000 }
"#,
    )]);
    let profile = &loaded.config.profiles["p"];

    assert_eq!(profile.capture.pre_roll_ms, 900);
    // Overriding one field of `capture` must not wipe the rest of it.
    assert_eq!(profile.capture.mode, CaptureMode::Preroll);
    assert_eq!(profile.capture.idle_release_secs, 300);
    assert_eq!(profile.session.cooldown_ms, 4_000);
    assert_eq!(profile.session.max_recording_secs, 120);
}

#[test]
fn profiles_are_independent_of_each_other() {
    let loaded = load(&[(
        "config.toml",
        r#"
[profiles.fast]
capture = { mode = "warm", pre_roll_ms = 0 }
[profiles.slow]
capture = { pre_roll_ms = 2000 }
"#,
    )]);

    assert_eq!(
        loaded.config.profiles["fast"].capture.mode,
        CaptureMode::Warm
    );
    assert_eq!(loaded.config.profiles["fast"].capture.pre_roll_ms, 0);
    assert_eq!(
        loaded.config.profiles["slow"].capture.mode,
        CaptureMode::Preroll
    );
    assert_eq!(loaded.config.profiles["slow"].capture.pre_roll_ms, 2_000);
}

#[test]
fn changing_a_default_reaches_profiles_that_did_not_override_it() {
    let loaded = load(&[(
        "config.toml",
        r#"
[defaults.session]
cooldown_ms = 2500
[profiles.inherits]
[profiles.overrides]
session = { cooldown_ms = 100 }
"#,
    )]);

    assert_eq!(
        loaded.config.profiles["inherits"].session.cooldown_ms,
        2_500
    );
    assert_eq!(loaded.config.profiles["overrides"].session.cooldown_ms, 100);
}

#[test]
fn transcription_can_be_switched_off_explicitly_or_by_omission() {
    let loaded = load(&[(
        "config.toml",
        r#"
[transcribers.t]
type = "command"
cmd = ["true"]
text = { from = "stdout" }
[profiles.explicit]
transcriber = false
[profiles.omitted]
[profiles.named]
transcriber = "t"
"#,
    )]);

    assert_eq!(loaded.config.profiles["explicit"].transcriber, None);
    assert_eq!(loaded.config.profiles["omitted"].transcriber, None);
    assert_eq!(
        loaded.config.profiles["named"].transcriber.as_deref(),
        Some("t")
    );
}

#[test]
fn sink_mode_defaults_to_parallel() {
    let loaded = load(&[("config.toml", "[profiles.p]\n")]);
    // Callbacks are independent of each other, so the total wait should be the slowest one
    // rather than the sum of all of them.
    assert_eq!(loaded.config.profiles["p"].sink_mode, SinkMode::Parallel);
}
