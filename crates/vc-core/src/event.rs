//! The event stream.
//!
//! This is a **public, versioned contract**, not an internal detail. No graphical indicator
//! ships with voice-commander; what ships is this stream, and the deliberate consequence is
//! that an overlay written later has no more access to the daemon than a twenty-line shell
//! script piping `voice-commander events --follow` into a status bar. Neither is privileged.
//!
//! Two indicators are anticipated, and the event set is shaped around what each one needs:
//!
//! **A listening indicator**, visible only while the user is speaking, driven by
//! [`Event::RecordingStarted`] → [`Event::Level`] → [`Event::RecordingStopped`], with
//! [`Event::InputWarning`] for "your microphone is muted" and [`Event::CooldownStarted`] /
//! [`Event::RecordingResumed`] so it can stay up across a continuation instead of flickering.
//!
//! **A processing indicator**, listing every callback with its progress. This is why
//! [`Event::PipelineStarted`] carries the *complete* list of sinks up front rather than
//! announcing each one as it starts: a progress list has to be drawn in full, greyed out,
//! before anything happens. Each row then resolves on its own [`Event::SinkFinished`], and
//! the panel closes after [`Event::PipelineFinished`].
//!
//! Every event is one line of JSON. Adding a variant or an optional field is backwards
//! compatible; anything else means bumping [`crate::EVENT_SCHEMA_VERSION`] and saying so in
//! `docs/events.md`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

use crate::session::{rfc3339, InputWarningKind, Outcome, SessionId, SinkOutcome, StopReason};

/// One line of the event stream.
///
/// The envelope carries what every consumer needs to route an event — when, which session,
/// which profile — and flattens the event's own fields alongside them, so a consumer reads
/// `.event` to switch and the payload keys directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Schema version. A consumer that does not recognise it should say so rather than
    /// guess.
    pub v: u32,
    #[serde(with = "rfc3339")]
    pub ts: OffsetDateTime,
    /// Absent for daemon-wide events that belong to no session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(flatten)]
    pub event: Event,
}

impl Envelope {
    /// Wrap an event that belongs to a session.
    pub fn for_session(
        ts: OffsetDateTime,
        session: SessionId,
        profile: impl Into<String>,
        event: Event,
    ) -> Self {
        Self {
            v: crate::EVENT_SCHEMA_VERSION,
            ts,
            session: Some(session),
            profile: Some(profile.into()),
            event,
        }
    }

    /// Wrap an event that belongs to the daemon rather than to any one session.
    pub fn for_daemon(ts: OffsetDateTime, event: Event) -> Self {
        Self {
            v: crate::EVENT_SCHEMA_VERSION,
            ts,
            session: None,
            profile: None,
            event,
        }
    }

    /// Render as one line of JSON, without the trailing newline.
    pub fn to_ndjson(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse one line of JSON.
    pub fn from_ndjson(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// A sink as it appears in the plan announced by [`Event::PipelineStarted`].
///
/// Everything an indicator needs to draw the row before the sink has done anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedSink {
    /// Position in the fan-out, and the key that [`Event::SinkStarted`] and
    /// [`Event::SinkFinished`] refer back to. Not the name: the same sink may be listed
    /// twice, and two rows that share an identifier cannot be told apart.
    pub id: u32,
    pub name: String,
    /// The sink's `type`, so a row can carry an icon without reading the config.
    pub kind: String,
    /// Whether this sink will be skipped if there is no transcript — which lets an indicator
    /// show it as conditional from the start.
    pub requires_text: bool,
}

/// Why the capture device was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCloseReason {
    /// `idle_release_secs` elapsed. The battery-saving path.
    Idle,
    /// The recording finished, in `on_demand` mode.
    RecordingEnded,
    /// The device disappeared — unplugged, or the default source changed.
    Lost,
    /// The daemon is shutting down or reloading.
    Shutdown,
}

/// Where an error came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Capture,
    Storage,
    Transcription,
    Sink,
    Config,
}

/// Everything the daemon announces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    // ── daemon lifecycle ────────────────────────────────────────────────────
    /// The daemon is up and accepting commands.
    DaemonReady {
        version: String,
        socket: PathBuf,
    },
    /// Configuration was reloaded. `warnings` is the count, so an indicator can prompt the
    /// user to go and read them.
    ConfigReloaded {
        warnings: usize,
    },
    /// The capture device was opened, with the format actually negotiated — which may not be
    /// what was asked for.
    DeviceOpened {
        device: String,
        sample_rate: u32,
        channels: u16,
    },
    DeviceClosed {
        reason: DeviceCloseReason,
    },

    // ── while the user is speaking ──────────────────────────────────────────
    /// Recording began. `pre_roll_ms` is how much audio from *before* the keypress was
    /// actually available, which is not necessarily the configured amount.
    RecordingStarted {
        segment: u32,
        pre_roll_ms: u32,
        device: String,
    },
    /// A periodic level measurement, throttled to `feedback.level_interval_ms`.
    ///
    /// Emitted only while recording. This is what a meter is drawn from, and what tells a
    /// user their voice is actually arriving rather than the recording merely being on.
    Level {
        rms_dbfs: f32,
        peak_dbfs: f32,
        /// Above the speech threshold, as opposed to room tone.
        speech: bool,
        clipping: bool,
    },
    /// Something about the input the user should act on.
    InputWarning {
        kind: InputWarningKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A segment ended. `reason` distinguishes a normal release from the watchdog firing,
    /// which is worth showing differently — one is expected, the other means a keybind is
    /// misconfigured.
    RecordingStopped {
        segment: u32,
        duration_ms: u64,
        reason: StopReason,
    },
    /// The key was released and the continuation window has opened. An indicator should stay
    /// visible for `ms` rather than disappearing, since the session is not over yet.
    CooldownStarted {
        ms: u32,
    },
    /// The user pressed again inside the window, so this is the same session continuing.
    RecordingResumed {
        segment: u32,
        /// How long after the release the user pressed again. Clusters just under the
        /// configured cooldown mean the window is too short.
        resumed_after_ms: u64,
    },
    /// The session was abandoned; nothing downstream will run.
    SessionCancelled {
        reason: String,
    },
    /// The audio is written. The listening indicator's cue to disappear.
    SessionFinalized {
        audio_path: PathBuf,
        total_ms: u64,
        segments: u32,
    },

    // ── after the user stops speaking ───────────────────────────────────────
    /// The pipeline is about to run, announcing its complete plan.
    ///
    /// The full sink list arrives here, before any of them start, specifically so a progress
    /// panel can be drawn complete and greyed out. Learning about each sink as it began
    /// would mean a list that grows while the user watches it.
    PipelineStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcriber: Option<String>,
        sinks: Vec<PlannedSink>,
    },
    TranscribeStarted {
        transcriber: String,
    },
    TranscribeDone {
        chars: usize,
        latency_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    TranscribeFailed {
        error: String,
        latency_ms: u64,
    },
    SinkStarted {
        id: u32,
        name: String,
    },
    /// A callback resolved — the tick, cross or greyed-out row.
    SinkFinished {
        id: u32,
        name: String,
        outcome: SinkOutcome,
        latency_ms: u64,
        attempts: u32,
    },
    /// Every callback has resolved. After this plus `feedback.processing_linger_ms`, a
    /// progress panel should close.
    PipelineFinished {
        ok: usize,
        failed: usize,
        skipped: usize,
        total_ms: u64,
        outcome: Outcome,
    },

    // ── failures that are nobody's callback ─────────────────────────────────
    Error {
        stage: Stage,
        message: String,
    },
}

impl Event {
    /// The value of the `event` field, for filtering.
    ///
    /// A presenter's `events = [...]` allow-list is matched against this, so a bar module can
    /// ask for `recording_started` and `recording_stopped` and not be woken twenty times a
    /// second by level updates.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DaemonReady { .. } => "daemon_ready",
            Self::ConfigReloaded { .. } => "config_reloaded",
            Self::DeviceOpened { .. } => "device_opened",
            Self::DeviceClosed { .. } => "device_closed",
            Self::RecordingStarted { .. } => "recording_started",
            Self::Level { .. } => "level",
            Self::InputWarning { .. } => "input_warning",
            Self::RecordingStopped { .. } => "recording_stopped",
            Self::CooldownStarted { .. } => "cooldown_started",
            Self::RecordingResumed { .. } => "recording_resumed",
            Self::SessionCancelled { .. } => "session_cancelled",
            Self::SessionFinalized { .. } => "session_finalized",
            Self::PipelineStarted { .. } => "pipeline_started",
            Self::TranscribeStarted { .. } => "transcribe_started",
            Self::TranscribeDone { .. } => "transcribe_done",
            Self::TranscribeFailed { .. } => "transcribe_failed",
            Self::SinkStarted { .. } => "sink_started",
            Self::SinkFinished { .. } => "sink_finished",
            Self::PipelineFinished { .. } => "pipeline_finished",
            Self::Error { .. } => "error",
        }
    }

    /// Whether this event belongs to the listening indicator.
    ///
    /// Used to honour `feedback.listening = false` without threading that decision through
    /// every emit site.
    pub fn is_listening_feedback(&self) -> bool {
        matches!(
            self,
            Self::RecordingStarted { .. }
                | Self::Level { .. }
                | Self::InputWarning { .. }
                | Self::RecordingStopped { .. }
                | Self::CooldownStarted { .. }
                | Self::RecordingResumed { .. }
                | Self::SessionFinalized { .. }
        )
    }

    /// Whether this event belongs to the processing indicator.
    pub fn is_processing_feedback(&self) -> bool {
        matches!(
            self,
            Self::PipelineStarted { .. }
                | Self::TranscribeStarted { .. }
                | Self::TranscribeDone { .. }
                | Self::TranscribeFailed { .. }
                | Self::SinkStarted { .. }
                | Self::SinkFinished { .. }
                | Self::PipelineFinished { .. }
        )
    }
}
