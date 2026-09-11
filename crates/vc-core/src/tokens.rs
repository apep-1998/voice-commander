//! Substituting session values into user-written commands, paths and templates.
//!
//! This is the interface between a recording and whatever the user told us to do with it.
//! It is deliberately dumb: `{audio_path}` becomes a path and nothing else happens. No shell
//! is involved, and substitution happens per argument, so a path containing a space cannot
//! turn into two arguments and a transcript containing `;` cannot turn into a second command.

use std::collections::BTreeMap;
use std::path::Path;

use crate::session::SessionRecord;

/// The values available to a template.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    values: BTreeMap<&'static str, String>,
}

/// Every token name that can appear, for documentation and for validation.
pub const TOKEN_NAMES: &[&str] = &[
    "audio_path",
    "text_path",
    "text",
    "session_id",
    "session_dir",
    "profile",
    "duration_ms",
    "started_at",
    "date",
    "language",
];

impl Tokens {
    /// Build the token set for a finished session.
    ///
    /// `text` and `text_path` are absent when the profile had no transcriber — the tokens
    /// still exist, and expand to nothing, so a command written for a profile with
    /// transcription does not break when pointed at one without it.
    pub fn for_session(
        record: &SessionRecord,
        text: Option<&str>,
        text_path: Option<&Path>,
    ) -> Self {
        let session_dir = record
            .audio
            .path
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();

        let started = record.started_at.to_offset(time::UtcOffset::UTC);
        let date = format!(
            "{:04}-{:02}-{:02}",
            started.year(),
            u8::from(started.month()),
            started.day()
        );

        let mut values = BTreeMap::new();
        values.insert("audio_path", record.audio.path.display().to_string());
        values.insert(
            "text_path",
            text_path
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        );
        values.insert("text", text.unwrap_or_default().to_owned());
        values.insert("session_id", record.id.to_string());
        values.insert("session_dir", session_dir);
        values.insert("profile", record.profile.clone());
        values.insert("duration_ms", record.audio.duration_ms.to_string());
        values.insert(
            "started_at",
            started
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        );
        values.insert("date", date);
        values.insert(
            "language",
            record
                .transcript
                .as_ref()
                .and_then(|transcript| transcript.language.clone())
                .unwrap_or_default(),
        );

        Self { values }
    }

    /// Override or add a value, for tokens a caller knows better than the record does.
    pub fn set(&mut self, name: &'static str, value: impl Into<String>) {
        self.values.insert(name, value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Replace `{token}` occurrences.
    ///
    /// Unknown tokens are left exactly as written rather than blanked. A shell script or a
    /// JSON body template may legitimately contain braces, and silently emptying something
    /// that merely looks like a token would corrupt it. [`Tokens::unknown_in`] exists so a
    /// caller can still warn about a likely typo.
    pub fn expand(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;

        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open..];
            match token_at(after) {
                Some((name, consumed)) => {
                    match self.values.get(name) {
                        Some(value) => out.push_str(value),
                        // A plausible name we have no value for: leave it exactly as written,
                        // so a typo is visible in the output rather than turning into a hole.
                        None => out.push_str(&after[..consumed]),
                    }
                    rest = &after[consumed..];
                }
                None => {
                    // Not a token — a JSON object, a shell brace expansion, anything. Emit
                    // the brace and carry on scanning from the next character, so a real
                    // token nested inside is still found.
                    out.push('{');
                    rest = &after[1..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Expand each argument separately.
    ///
    /// Per-argument is the whole point: a transcript is untrusted text that came out of a
    /// microphone, and expanding a whole command line as one string before splitting it
    /// would let "delete everything; rm -rf /" become two commands.
    pub fn expand_args(&self, args: &[String]) -> Vec<String> {
        args.iter().map(|arg| self.expand(arg)).collect()
    }

    /// Token-looking names in `template` that this set has no value for.
    pub fn unknown_in(&self, template: &str) -> Vec<String> {
        let mut unknown = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let after = &rest[open..];
            match token_at(after) {
                Some((name, consumed)) => {
                    if !self.values.contains_key(name) && !unknown.iter().any(|u| u == name) {
                        unknown.push(name.to_owned());
                    }
                    rest = &after[consumed..];
                }
                None => rest = &after[1..],
            }
        }
        unknown
    }

    /// The environment variables a child process receives, `VC_`-prefixed.
    ///
    /// The same values as the tokens, so a simple script can use `$1` and a more involved one
    /// can read `$VC_TEXT` without the caller having to choose for them.
    pub fn env(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(name, value)| (format!("VC_{}", name.to_uppercase()), value.clone()))
            .collect()
    }
}

/// Read a `{token}` at the start of `text`, returning its name and how much it spans.
///
/// A token name is lowercase letters, digits and underscores — which is what every real
/// token is, and what nothing in a JSON object or a shell brace expansion looks like. Being
/// strict here is what lets `{"text": "{text}"}` expand the inner token instead of the outer
/// brace swallowing it.
fn token_at(text: &str) -> Option<(&str, usize)> {
    let rest = text.strip_prefix('{')?;
    let end = rest.find('}')?;
    let name = &rest[..end];
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    valid.then_some((name, end + 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioFormat, CaptureMode, GapMode, TriggerMode};
    use crate::session::{
        AudioSummary, CaptureSummary, LevelSummary, Outcome, SessionId, SessionRecord,
    };
    use time::macros::datetime;

    fn record() -> SessionRecord {
        SessionRecord {
            v: 1,
            id: SessionId::from_raw("20260911T144812Z-dictate-2hc8b"),
            profile: "dictate".to_owned(),
            trigger: TriggerMode::PushToTalk,
            started_at: datetime!(2026-09-11 14:48:12 UTC),
            finalized_at: None,
            capture: CaptureSummary {
                mode: CaptureMode::Preroll,
                device: "mic".to_owned(),
                configured_pre_roll_ms: 500,
                gap: GapMode::Keep,
                gap_downgraded_to: None,
            },
            segments: Vec::new(),
            continuations: 0,
            audio: AudioSummary {
                path: "/data/recordings/2026/09/11/sess/audio.wav".into(),
                format: AudioFormat::Wav,
                sample_rate: 16_000,
                channels: 1,
                bytes: 1_000,
                duration_ms: 2_400,
            },
            levels: LevelSummary {
                peak_dbfs: -10.0,
                mean_rms_dbfs: -30.0,
                speech_ms: 2_000,
                silence_ms: 400,
                clipped_samples: 0,
            },
            warnings: Vec::new(),
            transcript: None,
            sinks: Vec::new(),
            outcome: Outcome::Ok,
        }
    }

    fn tokens() -> Tokens {
        Tokens::for_session(
            &record(),
            Some("open my calendar"),
            Some(Path::new("/t/out.txt")),
        )
    }

    #[test]
    fn the_documented_tokens_all_resolve() {
        // TOKEN_NAMES is what the documentation promises; anything missing from the set is a
        // token the docs claim exists and the code does not provide.
        let tokens = tokens();
        for name in TOKEN_NAMES {
            assert!(
                tokens.get(name).is_some(),
                "{name} is documented but has no value"
            );
        }
    }

    #[test]
    fn session_values_are_substituted() {
        let tokens = tokens();
        assert_eq!(
            tokens.expand("{audio_path}"),
            "/data/recordings/2026/09/11/sess/audio.wav"
        );
        assert_eq!(tokens.expand("{text}"), "open my calendar");
        assert_eq!(tokens.expand("{profile}"), "dictate");
        assert_eq!(tokens.expand("{duration_ms}"), "2400");
        assert_eq!(tokens.expand("{date}"), "2026-09-11");
        assert_eq!(
            tokens.expand("{session_dir}"),
            "/data/recordings/2026/09/11/sess"
        );
    }

    #[test]
    fn tokens_can_appear_inside_a_longer_string() {
        assert_eq!(
            tokens().expand("~/notes/{date}.md"),
            "~/notes/2026-09-11.md"
        );
        assert_eq!(
            tokens().expand("- {started_at} {text}\n"),
            "- 2026-09-11T14:48:12Z open my calendar\n"
        );
    }

    #[test]
    fn a_profile_without_transcription_expands_text_to_nothing() {
        // A command written for a transcribing profile should not break when pointed at one
        // that only records.
        let tokens = Tokens::for_session(&record(), None, None);
        assert_eq!(tokens.expand("{text}"), "");
        assert_eq!(tokens.expand("{text_path}"), "");
        assert!(!tokens.expand("{audio_path}").is_empty());
    }

    #[test]
    fn unknown_tokens_are_left_alone_rather_than_blanked() {
        // A JSON body template or a shell script may legitimately contain braces, and
        // silently emptying something that merely looks like a token would corrupt it.
        let tokens = tokens();
        assert_eq!(tokens.expand("{nope}"), "{nope}");
        assert_eq!(
            tokens.expand(r#"{"text": "{text}"}"#),
            r#"{"text": "open my calendar"}"#
        );
    }

    #[test]
    fn an_unmatched_brace_is_just_text() {
        assert_eq!(tokens().expand("100% { of it"), "100% { of it");
        assert_eq!(tokens().expand("}{"), "}{");
    }

    #[test]
    fn a_token_nested_inside_other_braces_is_still_found() {
        // The outer brace of a JSON object must not swallow the token inside it.
        let tokens = tokens();
        assert_eq!(
            tokens.expand(r#"{"text": "{text}", "n": {duration_ms}}"#),
            r#"{"text": "open my calendar", "n": 2400}"#
        );
    }

    #[test]
    fn likely_typos_are_reported_but_json_braces_are_not() {
        let tokens = tokens();
        assert_eq!(
            tokens.unknown_in("{audio_pth}"),
            vec!["audio_pth".to_owned()]
        );
        assert!(tokens.unknown_in("{audio_path}").is_empty());
        // Not plausible token names, so not reported as typos.
        assert!(tokens.unknown_in(r#"{"a": 1}"#).is_empty());
        assert!(tokens.unknown_in("{}").is_empty());
    }

    #[test]
    fn each_argument_is_expanded_on_its_own() {
        // This is what stops a transcript from becoming extra arguments. The whole string is
        // never reassembled, so there is nothing for a shell to re-split.
        let mut tokens = tokens();
        tokens.set("text", "delete everything; rm -rf /");

        let args = tokens.expand_args(&[
            "/bin/echo".to_owned(),
            "{text}".to_owned(),
            "{audio_path}".to_owned(),
        ]);
        assert_eq!(args.len(), 3, "substitution must not create arguments");
        assert_eq!(args[1], "delete everything; rm -rf /");
    }

    #[test]
    fn a_path_with_spaces_stays_one_argument() {
        let mut tokens = tokens();
        tokens.set("audio_path", "/home/u/my recordings/audio.wav");

        let args = tokens.expand_args(&["cp".to_owned(), "{audio_path}".to_owned()]);
        assert_eq!(args[1], "/home/u/my recordings/audio.wav");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn the_same_values_are_available_as_environment_variables() {
        // So a one-line script can use $1 and a real program can read $VC_TEXT, without the
        // caller deciding which style the user must write in.
        let env = tokens().env();
        assert_eq!(
            env.get("VC_TEXT").map(String::as_str),
            Some("open my calendar")
        );
        assert_eq!(env.get("VC_PROFILE").map(String::as_str), Some("dictate"));
        assert!(env.contains_key("VC_AUDIO_PATH"));
    }
}
