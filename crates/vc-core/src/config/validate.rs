//! Semantic checks that run after the configuration has parsed.
//!
//! The split matters: the type system catches "this should be a number", and this module
//! catches "this names a transcriber that does not exist" — the errors that actually happen
//! in practice, and that a user cannot diagnose from a serde message.

use super::adapter::SecretRef;
use super::error::Issue;
use super::sink::SinkKind;
use super::transcriber::TranscriberKind;
use super::{CaptureMode, Config, GapMode};

/// Sample rates worth storing speech at. Anything else is either lossy for no benefit or
/// wasteful for no benefit.
const VALID_SAMPLE_RATES: &[u32] = &[8_000, 16_000, 22_050, 24_000, 44_100, 48_000];

/// The outcome of checking a configuration.
#[derive(Debug, Default)]
pub struct Report {
    /// Problems that make the configuration unusable.
    pub errors: Vec<Issue>,
    /// Things that are legal but probably not what was meant.
    pub warnings: Vec<Issue>,
}

impl Report {
    fn error(&mut self, issue: Issue) {
        self.errors.push(issue);
    }

    fn warn(&mut self, issue: Issue) {
        self.warnings.push(issue);
    }
}

pub(super) fn check(config: &Config) -> Report {
    let mut report = Report::default();

    check_audio(config, &mut report);
    check_levels(config, &mut report);
    check_transcribers(config, &mut report);
    check_sinks(config, &mut report);
    check_presenters(config, &mut report);
    check_profiles(config, &mut report);
    check_unused(config, &mut report);

    report
}

fn check_audio(config: &Config, report: &mut Report) {
    if !VALID_SAMPLE_RATES.contains(&config.audio.sample_rate) {
        report.error(
            Issue::new(
                "audio.sample_rate",
                format!("{} is not a supported rate", config.audio.sample_rate),
            )
            .with_hint(format!("one of {VALID_SAMPLE_RATES:?}")),
        );
    }
    if !matches!(config.audio.channels, 1 | 2) {
        report.error(Issue::new(
            "audio.channels",
            format!("must be 1 or 2, got {}", config.audio.channels),
        ));
    }
    if config.audio.channels == 2 {
        report.warn(
            Issue::new(
                "audio.channels",
                "stereo doubles the file size and every speech model discards one channel",
            )
            .with_hint("use 1 unless something downstream genuinely needs two"),
        );
    }
}

fn check_levels(config: &Config, report: &mut Report) {
    let levels = &config.levels;
    for (path, value) in [
        ("levels.silence_dbfs", levels.silence_dbfs),
        ("levels.too_quiet_dbfs", levels.too_quiet_dbfs),
        ("levels.speech_dbfs", levels.speech_dbfs),
    ] {
        if value > 0.0 {
            report.error(
                Issue::new(path, format!("{value} is above full scale"))
                    .with_hint("dBFS values are negative; -40 is a quiet voice, -6 is loud"),
            );
        }
    }
    if levels.silence_dbfs >= levels.too_quiet_dbfs {
        report.error(
            Issue::new("levels.silence_dbfs", "must be below levels.too_quiet_dbfs")
                .with_hint("silence is quieter than merely too quiet"),
        );
    }
}

fn check_secret(path: &str, secret: &SecretRef, report: &mut Report) {
    match (&secret.env, &secret.command) {
        (None, None) => report.error(Issue::new(path, "no source for the API key").with_hint(
            "set `env = \"OPENAI_API_KEY\"` or `command = [\"pass\", \"show\", \"...\"]`",
        )),
        (Some(_), Some(_)) => report.warn(
            Issue::new(path, "both `env` and `command` are set; `env` wins")
                .with_hint("remove whichever is not intended"),
        ),
        (Some(name), None) if name.is_empty() => {
            report.error(Issue::new(format!("{path}.env"), "is empty"));
        }
        (None, Some(cmd)) if cmd.is_empty() => {
            report.error(Issue::new(format!("{path}.command"), "is empty"));
        }
        _ => {}
    }
}

fn check_url(path: &str, url: &str, report: &mut Report) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        report.error(
            Issue::new(path, format!("{url:?} is not an http(s) URL"))
                .with_hint("include the scheme, e.g. https://api.example.com/v1/transcribe"),
        );
    } else if url.starts_with("http://")
        && !url.contains("://localhost")
        && !url.contains("://127.")
    {
        report.warn(Issue::new(
            path,
            "sends audio over plain HTTP to a non-local host",
        ));
    }
}

fn check_transcribers(config: &Config, report: &mut Report) {
    for (name, transcriber) in &config.transcribers {
        let base = format!("transcribers.{name}");
        if transcriber.timeout_ms == 0 {
            report.error(Issue::new(format!("{base}.timeout_ms"), "must be non-zero"));
        }
        if transcriber.retry.attempts == 0 {
            report.error(
                Issue::new(format!("{base}.retry.attempts"), "must be at least 1")
                    .with_hint("1 means try once and do not retry"),
            );
        }
        match &transcriber.kind {
            TranscriberKind::Openai(openai) => {
                check_secret(&format!("{base}.api_key"), &openai.api_key, report);
                check_url(&format!("{base}.base_url"), &openai.base_url, report);
                if let Some(temperature) = openai.temperature {
                    if !(0.0..=1.0).contains(&temperature) {
                        report.error(Issue::new(
                            format!("{base}.temperature"),
                            format!("{temperature} is outside 0.0..=1.0"),
                        ));
                    }
                }
            }
            TranscriberKind::Http(http) => {
                check_url(&format!("{base}.url"), &http.url, report);
            }
            TranscriberKind::Command(command) => {
                if command.cmd.is_empty() {
                    report.error(Issue::new(format!("{base}.cmd"), "is empty"));
                }
            }
        }
    }
}

fn check_sinks(config: &Config, report: &mut Report) {
    for (name, sink) in &config.sinks {
        let base = format!("sinks.{name}");
        if sink.timeout_ms == 0 {
            report.error(Issue::new(format!("{base}.timeout_ms"), "must be non-zero"));
        }
        if sink.retry.attempts == 0 {
            report.error(Issue::new(
                format!("{base}.retry.attempts"),
                "must be at least 1",
            ));
        }
        match &sink.kind {
            SinkKind::Command(command) => {
                if command.cmd.is_empty() {
                    report.error(Issue::new(format!("{base}.cmd"), "is empty"));
                }
            }
            SinkKind::Http(http) => check_url(&format!("{base}.url"), &http.url, report),
            SinkKind::File(file) => {
                if file.path.is_empty() {
                    report.error(Issue::new(format!("{base}.path"), "is empty"));
                }
            }
            SinkKind::Clipboard(_) | SinkKind::Type(_) | SinkKind::Notify(_) => {}
        }
    }
}

fn check_presenters(config: &Config, report: &mut Report) {
    for (name, presenter) in &config.presenters {
        if let super::runtime::PresenterKind::Command(command) = &presenter.kind {
            if command.cmd.is_empty() {
                report.error(Issue::new(format!("presenters.{name}.cmd"), "is empty"));
            }
        }
    }

    let feedback = &config.feedback;
    for name in &feedback.presenters {
        if !config.presenters.contains_key(name) {
            let mut issue = Issue::new(
                "feedback.presenters",
                format!("no presenter named {name:?} is defined"),
            );
            if let Some(close) = closest(name, config.presenters.keys()) {
                issue = issue.with_hint(format!("did you mean {close:?}?"));
            }
            report.error(issue);
        }
    }
    if feedback.enabled && feedback.presenters.is_empty() {
        report.warn(
            Issue::new("feedback.presenters", "is empty, so nothing will be shown").with_hint(
                "add \"socket\" to make events readable by `voice-commander events --follow`",
            ),
        );
    }
    if feedback.enabled && feedback.level_interval_ms == 0 {
        report.error(
            Issue::new("feedback.level_interval_ms", "must be non-zero")
                .with_hint("this throttles level events; 50 gives a smooth 20 per second"),
        );
    }
}

fn check_profiles(config: &Config, report: &mut Report) {
    if config.profiles.is_empty() {
        report.error(
            Issue::new("profiles", "no profiles are defined")
                .with_hint("a profile is what a keybind selects; define at least one"),
        );
    }

    for (name, profile) in &config.profiles {
        let base = format!("profiles.{name}");

        if let Some(transcriber) = &profile.transcriber {
            if !config.transcribers.contains_key(transcriber) {
                let mut issue = Issue::new(
                    format!("{base}.transcriber"),
                    format!("no transcriber named {transcriber:?} is defined"),
                );
                if let Some(close) = closest(transcriber, config.transcribers.keys()) {
                    issue = issue.with_hint(format!("did you mean {close:?}?"));
                }
                report.error(issue);
            }
        }

        let mut seen = Vec::new();
        for sink_name in &profile.sinks {
            if !config.sinks.contains_key(sink_name) {
                let mut issue = Issue::new(
                    format!("{base}.sinks"),
                    format!("no sink named {sink_name:?} is defined"),
                );
                if let Some(close) = closest(sink_name, config.sinks.keys()) {
                    issue = issue.with_hint(format!("did you mean {close:?}?"));
                }
                report.error(issue);
            }
            if seen.contains(&sink_name) {
                report.warn(Issue::new(
                    format!("{base}.sinks"),
                    format!("{sink_name:?} is listed twice and will run twice"),
                ));
            }
            seen.push(sink_name);
        }

        check_profile_timing(&base, profile, report);
        check_profile_coherence(&base, name, profile, config, report);
    }
}

fn check_profile_timing(base: &str, profile: &super::Profile, report: &mut Report) {
    let session = &profile.session;
    if session.max_recording_secs == 0 {
        report.error(
            Issue::new(
                format!("{base}.session.max_recording_secs"),
                "must be non-zero",
            )
            .with_hint(
                "this is the watchdog that stops a recording when a release keybind is \
                     missed; disabling it means recording until the disk fills",
            ),
        );
    }
    if session.max_total_secs < session.max_recording_secs {
        report.error(Issue::new(
            format!("{base}.session.max_total_secs"),
            "must be at least session.max_recording_secs",
        ));
    }
    if profile.continuation.max_segments == 0 {
        report.error(Issue::new(
            format!("{base}.continuation.max_segments"),
            "must be at least 1",
        ));
    }
    if profile.capture.pre_roll_ms > 30_000 {
        report.error(
            Issue::new(
                format!("{base}.capture.pre_roll_ms"),
                format!(
                    "{}ms is more than the 30s maximum",
                    profile.capture.pre_roll_ms
                ),
            )
            .with_hint("the pre-roll window is held in memory continuously"),
        );
    }
}

/// Combinations that parse and validate individually but do not mean anything together.
fn check_profile_coherence(
    base: &str,
    name: &str,
    profile: &super::Profile,
    config: &Config,
    report: &mut Report,
) {
    if profile.capture.mode == CaptureMode::OnDemand {
        if profile.capture.pre_roll_ms > 0 {
            report.warn(
                Issue::new(
                    format!("{base}.capture.pre_roll_ms"),
                    "has no effect in on_demand mode, where nothing is captured before the \
                     keypress",
                )
                .with_hint("switch to mode = \"preroll\" to get the audio before the keypress"),
            );
        }
        if profile.capture.idle_release_secs > 0 {
            report.warn(Issue::new(
                format!("{base}.capture.idle_release_secs"),
                "has no effect in on_demand mode, where the device is already closed when idle",
            ));
        }
        if profile.continuation.gap == GapMode::Keep {
            report.warn(
                Issue::new(
                    format!("{base}.continuation.gap"),
                    "cannot keep gap audio in on_demand mode, where the device is closed \
                     during the gap; it will behave as \"drop\"",
                )
                .with_hint("set gap = \"drop\" to make that explicit"),
            );
        }
    }

    if profile.transcriber.is_none() {
        let text_only: Vec<&str> = profile
            .sinks
            .iter()
            .filter(|sink_name| {
                config
                    .sinks
                    .get(*sink_name)
                    .is_some_and(super::SinkConfig::needs_text)
            })
            .map(String::as_str)
            .collect();
        if !text_only.is_empty() {
            report.warn(
                Issue::new(
                    format!("{base}.sinks"),
                    format!(
                        "{text_only:?} need a transcript, but profile {name:?} has no \
                         transcriber, so they will always be skipped"
                    ),
                )
                .with_hint("set a transcriber, or set requires_text = false on those sinks"),
            );
        }
    }

    if profile.sinks.is_empty() && profile.transcriber.is_none() {
        report.warn(Issue::new(
            base,
            "has no transcriber and no sinks, so it only archives the recording",
        ));
    }
}

/// Things defined but never referenced. Usually a rename that was only half applied.
fn check_unused(config: &Config, report: &mut Report) {
    for name in config.transcribers.keys() {
        let used = config
            .profiles
            .values()
            .any(|profile| profile.transcriber.as_deref() == Some(name.as_str()));
        if !used {
            report.warn(Issue::new(
                format!("transcribers.{name}"),
                "is defined but no profile uses it",
            ));
        }
    }
    for name in config.sinks.keys() {
        let used = config
            .profiles
            .values()
            .any(|profile| profile.sinks.contains(name));
        if !used {
            report.warn(Issue::new(
                format!("sinks.{name}"),
                "is defined but no profile uses it",
            ));
        }
    }
    for name in config.presenters.keys() {
        if !config.feedback.presenters.contains(name) {
            report.warn(Issue::new(
                format!("presenters.{name}"),
                "is defined but feedback.presenters does not list it",
            ));
        }
    }
}

/// The defined name closest to `needle`, when one is close enough to be a likely typo.
fn closest<'a>(needle: &str, candidates: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    candidates
        .map(|candidate| (super::merge::edit_distance(needle, candidate), candidate))
        .filter(|(distance, _)| *distance * 3 <= needle.len().max(1))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}
