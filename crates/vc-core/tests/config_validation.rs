//! Every way a configuration can be wrong, and what the user is told about it.
//!
//! These assert on the *message*, not just on failure. A validator that rejects a config
//! without saying which key or why is barely better than one that accepts it silently.
// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the shared helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use vc_core::config::{Config, ConfigError, Issue, Layer, Loaded};

fn try_load(text: &str) -> Result<Loaded, ConfigError> {
    Config::from_layers(&[
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("config.toml", text),
    ])
}

fn errors(text: &str) -> Vec<Issue> {
    match try_load(text) {
        Err(ConfigError::Invalid { issues }) => issues,
        Err(other) => panic!("expected validation errors, got {other}"),
        Ok(_) => panic!("expected this configuration to be rejected:\n{text}"),
    }
}

fn warnings(text: &str) -> Vec<Issue> {
    try_load(text)
        .expect("expected this to load with warnings")
        .warnings
}

fn assert_reports(issues: &[Issue], path: &str, needle: &str) {
    let matched = issues
        .iter()
        .any(|issue| issue.path == path && issue.message.contains(needle));
    assert!(
        matched,
        "expected an issue at {path:?} mentioning {needle:?}, got: {issues:#?}"
    );
}

// ── dangling references ──────────────────────────────────────────────────────

#[test]
fn a_profile_naming_a_missing_transcriber_is_rejected() {
    let issues = errors("[profiles.p]\ntranscriber = \"nope\"\n");
    assert_reports(&issues, "profiles.p.transcriber", "no transcriber named");
}

#[test]
fn a_profile_naming_a_missing_sink_is_rejected() {
    let issues = errors("[profiles.p]\nsinks = [\"nope\"]\n");
    assert_reports(&issues, "profiles.p.sinks", "no sink named");
}

#[test]
fn a_near_miss_reference_suggests_the_intended_name() {
    let issues = errors(
        r#"
[transcribers.openai]
type = "openai"
api_key = { env = "K" }
[profiles.p]
transcriber = "openia"
"#,
    );
    let hint = issues
        .iter()
        .find(|issue| issue.path == "profiles.p.transcriber")
        .and_then(|issue| issue.hint.clone())
        .expect("a one-transposition typo deserves a suggestion");
    assert!(hint.contains("openai"), "unhelpful hint: {hint}");
}

// ── unknown keys ─────────────────────────────────────────────────────────────

#[test]
fn a_misspelled_key_is_rejected_rather_than_ignored() {
    // The failure mode this exists to prevent: the config parses, the daemon starts, and the
    // setting quietly does nothing.
    let issues = errors("[defaults.session]\ncooldown_msec = 3000\n");
    assert_reports(
        &issues,
        "defaults.session.cooldown_msec",
        "unknown configuration key",
    );
}

#[test]
fn a_misspelled_key_suggests_the_real_one() {
    let issues = errors("[defaults.capture]\npre_roll_ms_ = 200\n");
    let hint = issues
        .iter()
        .find(|issue| issue.path.ends_with("pre_roll_ms_"))
        .and_then(|issue| issue.hint.clone())
        .expect("expected a suggestion");
    assert!(hint.contains("pre_roll_ms"), "unhelpful hint: {hint}");
}

#[test]
fn user_chosen_keys_in_free_form_maps_are_not_flagged() {
    // Headers, form fields and environment variables are whatever the provider calls them.
    // Checking those against a schema would make the generic adapters useless.
    let loaded = try_load(
        r#"
[transcribers.t]
type = "http"
url = "https://example.com/v1"
audio = { how = "multipart", field = "file" }
headers = { Authorization = "Bearer ${K}", "X-Whatever-They-Call-It" = "1" }
form = { model = "some-model", diarize = "true" }
[sinks.s]
type = "command"
cmd = ["true"]
env = { MY_OWN_VARIABLE = "1" }
[profiles.p]
transcriber = "t"
sinks = ["s"]
"#,
    );
    assert!(
        loaded.is_ok(),
        "free-form keys were wrongly rejected: {loaded:?}"
    );
}

// ── values that parse but cannot work ────────────────────────────────────────

#[test]
fn an_unsupported_sample_rate_is_rejected_with_the_valid_set() {
    let issues = errors("[audio]\nsample_rate = 12345\n");
    assert_reports(&issues, "audio.sample_rate", "not a supported rate");
    let hint = issues[0].hint.clone().unwrap_or_default();
    assert!(
        hint.contains("16000"),
        "hint should list valid rates: {hint}"
    );
}

#[test]
fn a_disabled_watchdog_is_rejected() {
    // Without it, a missed release keybind records until the disk fills.
    let issues = errors("[defaults.session]\nmax_recording_secs = 0\n");
    assert_reports(
        &issues,
        "profiles.default.session.max_recording_secs",
        "non-zero",
    );
}

#[test]
fn a_total_limit_below_the_per_recording_limit_is_rejected() {
    let issues = errors("[defaults.session]\nmax_recording_secs = 300\nmax_total_secs = 60\n");
    assert_reports(
        &issues,
        "profiles.default.session.max_total_secs",
        "at least",
    );
}

#[test]
fn a_positive_dbfs_threshold_is_rejected() {
    let issues = errors("[levels]\nsilence_dbfs = 6.0\n");
    assert_reports(&issues, "levels.silence_dbfs", "above full scale");
}

#[test]
fn inverted_level_thresholds_are_rejected() {
    let issues = errors("[levels]\nsilence_dbfs = -20.0\ntoo_quiet_dbfs = -50.0\n");
    assert_reports(
        &issues,
        "levels.silence_dbfs",
        "below levels.too_quiet_dbfs",
    );
}

#[test]
fn an_api_key_with_no_source_is_rejected() {
    let issues = errors(
        r#"
[transcribers.t]
type = "openai"
api_key = {}
[profiles.p]
transcriber = "t"
"#,
    );
    assert_reports(
        &issues,
        "transcribers.t.api_key",
        "no source for the API key",
    );
}

#[test]
fn a_url_without_a_scheme_is_rejected() {
    let issues = errors(
        r#"
[transcribers.t]
type = "http"
url = "api.example.com/v1"
audio = { how = "raw_body" }
[profiles.p]
transcriber = "t"
"#,
    );
    assert_reports(&issues, "transcribers.t.url", "not an http(s) URL");
}

#[test]
fn an_empty_command_is_rejected() {
    let issues = errors(
        r#"
[sinks.s]
type = "command"
cmd = []
[profiles.p]
sinks = ["s"]
"#,
    );
    assert_reports(&issues, "sinks.s.cmd", "is empty");
}

#[test]
fn a_pre_roll_longer_than_the_cap_is_rejected() {
    let issues = errors("[defaults.capture]\npre_roll_ms = 60000\n");
    assert_reports(&issues, "profiles.default.capture.pre_roll_ms", "maximum");
}

#[test]
fn a_config_with_no_profiles_is_rejected() {
    // Nothing is bound to anything, so nothing can ever run.
    let issues = match Config::from_layers(&[Layer::new(
        "only.toml",
        r#"
[audio]
sample_rate = 16000
channels = 1
format = "wav"
[levels]
silence_dbfs = -55.0
too_quiet_dbfs = -40.0
speech_dbfs = -45.0
silence_warn_after_ms = 1000
[storage]
max_age_days = 0
max_total_bytes = 0
keep_audio_after_transcribe = true
[feedback]
enabled = false
listening = false
processing = false
processing_linger_ms = 0
level_interval_ms = 50
presenters = []
[defaults.capture]
mode = "warm"
pre_roll_ms = 0
idle_release_secs = 0
device = "default"
[defaults.session]
cooldown_ms = 0
max_recording_secs = 60
max_total_secs = 60
[defaults.continuation]
gap = "drop"
silence_ms = 0
max_segments = 1
"#,
    )]) {
        Err(ConfigError::Invalid { issues }) => issues,
        other => panic!("expected rejection, got {other:?}"),
    };
    assert_reports(&issues, "profiles", "no profiles are defined");
}

#[test]
fn every_problem_is_reported_in_one_pass() {
    // Fixing one typo, re-running, and finding the next one is a miserable loop.
    let issues = errors(
        r#"
[audio]
sample_rate = 999
[levels]
silence_dbfs = 5.0
[profiles.p]
transcriber = "missing"
sinks = ["also-missing"]
"#,
    );
    assert!(
        issues.len() >= 4,
        "expected all four problems at once, got: {issues:#?}"
    );
}

// ── things that are legal but probably unintended ────────────────────────────

#[test]
fn pre_roll_in_on_demand_mode_warns_that_it_does_nothing() {
    let issues = warnings("[defaults.capture]\nmode = \"on_demand\"\npre_roll_ms = 500\n");
    assert_reports(&issues, "profiles.default.capture.pre_roll_ms", "no effect");
}

#[test]
fn keeping_gap_audio_in_on_demand_mode_warns_about_the_fallback() {
    let issues = warnings(
        "[defaults.capture]\nmode = \"on_demand\"\npre_roll_ms = 0\nidle_release_secs = 0\n",
    );
    assert_reports(&issues, "profiles.default.continuation.gap", "behave as");
}

#[test]
fn a_text_only_sink_on_a_profile_without_a_transcriber_warns() {
    // Otherwise it silently never fires, and the user has no idea why.
    let issues = warnings(
        r#"
[sinks.clip]
type = "clipboard"
[profiles.p]
transcriber = false
sinks = ["clip"]
"#,
    );
    assert_reports(&issues, "profiles.p.sinks", "will always be skipped");
}

#[test]
fn a_sink_listed_twice_warns_that_it_runs_twice() {
    let issues = warnings(
        r#"
[sinks.s]
type = "command"
cmd = ["true"]
[profiles.p]
sinks = ["s", "s"]
"#,
    );
    assert_reports(&issues, "profiles.p.sinks", "listed twice");
}

#[test]
fn a_defined_but_unreferenced_sink_warns() {
    // Usually a rename that was only half applied.
    let issues = warnings(
        r#"
[sinks.orphan]
type = "command"
cmd = ["true"]
[profiles.p]
"#,
    );
    assert_reports(&issues, "sinks.orphan", "no profile uses it");
}

#[test]
fn feedback_with_no_presenters_warns_that_nothing_is_shown() {
    let issues = warnings("[feedback]\npresenters = []\n");
    assert_reports(&issues, "feedback.presenters", "nothing will be shown");
}

#[test]
fn warnings_do_not_prevent_loading() {
    let loaded =
        try_load("[defaults.capture]\nmode = \"on_demand\"\n").expect("a warning is not a failure");
    assert!(!loaded.warnings.is_empty());
}

// ── malformed input ──────────────────────────────────────────────────────────

#[test]
fn a_syntax_error_names_the_file_it_came_from() {
    let error = Config::from_layers(&[
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("conf.d/20-broken.toml", "[profiles.p\n"),
    ])
    .expect_err("unbalanced bracket should not parse");

    let rendered = error.to_string();
    assert!(
        rendered.contains("conf.d/20-broken.toml"),
        "the user needs to know which file: {rendered}"
    );
}

#[test]
fn transcriber_true_is_rejected_with_an_explanation() {
    // `false` disables transcription, so `true` looks like it should enable it — but there
    // is nothing for it to name.
    let error = try_load("[profiles.p]\ntranscriber = true\n").expect_err("should be rejected");
    let rendered = error.to_string();
    assert!(
        rendered.contains("transcriber name"),
        "should explain what was expected: {rendered}"
    );
}
