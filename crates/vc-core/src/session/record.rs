//! What gets written to `session.json` beside every recording.
//!
//! This is the tuning data. The point of recording key-down and key-up separately, of
//! tracking how much speech landed inside the pre-roll window, and of noting how close to
//! the cooldown deadline a continuation arrived, is that after a few weeks of real use those
//! numbers say what `pre_roll_ms` and `cooldown_ms` *should* be — rather than leaving the
//! user to guess and re-guess.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

use super::SessionId;
use crate::config::{AudioFormat, CaptureMode, GapMode, TriggerMode};

/// Serialization format for every timestamp here and in the event stream: RFC 3339, so that
/// `jq` and every other tool can read it without being told how.
pub mod rfc3339 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(value: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        value
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let raw = String::deserialize(d)?;
        OffsetDateTime::parse(&raw, &Rfc3339).map_err(serde::de::Error::custom)
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            value: &Option<OffsetDateTime>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => super::serialize(value, s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<OffsetDateTime>, D::Error> {
            let raw = Option::<String>::deserialize(d)?;
            raw.map(|raw| OffsetDateTime::parse(&raw, &Rfc3339))
                .transpose()
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Why a segment of recording ended.
///
/// Worth distinguishing: a session that ends on `Watchdog` means a release keybind was
/// missed, which is a configuration problem the user would otherwise never diagnose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The key was released, or the toggle was pressed again. The normal case.
    Released,
    /// `max_recording_secs` elapsed — almost always a release keybind that never fired.
    Watchdog,
    /// `max_total_secs` elapsed across the whole session.
    TotalLimit,
    /// `continuation.max_segments` was reached.
    SegmentLimit,
    /// The user cancelled.
    Cancelled,
    /// The daemon is shutting down.
    Shutdown,
}

/// One press-and-release within a session. A session has more than one of these exactly when
/// the user continued it during the cooldown window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub index: u32,
    #[serde(with = "rfc3339")]
    pub key_down_at: OffsetDateTime,
    #[serde(with = "rfc3339::option")]
    pub key_up_at: Option<OffsetDateTime>,
    /// How much audio from before the keypress was actually available. Less than the
    /// configured `pre_roll_ms` when the device had only just opened.
    pub pre_roll_ms: u32,
    /// How much of that pre-roll contained speech rather than room tone.
    ///
    /// This is the number that says whether `pre_roll_ms` is set correctly. Consistently
    /// near the configured value means speech is still being clipped and it should go up;
    /// consistently zero means it is only costing memory.
    pub speech_in_pre_roll_ms: u32,
    /// Where this segment starts within the finished audio file.
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    /// For a continuation, how long after the previous release the user pressed again.
    ///
    /// The other half of the tuning story: if these cluster just under `cooldown_ms`, the
    /// window is too short and continuations are being missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_after_ms: Option<u64>,
    pub stop_reason: StopReason,
}

/// What the capture layer was actually doing, as opposed to what was configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSummary {
    pub mode: CaptureMode,
    /// The device the audio actually came from, by name.
    pub device: String,
    pub configured_pre_roll_ms: u32,
    pub gap: GapMode,
    /// Set when `gap = "keep"` was requested but the capture mode could not honour it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_downgraded_to: Option<GapMode>,
}

/// The stored recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSummary {
    pub path: PathBuf,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// Signal statistics for the session, summarised from the per-block level measurements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelSummary {
    pub peak_dbfs: f32,
    pub mean_rms_dbfs: f32,
    /// Audio above the speech threshold. Compare against `duration_ms` to see how much of a
    /// recording was the user hesitating.
    pub speech_ms: u64,
    pub silence_ms: u64,
    pub clipped_samples: u64,
}

/// Something about the input that the user would want to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputWarningKind {
    /// No capture device could be opened at all.
    NoDevice,
    /// The source exists but is muted — the single most common cause of an empty recording.
    DeviceMuted,
    /// Nothing above the silence floor for long enough to be worth saying.
    Silence,
    /// Audible, but quiet enough that transcription accuracy will suffer.
    TooQuiet,
    /// Samples at full scale; the input gain is too high.
    Clipping,
    /// The audio backend dropped buffers, so the recording has gaps.
    Xrun,
}

/// The result of transcription, when a profile asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    /// The name of the `[transcribers.*]` entry that produced this.
    pub transcriber: String,
    pub path: Option<PathBuf>,
    pub chars: usize,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why a sink did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The sink needs a transcript and the profile produced none.
    NoTranscript,
    /// Transcription was configured but failed.
    TranscriptionFailed,
    /// An earlier sink failed with `on_error = "fail_session"`.
    EarlierSinkFailed,
    /// The session was cancelled before the sink ran.
    Cancelled,
}

/// How one callback ended.
///
/// Modelled as a closed set rather than a boolean plus optional strings, because this is
/// precisely what an indicator renders: a tick, a cross, or a greyed-out row with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SinkOutcome {
    Ok,
    Failed { error: String },
    Skipped { reason: SkipReason },
}

impl SinkOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// One callback's execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkRecord {
    /// Position in the fan-out. Stable within a session, and what the event stream uses to
    /// correlate a `sink_finished` with the row it belongs to — a name would be ambiguous
    /// when the same sink is listed twice.
    pub id: u32,
    /// The name of the `[sinks.*]` entry.
    pub name: String,
    /// Its `type`, so an indicator can show an icon without consulting the config.
    pub kind: String,
    pub outcome: SinkOutcome,
    pub latency_ms: u64,
    /// How many attempts it took, including the successful one.
    pub attempts: u32,
}

/// How the session ended overall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything that was meant to run, ran.
    Ok,
    /// The audio was captured, but something downstream failed.
    Partial,
    /// Nothing usable came out of it.
    Failed,
    /// The user cancelled before it finished.
    Cancelled,
}

/// The complete record of one session, written to `session.json` and handed to callbacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Schema version, matching [`crate::EVENT_SCHEMA_VERSION`].
    pub v: u32,
    pub id: SessionId,
    pub profile: String,
    pub trigger: TriggerMode,
    #[serde(with = "rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "rfc3339::option")]
    pub finalized_at: Option<OffsetDateTime>,
    pub capture: CaptureSummary,
    pub segments: Vec<Segment>,
    /// How many times this session was continued during its cooldown window.
    pub continuations: u32,
    pub audio: AudioSummary,
    pub levels: LevelSummary,
    #[serde(default)]
    pub warnings: Vec<InputWarningKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptRecord>,
    #[serde(default)]
    pub sinks: Vec<SinkRecord>,
    pub outcome: Outcome,
}

impl SessionRecord {
    /// Total audio, across every segment.
    pub fn total_duration_ms(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| segment.duration_ms)
            .sum()
    }

    /// Whether a release keybind was missed, i.e. any segment hit the watchdog.
    ///
    /// A user seeing this repeatedly has a binding problem, not a voice problem.
    pub fn hit_watchdog(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.stop_reason == StopReason::Watchdog)
    }
}
